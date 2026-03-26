# youyou-agent

`youyou-agent` 是一个单会话 Agent 内核，围绕以下约束设计：

- Session 只有一个活跃槽位，生命周期和恢复路径明确
- Ledger 是事实源，实时路径与 resume 路径共享同一套持久化协议
- Prompt、Tool、Hook、Compact、Memory 分层清晰，便于调用方替换外部适配器

## 快速开始

最小可运行示例：

```bash
cargo run --example minimal_agent
```

示例里包含：

- 一个脚本化 `ModelProvider`
- 一个简单的 `ToolHandler`
- 内存版 `SessionStorage`
- 内存版 `MemoryStorage`
- `new_session -> send -> cancel -> resume -> close` 的完整链路

## 注册组件

最常见的接入方式是通过 `AgentBuilder` 注册 provider、tool、skill、plugin 和存储适配器：

```rust
use serde_json::json;
use youyou_agent::{AgentBuilder, AgentConfig, SkillDefinition};

let config = AgentConfig::new("my-model", "my-app/default");

let agent = AgentBuilder::new(config)
    .register_model_provider(my_provider)
    .register_tool(my_tool)
    .register_skill(SkillDefinition {
        name: "search".to_string(),
        display_name: "Search".to_string(),
        description: "Search the local index".to_string(),
        prompt_template: "Use the search skill when needed.".to_string(),
        required_tools: vec!["search_index".to_string()],
        allow_implicit_invocation: true,
    })
    .register_plugin(my_plugin, json!({ "enabled": true }))
    .register_session_storage(my_session_storage)
    .register_memory_storage(my_memory_storage)
    .build()
    .await?;
```

各类组件职责：

- `ModelProvider` 负责把 `ChatRequest` 映射到外部模型协议，并以流式 `ChatEvent` 返回结果
- `ToolHandler` 负责执行单个 tool 调用，并返回结构化 `ToolOutput`
- `Plugin` 通过 hooks 观察或影响 `SessionStart`、`TurnStart`、`BeforeToolUse`、`AfterToolUse`、`BeforeCompact` 等节点
- `SessionStorage` 负责持久化 ledger 事件，支持 resume / list / find / delete
- `MemoryStorage` 负责 bootstrap、turn search 和 checkpoint/close extraction 的读写

## 消费 RunningTurn

`send_message()` 会返回一个 `RunningTurn`。调用方需要同时处理实时事件流和最终终态：

```rust
use tokio_stream::StreamExt;
use youyou_agent::{AgentEventPayload, SessionConfig, UserInput};

let session = agent.new_session(SessionConfig::default()).await?;
let mut turn = session.send_message(UserInput { content: vec![] }, None).await?;

while let Some(event) = turn.events.next().await {
    match event.payload {
        AgentEventPayload::TextDelta(text) => {
            // 把增量文本转发给 UI
        }
        AgentEventPayload::ToolCallStart { .. }
        | AgentEventPayload::ToolCallEnd { .. }
        | AgentEventPayload::ContextCompacted
        | AgentEventPayload::TurnComplete
        | AgentEventPayload::TurnCancelled
        | AgentEventPayload::ReasoningDelta(_)
        | AgentEventPayload::Error(_) => {}
    }
}

let outcome = turn.join().await?;
```

如果调用方需要取消当前 turn：

- 调用 `RunningTurn::cancel()`
- 或在 `send_message(input, Some(external_cancel))` 中传入外部 `CancellationToken`

## 存储一致性要求

自定义 `SessionStorage` / `MemoryStorage` 时，需要满足这些契约：

- `SessionStorage::save_event()` 必须在返回前确保该事件已经成功写入事实源
- `SessionStorage::load_session()` 必须返回完整、有序、不可猜测补全的 ledger 事件流
- `SessionStorage` 不能只存 message，必须把 `Metadata`、synthetic `ToolResult`、system message 一并持久化
- `MemoryStorage::list_recent()` 与 `search()` 必须按调用方约定返回稳定顺序，避免 prompt 注入抖动
- `MemoryStorage::upsert()` / `delete()` 必须对同一 `memory.id` 保持幂等行为

换句话说，resume 只能依赖 ledger 和 memory backend，不能依赖运行时缓存。

## 开发校验

本仓库常用检查命令：

```bash
cargo +nightly fmt --all
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -- -D warnings -W clippy::pedantic
```

CI 已包含：

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features --tests --benches -- -D warnings`
- `cargo nextest run --all-features`

## 目录说明

- `src/domain`: 配置、事件、错误、ledger、hook 等核心契约
- `src/ports`: provider / tool / plugin / storage 抽象
- `src/application`: turn engine、session service、prompt builder、memory manager 等编排逻辑
- `src/api`: `Agent`、`SessionHandle`、`RunningTurn` 等对外入口
- `examples/minimal_agent.rs`: 最小可运行示例
- `tests/`: 分 phase 的契约测试与 acceptance 测试

## License

This project is distributed under the terms of MIT.
