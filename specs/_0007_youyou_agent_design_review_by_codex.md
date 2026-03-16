# YouYou Agent Design Review by Codex

| Field | Value |
|---|---|
| Target | `specs/0006_youyou_agent_design.md` |
| Baseline | `specs/0005_youyou_demand.md`, `.chat/codex_review_standard.md` |
| Date | `2026-03-13` |
| Score | `82 / 100` |
| Verdict | `不通过，需先关闭高严重级别问题后再进入实现` |

## 总评

这版 `0006` 比上一轮明显更完整，核心模块、Hook patch、单会话槽、错误码和事件面都已经基本成型，文档也开始主动解释 Rust 异步语义下的实现约束。

但按照 `.chat/codex_review_standard.md` 的口径，这一版仍有 3 个高严重级别问题没有闭合：

1. `shutdown()` 与 `new_session()/resume_session()` 的关闭互斥不是原子的，`AGENT_SHUTDOWN` 契约仍可能被打穿。
2. compact 会改写上下文，但当前 `SessionEvent` 账本无法 replay 出实时路径看到的同一份消息序列。
3. `RunningTurn::join()` 的对外错误契约没有被当前结构真实承接，文档签名和行为不一致。

只要这些问题还在，总体结论就不应是“可直接开工”。

## 分项评分

| 维度 | 分数 | 说明 |
|---|---:|---|
| 需求覆盖度 | 26 / 30 | 大部分需求块已覆盖，注册接口、生命周期、Hook、记忆、错误和事件都有明确落点，但 compact/replay 与 turn 终态契约仍未闭环。 |
| 架构分层与模块边界 | 18 / 20 | 分层清楚，`AgentBuilder -> Agent -> Session -> TurnLoop` 的边界明确，外部依赖也基本通过 trait 下沉。 |
| 运行时正确性 | 13 / 25 | 存在 shutdown 并发竞态和 `RunningTurn::join()` 终态契约缺口，高风险。 |
| 扩展性与接口契约 | 10 / 15 | trait 设计总体可落地，但有“文档说可以这样用，现有结构却接不住”的公开契约问题。 |
| 持久化、恢复与记忆闭环 | 8 / 15 | 一般消息账本比上一轮清楚很多，但 compact 改写上下文后无法从事件流恢复，checkpoint 边界表达也不稳定。 |
| 可测试性与可观测性 | 7 / 10 | 错误码和事件已经较完整，但若没有补齐终态和回滚契约，关键边界仍不好验收。 |

## 历史问题复核

| ID | 上次严重级别 | 当前状态 | 说明 |
|---|---|---|---|
| R1 | 高 | 已关闭 | 关闭令牌链路已基本收敛为 `session_close_token -> turn_cancel_token`，唯一来源和传播关系写清楚了。 |
| R2 | 高 | 已关闭 | `resume_session()` 已改成单一路径的 `load -> rebuild -> claim once`。 |
| R3 | 高 | 部分关闭 | 普通 `SystemMessage` 的落盘规则补上了，但 compact 对历史消息的“替换”仍没有事件级表达，replay 仍不成立。 |
| R4 | 高 | 部分关闭 | `close()` / `shutdown()` 的幂等和句柄语义补了不少，但 shutdown 与 claim 的原子性仍不足。 |
| R5 | 中 | 部分关闭 | checkpoint 计数器语义已统一，但增量边界仍依赖易失的 `Vec` 下标。 |
| R6 | 中 | 已关闭 | storage 写失败已经明确进入事件流。 |
| R7 | 中 | 已关闭 | 未知 Tool 已有统一的 synthetic error 路径。 |

## 主要问题

### 1. `claim_session_slot()` 与 `shutdown()` 之间仍有竞态，shutdown 后仍可能 claim 新 Session

- 严重级别：高
- 需求基线：`0005:368-372`, `0005:444-447`, `0005:453-458`
- 设计证据：`0006:727-739`, `0006:848-855`
- 问题：
  `claim_session_slot()` 先读 `is_shutdown`，再获取 `active_session` 锁占槽；`shutdown()` 则先 `store(true)`，再走关闭流程。两者不在同一原子临界区内。并发时，`new_session()` / `resume_session()` 完全可能先读到 `false`，随后 `shutdown()` 把 Agent 标记为关闭，再由前者继续成功占槽。
- 影响：
  这会直接打破 `AGENT_SHUTDOWN` 契约，出现“Agent 已经 shutdown，但仍然成功创建/恢复 Session”的行为。随后 Plugin shutdown、资源释放、活跃 Session 槽状态都会进入未定义竞争区。
- 建议：
  把“生命周期状态判断”和“session slot claim”合并到同一把锁或同一原子状态机内。最简单的方式是把 `Agent` 生命周期改成受锁保护的 `Running / ShuttingDown / Shutdown`，并在同一临界区内同时检查生命周期状态和 slot 是否为空；只有两者都满足时才允许 claim。

### 2. compact 会改写上下文，但当前 `SessionEvent` 账本无法 replay 出实时路径看到的同一份消息序列

- 严重级别：高
- 需求基线：`0005:154-164`, `0005:236-246`, `0005:353-366`
- 设计证据：`0006:1007-1019`, `0006:1144-1154`, `0006:1244-1269`
- 问题：
  当前恢复路径会把 `load_session()` 读到的每个 `SessionEvent` 逐条映射回 `Message`。但 compact 不是“追加一条摘要消息”这么简单，它会把早期消息从 `ContextManager.messages()` 中替换掉。设计里只持久化了 compact 产出的 `SystemMessage(summary)`，却没有任何事件描述“哪些旧消息已经被这个 summary 取代”。
- 影响：
  恢复时会得到“原始早期消息 + summary”这两份信息，而实时路径在 compact 后只保留了 summary 和较新的消息。也就是说，设计文档宣称的“实时路径和恢复路径上下文完全一致”在 compact 场景下并不成立。这个问题不仅影响恢复，还会连带污染 token 预算、后续 compact 判定和记忆提取输入。
- 建议：
  需要给 compact 引入显式的持久化协议，而不是只落一个 `SystemMessage`。至少应补一个能表达“替换边界”的事件，例如 `ContextCompactionCheckpoint { replaces_until_seq, summary_message }`，恢复时按该边界丢弃被取代的旧前缀。另一种做法是引入可恢复的 context snapshot，但无论哪种方案，都必须让 replay 和实时路径可证明一致。

### 3. `RunningTurn::join()` 的错误返回契约没有被当前结构真实承接

- 严重级别：高
- 需求基线：`0005:355-365`, `0005:430-447`
- 设计证据：`0006:925-943`, `0006:962-987`, `0006:1778-1779`
- 问题：
  `RunningTurn` 现在只持有 `mpsc::Receiver<AgentEvent>`、`CancellationToken` 和 `JoinHandle<()>`。supervisor task 会把运行中的错误折叠成事件流，再把任务本身收口为 `() `结束。因此 `join()` 最多只能通过 `JoinHandle` 感知 panic，无法在事件已经被调用方消费之后再可靠地区分“正常完成”“用户取消”“Provider/Tool 错误但 Turn 正常收尾”这些终态。
- 影响：
  公开 API 注释和错误表写着 `join()` 会返回 `REQUEST_CANCELLED` / `INTERNAL_PANIC`，但当前结构实际上没有稳定的信息源来做这个判定。这会让调用方误以为 `join()` 是可依赖的终态接口，实际实现时却只能猜测，或者偷偷依赖调用方是否保留了最后一个事件。
- 建议：
  为 `RunningTurn` 增加独立的终态通道，例如 `oneshot::Receiver<TurnOutcome>`，由 supervisor 在退出前写入 `Completed / Cancelled / Panic / FatalError`。或者让 `task_handle` 直接返回 `Result<TurnOutcome>`。总之，`join()` 的语义必须由单独的终态账本承接，不能从已经被消费掉的事件流倒推。

## 次要问题

### 4. `last_checkpoint_message_index` 以 `Vec<Message>` 下标表达增量边界，在 compact 后并不稳定

- 严重级别：中
- 需求基线：`0005:296-316`
- 设计证据：`0006:899-902`, `0006:1122-1125`, `0006:1267-1269`, `0006:1674-1678`
- 问题：
  文档把 checkpoint 边界定义成 `ContextManager.messages()` 的下标，并断言 compact 不会影响后续索引偏移。但 `ContextManager` 的底层结构就是 `Vec<Message>`，compact 明确会“保留最近消息并用 summary 替换早期消息”。这会让旧消息后面的索引整体左移，`last_checkpoint_message_index` 不再指向原来的逻辑边界。
- 影响：
  即使不考虑恢复，仅在单次运行中发生 compact，也可能让后续 checkpoint 的增量切片跳过一段消息，或者重复提取已经 checkpoint 过的内容。
- 建议：
  不要再用 `Vec` 下标表达 checkpoint 边界。改用单调递增的消息序号、事件序号，或者直接用 `SessionEvent` ledger 的 sequence 作为 checkpoint 游标。这样 compact 改写内存结构时，不会破坏增量提取边界。

### 5. `SessionStart` Abort 的回滚只定义了 delete happy path，没有定义 delete 失败时的补偿策略

- 严重级别：中
- 需求基线：`0005:326-345`, `0005:366-372`, `0005:441-447`
- 设计证据：`0006:367`, `0006:839-842`, `0006:1759-1777`
- 问题：
  当前文档规定：先持久化 `session_config`，再跑 `SessionStart Hook`，若 Plugin `Abort` 则调用 `SessionStorage::delete_session(session_id)` 回滚。但没有定义 delete 失败时应该返回什么、是否保留 tombstone、后续 `list/find/resume` 应如何处理这个半失败 session。
- 影响：
  这会把 session 创建失败路径变成“内存里失败，存储里成功了一半”的脏状态。后面做 session 列表、恢复或清理时，行为会分叉。
- 建议：
  明确回滚失败的优先级和补偿协议。更稳妥的方案是把新建会话的 Metadata 写成 `pending`，`SessionStart Hook` 通过后再提交成 `active`；若 abort 或回滚失败，则把该记录标记为 `aborted`/`tombstoned`，并要求 `list/find/resume` 忽略这类记录。

## 修改优先级建议

### P0

- 把 `shutdown` 与 `claim_session_slot` 重构为同一原子状态机，消掉 `AGENT_SHUTDOWN` 竞态。
- 重新设计 compact 的持久化协议，保证 replay 能恢复出实时路径真正看到的上下文。
- 为 `RunningTurn` 增加独立的终态结果通道，重写 `join()` 契约。

### P1

- 用单调 sequence 替代 `last_checkpoint_message_index` 这种 `Vec` 下标游标。
- 定义 `SessionStart` Abort 回滚失败时的补偿策略和对外可见行为。
- 补一组文档级验收场景：`shutdown vs new_session` 并发、compact 后 resume、cancel 后 `join()`、compact 后 checkpoint、SessionStart abort + rollback failure。

## 复评门槛

下一个版本若想评为“通过”，建议至少满足以下条件：

1. 关闭上面 3 个高严重级别问题。
2. compact、resume、checkpoint 三条链路能够画出一条一致的账本闭环。
3. `RunningTurn::join()` 的最终语义可以直接写成测试，而不是依赖调用方约定。
4. `shutdown()`、`new_session()`、`resume_session()` 的并发关系有唯一实现基线。

## 建议新增的验收用例

- `shutdown()` 与 `new_session()` 并发，确认不会在 shutdown 后成功 claim session。
- 会话发生 compact 后立刻 `resume_session()`，确认恢复出的消息序列与实时路径一致。
- 用户 cancel 后消费完事件流再调用 `RunningTurn::join()`，确认能稳定得到同一终态。
- checkpoint 前后发生 compact，确认不会漏提或重复提取消息。
- `SessionStart` Hook abort 且 `delete_session()` 失败，确认 list/find/resume 不会看到脏 session。
