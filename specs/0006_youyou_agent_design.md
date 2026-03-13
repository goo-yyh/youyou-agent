# YouYou Agent - Architecture Design

| Field       | Value                |
| ----------- | -------------------- |
| Document ID | 0006                 |
| Type        | design               |
| Status      | Draft                |
| Created     | 2026-03-13           |
| Related     | 0005 (requirements)  |
| Module Path | src-tauri/src/agent/ |

---

## 1. Design Principles

- **Clean Architecture**：依赖方向从外向内。trait（port）定义在核心层，实现由调用方提供
- **SOLID**：单一职责（每个模块一件事）、开闭原则（通过 trait 扩展，不修改核心）、依赖倒置（核心依赖抽象而非具体实现）
- **KISS**：不抽象不需要的东西。无 DI 容器、无 Actor 模型、无消息总线——直接的函数调用和 struct 组合
- **YAGNI**：仅实现需求文档 0005 中明确要求的功能

---

## 2. Module Structure

```
src-tauri/src/agent/
├── mod.rs                  // 模块入口，re-export 公共 API
├── error.rs                // AgentError 错误枚举
├── types.rs                // 共享值类型（Message, Content, TokenUsage 等）
├── event.rs                // AgentEvent 定义
├── config.rs               // AgentConfig, EnvironmentContext
│
├── traits/                 // Port 层：所有外部依赖的 trait 定义
│   ├── mod.rs
│   ├── model.rs            // ModelProvider, ModelInfo, ChatRequest, ChatEvent
│   ├── tool.rs             // ToolHandler, ToolInput, ToolOutput
│   ├── plugin.rs           // Plugin trait
│   └── storage.rs          // SessionStorage, MemoryStorage
│
├── builder.rs              // AgentBuilder：校验 + 构建
├── agent.rs                // Agent：不可变的组件持有者 + Session 守卫
├── session.rs              // Session：一次对话的可变状态
├── turn.rs                 // TurnLoop：单轮对话的执行逻辑
├── context.rs              // ContextManager：消息历史 + 压缩
├── prompt/                 // System Prompt 组装
│   ├── mod.rs              // PromptBuilder
│   └── templates.rs        // 内置 prompt 模板常量（Appendix B）
├── tool_dispatch.rs        // ToolDispatcher：路由、并发/串行、超时
├── skill.rs                // SkillManager：Skill 注册表 + 触发检测 + 注入
├── hook.rs                 // HookRegistry：事件注册 + 顺序分发
├── plugin_mgr.rs           // PluginManager：生命周期管理
└── memory.rs               // MemoryManager：加载、注入、提取
```

---

## 3. Dependency Graph

依赖方向严格从外向内，核心模块之间无循环依赖。

```
调用方 (Tauri App)
  │
  ▼
AgentBuilder ──────────► Agent
                           │
                           ▼
                        Session
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
          TurnLoop    ContextMgr    MemoryMgr
              │            │
         ┌────┼────┐       │
         ▼    ▼    ▼       ▼
      ToolDisp SkillMgr PromptBuilder
              │
              ▼
         HookRegistry
```

**依赖规则：**

- `traits/` 不依赖任何其他模块
- `types.rs`, `error.rs`, `event.rs`, `config.rs` 仅依赖标准库和 serde
- 所有 core 模块依赖 `traits/` 和 `types.rs`，但不互相依赖（通过参数传递协作）
- `TurnLoop` 是唯一的编排者，它调用其他模块但其他模块不调用它

---

## 4. Core Types

### 4.1 Message（消息）

```rust
/// 对话中的一条消息
pub enum Message {
    User {
        content: Vec<ContentBlock>,
    },
    Assistant {
        content: Vec<ContentBlock>,
        status: MessageStatus,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    System {
        content: String,
    },
}

pub enum MessageStatus {
    Complete,
    Incomplete,
}

pub enum ContentBlock {
    Text(String),
    Image {
        data: String, // base64
        media_type: String,
    },
    /// 文件内容：由调用方读取后以文本形式传入，保留文件元信息用于上下文渲染和 Provider 适配
    File {
        name: String,
        media_type: String,
        text: String,
    },
}
```

### 4.2 AgentEvent（事件）

```rust
pub struct AgentEvent {
    pub session_id: String,
    pub turn_id: String,
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
    pub payload: AgentEventPayload,
}

pub enum AgentEventPayload {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { call_id: String, tool_name: String, arguments: serde_json::Value },
    ToolCallEnd { call_id: String, tool_name: String, output: ToolOutput, duration_ms: u64, success: bool },
    ContextCompacted,
    TurnComplete,
    TurnCancelled,
    Error(AgentError),
}
```

### 4.3 SessionConfig（会话配置）

```rust
pub struct SessionConfig {
    /// 使用的模型 ID（不指定则使用 AgentConfig.default_model）
    pub model_id: Option<String>,
    /// 可选的 System Prompt 覆盖（追加到 system_instructions 之后）
    pub system_prompt_override: Option<String>,
}
```

**持久化与恢复规则：** SessionConfig 在会话创建时通过 `SessionStorage::save_event()` 持久化为 `Metadata` 事件（key="session_config"）。恢复会话时，`resume_session()` 从 Metadata 事件中还原 `model_id` 和 `system_prompt_override`，用于重建 SessionState 和 System Prompt。若 Metadata 中无 session_config（旧格式兼容），则使用 AgentConfig 的默认值。

### 4.4 AgentConfig（Agent 配置）

`AgentConfig` 在 `AgentBuilder::new()` 时传入，构建后不可变。各字段与消费模块的映射关系如下：

```rust
pub struct AgentConfig {
    /// 默认模型 ID，新建会话时未指定 model_id 则使用此值
    /// 消费方：Agent::new_session()、ModelRouter
    pub default_model: String,
    /// 系统指令文本列表，按序拼接注入 System Prompt
    /// 消费方：PromptBuilder::build_stable()
    pub system_instructions: Vec<String>,
    /// 人设定义文本，Agent 自动包裹 <personality_spec> 标签后注入
    /// 消费方：PromptBuilder::build_stable()
    pub personality: Option<String>,
    /// 环境上下文数据（cwd / shell / date / timezone 等）
    /// 消费方：PromptBuilder::build_dynamic()
    pub environment_context: Option<EnvironmentContext>,

    // ── Tool 相关 ──
    /// Tool 执行超时（毫秒），默认 120_000
    /// 消费方：ToolDispatcher
    pub tool_timeout_ms: u64,
    /// 单轮最大 Tool 调用次数，默认 50
    /// 消费方：TurnLoop（步骤 4i）
    pub max_tool_calls_per_turn: usize,

    // ── 压缩相关 ──
    /// 上下文压缩阈值（0.0–1.0），默认 0.8
    /// 消费方：ContextManager::needs_compaction()
    pub compact_threshold: f64,
    /// 压缩使用的模型 ID，不配置则使用当前对话模型
    /// 消费方：ContextManager::compact()
    pub compact_model: Option<String>,
    /// 压缩 prompt 模板，不配置则使用 DEFAULT_COMPACT_PROMPT
    /// 消费方：ContextManager::compact()
    pub compact_prompt: Option<String>,

    // ── 记忆相关 ──
    /// 记忆提取使用的模型 ID，不配置则使用当前对话模型
    /// 消费方：MemoryManager::extract_memories()
    pub memory_model: Option<String>,
    /// 记忆 checkpoint 间隔（轮次），默认 10
    /// 消费方：TurnLoop（步骤 6）
    pub memory_checkpoint_interval: u64,
    /// 每次注入 System Prompt 的记忆数量上限，默认 20
    /// 消费方：MemoryManager::load_memories()
    pub memory_max_items: usize,
    /// 记忆 namespace，用于记忆隔离
    /// 消费方：MemoryManager
    pub memory_namespace: String,
}
```

### 4.5 UserInput（用户输入）

```rust
pub struct UserInput {
    /// 输入内容块（支持多模态：文本 + 图片 + 文件混合）
    pub content: Vec<ContentBlock>,
}
```

### 4.6 SessionEvent（会话持久化事件）

```rust
pub struct SessionEvent {
    pub timestamp: DateTime<Utc>,
    pub payload: SessionEventPayload,
}

pub enum SessionEventPayload {
    UserMessage { content: Vec<ContentBlock> },
    AssistantMessage { content: Vec<ContentBlock>, status: MessageStatus },
    ToolCall { call_id: String, tool_name: String, arguments: serde_json::Value },
    ToolResult { call_id: String, content: String, is_error: bool },
    SystemMessage { content: String },
    Metadata { key: String, value: serde_json::Value },
}
```

### 4.7 HookPayload 与 HookResult

```rust
pub struct HookPayload {
    pub event: HookEvent,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub plugin_config: serde_json::Value,
    pub data: serde_json::Value, // 事件特定数据（见 3.5 Hook 事件表）
    pub timestamp: DateTime<Utc>,
}

pub enum HookResult {
    Continue,
    ContinueWith(serde_json::Value),
    Abort(String),
}

#[derive(Clone, Eq, PartialEq, Hash)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    TurnStart,
    TurnEnd,
    BeforeToolUse,
    AfterToolUse,
    BeforeCompact,
}
```

**Hook Payload `data` 字段示例：** 各 Hook 事件的 `data` 字段遵循以下结构约定，Plugin 作者可据此解析和修改：

```json
// SessionStart
{ "session_id": "abc-123", "model_id": "claude-4-opus" }

// TurnEnd
{
  "session_id": "abc-123",
  "turn_id": "turn-7",
  "assistant_output": "Here is the result...",
  "tool_calls_count": 3,
  "cancelled": false
}

// BeforeToolUse（ContinueWith 可修改 arguments 实现参数拦截/改写）
{
  "session_id": "abc-123",
  "turn_id": "turn-7",
  "tool_name": "file_write",
  "arguments": { "path": "/tmp/test.txt", "content": "hello" }
}
```

**Hook Dispatch 结果处理矩阵：** 每个 Hook 事件对 `Abort` 和 `ContinueWith` 的处理规则如下：

| Hook | Abort 行为 | ContinueWith 行为 |
|------|-----------|-------------------|
| SessionStart | 回滚 Metadata，返回 `PluginAborted`，会话不创建 | 修改 payload.data（如覆盖 model_id） |
| TurnStart | 终止本轮 Turn，emit `Error(PluginAborted)`，不进入模型调用 | 链式传递 data，最终提取 dynamic_sections |
| BeforeCompact | 跳过本次压缩，保持当前上下文不变（不视为错误） | 修改压缩参数（如 compact_prompt） |
| BeforeToolUse | 不执行 handler，生成 synthetic `ToolResult(is_error=true)`，继续下一个 Tool | 修改 arguments（参数拦截/改写） |
| AfterToolUse | 中止后续流程：将 reason 作为 System 消息注入上下文，BREAK 退出 Tool Loop，进入最终模型调用生成收尾回复。不回滚已执行 Tool | 修改 tool output（如过滤敏感信息） |
| TurnEnd | **不阻断收尾流程**：记录 warn 日志，降级处理。理由：TurnEnd 处于收尾阶段，阻断会导致事件持久化和 checkpoint 流程无法执行 | 修改 payload.data（如追加审计信息） |
| SessionEnd | **不阻断关闭流程**：记录 warn 日志，降级处理。理由：SessionEnd 处于关闭路径，阻断会导致记忆提取和资源释放无法执行 | 修改 payload.data |

### 4.8 ToolInput 与 ToolOutput

```rust
pub struct ToolInput {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Tool 附加的结构化元数据（如文件路径、行号等），由 ToolHandler 自行填充
    /// 该字段不进入 LLM 上下文，但通过 AgentEvent::ToolCallEnd 透传给调用方
    pub metadata: serde_json::Value,
}
```

### 4.9 EnvironmentContext

```rust
pub struct EnvironmentContext {
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub os: Option<String>,
    pub date: Option<String>,
    pub timezone: Option<String>,
}
```

### 4.10 Memory

```rust
pub struct Memory {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub source: String,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

### 4.11 SessionSummary 与 SessionPage

```rust
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    /// 用于分页的游标，None 表示已到最后一页
    pub next_cursor: Option<String>,
}
```

---

## 5. Trait Definitions (Port 层)

### 5.1 ModelProvider

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &str;
    fn models(&self) -> &[ModelInfo];
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent>> + Send>>>;
}

pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_window: usize,
    pub capabilities: ModelCapabilities,
}

pub struct ModelCapabilities {
    pub tool_use: bool,
    pub vision: bool,
    pub streaming: bool,
}
```

`ChatRequest` 和 `ChatEvent` 见需求文档 3.1 节，这里不重复。

### 5.2 ToolHandler

```rust
#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn is_mutating(&self) -> bool;
    async fn execute(&self, input: ToolInput) -> Result<ToolOutput>;
}
```

### 5.3 Plugin

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn description(&self) -> &str;
    async fn initialize(&self, config: serde_json::Value) -> Result<()>;
    fn apply(&self, ctx: PluginContext);
    async fn shutdown(&self) -> Result<()>;
}

/// apply() 的上下文，封装 plugin 元信息和 Hook 注册能力
pub struct PluginContext<'a> {
    pub plugin_id: &'a str,
    pub plugin_config: serde_json::Value,
    registry: &'a mut HookRegistry,
}

impl<'a> PluginContext<'a> {
    /// 注册 hook handler（内部自动注入 plugin_id 和 plugin_config）
    pub fn tap(
        &mut self,
        event: HookEvent,
        handler: impl Fn(HookPayload) -> Pin<Box<dyn Future<Output = HookResult> + Send>>
            + Send + Sync + 'static,
    ) {
        self.registry.tap(event, self.plugin_id.to_string(),
            self.plugin_config.clone(), handler);
    }
}
```

这样 Plugin 在 `apply()` 中无需自行保存/传递 config，直接通过 `ctx.tap()` 注册 handler，config 由框架自动注入。

### 5.4 Storage

```rust
#[async_trait]
pub trait SessionStorage: Send + Sync {
    async fn save_event(&self, session_id: &str, event: SessionEvent) -> Result<()>;
    async fn load_session(&self, session_id: &str) -> Result<Option<Vec<SessionEvent>>>;
    async fn list_sessions(&self, cursor: Option<&str>, limit: usize) -> Result<SessionPage>;
    async fn find_session(&self, query: &str) -> Result<Vec<SessionSummary>>;
    async fn delete_session(&self, session_id: &str) -> Result<()>;
}

#[async_trait]
pub trait MemoryStorage: Send + Sync {
    async fn search(&self, namespace: &str, query: &str, limit: usize) -> Result<Vec<Memory>>;
    async fn save(&self, memory: Memory) -> Result<()>;
    async fn delete(&self, id: &str) -> Result<()>;
}
```

---

## 6. AgentBuilder

AgentBuilder 采用 typestate 模式编译期保证 `ModelProvider` 至少注册一个。其余组件可选。

```rust
pub struct AgentBuilder<S = NoProvider> {
    config: AgentConfig,
    providers: Vec<Arc<dyn ModelProvider>>,
    tools: Vec<Arc<dyn ToolHandler>>,
    skills: Vec<SkillDefinition>,
    plugins: Vec<(Arc<dyn Plugin>, serde_json::Value)>,
    session_storage: Option<Arc<dyn SessionStorage>>,
    memory_storage: Option<Arc<dyn MemoryStorage>>,
    _state: PhantomData<S>,
}

pub struct NoProvider;
pub struct HasProvider;

impl AgentBuilder<NoProvider> {
    pub fn new(config: AgentConfig) -> Self { /* ... */ }
}

impl<S> AgentBuilder<S> {
    pub fn register_model_provider(self, p: impl ModelProvider + 'static)
        -> AgentBuilder<HasProvider> { /* ... */ }
    pub fn register_tool(mut self, t: impl ToolHandler + 'static) -> Self { /* ... */ }
    pub fn register_skill(mut self, s: SkillDefinition) -> Self { /* ... */ }
    pub fn register_plugin(mut self, p: impl Plugin + 'static, config: serde_json::Value) -> Self { /* ... */ }
    pub fn register_session_storage(mut self, s: impl SessionStorage + 'static) -> Self { /* ... */ }
    pub fn register_memory_storage(mut self, s: impl MemoryStorage + 'static) -> Self { /* ... */ }
}

impl AgentBuilder<HasProvider> {
    /// 校验所有注册项 + 初始化 Plugin + 返回 Agent
    pub async fn build(self) -> Result<Agent> { /* ... */ }
}
```

`build()` 内部执行所有校验规则（见需求文档 6.1 节），初始化 Plugin 并调用 `apply()` 注册 hook handler。

---

## 7. Agent

Agent 是一个轻量 handle，内部持有 `Arc<AgentInner>`。`AgentInner` 包含所有不可变注册表和一个可关闭的活跃 Session 状态槽。

```rust
/// 调用方持有的 handle
pub struct Agent {
    inner: Arc<AgentInner>,
}

/// 共享的内部状态，Agent 和 Session 共同持有
pub(crate) struct AgentInner {
    pub config: AgentConfig,
    // 注册表（构建后不可变）
    pub model_router: ModelRouter,
    pub tool_dispatcher: ToolDispatcher,
    pub skill_manager: SkillManager,
    pub hook_registry: HookRegistry,
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub session_storage: Option<Arc<dyn SessionStorage>>,
    pub memory_storage: Option<Arc<dyn MemoryStorage>>,
    // 活跃 Session 状态槽
    pub active_session: Mutex<Option<ActiveSessionHandle>>,
}

/// Agent 持有的 Session 控制句柄，用于 shutdown 时优雅关闭
/// running_guard 的生命周期语义详见 §8 "running_guard 与 active_session 的生命周期绑定"
pub(crate) struct ActiveSessionHandle {
    pub session_id: String,
    pub cancel_token: CancellationToken,
    /// 触发完整的 Session 关闭流程（SessionEnd Hook + 记忆提取）
    pub close_signal: oneshot::Sender<()>,
    /// Session 关闭流程完全结束后发出信号
    pub completion_rx: oneshot::Receiver<()>,
    /// 当 strong count 归零时，表示 Session 已释放且所有后台 Turn task 都已结束
    pub running_guard: Weak<()>,
}
```

Session 创建时产生两对 channel：

- `close_signal`：Agent 通过它通知 Session 执行完整关闭流程（区别于 cancel_token 仅取消当前 Turn）
- `completion_tx/rx`：Session 关闭完成后通知 Agent

Session 内部有一个后台 task 监听 `close_signal`。收到信号后执行完整关闭流程：等待当前 Turn 结束 → SessionEnd Hook → 记忆提取 → 清除 active_session 槽 → completion_tx.send(())。

**公共 API：**

```rust
impl Agent {
    /// 创建新 Session
    pub async fn new_session(&self, config: SessionConfig) -> Result<Session>;

    /// 恢复已有 Session（不触发 SessionStart Hook，仅恢复上下文）
    pub async fn resume_session(&self, session_id: &str) -> Result<Session>;

    /// 关闭 Agent，释放所有资源
    pub async fn shutdown(&self) -> Result<()>;
}
```

**`new_session()` 执行路径：**

1. 检查 `active_session` 槽：若已有活跃 Session 且 `running_guard.strong_count() > 0`，返回 `AgentError::SessionBusy`
2. 分配 `session_id`（UUID v4）
3. 确定 `model_id`：`config.model_id.unwrap_or(agent_config.default_model)`
4. 持久化 `session_config` 为 Metadata 事件（通过 `SessionStorage::save_event()`）
5. 触发 **SessionStart Hook**：
   - payload.data 包含 `{ "session_id": "...", "model_id": "..." }`
   - 若任一 Plugin 返回 `Abort(reason)`：回滚已持久化的 Metadata 事件（调用 `SessionStorage::delete_session()`），返回 `AgentError::PluginAborted { hook: "SessionStart", reason }`
6. 从 MemoryStorage 加载通用记忆（`load_memories("")`，空 query 泛化召回）
7. 构建 `SessionState`（含 `system_prompt_override`）
8. 将 `ActiveSessionHandle` 写入 `active_session` 槽
9. 返回 `Session`

**shutdown 执行路径：**

1. 获取 `active_session` 锁，取出 `ActiveSessionHandle`
2. 若有活跃 Session：
   a. `cancel_token.cancel()` — 取消正在运行的 Turn
   b. `close_signal.send(())` — 触发 Session 级完整关闭流程（SessionEnd Hook + 记忆提取）
   c. `completion_rx.await` — 等待关闭流程完全结束
3. 按逆序调用所有 Plugin 的 `shutdown()`
4. 释放资源

**注意：** `shutdown()` 触发的是完整关闭路径（等同于调用方调用 `Session::close()`），不是 Drop 兜底路径。

### 7.1 ModelRouter

简单的 HashMap 查找，不做额外抽象。

```rust
pub(crate) struct ModelRouter {
    providers: HashMap<String, Arc<dyn ModelProvider>>, // model_id -> provider
}

impl ModelRouter {
    pub fn resolve(&self, model_id: &str) -> Result<&Arc<dyn ModelProvider>> {
        self.providers.get(model_id).ok_or(AgentError::model_not_supported(model_id))
    }
}
```

---

## 8. Session

Session 持有一次对话的全部可变状态。内部状态通过 `Arc<Mutex<SessionState>>` 共享，使得 `send_message` 返回的事件流不借用 `&mut self`。

```rust
pub struct Session {
    id: String,
    state: Arc<Mutex<SessionState>>,
    inner: Arc<AgentInner>,  // 与 Agent 共享
}

pub(crate) struct SessionState {
    pub model_id: String,
    /// 可选的 System Prompt 覆盖，来自 SessionConfig，持久化于 Metadata
    pub system_prompt_override: Option<String>,
    pub context: ContextManager,
    pub turn_counter: u64,
    pub memory_checkpoint_counter: u64,
    pub cancel_token: CancellationToken,
    /// 当前是否有 Turn 正在运行，禁止并发 send_message()
    pub turn_in_progress: bool,
}
```

**公共 API：**

```rust
impl Session {
    /// 发送消息，返回一个独立的 RunningTurn 句柄
    /// RunningTurn 不借用 Session，调用方可同时持有 Session 和 RunningTurn
    pub async fn send_message(&self, input: UserInput) -> Result<RunningTurn>;

    /// 关闭 Session（显式关闭）
    pub async fn close(self) -> Result<()>;
}

/// 一轮对话的运行句柄，持有事件流和取消能力
pub struct RunningTurn {
    events: mpsc::Receiver<AgentEvent>,
    cancel_token: CancellationToken,
}

impl RunningTurn {
    /// 获取事件流
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent>;

    /// 取消当前 Turn
    pub fn cancel(&self);
}
```

`send_message` 流程：

1. 获取 `state` 锁，检查 `turn_in_progress`。若为 `true`，返回 `AgentError::TurnBusy`
2. 设置 `turn_in_progress = true`
3. 将 `running_guard: Arc<()>` clone 传入后台 task（绑定 active_session 生命周期）
4. spawn tokio task 运行 `run_turn()`，task 结束时设置 `turn_in_progress = false` 并 Drop `running_guard` clone
5. 返回 `RunningTurn`（仅包含 `events` 和 `cancel_token`，不持有 `running_guard`）

RunningTurn 不持有 Session 的任何借用，调用方可以在消费事件流的同时调用 `cancel()`。同一 Session 同一时间只允许一个 Turn 运行，并发调用 `send_message()` 将返回 `AgentError::TurnBusy`。

**running_guard 与 active_session 的生命周期绑定：**

`running_guard` 用于判定活跃 Session 是否"真正结束"。它的 strong ref 绑定到 **Session 本身**和 **后台 `run_turn()` task**，而非 `RunningTurn` 前端消费句柄。

- Session 创建时持有一个 `Arc<()>` clone
- 每次 `send_message()` spawn 后台 `run_turn()` task 时，将 `Arc<()>` clone 传入 task，task 结束时 clone 自动 Drop
- `RunningTurn` **不持有** `running_guard`，仅持有 `events` 和 `cancel_token`
- `ActiveSessionHandle` 持有 `Weak<()>`（`ActiveSessionHandle` 的完整定义见 §7，此处不重复）

Session 和后台 `run_turn()` task 都持有同一个 `Arc<()>` 的 clone。Agent 在创建新 Session 前检查：

1. `active_session` 槽是否为 Some
2. 若为 Some，检查 `running_guard.strong_count() > 0`
3. 只有槽为 None 或 strong_count == 0 时才允许创建新 Session

这保证了即使 Session 和 RunningTurn 都被 Drop，只要后台 `run_turn()` task 仍在运行，`active_session` 就不会被过早释放。因为 `RunningTurn` 不参与 `running_guard` 的引用计数，调用方丢弃 `RunningTurn` 不会影响 active_session 的判定。

### 8.1 会话恢复流程（resume_session）

`Agent::resume_session(session_id)` 从 SessionStorage 加载历史事件并重建 Session：

1. 调用 `SessionStorage::load_session(session_id)` 获取 `Vec<SessionEvent>`
2. 从 Metadata 事件中提取 `session_config`（model_id + system_prompt_override），用于重建 SessionState
3. 按顺序将每个 `SessionEvent` 映射为 `Message`：

| SessionEventPayload                              | 映射为 Message                                                                                                        |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------- |
| UserMessage { content }                          | Message::User { content }                                                                                             |
| AssistantMessage { content, status: Complete }   | Message::Assistant { content, status: Complete }                                                                      |
| AssistantMessage { content, status: Incomplete } | Message::Assistant { content, status: Incomplete } **+ 追加** Message::System { content: "[此消息因用户取消而中断]" } |
| ToolCall { call_id, tool_name, arguments }       | Message::ToolCall { call_id, tool_name, arguments }                                                                   |
| ToolResult { call_id, content, is_error }        | Message::ToolResult { call_id, content, is_error }                                                                    |
| SystemMessage { content }                        | Message::System { content }                                                                                           |
| Metadata { .. }                                  | 跳过，不加入上下文                                                                                                    |

4. 将映射后的 Message 列表加载到 ContextManager
5. **恢复计数器**：从已加载的事件列表中推导 `turn_counter` 和 `memory_checkpoint_counter`：
   - `turn_counter`：统计 `SessionEventPayload::UserMessage` 事件的数量（每个 UserMessage 代表一轮 Turn）
   - `memory_checkpoint_counter`：`turn_counter % memory_checkpoint_interval`，延续原有的 checkpoint 节奏
   - 这保证了恢复后 turn 编号不重复、checkpoint 间隔不漂移
6. 不触发 SessionStart Hook（resume 不是新建会话）
7. 重新从 MemoryStorage 加载最新记忆（因为 Memory 是动态 System Prompt 段，在 resume 时可能已有新的记忆写入，需要获取最新视图）
8. 使 stable_prompt 缓存失效，下一轮 Turn 时 PromptBuilder 会用恢复的 session_config 重建

**三条关闭路径：**

| 路径                               | 触发方                | SessionEnd Hook | 记忆提取           | active_session 释放时机                        |
| ---------------------------------- | --------------------- | --------------- | ------------------ | ---------------------------------------------- |
| `Session::close()`                 | 调用方显式调用        | 执行            | 执行（失败不阻塞） | close 完成后，running_guard 归零时             |
| `Agent::shutdown()` → close_signal | Agent 内部触发        | 执行            | 执行（失败不阻塞） | close 完成后，running_guard 归零时             |
| `Drop`（未调用 close）             | Session handle 被丢弃 | best-effort 执行 | best-effort 执行  | running_guard 归零时（后台 Turn task 全部结束后） |

- `close()` 和 `shutdown()` 触发的是同一条内部关闭流程，只是入口不同
- **`Drop` 路径的 best-effort 关闭**：Session 的 `Drop` 实现中通过 `close_signal.send(())` 触发后台关闭任务。后台关闭任务执行与 `close()` 相同的流程（SessionEnd Hook + 记忆提取），但有以下差异：
  - `Drop` 中无法 `.await`，因此关闭流程在后台异步执行，调用方无法得知完成时间
  - 若 `close_signal` 已被消费（如重复 Drop），静默忽略
  - 后台关闭任务的 panic/error 不会传播，仅记录 error 日志
  - 这保证了"调用方忘记 close"不会静默跳过 SessionEnd Hook 和记忆提取
- active_session 槽的释放绑定到 `running_guard` 的 strong count 归零，而非 Session handle 的 Drop。这保证了后台 Turn task 仍在运行时不会过早允许创建新 Session。`RunningTurn` 不持有 `running_guard`，其生命周期不影响 active_session 的判定
- 无论哪条路径，`completion_tx.send(())` 都会被调用，确保 `shutdown()` 不会永远挂起

---

## 9. TurnLoop

TurnLoop 是一个无状态的 async 函数，不是 struct。它是整个 Agent 运行时的唯一编排者。

```rust
pub(crate) async fn run_turn(
    state: Arc<Mutex<SessionState>>,
    inner: Arc<AgentInner>,
    input: UserInput,
    event_tx: mpsc::Sender<AgentEvent>,
) -> Result<()>
```

### 9.1 执行流程

```
run_turn(state, inner, input, event_tx)
│
├─ 1. Hook: TurnStart
│     ├─ Abort → emit Error(PluginAborted)，终止本轮 Turn，不进入模型调用
│     └─ 正常 → DispatchOutcome.payload.data 即为 dynamic_sections
│           多个 Plugin 的 ContinueWith 采用链式传递：后一个 handler 看到前一个修改后的 data
│           TurnLoop 从最终 payload.data 中提取 dynamic_sections（约定为 JSON string array）
├─ 2. SkillManager: 从 UserInput 的 Text 块中检测 /skill_name → 注入 Skill prompt
├─ 3. MemoryManager: load_memories(user_input_text) → 基于用户输入检索相关记忆
├─ 4. ContextManager: 追加用户消息 + SessionStorage: 即时持久化 UserMessage
│
├─ 5. LOOP {
│   ├─ 5a. PromptBuilder: 组装 System Prompt（每轮重建 dynamic 段，含最新 memories）
│   ├─ 5b. ContextManager: 检查是否需要压缩（含 prompt + tools 估算）
│   │     └─ 是 → Hook: BeforeCompact（Abort 则跳过压缩）→ 执行压缩 → emit ContextCompacted
│   ├─ 5c. 构建 ChatRequest（消息历史 + tool specs）
│   ├─ 5d. ModelProvider: 流式调用
│   │     ├─ 正常 → 逐个 ChatEvent → emit TextDelta / ReasoningDelta / Error
│   │     │   → 聚合本轮 assistant 输出为完整 Message
│   │     └─ context_length_exceeded 错误 →
│   │           ├─ Hook: BeforeCompact
│   │           ├─ ContextManager: compact()（优先 summarize，失败则 truncate）
│   │           ├─ emit ContextCompacted
│   │           └─ 自动重试一次（回到 5a）；若重试仍失败则返回 ProviderError
│   ├─ 5e. ContextManager: 追加 assistant message（无论是否有 ToolCall）
│   │     + SessionStorage: 即时持久化 AssistantMessage
│   ├─ 5f. 收集 ToolCall 列表
│   │     └─ 无 ToolCall → BREAK
│   ├─ 5g. ToolDispatcher: 执行 ToolCall 批次（结果按原始顺序归并）
│   │     ├─ 全只读 → 并发执行
│   │     └─ 含 mutating → 串行执行
│   │     每个 ToolCall:
│   │       ├─ Hook: BeforeToolUse（可 Abort）
│   │       ├─ emit ToolCallStart + SessionStorage: 即时持久化 ToolCall
│   │       ├─ ToolHandler.execute()（带超时）
│   │       ├─ emit ToolCallEnd + SessionStorage: 即时持久化 ToolResult
│   │       └─ Hook: AfterToolUse（Abort 则中止后续流程，进入最终模型调用）
│   ├─ 5h. ContextManager: 追加 tool results
│   ├─ 5i. tool_call_count += batch.len()
│   │     └─ 超限 → 进入"超限收尾"流程（见下文）
│   └─ } // 回到 5a
│
├─ 6. MemoryManager: checkpoint 检查（每 N 轮）
├─ 7. Hook: TurnEnd（Abort 降级处理，不阻断）
└─ 8. emit TurnComplete / TurnCancelled
```

**Tool 超限收尾流程（两段式）：** 当 `tool_call_count` 超过 `max_tool_calls_per_turn` 时，不直接 BREAK，而是执行以下两阶段流程，确保用户拿到解释性回答：

1. **注入超限提示**：将超限提示作为 System 消息注入上下文：`"[TOOL_LIMIT_REACHED] You have reached the maximum number of tool calls ({limit}) for this turn. You MUST NOT call any more tools. Summarize your progress and provide a final response to the user."`
2. **最终模型调用**：构建 ChatRequest 时**不传入任何 tool definitions**（tools 参数为空数组），强制模型只生成纯文本回复
3. **聚合最终回复**：将模型返回的 assistant message 追加到上下文，然后正常退出 Tool Loop

若最终模型调用失败，记录错误日志，emit `Error` 事件，Turn 仍正常结束。

**事件持久化策略（即时落盘）：** 会话事件采用结构化即时落盘，而非 Turn 结束后统一写入，以保证异常退出时的数据完整性：

- **UserMessage**：收到用户输入后立即持久化
- **ToolCall / ToolResult**：Tool 开始/结束后立即持久化
- **AssistantMessage**：在模型流式响应完成或取消时持久化一次最终态，携带 `status`（Complete / Incomplete）
- **Metadata**：在写入时立即持久化（如 session_config）
- **AgentEvent delta**（TextDelta、ReasoningDelta 等）：仅通过事件流推送，不持久化

若 SessionStorage 未注册，所有持久化操作静默跳过。持久化失败记录 warn 日志但不阻塞 Turn 执行。

**关键设计点：** assistant message 在步骤 5e 中无条件追加到上下文——无论模型是返回纯文本还是带 ToolCall。这保证了：

- 多轮对话中后续 Turn 可以看到上一轮回答
- SessionStorage 在 Turn 结束时能拿到完整 assistant message
- 取消导致的 incomplete message 也能被正确记录

### 9.2 取消处理

取消信号通过 `CancellationToken` 传播。TurnLoop 在以下 yield 点检查：

- 模型流式接收的每个 event 之间
- Tool 批次中每个 Tool 执行前（尚未开始的 Tool 跳过）
- 压缩请求发起前

已开始的 Tool 执行不中断（等待完成）。取消后 TurnLoop 发出 `TurnCancelled` 事件并退出。

**未执行 Tool 的上下文注入规则：** 当取消发生时，批次中尚未开始执行的 Tool 按以下规则处理：

- 为每个未执行的 ToolCall 注入一条 `ToolResult`：`ToolResult { call_id, content: "[Tool cancelled: user cancelled the request]", is_error: true }`
- 该 `ToolResult` 追加到 `ContextManager` 的消息历史中，并通过 `SessionStorage` 持久化
- 模型在下一轮 Turn 中可以看到这些"已取消"结果，从而知晓哪些 Tool 未实际执行
- 同时发出对应的 `ToolCallStart` + `ToolCallEnd(success=false)` 事件对，确保前端可感知

---

## 10. ContextManager

ContextManager 拥有消息历史，负责追加、查询和压缩。

```rust
pub(crate) struct ContextManager {
    messages: Vec<Message>,
    /// 缓存的稳定 System Prompt 段（system_instructions + override + personality + skills + plugins）
    /// 在压缩后失效重建；system_prompt_override 在 Session 内不变，因此不会额外触发失效
    stable_prompt: Option<String>,
    context_window: usize,
    compact_threshold: f64,
}
```

**System Prompt 分段策略：** System Prompt 分为"稳定段"和"动态段"两部分：

- **稳定段**（缓存）：system_instructions、system_prompt_override、personality、skill list、plugin list。这些在 Session 生命周期内不变，可在首次 Turn 构建后缓存，仅压缩后重建。
- **动态段**（每轮重建）：memories（可能随 Turn 变化）、environment context、TurnStart Hook 的 ContinueWith 返回值。这部分在每轮 Turn 开始时重新获取并拼接到稳定段之后。

**最终顺序与需求文档 0005 §5.3 保持一致：** System Instructions → System Prompt Override（若有）→ Personality → (Tool Definitions via API) → Skill List → Plugin List → Memories → Environment Context → Dynamic Sections。

**关键方法：**

```rust
impl ContextManager {
    /// 追加消息
    pub fn push(&mut self, msg: Message);

    /// 获取当前消息历史（供 ChatRequest 使用）
    pub fn messages(&self) -> &[Message];

    /// 估算当前完整请求的 token 数（含 prompt + messages + tools 预算）
    pub fn estimated_total_tokens(
        &self,
        prompt_chars: usize,
        tools_chars: usize,
    ) -> usize {
        let messages_chars: usize = self.messages.iter().map(|m| m.char_count()).sum();
        (messages_chars + prompt_chars + tools_chars) / 4
    }

    /// 是否需要压缩
    pub fn needs_compaction(&self, prompt_chars: usize, tools_chars: usize) -> bool {
        let estimated = self.estimated_total_tokens(prompt_chars, tools_chars);
        let threshold = (self.context_window as f64 * self.compact_threshold) as usize;
        estimated > threshold
    }

    /// 压缩入口：优先使用摘要压缩，失败时回退到截断压缩
    ///
    /// compact_model 从 AgentConfig.compact_model 获取（未配置则使用当前对话模型）。
    /// compact_prompt 从 AgentConfig.compact_prompt 获取（未配置则使用 DEFAULT_COMPACT_PROMPT）。
    pub async fn compact(
        &mut self,
        provider: &dyn ModelProvider,
        compact_model: &str,
        compact_prompt: &str,
        prefix: &str,
    ) {
        match self.summarize_compact(provider, compact_model, compact_prompt, prefix).await {
            Ok(()) => {}
            Err(_) => self.truncate_compact(prefix),
        }
        self.stable_prompt = None; // 稳定段缓存失效，下轮重建
    }

    /// 摘要压缩：调用模型生成早期消息的摘要，替换原消息
    async fn summarize_compact(
        &mut self,
        provider: &dyn ModelProvider,
        compact_model: &str,
        compact_prompt: &str,
        prefix: &str,
    ) -> Result<()> {
        // 1. 用 compact_prompt 构建压缩请求
        // 2. 调用 provider.chat(compact_model, ...) 生成摘要
        // 3. 保留最近的消息（约占窗口 20%）
        // 4. 用 System { content: prefix + summary } 替换早期消息
        Ok(())
    }

    /// 截断压缩（降级策略）：直接丢弃最早的消息，保留最近 N 条使预算回落到窗口 30% 以内
    fn truncate_compact(&mut self, prefix: &str) {
        // 1. 从最早的消息开始丢弃，直到 estimated_total_tokens 降到 context_window * 0.3
        // 2. 在最前面插入 System { content: prefix + "[早期对话已被截断]" }
        // 3. 保证不破坏 ToolCall/ToolResult 的配对完整性（若截断点在一对中间，整对保留）
    }
}
```

---

## 11. PromptBuilder

PromptBuilder 提供两个方法：`build_stable()` 构建可缓存的稳定段，`build_dynamic()` 构建每轮变化的动态段。TurnLoop 将两者拼接为完整 System Prompt。

```rust
pub(crate) struct PromptBuilder;

impl PromptBuilder {
    /// 构建稳定段（Session 内可缓存，仅压缩后或 system_prompt_override 变更时重建）
    ///
    /// system_prompt_override 来自 SessionConfig，在 system_instructions 之后、
    /// personality 之前注入。new_session()、resume_session()、压缩后重建
    /// 三条路径统一经由此方法处理。
    pub fn build_stable(
        config: &AgentConfig,
        system_prompt_override: Option<&str>,
        skills: &[SkillDefinition],
        plugins: &[(String, String, String)], // (id, display_name, description)
    ) -> String {
        let mut parts = Vec::new();

        // 1. System Instructions
        if !config.system_instructions.is_empty() {
            parts.push(wrap_tag("system_instructions",
                &config.system_instructions.join("\n\n")));
        }

        // 2. System Prompt Override（追加到 system_instructions 之后、personality 之前）
        if let Some(override_prompt) = system_prompt_override {
            parts.push(wrap_tag("system_prompt_override", override_prompt));
        }

        // 3. Personality
        if let Some(p) = &config.personality {
            parts.push(Self::render_personality(p));
        }

        // 4. Tool Definitions → 不在这里，通过 ChatRequest.tools 传递

        // 5. Skill List
        if let Some(s) = Self::render_skills(skills) {
            parts.push(s);
        }

        // 6. Plugin List
        if let Some(p) = Self::render_plugins(plugins) {
            parts.push(p);
        }

        parts.join("\n\n")
    }

    /// 构建动态段（每轮 Turn 重建）
    /// 顺序：Memories → Environment Context → Dynamic Sections（与需求文档 0005 §5.3 一致）
    pub fn build_dynamic(
        memories: &[Memory],
        environment_context: Option<&EnvironmentContext>,
        dynamic_sections: &[String], // 来自 TurnStart Hook 的 ContinueWith
    ) -> String {
        let mut parts = Vec::new();

        // 6. Memories
        if !memories.is_empty() {
            parts.push(Self::render_memories(memories));
        }

        // 7. Environment Context
        if let Some(env) = environment_context {
            parts.push(Self::render_environment(env));
        }

        // 8. Dynamic Sections
        for section in dynamic_sections {
            parts.push(section.clone());
        }

        parts.join("\n\n")
    }

    /// 拼接稳定段 + 动态段为完整 System Prompt
    pub fn combine(stable: &str, dynamic: &str) -> String {
        if dynamic.is_empty() {
            stable.to_string()
        } else {
            format!("{}\n\n{}", stable, dynamic)
        }
    }
}
```

内置的 prompt 模板独立在 `prompt/templates.rs` 中（对应需求文档 Appendix B）：

```rust
// prompt/templates.rs

// B.1 上下文压缩
pub(crate) const DEFAULT_COMPACT_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION...";
// B.2 压缩摘要前缀
pub(crate) const COMPACT_SUMMARY_PREFIX: &str = "Another language model started to solve...";
// B.4 Skill 列表固定文本
pub(crate) const SKILL_SECTION_HEADER: &str = "## Skills\n\nBelow is the list of skills...";
pub(crate) const SKILL_USAGE_RULES: &str = "### How to use skills\n- Trigger rules: ...";
// B.5 Plugin 列表固定文本
pub(crate) const PLUGIN_SECTION_HEADER: &str = "## Plugins\n\nThe following plugins are active...";
// B.6 Memory 使用指令
pub(crate) const MEMORY_INSTRUCTIONS: &str = "## Memory\n\nYou have access to memories from prior sessions...";
// B.10 Memory 提取 prompt
pub(crate) const MEMORY_EXTRACTION_PROMPT: &str = "You are a Memory Writing Agent...";
// B.11 Memory 提取输入模板
pub(crate) const MEMORY_EXTRACTION_INPUT_TEMPLATE: &str = "Analyze this session and produce JSON...";
// B.12 Memory 整合 prompt
pub(crate) const MEMORY_CONSOLIDATION_PROMPT: &str = "You are a Memory Writing Agent. Your job: consolidate...";
```

---

## 12. ToolDispatcher

```rust
pub(crate) struct ToolDispatcher {
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
    timeout: Duration,
}

impl ToolDispatcher {
    /// 执行一批 ToolCall，根据 mutating 标记决定并发或串行
    pub async fn execute_batch(
        &self,
        calls: Vec<ToolCallRequest>,
        hooks: &HookRegistry,
        cancel: &CancellationToken,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Vec<ToolResult> {
        let has_mutating = calls.iter().any(|c| {
            self.handlers.get(&c.tool_name)
                .map_or(false, |h| h.is_mutating())
        });

        if has_mutating {
            self.execute_serial(calls, hooks, cancel, event_tx).await
        } else {
            // 并发执行，但结果按 calls 的原始顺序归并
            // 保证事件顺序、上下文顺序、持久化顺序与模型返回顺序一致
            self.execute_concurrent(calls, hooks, cancel, event_tx).await
        }
    }
}
```

**单个 Tool 执行流程：**

```
┌─ Hook: BeforeToolUse
│   └─ Abort? → 不执行 handler，生成 synthetic ToolResult:
│               ToolResult { is_error: true, content: "Tool aborted by hook: {reason}" }
│               emit ToolCallStart + ToolCallEnd(success=false)
│               → 继续下一个 Tool
├─ emit ToolCallStart
├─ tokio::time::timeout(handler.execute())
│   └─ 超时? → ToolResult { is_error: true, content: "Tool timed out after {ms}ms" }
├─ 输出截断：若 output.content.len() > 1MB → 截断 + 追加 "\n\n[output truncated at 1MB]"
├─ emit ToolCallEnd(success = !output.is_error)
└─ Hook: AfterToolUse
```

**关键契约：**

- Abort 不执行 handler，但仍发出 ToolCallStart/ToolCallEnd 事件对（前端可感知）
- 1MB 截断在 handler 返回后、事件发送前执行，保证事件、上下文、持久化看到的是同一份截断后结果
- 截断提示固定为 `\n\n[output truncated at 1MB]`
- **结果顺序保证**：无论并发还是串行执行，`execute_batch()` 返回的 `Vec<ToolResult>` 始终按输入 `calls` 的原始顺序排列。并发执行时使用 `JoinSet` + index 映射，完成后按 index 重排。这保证了事件流、上下文历史、持久化存储中的 ToolResult 顺序与模型请求顺序一致

---

## 13. HookRegistry

```rust
pub struct HookRegistry {
    handlers: HashMap<HookEvent, Vec<HookEntry>>,
}

struct HookEntry {
    plugin_id: String,
    plugin_config: serde_json::Value,
    handler: Arc<dyn Fn(HookPayload) -> Pin<Box<dyn Future<Output = HookResult> + Send>> + Send + Sync>,
}

impl HookRegistry {
    /// Plugin 在 apply() 中调用此方法注册 handler
    pub fn tap(
        &mut self,
        event: HookEvent,
        plugin_id: String,
        plugin_config: serde_json::Value,
        handler: impl Fn(HookPayload) -> Pin<Box<dyn Future<Output = HookResult> + Send>> + Send + Sync + 'static,
    );

    /// 按注册顺序依次执行，遇 Abort 立即停止
    /// 返回 DispatchOutcome，调用方可获取最终修改后的 payload
    pub(crate) async fn dispatch(
        &self,
        event: HookEvent,
        mut payload: HookPayload,
    ) -> DispatchOutcome {
        let handlers = match self.handlers.get(&event) {
            Some(h) => h,
            None => return DispatchOutcome { payload, aborted: None },
        };
        for entry in handlers {
            payload.plugin_config = entry.plugin_config.clone();
            match (entry.handler)(payload.clone()).await {
                HookResult::Continue => {}
                HookResult::ContinueWith(data) => { payload.data = data; }
                HookResult::Abort(reason) => {
                    return DispatchOutcome {
                        payload,
                        aborted: Some(reason),
                    };
                }
            }
        }
        DispatchOutcome { payload, aborted: None }
    }
}

/// dispatch 的返回值，携带最终修改后的 payload
pub(crate) struct DispatchOutcome {
    /// 经所有 handler 修改后的最终 payload
    pub payload: HookPayload,
    /// 若被 Abort，携带 reason；None 表示正常完成
    pub aborted: Option<String>,
}
```

---

## 14. SkillManager

```rust
pub(crate) struct SkillManager {
    skills: HashMap<String, SkillDefinition>,
}

impl SkillManager {
    /// 从用户输入中检测 /skill_name 触发
    /// 仅扫描 ContentBlock::Text 块中的文本，不扫描 File 内容和 Image
    /// 多个 Skill 触发时按输入中出现的顺序返回，全部注入
    pub fn detect_invocations(&self, input: &UserInput) -> Vec<&SkillDefinition> {
        // 1. 遍历 input.content，仅处理 ContentBlock::Text 变体
        // 2. 对每个文本块执行正则匹配 /word_chars，查表返回
        // 3. 去重（同一 Skill 只触发一次），保持首次出现顺序
    }

    /// 将触发的 Skill 渲染为注入消息
    pub fn render_injection(&self, skill: &SkillDefinition) -> Message {
        Message::System {
            content: format!("<skill>\n<name>{}</name>\n{}\n</skill>",
                skill.name, skill.prompt),
        }
    }

    /// 返回 allow_implicit_invocation=true 的 Skill（供 PromptBuilder 使用）
    pub fn implicit_skills(&self) -> Vec<&SkillDefinition> {
        self.skills.values().filter(|s| s.allow_implicit_invocation).collect()
    }
}
```

---

## 15. MemoryManager

```rust
pub(crate) struct MemoryManager {
    storage: Arc<dyn MemoryStorage>,
    namespace: String,
    max_items: usize,
}

impl MemoryManager {
    /// Session 开始或每轮 Turn 开始时加载记忆
    /// query 基于当前用户输入文本，用于相关性检索
    pub async fn load_memories(&self, query: &str) -> Result<Vec<Memory>> {
        self.storage.search(&self.namespace, query, self.max_items).await
    }

    /// 提取记忆（Session 结束或 checkpoint 时调用）
    /// 采用两阶段流程：Phase 1 抽取候选记忆，Phase 2 与现存记忆整合
    pub async fn extract_memories(
        &self,
        session_messages: &[Message],
        provider: &dyn ModelProvider,
        model_id: &str,
    ) -> Result<()> {
        // ── Phase 1: 抽取候选记忆 ──
        // 1. 将 session_messages 渲染为文本
        // 2. 使用 MEMORY_EXTRACTION_PROMPT + MEMORY_EXTRACTION_INPUT_TEMPLATE 构建请求
        // 3. 调用 provider.chat() 获取提取结果
        // 4. 解析 JSON → 候选 Memory 列表

        // ── Phase 2: 与现存记忆整合（Consolidation） ──
        // 5. 从 storage 加载当前 namespace 下的已有记忆
        // 6. 使用 MEMORY_CONSOLIDATION_PROMPT 构建整合请求，
        //    输入包含：候选记忆列表 + 已有记忆列表
        // 7. 调用 provider.chat() 获取整合决策
        // 8. 解析 JSON → 操作列表，每个操作为以下之一：
        //    - create: 新建记忆 → storage.save(new_memory)
        //    - update: 更新已有记忆 → storage.save(updated_memory)（按 id upsert）
        //    - delete: 删除过时记忆 → storage.delete(id)
        //    - skip: 重复记忆，不做任何操作
        Ok(())
    }
}
```

**记忆加载时机与 query 来源：**
- **Session 开始时**：使用空 query `""` 加载 namespace 下的通用记忆（泛化召回）
- **每轮 Turn 开始时**：使用当前用户输入的文本内容作为 query，获取与当前请求相关的记忆，更新动态段中的 memories
- MemoryStorage 的 `search` 实现负责按相关度排序，Agent 取 top_k 结果

**记忆整合与 B.12 的对应关系：** `MEMORY_CONSOLIDATION_PROMPT`（Appendix B.12）在 Phase 2 中使用。模型接收候选记忆和已有记忆，输出结构化的操作列表。这保证了模型能稳定判断"更新旧记忆还是新增新记忆"，避免记忆越跑越脏。

**Checkpoint 的幂等性：** 每次 checkpoint 提取的是当前 Session 从上次 checkpoint 到现在的增量消息。`memory_checkpoint_counter` 记录上次 checkpoint 时的 turn 位置，避免重复提取同一段对话。若 checkpoint 执行中途失败，下次 checkpoint 会重新覆盖相同范围，由 Phase 2 的整合逻辑保证幂等。

---

## 16. Error Handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    // 构建阶段
    #[error("[NO_MODEL_PROVIDER] at least one ModelProvider is required")]
    NoModelProvider,
    #[error("[NAME_CONFLICT] {kind} name '{name}' is duplicated")]
    NameConflict { kind: &'static str, name: String },
    #[error("[SKILL_DEPENDENCY_NOT_MET] skill '{skill}' requires tool '{tool}'")]
    SkillDependencyNotMet { skill: String, tool: String },
    #[error("[PLUGIN_INIT_FAILED] plugin '{id}': {source}")]
    PluginInitFailed { id: String, source: anyhow::Error },
    #[error("[STORAGE_DUPLICATE] {kind} storage registered more than once")]
    StorageDuplicate { kind: &'static str },
    #[error("[INVALID_DEFAULT_MODEL] model '{0}' is not registered")]
    InvalidDefaultModel(String),

    // 运行阶段
    #[error("[SESSION_BUSY] a session is already running")]
    SessionBusy,
    #[error("[TURN_BUSY] a turn is already running in this session")]
    TurnBusy,
    #[error("[MODEL_NOT_SUPPORTED] model '{0}' is not supported by any provider")]
    ModelNotSupported(String),
    #[error("[PROVIDER_ERROR] {message}")]
    ProviderError { message: String, source: anyhow::Error, retryable: bool },
    #[error("[TOOL_EXECUTION_ERROR] tool '{name}': {source}")]
    ToolExecutionError { name: String, source: anyhow::Error },
    #[error("[TOOL_TIMEOUT] tool '{name}' timed out after {timeout_ms}ms")]
    ToolTimeout { name: String, timeout_ms: u64 },
    #[error("[TOOL_NOT_FOUND] tool '{0}'")]
    ToolNotFound(String),
    #[error("[SKILL_NOT_FOUND] skill '{0}'")]
    SkillNotFound(String),
    #[error("[SESSION_NOT_FOUND] session '{0}'")]
    SessionNotFound(String),
    #[error("[STORAGE_ERROR] {0}")]
    StorageError(anyhow::Error),
    #[error("[MAX_TOOL_CALLS_EXCEEDED] limit is {limit}")]
    MaxToolCallsExceeded { limit: usize },
    #[error("[COMPACT_ERROR] {0}")]
    CompactError(anyhow::Error),
    #[error("[PLUGIN_ABORTED] hook '{hook}' aborted: {reason}")]
    PluginAborted { hook: &'static str, reason: String },
    #[error("[REQUEST_CANCELLED]")]
    RequestCancelled,
}

impl AgentError {
    pub fn code(&self) -> &'static str { /* match self => "NO_MODEL_PROVIDER" etc */ }
    pub fn retryable(&self) -> bool { matches!(self, Self::ProviderError { retryable: true, .. }) }
    pub fn source_component(&self) -> &'static str {
        match self {
            Self::ProviderError { .. } => "provider",
            Self::ToolExecutionError { .. } | Self::ToolTimeout { .. } => "tool",
            Self::PluginInitFailed { .. } | Self::PluginAborted { .. } => "plugin",
            Self::StorageError(_) => "storage",
            _ => "agent",
        }
    }
}
```

---

## 17. Public API Summary

调用方看到的全部公共类型：

```
// 构建
AgentBuilder::new(config) -> AgentBuilder<NoProvider>
  .register_model_provider(p) -> AgentBuilder<HasProvider>
  .register_tool(t) -> Self
  .register_skill(s) -> Self
  .register_plugin(p, config) -> Self
  .register_session_storage(s) -> Self
  .register_memory_storage(s) -> Self
  .build() -> Result<Agent>

// 运行
Agent::new_session(config) -> Result<Session>
Agent::resume_session(id) -> Result<Session>
Agent::shutdown() -> Result<()>

Session::send_message(input) -> Result<RunningTurn>
Session::close() -> Result<()>

RunningTurn::events() -> &mut mpsc::Receiver<AgentEvent>
RunningTurn::cancel()

// 类型
AgentConfig, SessionConfig, UserInput, RunningTurn
AgentEvent, AgentEventPayload
AgentError
Message, ContentBlock, MessageStatus
SessionEvent, SessionEventPayload
SkillDefinition, Memory, EnvironmentContext
HookEvent, HookPayload, HookResult
PluginContext (传给 Plugin::apply)
SessionSummary, SessionPage

// Traits
ModelProvider, ModelInfo, ChatRequest, ChatEvent
ToolHandler, ToolInput, ToolOutput
Plugin
SessionStorage, MemoryStorage
```

---

## 18. Design Decisions

### D1: 为什么 TurnLoop 是函数而非 struct？

TurnLoop 没有自己的状态——它操作 Session 的状态。将它做成函数而非 Actor/struct 避免了状态管理复杂度。Session 本身已经是一个状态容器，不需要再套一层。

### D2: 为什么不用 Actor 模型？

需求是单会话模型，不存在多个并发 Actor 的场景。Agent 的互斥只需要一个 `Mutex<Option<ActiveSessionHandle>>` 守卫。Actor 模型引入的 channel/mailbox 机制对这个场景过重。

### D3: 为什么 AgentBuilder 用 typestate？

编译期保证至少注册一个 ModelProvider，比运行时检查更安全。只用了一个 typestate 参数（NoProvider/HasProvider），没有过度泛型化。

### D4: 为什么 PromptBuilder 无状态？

System Prompt 的组装是纯函数：相同输入必须产出相同输出。没有理由让它持有状态。将它做成 struct 的静态方法（或关联函数）最简单。

### D5: 为什么 Hook handler 是 Fn 而非独立 trait？

Plugin trait 已经是注册入口。Hook handler 只是 Plugin 内部注册的回调。用 `Fn` 闭包比再定义一个 `HookHandler` trait 更轻量，Plugin 可以在 `apply()` 中直接用闭包捕获 self。

### D6: 为什么 ContextManager 用 Vec<Message> 而非自定义数据结构？

KISS。消息历史本质是有序列表，Vec 完全够用。不需要 ring buffer（压缩不是删除头部而是替换为摘要）、不需要 B-tree（不按 key 查找）。
