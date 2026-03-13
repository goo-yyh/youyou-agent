# YouYou Agent Design Review by Codex

| Field | Value |
|---|---|
| Document ID | 0007 |
| Type | review |
| Status | Draft |
| Created | 2026-03-13 |
| Reviewed Target | 0006 |
| Requirements Baseline | 0005 |
| Reviewer | Codex |

---

## 1. Review Summary

本次评审以 `specs/0005_youyou_demand.md` 为基线，对 `specs/0006_youyou_agent_design.md` 做一致性、可实现性和架构风险审查。

结论：`0006` 的模块拆分、职责边界、Prompt 附录和事件/持久化主流程设计较完整，已经具备较强的实现指导价值；但当前版本仍存在 4 个高风险缺口和 2 个中风险缺口，尤其集中在 Session 生命周期、取消模型、多 Provider 路由以及会话历史一致性上。以当前状态直接进入实现，后续返工概率较高。

**总分：78 / 100**

| 维度 | 分值 | 说明 |
|---|---:|---|
| 需求覆盖度 | 33 / 35 | 核心模块、插件、Hook、Session、Memory、Prompt 基本都已覆盖 |
| 架构一致性 | 21 / 30 | 多 Provider 路由、消息模型和 Hook 数据约定存在不一致 |
| 生命周期与并发安全 | 10 / 20 | Session 关闭屏障和取消令牌边界不够严谨 |
| 可实现性与可演进性 | 14 / 15 | 文档颗粒度较细，但部分 trait 契约不足以支撑设计目标 |

**建议结论：** 先修正文档中的高风险项，再进入代码实现。

---

## 2. Findings

### 2.1 高风险：`active_session` 的放行条件早于关闭流程完成，违反“关闭后才可新建/恢复”的需求

需求基线要求“运行中的 Session 必须关闭后才能创建或恢复另一个 Session”，并再次强调“关闭后才可创建或恢复另一个 Session”。见 `specs/0005_youyou_demand.md:47-48` 与 `specs/0005_youyou_demand.md:357-357`。

但设计稿在 `new_session()` 中采用“`active_session` 为 `Some` 且 `running_guard.strong_count() > 0` 才拒绝”的规则，见 `specs/0006_youyou_agent_design.md:596-606` 与 `specs/0006_youyou_agent_design.md:708-758`。这意味着只要 `Session` handle 和后台 Turn task 都释放了 `running_guard`，即使 `SessionEnd Hook`、记忆提取或后台关闭任务仍未完成，也可能允许新的 Session 提前创建。

这会直接带来两个问题。第一，单会话模型的关闭屏障被削弱，旧 Session 的收尾逻辑可能与新 Session 的启动逻辑并行执行。第二，`Drop` 路径下的 best-effort close 与 `running_guard` 解绑，文档中的“close 完成后再释放”与“strong_count == 0 即可放行”本身也存在内部矛盾。

建议修改为显式的 Session 槽状态机，例如 `Idle / Running / Closing`。`new_session()` 与 `resume_session()` 只能在 `Idle` 状态放行；只有当关闭流程完整结束后，才能把槽位切回 `Idle`。`running_guard` 可以保留给后台 task 生命周期管理，但不应作为创建新 Session 的准入条件。

### 2.2 高风险：取消令牌定义在 Session 级别，无法保证“每轮取消一次、下一轮可继续”

需求文档将取消语义定义为“取消正在进行的请求”，并明确约束取消时 Model Provider、Tool、SessionStorage 的行为，见 `specs/0005_youyou_demand.md:63-63` 与 `specs/0005_youyou_demand.md:348-355`。

设计稿把 `CancellationToken` 放在 `SessionState.cancel_token` 中，同时 `RunningTurn` 也直接持有该 token，见 `specs/0006_youyou_agent_design.md:649-678` 与 `specs/0006_youyou_agent_design.md:689-695`。`Agent::shutdown()` 也复用 `ActiveSessionHandle.cancel_token` 来取消当前 Turn，见 `specs/0006_youyou_agent_design.md:610-614`。

问题在于，文档没有定义 token 的重建时机。如果某一轮 Turn 已调用 `cancel()`，同一个 Session 后续 `send_message()` 很容易继承一个已经 canceled 的 token，造成“这个 Session 永久不可再用”或“新 Turn 启动即被取消”的异常行为。与此同时，“取消当前 Turn”和“关闭整个 Session”也被混进了同一套令牌语义，边界不清。

建议把取消模型拆成两层：`session_close_token` 只用于 Session 级关闭；每次 `send_message()` 创建独立的 `turn_cancel_token`，由 `RunningTurn` 持有并在 Turn 结束后销毁。若需要联动关闭，可让 `turn_cancel_token` 作为 `session_close_token` 的子 token，但不能复用同一个根 token。

### 2.3 高风险：`compact_model` 与 `memory_model` 无法跨 Provider 路由，和多 Provider 要求不一致

需求文档允许多个 Provider 共存，模型 ID 在全局唯一；`compact_model` 与 `memory_model` 都是独立配置的模型 ID，不要求与当前对话模型属于同一个 Provider，见 `specs/0005_youyou_demand.md:74-74`、`specs/0005_youyou_demand.md:192-195`、`specs/0005_youyou_demand.md:241-243` 与 `specs/0005_youyou_demand.md:296-299`。

设计稿虽然定义了 `ModelRouter`，见 `specs/0006_youyou_agent_design.md:625-633`，但 `ContextManager::compact()` 和 `MemoryManager::extract_memories()` 都只接收当前 `provider: &dyn ModelProvider`，见 `specs/0006_youyou_agent_design.md:921-945` 与 `specs/0006_youyou_agent_design.md:1249-1266`。这使得 `compact_model` 或 `memory_model` 一旦配置成另一个 Provider 下的模型，设计就无法落地。

这不是实现细节，而是配置契约和调用路径之间的直接断裂。当前文档相当于“在配置层允许跨 Provider 选模型，但在执行层又只能用当前 Provider”。

建议把模型调用统一收敛到一个 `ModelExecutor` 或直接复用 `ModelRouter`。压缩、主对话、记忆提取、记忆整合都只传入 `model_id`，真正发请求前再做一次 `model_id -> provider` 解析。与此同时，应在 build 阶段校验 `compact_model` 和 `memory_model` 是否已注册。

### 2.4 高风险：运行态上下文与恢复态上下文不一致，`ToolCall` 历史模型前后不统一

需求文档要求 SessionStorage 能保存完整会话历史，并在恢复时重建上下文，见 `specs/0005_youyou_demand.md:156-163` 与 `specs/0005_youyou_demand.md:210-219`。这隐含要求“运行中看到的上下文”和“恢复后重建出的上下文”必须是同一种规范。

设计稿在类型层引入了 `Message::ToolCall`，见 `specs/0006_youyou_agent_design.md:97-118`；恢复路径也会把 `SessionEventPayload::ToolCall` 映射为 `Message::ToolCall`，见 `specs/0006_youyou_agent_design.md:724-734`。但实时 Turn 流程中，文档只说明在 5e 追加 `assistant message`，在 5h 追加 `tool results`，没有任何一步把 `ToolCall` 写入 `ContextManager`，见 `specs/0006_youyou_agent_design.md:802-815` 与 `specs/0006_youyou_agent_design.md:843-847`。

这会导致恢复后的消息历史比实时运行时多出一层 `ToolCall` 消息，形成行为分叉。更严重的是，如果底层 Provider 适配实际上需要显式的 assistant-tool-call 消息，那么实时路径的上下文就定义不完整；如果 Provider 不需要显式 `ToolCall`，那恢复路径把它恢复回上下文反而是错误的。

建议先固定一套唯一的“规范消息账本”。要么实时路径和恢复路径都显式保留 `ToolCall` 消息；要么 `ToolCall` 只作为持久化审计事件，不进入 LLM 上下文，两条路径都统一为 `assistant + tool_result`。文档还需要补上 `ChatEvent::ToolCall` 如何聚合进上下文的明确规则，避免实现者各自理解。

### 2.5 中风险：`MemoryStorage` trait 契约不足，支撑不了文档自己定义的 Phase 2 记忆整合

需求文档对 MemoryStorage 的职责是“加载记忆、按 id upsert 保存/更新、删除、按 namespace + query 搜索相关记忆”，见 `specs/0005_youyou_demand.md:171-175`。同时需求还要求“内容相同只更新时间戳，内容不同则更新内容”，见 `specs/0005_youyou_demand.md:173-173`。

但设计稿里的 `MemoryStorage` 只有 `search / save / delete` 三个接口，见 `specs/0006_youyou_agent_design.md:480-485`。与此同时，`MemoryManager::extract_memories()` 的 Phase 2 又明确写着“从 storage 加载当前 namespace 下的已有记忆”，见 `specs/0006_youyou_agent_design.md:1247-1269`。也就是说，设计本身依赖一个“列出已有记忆”的能力，但 trait 并没有提供。

此外，`save()` 的语义也没有对齐需求文档中的 upsert 约束，尤其是“同内容仅刷新时间戳”的规则没有进入接口契约。

建议把 trait 收紧为和需求一致的显式契约，例如新增 `list_by_namespace()` 或 `load_all()`，并把 `save()` 改名为 `upsert()`，同时在接口注释中明确时间戳刷新规则。否则实现 Memory Phase 2 时只能依赖模糊约定，容易导致不同 storage backend 行为漂移。

### 2.6 中风险：`TurnStart` 的 `ContinueWith` 会覆盖原始 payload，破坏多 Plugin 可组合性

需求文档定义了 Hook 的固定 payload 结构，同时约定 `Dynamic Sections` 来自 `TurnStart Hook` 的 `ContinueWith`，见 `specs/0005_youyou_demand.md:135-146` 与 `specs/0005_youyou_demand.md:261-261`。这里的关键点不是“能不能修改”，而是多个 Plugin 串联时仍应保留统一的数据语义。

设计稿把 `TurnStart` 的 `DispatchOutcome.payload.data` 直接解释为 `dynamic_sections`，多个 Plugin 通过链式 `ContinueWith` 覆盖整个 `data` 字段，见 `specs/0006_youyou_agent_design.md:316-316`、`specs/0006_youyou_agent_design.md:780-784`、`specs/0006_youyou_agent_design.md:1015-1035` 与 `specs/0006_youyou_agent_design.md:1169-1174`。

问题在于，`TurnStart` 原始 payload 本来应包含 `user_input`。如果前一个 Plugin 把 `data` 改造成“纯 dynamic_sections 数组”，后一个 Plugin 就拿不到原始 `user_input` 了。这和 Tapable/Hook 的可组合初衷相冲突，也会让 Plugin 作者必须私下约定 JSON 结构，导致系统脆弱。

建议把 Hook payload 改成强类型对象，至少为 `TurnStart` 定义 `user_input` 与 `dynamic_sections` 共存的稳定结构。`ContinueWith` 不应该整体替换 `data`，而应只允许修改指定字段或返回一个 merge patch。

---

## 3. Recommended Revisions

### 3.1 必须先修的文档改动

1. 重写 Session 槽状态模型。去掉“`strong_count == 0` 即允许新建 Session”的放行规则，改成关闭完成后统一清槽。
2. 把取消机制改为“Session 关闭令牌 + 每轮独立取消令牌”的双层设计，并补充 token 创建、销毁、重置时机。
3. 把所有模型调用路径统一经过 `ModelRouter` 或 `ModelExecutor`，让 `default_model`、`compact_model`、`memory_model` 走同一套解析逻辑。
4. 明确唯一的上下文消息规范，统一实时运行与恢复重建的 `ToolCall` 表示方式。
5. 扩展 `MemoryStorage` trait，使其真的能支持 Phase 2 记忆整合，而不是只支持检索 top-k。
6. 收紧 Hook 的数据契约，至少把 `TurnStart` 与 `BeforeToolUse` 的可修改字段类型化。

### 3.2 建议一并补强的设计细节

1. 为多模态输入补充验证责任归属，明确 20MB 图片上限和格式校验由谁执行，以及在哪个阶段返回错误。需求已定义约束，设计稿尚未给出落点。参见 `specs/0005_youyou_demand.md:396-401`。
2. 为 `tokio::spawn` 的后台 Turn task 和 Session 关闭 task 增加 panic 后的清理策略说明，避免 `turn_in_progress` 或 Session 槽位卡死。
3. 在设计稿中补齐 `SkillDefinition` 的正式类型定义，当前公共 API 中暴露了该类型，但正文没有给出结构体字段，降低了文档自洽性。参见 `specs/0006_youyou_agent_design.md:1380-1389`。
4. 为 `ToolDispatcher` 的并发执行补充“事件顺序”和“实时性”之间的取舍说明。当前文档同时承诺并发执行和严格顺序一致，建议把发送策略写清楚，避免实现时出现两种不同解释。

---

## 4. Positive Notes

1. 模块划分总体清晰，`Agent / Session / TurnLoop / ContextManager / PromptBuilder / ToolDispatcher / MemoryManager` 的边界大体合理。
2. Prompt 附录和 codex 参考映射很完整，能明显降低后续实现时的“资料跳转成本”。
3. 即时落盘、Incomplete message 恢复、Tool 并发策略和记忆 checkpoint 这些关键运行机制都已经进入设计稿，整体完成度高于普通草案。

---

## 5. Final Assessment

`0006` 不是一份“方向错误”的设计稿，相反，它的主体框架已经比较扎实；问题主要集中在几个关键边界条件没有真正闭环。如果先修正本评审列出的 4 个高风险问题，再补齐 2 个中风险问题，这份设计文档可以比较稳妥地作为实现基线。

当前建议分数维持在 **78 / 100**。若完成本次评审中的必须修订项，预期可提升到 **88 分以上**。
