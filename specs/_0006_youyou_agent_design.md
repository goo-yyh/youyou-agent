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

**持久化与恢复：** SessionConfig 在会话创建时通过 `SessionStorage::save_event()` 持久化为 `Metadata` 事件（key="session_config"）。恢复会话时，`resume_session()` 从 Metadata 事件中还原 `model_id` 和 `system_prompt_override`，用于重建 SessionState。若 Metadata 中无 session_config，则使用 AgentConfig 的默认值。

### 4.4 AgentConfig（Agent 配置）

`AgentConfig` 在 `AgentBuilder::new()` 时传入，构建后不可变。各字段与消费模块的映射关系如下：

```rust
pub struct AgentConfig {
    /// 默认模型 ID，新建会话时未指定 model_id 则使用此值
    /// 消费方：Agent::new_session()、ModelRouter
    pub default_model: String,
    /// 系统指令文本列表，按序拼接注入 System Prompt
    /// 消费方：PromptBuilder::build()
    pub system_instructions: Vec<String>,
    /// 人设定义文本，Agent 自动包裹 <personality_spec> 标签后注入
    /// 消费方：PromptBuilder::build()
    pub personality: Option<String>,
    /// 环境上下文数据（cwd / shell / date / timezone 等）
    /// 消费方：PromptBuilder::build()
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

**多模态输入校验：** `send_message()` 在步骤 1（**任何状态修改之前**）对 `UserInput` 进行纯校验：
- 图片大小上限 20MB，超出返回 `AgentError::InputValidation`
- 图片格式仅接受 PNG、JPEG、GIF、WebP，不符合返回 `AgentError::InputValidation`
- `content` 不能为空，空输入返回 `AgentError::InputValidation`

**关键顺序保证：** 输入校验在步骤 1 完成（不持有任何锁、不修改任何状态），步骤 2 才进入 `active_session` 临界区执行 `Idle → Running` 状态切换。这确保校验失败的同步错误路径不会将 Session 卡在 `Running` 状态。

校验在 Agent 层完成，不依赖 ModelProvider。ModelProvider 的格式兼容性校验（如 vision 能力检查）由 Provider 自身在 `chat()` 调用时返回错误。

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
    pub data: HookData, // 强类型的事件特定数据（见下方定义）
    pub timestamp: DateTime<Utc>,
}

/// 各 Hook 事件的强类型数据，保证 Plugin 间的可组合性
pub enum HookData {
    SessionStart { model_id: String },
    SessionEnd { message_count: usize },
    TurnStart {
        user_input: UserInput,
        /// Plugin 通过 ContinueWith 追加的动态段，不会覆盖 user_input
        dynamic_sections: Vec<String>,
    },
    TurnEnd {
        assistant_output: String,
        tool_calls_count: usize,
        cancelled: bool,
    },
    BeforeToolUse {
        tool_name: String,
        arguments: serde_json::Value,
    },
    AfterToolUse {
        tool_name: String,
        output: ToolOutput,
        duration_ms: u64,
        success: bool,
    },
    BeforeCompact {
        message_count: usize,
        estimated_tokens: usize,
    },
}

/// Hook 结果：仅 BeforeToolUse 和 TurnStart 支持 ContinueWith，
/// 其余 Hook 只支持 Continue 和 Abort。
/// ContinueWith 使用事件特化的 patch 类型，通过字段范围收窄约束 Plugin 可修改的字段，
/// 但 event/patch 变体的匹配仍在 runtime 校验（不匹配时 warn 降级为 Continue）。
pub enum HookResult {
    Continue,
    /// 继续执行，并应用 patch 修改。仅 TurnStart 和 BeforeToolUse 支持。
    /// 其他 Hook 返回此变体时记录 warn 日志，降级为 Continue。
    ContinueWith(HookPatch),
    Abort(String),
}

/// 事件特化的 patch 类型，限制 Plugin 可修改的字段范围。
/// 每种 patch 只暴露该 Hook 允许修改的字段，不可能越权修改其他数据。
pub enum HookPatch {
    /// TurnStart 的 patch：仅允许追加 dynamic sections，不可修改 user_input
    TurnStart {
        append_dynamic_sections: Vec<String>,
    },
    /// BeforeToolUse 的 patch：仅允许修改 arguments
    BeforeToolUse {
        arguments: serde_json::Value,
    },
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

impl HookEvent {
    /// 是否支持 ContinueWith 返回值
    pub fn supports_continue_with(&self) -> bool {
        matches!(self, Self::TurnStart | Self::BeforeToolUse)
    }
}
```

**ContinueWith 的字段范围约束：** `HookResult::ContinueWith(HookPatch)` 使用事件特化的 patch 类型而非复用整个 `HookData`，通过字段范围收窄约束 Plugin 的修改范围（Plugin 无法构造出越权修改 `user_input` 或 `tool_name` 的 patch 值）。但由于 `HookPatch` 是总枚举，Plugin 仍可在错误的 Hook 事件中返回不匹配的 patch 变体，此情况在 runtime 通过 `warn!` + 降级为 `Continue` 处理：

| Hook | HookPatch 变体 | 语义 |
|------|---------------|------|
| TurnStart | `HookPatch::TurnStart { append_dynamic_sections }` | 向 `dynamic_sections` 追加新段（多个 Plugin 链式传递，后一个看到前一个的结果）。**不可修改 `user_input`** |
| BeforeToolUse | `HookPatch::BeforeToolUse { arguments }` | 修改 `arguments`（参数拦截/改写）。**不可修改 `tool_name`** |

**校验规则：**
- 若 Hook event 不支持 `ContinueWith`（非 TurnStart / BeforeToolUse），记录 `warn!` 日志，降级为 `Continue`
- 若 `HookPatch` 变体与当前 `HookData` 不匹配（编程错误），记录 `warn!` 日志，降级为 `Continue`

**Abort 处理规则：**

| Hook | Abort 行为 |
|------|-----------|
| SessionStart | 回滚已持久化的 Metadata（通过 `SessionStorage::delete_session(session_id)` 清除），`SessionSlotGuard` 自动 Drop 释放槽位，返回 `PluginAborted`，会话不创建 |
| TurnStart | 终止本轮 Turn，emit `Error(PluginAborted)`，不进入模型调用 |
| BeforeCompact | 跳过本次压缩（不视为错误） |
| BeforeToolUse | 不执行 handler，生成 synthetic `ToolResult(is_error=true)`，继续下一个 Tool |
| AfterToolUse | 将 reason 注入上下文，BREAK 退出 Tool Loop，进入最终模型调用生成收尾回复 |
| TurnEnd / SessionEnd | **不阻断**：记录 warn 日志，降级处理（处于收尾/关闭路径，阻断会影响持久化和资源释放） |

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

### 4.9 ToolCallRequest

```rust
/// ToolDispatcher 执行批次的输入项，从模型返回的 ToolCall 消息中构建
pub struct ToolCallRequest {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}
```

### 4.11 EnvironmentContext

```rust
pub struct EnvironmentContext {
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub os: Option<String>,
    pub date: Option<String>,
    pub timezone: Option<String>,
}
```

### 4.12 Memory

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

### 4.13 SkillDefinition

```rust
pub struct SkillDefinition {
    /// 唯一名称，用于 /name 触发
    pub name: String,
    /// 显示名称
    pub display_name: String,
    /// 简短描述
    pub description: String,
    /// 完整的 prompt 内容
    pub prompt: String,
    /// 依赖的 Tool 名称列表（构建阶段校验）
    pub required_tools: Vec<String>,
    /// 是否在 System Prompt 中列出供模型参考
    pub allow_implicit_invocation: bool,
}
```

### 4.14 SessionSummary 与 SessionPage

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
    /// 注册 Hook handler。接收 `Arc<Self>` 以便 handler 闭包可以克隆并捕获 Plugin 的所有权引用。
    /// 框架通过 `Arc::clone(&plugin)` 传入，handler 闭包中 `let me = Arc::clone(&self);` 即可捕获到 `'static` 闭包。
    fn apply(self: Arc<Self>, ctx: &mut PluginContext);
    async fn shutdown(&self) -> Result<()>;
}

/// apply() 的上下文，封装 plugin 元信息和 Hook 注册能力
pub struct PluginContext {
    pub plugin_id: String,
    pub plugin_config: serde_json::Value,
    registry: HookRegistry,
}

impl PluginContext {
    /// 注册 hook handler（内部自动注入 plugin_id 和 plugin_config）
    /// handler 接收强类型 HookPayload（含 HookData 枚举），返回 HookResult
    pub fn tap(
        &mut self,
        event: HookEvent,
        handler: impl Fn(HookPayload) -> Pin<Box<dyn Future<Output = HookResult> + Send>>
            + Send + Sync + 'static,
    ) {
        self.registry.tap(event, self.plugin_id.clone(),
            self.plugin_config.clone(), handler);
    }

    /// 消费 PluginContext，返回内部的 HookRegistry（框架在所有 Plugin apply 完成后合并）
    pub(crate) fn into_registry(self) -> HookRegistry {
        self.registry
    }
}
```

**生命周期设计：** `apply(self: Arc<Self>, ctx: &mut PluginContext)` 接收 `Arc<Self>`，handler 闭包可以 `clone()` 该 `Arc` 并捕获到 `'static` 闭包中，无借用生命周期冲突。`PluginContext` 不再持有生命周期参数，避免了 `&self` 无法被 `'static` handler 持有的问题。Plugin 通过模式匹配 `HookData` 变体获取和修改事件数据，编译期保证类型安全。

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
    /// 按 namespace + query 搜索相关记忆（用于 Turn 级注入上下文，返回 top-k 结果，按相关度排序）
    /// query 不能为空字符串，空 query 应使用 list_recent()
    async fn search(&self, namespace: &str, query: &str, limit: usize) -> Result<Vec<Memory>>;
    /// 按 namespace 列出最近更新的记忆（用于 Session 启动时的 bootstrap 加载，按 updated_at 降序）
    async fn list_recent(&self, namespace: &str, limit: usize) -> Result<Vec<Memory>>;
    /// 列出指定 namespace 下的所有记忆（用于提取时加载已有记忆供模型做整合判断）
    async fn list_by_namespace(&self, namespace: &str) -> Result<Vec<Memory>>;
    /// Upsert 记忆：按 id 匹配，内容相同则仅更新 updated_at 时间戳，内容不同则更新内容和时间戳
    async fn upsert(&self, memory: Memory) -> Result<()>;
    /// 删除指定 id 的记忆
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
    /// 注册 SessionStorage。内部仅暂存，重复注册的校验统一延迟到 build() 执行。
    pub fn register_session_storage(mut self, s: impl SessionStorage + 'static) -> Self { /* ... */ }
    /// 注册 MemoryStorage。内部仅暂存，重复注册的校验统一延迟到 build() 执行。
    pub fn register_memory_storage(mut self, s: impl MemoryStorage + 'static) -> Self { /* ... */ }
}

impl AgentBuilder<HasProvider> {
    /// 校验所有注册项 + 初始化 Plugin + 返回 Agent
    pub async fn build(self) -> Result<Agent> { /* ... */ }
}
```

`build()` 内部执行所有校验规则（见需求文档 6.1 节），初始化 Plugin 并调用 `apply(Arc::clone(&plugin), &mut ctx)` 注册 hook handler，最后将各 `PluginContext` 的 `HookRegistry` 合并为最终的全局 `HookRegistry`。额外校验：
- 若配置了 `compact_model`，验证对应的 model_id 已在某个已注册的 Provider 中声明，否则返回 `AgentError::InvalidCompactModel`
- 若配置了 `memory_model`，验证对应的 model_id 已在某个已注册的 Provider 中声明，否则返回 `AgentError::InvalidMemoryModel`
- **Storage 重复注册校验**：若 `register_session_storage()` 被调用超过一次，返回 `AgentError::StorageDuplicate { kind: "session" }`；`register_memory_storage()` 同理。Builder 内部使用 `Vec` 暂存所有注册的 Storage，`build()` 时检查长度是否超过 1。这避免了在链式调用的中间步骤 panic 或返回 `Result`，所有校验统一在 `build()` 返回错误

---

## 7. Agent

Agent 是一个轻量 handle，内部持有 `Arc<AgentInner>`。`AgentInner` 包含所有不可变注册表和一个 Session 槽。

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
    /// Agent 生命周期状态：shutdown() 调用后设为 true，后续所有操作返回 AgentShutdown 错误。
    /// 使用 AtomicBool 避免锁嵌套（与 active_session 锁独立）。
    pub is_shutdown: AtomicBool,
    // 活跃 Session 槽：None = 空闲，Some = 运行中
    pub active_session: Mutex<Option<ActiveSessionInfo>>,
}

/// 活跃 Session 的控制信息，包含执行关闭流程所需的全部状态。
/// Agent::shutdown() 通过此结构直接驱动关闭，无需持有 Session handle。
pub(crate) struct ActiveSessionInfo {
    pub session_id: String,
    /// Session 级关闭令牌（唯一来源），close_internal() cancel 此 token 级联取消 Turn。
    /// send_message() 从此处 clone token 并创建 child_token 给 Turn 使用。
    pub session_close_token: CancellationToken,
    /// 共享 SessionState，close_internal() 需要访问上下文做记忆提取
    pub state: Arc<Mutex<SessionState>>,
    /// Session 阶段状态机，标记当前 Turn 的运行状态。
    /// send_message() 原子地从 Idle 切换到 Running 并安装 RunningTurnHandle。
    /// close_internal() 根据此状态决定是否需要等待 Turn 结束。
    pub phase: SessionPhase,
}

/// Session 阶段状态机，保证 Turn 启动和关闭的原子性。
/// send_message() 在单次获取 active_session 锁期间完成 Idle → Running 切换，
/// 消除了原 turn_in_progress + turn_finished_rx 分步写入的竞态窗口。
pub(crate) enum SessionPhase {
    /// 空闲，无活跃 Turn
    Idle,
    /// 有活跃 Turn 正在运行，持有该 Turn 的控制句柄
    Running(RunningTurnHandle),
}

/// 活跃 Turn 的控制句柄，在 send_message() 的单次临界区内原子创建。
/// 包含 close_internal() 等待 Turn 结束所需的全部信息。
pub(crate) struct RunningTurnHandle {
    /// Turn 级取消令牌（session_close_token 的 child_token）
    pub turn_cancel_token: CancellationToken,
    /// Turn 完成信号 receiver，supervisor task 退出时 sender 自动 drop
    pub turn_finished_rx: oneshot::Receiver<()>,
}
```

**Session 槽是简单的 `Option`**：`None` 表示空闲，`Some` 表示有活跃 Session。`close()` 是同步关闭——调用方 await `close()` 完成后，槽才清为 `None`。不需要 `Closing` 中间态。`ActiveSessionInfo` 内部通过 `SessionPhase` 状态机区分 Turn 运行状态（`Idle` / `Running`），`send_message()` 在单次临界区内原子完成 `Idle → Running` 切换。

**关闭协调模型：** `ActiveSessionInfo` 持有执行关闭流程所需的全部状态（`SessionState`、`SessionPhase`、Turn 完成信号）。`Session::close()` 和 `Agent::shutdown()` 均通过 `AgentInner::close_internal()` 统一执行关闭流程，保证无论从哪条路径关闭，都走同一条确定的状态闭环。

**锁安全规则：** `active_session` 和 `SessionState` 均使用 `std::sync::Mutex`，**严禁持锁跨 `.await`**。所有操作遵循"短暂获取锁 → 读写状态 → 立即释放 → 再做异步操作"的模式。两把锁之间不存在嵌套获取关系。Turn 运行状态（`SessionPhase`）仅通过 `active_session` 锁访问，`SessionState` 锁仅用于上下文数据（消息历史、计数器等），职责清晰分离。

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

**Session Slot Claim/Release 机制（统一）：**

`new_session()` 和 `resume_session()` 共用同一个 `claim_session_slot()` / `release_session_slot()` 内部方法，保证单会话槽的互斥闭环：

```rust
impl AgentInner {
    /// 尝试占据 Session 槽。成功返回 SessionSlotGuard（RAII），失败返回 SessionBusy。
    /// Guard 持有 Arc<AgentInner>，Drop 时自动清除槽位，防止失败路径遗漏清理。
    pub(crate) fn claim_session_slot(
        self: &Arc<Self>,
        session_id: String,
        state: Arc<Mutex<SessionState>>,
    ) -> Result<SessionSlotGuard> {
        // 校验 Agent 未 shutdown
        if self.is_shutdown.load(Ordering::Acquire) {
            return Err(AgentError::AgentShutdown);
        }
        let mut slot = self.active_session.lock().unwrap();
        if slot.is_some() {
            return Err(AgentError::SessionBusy);
        }
        let close_token = CancellationToken::new();
        *slot = Some(ActiveSessionInfo {
            session_id: session_id.clone(),
            session_close_token: close_token.clone(),
            state: Arc::clone(&state),
            phase: SessionPhase::Idle,
        });
        Ok(SessionSlotGuard {
            inner: Arc::clone(self),
            disarmed: false,
        })
    }

    /// 统一关闭流程，Session::close() 和 Agent::shutdown() 共用。
    /// 顺序保证：取消 Turn → 等待 Turn 结束 → SessionEnd Hook → 记忆提取 → 释放槽位
    pub(crate) async fn close_internal(&self, session_id: &str) -> Result<()> {
        // 1. 短暂获取锁，取出关闭所需状态，将 phase 切换为 Idle
        let (close_token, state, turn_handle) = {
            let mut slot = self.active_session.lock().unwrap();
            let info = slot.as_mut()
                .ok_or_else(|| AgentError::SessionNotFound(session_id.to_owned()))?;
            let close_token = info.session_close_token.clone();
            let state = Arc::clone(&info.state);
            // 取出 RunningTurnHandle（若有），同时将 phase 归位为 Idle
            let turn_handle = match std::mem::replace(&mut info.phase, SessionPhase::Idle) {
                SessionPhase::Running(handle) => Some(handle),
                SessionPhase::Idle => None,
            };
            (close_token, state, turn_handle)
        }; // 锁已释放

        // 2. 取消当前 Turn（级联取消 turn_cancel_token）
        close_token.cancel();

        // 3. 等待 Turn 真正结束（若有活跃 Turn）
        //    由于 turn_handle 在步骤 1 的同一临界区内取出，不存在竞态：
        //    - 若 send_message() 已安装 handle → 一定能取到
        //    - 若 send_message() 尚未安装 handle → phase 仍为 Idle，无需等待
        if let Some(handle) = turn_handle {
            let _ = handle.turn_finished_rx.await; // 忽略 sender dropped 的情况
        }

        // 4. 触发 SessionEnd Hook（Abort 降级，不阻断）
        let message_count = {
            let st = state.lock().unwrap();
            st.context.messages().len()
        };
        let _ = self.hook_registry.dispatch(
            HookEvent::SessionEnd,
            /* payload with SessionEnd data */
        ).await;

        // 5. 执行记忆提取（失败不阻塞）
        //    使用增量提取：从 last_checkpoint_message_index 开始的消息
        if self.memory_storage.is_some() {
            let (messages, start_idx) = {
                let st = state.lock().unwrap();
                let idx = st.last_checkpoint_message_index;
                (st.context.messages()[idx..].to_vec(), idx)
            };
            if !messages.is_empty() {
                let _ = MemoryManager::extract_memories(&messages, /* ... */).await;
            }
        }

        // 6. 释放槽位
        *self.active_session.lock().unwrap() = None;
        Ok(())
    }
}

/// RAII Guard：Drop 时自动 release 槽位。
/// 构建 Session 成功后调用 disarm() 转移所有权给 Session::close()。
pub(crate) struct SessionSlotGuard {
    inner: Arc<AgentInner>,
    disarmed: bool,
}

impl SessionSlotGuard {
    /// 标记为已交接，Drop 不再自动清槽
    pub fn disarm(&mut self) { self.disarmed = true; }
}

impl Drop for SessionSlotGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            *self.inner.active_session.lock().unwrap() = None;
        }
    }
}
```

**`new_session()` 执行路径：**

1. 分配 `session_id`（UUID v4）
2. 确定 `model_id`：`config.model_id.unwrap_or(agent_config.default_model)`
3. 从 MemoryStorage 加载 bootstrap 记忆（`load_bootstrap_memories()`），保存到 `SessionState.bootstrap_memories`
4. 构建 `SessionState`（含 `bootstrap_memories`）
5. **`claim_session_slot(session_id, state)`**：获取 `SessionSlotGuard`，若已有活跃 Session 返回 `SessionBusy`
6. 持久化 `session_config` 为 Metadata 事件（通过 `SessionStorage::save_event()`）
7. 触发 **SessionStart Hook**：
   - payload.data 为 `HookData::SessionStart { model_id }`
   - 若任一 Plugin 返回 `Abort(reason)`：回滚已持久化的 Metadata（调用 `SessionStorage::delete_session(session_id)`），Guard 自动 Drop 清槽，返回 `AgentError::PluginAborted`
8. **`guard.disarm()`**：标记 Guard 已交接，槽位由 `Session::close()` / `Agent::shutdown()` 的 `close_internal()` 负责清理
9. 返回 `Session`

**shutdown 执行路径：**

1. **设置 `is_shutdown = true`**（`AtomicBool::store(true, Ordering::Release)`），阻止后续 `new_session()`、`resume_session()`、`send_message()` 调用
2. 若有活跃 Session，通过 `close_internal(session_id)` 执行统一关闭流程（取消 Turn → 等待 Turn 结束 → SessionEnd Hook → 记忆提取 → 释放槽）。`shutdown()` 不需要持有 `Session` handle，因为 `ActiveSessionInfo` 中已包含关闭所需的全部状态
3. 按逆序调用所有 Plugin 的 `shutdown()`
4. 释放资源

**shutdown 后的对象可用性：**
- `Agent::new_session()` / `resume_session()` → 返回 `AgentError::AgentShutdown`
- `Session::send_message()` → 返回 `AgentError::AgentShutdown`（在步骤 2 的 `active_session` 临界区内检查 `is_shutdown`）
- `Session::close()` → 幂等无害（若 slot 已清空，直接返回 Ok）
- `Agent::shutdown()` 重复调用 → 幂等（`is_shutdown` 已为 true，直接返回 Ok）

**close() 与 shutdown() 的并发竞争：** 两者都调用 `close_internal()`，该方法内部从 `active_session` slot 取出状态后清空 slot。若 `close()` 先完成，`shutdown()` 发现 slot 已空，跳过关闭步骤。若 `shutdown()` 先完成，`close()` 发现 slot 已空，返回 Ok（幂等）。不存在死锁或双重关闭。

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
    pub system_prompt_override: Option<String>,
    pub context: ContextManager,
    pub turn_counter: u64,
    /// 自上次 memory checkpoint 以来经过的轮次数（remainder 语义）。
    /// 每轮 Turn 结束后 +1，达到 memory_checkpoint_interval 时归零并执行 checkpoint。
    /// 恢复时通过 turn_counter % memory_checkpoint_interval 计算。
    pub turns_since_last_checkpoint: u64,
    /// 上次 checkpoint 截止的消息索引（ContextManager.messages() 中的下标）。
    /// checkpoint 提取时仅取 [last_checkpoint_message_index..] 的增量消息。
    /// 初始值为 0（从头开始），恢复时从持久化的 Metadata 中还原。
    pub last_checkpoint_message_index: usize,
    /// Session 启动时加载的 bootstrap 记忆（通用记忆，按 updated_at 降序）。
    /// 每轮 Turn 的 PromptBuilder 从此处获取 bootstrap 记忆并与 query 记忆合并后注入。
    pub bootstrap_memories: Vec<Memory>,
}

// 注意：Turn 运行状态（原 turn_in_progress）已迁移至 ActiveSessionInfo.phase（SessionPhase 状态机），
// 不再存放在 SessionState 中。这保证了 send_message() 和 close_internal() 在同一把锁
// （active_session）下原子操作 Turn 状态，消除了竞态窗口。
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

/// 一轮对话的运行句柄，持有事件流、取消能力和后台 task 句柄
pub struct RunningTurn {
    events: mpsc::Receiver<AgentEvent>,
    /// 本轮独立的取消令牌，cancel() 仅取消本轮 Turn，不影响 Session
    turn_cancel_token: CancellationToken,
    /// 后台 task 句柄，用于检测 panic（JoinError）
    task_handle: JoinHandle<()>,
}

impl RunningTurn {
    /// 获取事件流
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent>;

    /// 取消当前 Turn（仅取消本轮，Session 可继续 send_message）
    pub fn cancel(&self);

    /// 等待 Turn 完成（消费所有事件后调用，检测 panic）
    /// 若后台 task panic，返回 AgentError::InternalPanic
    pub async fn join(self) -> Result<()>;
}
```

`send_message` 流程：

1. **纯输入校验**（不持有任何锁）：校验 `UserInput`（非空、图片大小/格式）。校验失败直接返回 `InputValidation` 错误，不改变任何状态。
2. **原子 Turn 启动**（单次获取 `active_session` 锁）：在同一个临界区内完成以下全部操作，释放锁后再 spawn task：
   - 检查 `is_shutdown`，若为 `true` 返回 `AgentShutdown`
   - 检查 `ActiveSessionInfo.phase`，若为 `Running` 返回 `TurnBusy`
   - clone `session_close_token`，创建 `turn_cancel_token`（`session_close_token.child_token()`）
   - 创建 `oneshot::channel<()>`（`turn_finished_tx`, `turn_finished_rx`）
   - 将 `phase` 切换为 `SessionPhase::Running(RunningTurnHandle { turn_cancel_token, turn_finished_rx })`
   - 释放锁

   **设计保证：**
   - `close_internal()` 在任何时刻获取锁时，要么看到 `Idle`（无需等待），要么看到完整的 `Running(handle)`（可以 await）
   - 不存在"turn 已标记运行但 handle 尚未安装"的中间态
   - 输入校验在步骤 1 完成，不会因校验失败导致 Session 卡在 `Running` 状态
3. **spawn supervisor task**（`tokio::spawn`），supervisor 内部 **spawn 内层 run_turn task** 并通过 `JoinHandle.await` 检测结果。两层 task 的结构保证 `run_turn()` panic 时 supervisor 不受影响：
   ```
   supervisor task {
       let inner_handle = tokio::spawn(run_turn(...));
       let result = inner_handle.await;
       // 无论 run_turn 正常返回、返回 Error、还是 panic，supervisor 都能执行清理
       match result {
           Ok(Ok(())) => { /* 正常完成 */ }
           Ok(Err(e)) => { emit Error event }
           Err(join_err) if join_err.is_panic() => {
               // panic 恢复：run_turn 在子 task 中 panic，supervisor 不受影响
               emit Error(AgentError::InternalPanic)
               error!("run_turn panicked: {:?}", join_err);
           }
           Err(join_err) => { /* task 被取消 */ }
       }
       // 统一清理：将 phase 归位为 Idle
       {
           let mut slot = inner.active_session.lock().unwrap();
           if let Some(info) = slot.as_mut() {
               info.phase = SessionPhase::Idle;
           }
       }
       // turn_finished_tx 在 supervisor 退出时自动 drop，通知 close_internal()
   }
   ```
4. 返回 `RunningTurn`（包含 `events`、`turn_cancel_token` 和 supervisor 的 `JoinHandle`）

**两层 task 的 panic 安全保证：** supervisor `tokio::spawn` 一个内层 `run_turn` task，通过 `JoinHandle.await` 检测 `JoinError::is_panic()`。由于 `run_turn()` 在独立的子 task 中执行，其 panic 被 tokio 运行时捕获，不会传播到 supervisor。supervisor 始终能执行 `phase` 归位为 `Idle`、错误事件发射和日志记录。

**Turn 完成信号：** supervisor task 持有 `turn_finished_tx`（oneshot sender）。supervisor 退出时（无论正常、错误还是 panic），`turn_finished_tx` 自动 drop，`close_internal()` 中 await 的 `turn_finished_rx` 立即返回。这保证了 `close()` / `shutdown()` 可以可靠等待 Turn 真正结束。

**原子 Turn 启动与关闭的竞态消除：** `send_message()` 在单次 `active_session` 临界区内完成 `Idle → Running(handle)` 切换（步骤 2），`close_internal()` 在单次 `active_session` 临界区内取出 `Running(handle)` 并切换为 `Idle`（步骤 1）。两个操作互斥且各自原子，不存在中间态。

RunningTurn 不持有 Session 的任何借用，调用方可以在消费事件流的同时调用 `cancel()`。同一 Session 同一时间只允许一个 Turn 运行，并发调用 `send_message()` 将返回 `AgentError::TurnBusy`。调用方可通过 `RunningTurn::join()` 等待后台 task 完成并检测 panic。

### 8.1 会话恢复流程（resume_session）

`Agent::resume_session(session_id)` 从 SessionStorage 加载历史事件并重建 Session：

采用"先 load → rebuild state → claim once"流程，只有一次占槽操作：

1. 校验 `SessionStorage` 已注册，否则返回 `AgentError::StorageError("SessionStorage not registered")`
2. 调用 `SessionStorage::load_session(session_id)` 获取 `Vec<SessionEvent>`，若返回 `None` 返回 `AgentError::SessionNotFound`（此时未占槽，无需清理）
3. 从 Metadata 事件中提取 `session_config`（model_id + system_prompt_override），用于重建 SessionState
4. 按顺序将每个 `SessionEvent` 映射为 `Message`：

| SessionEventPayload                              | 映射为 Message                                                                                                        |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------- |
| UserMessage { content }                          | Message::User { content }                                                                                             |
| AssistantMessage { content, status: Complete }   | Message::Assistant { content, status: Complete }                                                                      |
| AssistantMessage { content, status: Incomplete } | Message::Assistant { content, status: Incomplete }（**不追加任何派生消息**，取消提示已在运行时作为 SystemMessage 持久化，replay 时由下一行 SystemMessage 规则还原） |
| ToolCall { call_id, tool_name, arguments }       | Message::ToolCall { call_id, tool_name, arguments }                                                                   |
| ToolResult { call_id, content, is_error }        | Message::ToolResult { call_id, content, is_error }                                                                    |
| SystemMessage { content }                        | Message::System { content }                                                                                           |
| Metadata { .. }                                  | 跳过，不加入上下文                                                                                                    |

5. 将映射后的 Message 列表加载到 ContextManager
6. **恢复计数器和游标**：
   - `turn_counter`：统计 `SessionEventPayload::UserMessage` 事件的数量（每个 UserMessage 代表一轮 Turn）
   - `turns_since_last_checkpoint`：`turn_counter % memory_checkpoint_interval`（remainder 语义，表示自上次 checkpoint 以来经过的轮次数）
   - `last_checkpoint_message_index`：从 Metadata 事件中取 key=`last_checkpoint_message_index` 的最后一个值，若无则默认 0
7. 从 MemoryStorage 加载最新 bootstrap 记忆（`load_bootstrap_memories()`），保存到 `SessionState.bootstrap_memories`（resume 时可能已有新的记忆写入，需要获取最新视图）
8. 构建 `SessionState`
9. **`claim_session_slot(session_id, state)`**：获取 `SessionSlotGuard`，若已有活跃 Session 返回 `SessionBusy`（此时 state 已构建但未占槽，无泄漏风险）
10. 不触发 SessionStart Hook（resume 不是新建会话）
11. **`guard.disarm()`**：标记 Guard 已交接，槽位由 `close_internal()` 负责清理
12. 返回 `Session`

**关闭路径：**

`Session::close()` 和 `Agent::shutdown()` 均调用 `AgentInner::close_internal(session_id)`，执行统一的关闭流程：

1. 短暂获取锁，从 `ActiveSessionInfo` 中取出 `session_close_token`、`state`、`RunningTurnHandle`（若有），将 `phase` 归位为 `Idle`，立即释放锁
2. 取消当前 Turn（通过 `session_close_token.cancel()`，级联取消 `turn_cancel_token`）
3. **等待 Turn 真正结束**（若步骤 1 取到了 `RunningTurnHandle`）：`await turn_finished_rx`（supervisor task 正常退出或 panic 后都会 drop sender，此时 rx 返回）
4. 触发 SessionEnd Hook（Abort 降级，不阻断）
5. 执行记忆提取（从 `state` 中获取 `context.messages()` 和 `bootstrap_memories`，失败不阻塞）
6. 清除 `active_session` 槽为 `None`

**顺序保证：** 步骤 3 确保 Turn 已完全结束后才进入步骤 4-5。`RunningTurnHandle` 中的 `turn_finished_rx` 由 supervisor task 持有 sender 端，supervisor 退出（无论正常、错误还是 panic）时 sender 自动 drop，rx 立即返回。由于 `send_message()` 在单次临界区内原子安装 handle，`close_internal()` 不可能遇到"handle 尚未就绪"的中间态。

**调用方必须显式调用 `close()`**。Session 的 `Drop` 实现仅记录 warn 日志（若未 close），不做后台异步关闭。理由：Agent 是库，调用方应对 Session 生命周期负责。后台异步 Drop 关闭引入了大量复杂度（后台 task、channel、中间态状态机），且难以保证正确性

---

## 9. TurnLoop

TurnLoop 是一个无状态的 async 函数，不是 struct。它是整个 Agent 运行时的唯一编排者。

```rust
pub(crate) async fn run_turn(
    state: Arc<Mutex<SessionState>>,
    inner: Arc<AgentInner>,
    input: UserInput,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken, // Turn 级取消令牌
) -> Result<()>
```

### 9.1 执行流程

```
run_turn(state, inner, input, event_tx)
│
├─ 1. Hook: TurnStart
│     ├─ Abort → emit Error(PluginAborted)，终止本轮 Turn，不进入模型调用
│     └─ 正常 → 从 DispatchOutcome.payload.data（HookData::TurnStart）中提取 dynamic_sections
│           Plugin 通过 ContinueWith(HookPatch::TurnStart { append_dynamic_sections }) 追加新段
│           多个 Plugin 链式传递，后一个 handler 看到前一个追加后的 dynamic_sections
│           HookPatch 类型保证 Plugin 无法修改 user_input（签名级约束）
├─ 2. SkillManager: 从 UserInput 的 Text 块中检测 /skill_name → 注入 Skill prompt
├─ 3. MemoryManager: 加载 Turn 级记忆并与 bootstrap 记忆合并
│     ├─ 提取 user_input 中的文本内容作为 query
│     ├─ query 归一化：
│     │   ├─ 有文本 → search(namespace, query, max_items) 获取相关记忆
│     │   └─ 无文本（纯图片/文件输入）→ 跳过 search，仅使用 bootstrap 记忆
│     ├─ 合并策略：bootstrap_memories ∪ query_memories，按 id 去重，bootstrap 优先
│     └─ 最终记忆集合传给 PromptBuilder（受 max_items 限制）
├─ 4. ContextManager: 追加用户消息 + SessionStorage: 即时持久化 UserMessage
│
├─ 5. LOOP {
│   ├─ 5a. PromptBuilder: 组装完整 System Prompt（每轮完整重建）
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
│   ├─ 5e. ContextManager: 追加 assistant message（纯文本部分，无论是否有 ToolCall）
│   │     + SessionStorage: 即时持久化 AssistantMessage
│   ├─ 5f. 收集 ToolCall 列表
│   │     └─ 无 ToolCall → BREAK
│   ├─ 5f'. ContextManager: 为每个 ToolCall 追加 Message::ToolCall
│   │     + SessionStorage: 即时持久化每个 ToolCall 事件
│   │     **注意：** 这保证实时路径和恢复路径的上下文完全一致——
│   │     恢复时 ToolCall 事件映射回 Message::ToolCall，
│   │     实时运行时也显式写入 Message::ToolCall。
│   ├─ 5g. ToolDispatcher: 执行 ToolCall 批次（结果按原始顺序归并）
│   │     ├─ 全只读 → 并发执行
│   │     └─ 含 mutating → 串行执行
│   │     每个 ToolCall:
│   │       ├─ Hook: BeforeToolUse（可 Abort）
│   │       ├─ emit ToolCallStart
│   │       ├─ ToolHandler.execute()（带超时）
│   │       ├─ emit ToolCallEnd + SessionStorage: 即时持久化 ToolResult
│   │       └─ Hook: AfterToolUse（Abort 则中止后续流程，进入最终模型调用）
│   ├─ 5h. ContextManager: 追加 tool results（Message::ToolResult）
│   ├─ 5i. tool_call_count += batch.len()
│   │     └─ 超限 → 进入"超限收尾"流程（见下文）
│   └─ } // 回到 5a
│
├─ 6. MemoryManager: checkpoint 检查（每 N 轮）
│     turns_since_last_checkpoint += 1
│     若达到 memory_checkpoint_interval：
│       提取 messages[last_checkpoint_message_index..] 的增量消息
│       调用 extract_memories(增量消息)
│       更新 last_checkpoint_message_index = messages().len()
│       持久化 Metadata(last_checkpoint_message_index)
│       归零 turns_since_last_checkpoint
├─ 7. Hook: TurnEnd（Abort 降级处理，不阻断）
└─ 8. emit TurnComplete / TurnCancelled
```

**Tool 超限收尾流程（两段式）：** 当 `tool_call_count` 超过 `max_tool_calls_per_turn` 时，不直接 BREAK，而是执行以下两阶段流程，确保用户拿到解释性回答。这是正常控制流，不抛出错误：

1. **注入超限提示**：将超限提示作为 System 消息注入上下文：`"[TOOL_LIMIT_REACHED] You have reached the maximum number of tool calls ({limit}) for this turn. You MUST NOT call any more tools. Summarize your progress and provide a final response to the user."`
2. **最终模型调用**：构建 ChatRequest 时**不传入任何 tool definitions**（tools 参数为空数组），强制模型只生成纯文本回复
3. **聚合最终回复**：将模型返回的 assistant message 追加到上下文，然后正常退出 Tool Loop

若最终模型调用失败，记录错误日志，emit `Error(ProviderError)` 事件，Turn 仍正常结束。

**事件持久化策略（即时落盘）：** 会话事件采用结构化即时落盘，而非 Turn 结束后统一写入，以保证异常退出时的数据完整性：

- **UserMessage**：收到用户输入后立即持久化
- **ToolCall / ToolResult**：Tool 开始/结束后立即持久化
- **AssistantMessage**：在模型流式响应完成或取消时持久化一次最终态，携带 `status`（Complete / Incomplete）
- **SystemMessage**：**凡是进入 `ContextManager.messages()` 的 `Message::System` 都必须持久化**，保证恢复路径和实时路径的上下文完全一致。具体包括：
  - Skill 注入消息（步骤 2 `render_injection()`）→ 立即持久化为 `SystemMessage`
  - 压缩摘要消息（compact 完成后的 `prefix + summary`）→ compact 完成后立即持久化为 `SystemMessage`
  - Tool 超限提示（步骤 5i `[TOOL_LIMIT_REACHED]...`）→ 注入上下文后立即持久化为 `SystemMessage`
  - 取消中断提示（`[此消息因用户取消而中断]`）→ 追加后立即持久化为 `SystemMessage`（**唯一来源**：恢复时从此 SystemMessage 事件 replay，不从 Incomplete AssistantMessage 派生）
  - 未执行 Tool 的取消提示（`[Tool cancelled: ...]` ToolResult）→ 已通过 ToolResult 持久化，不走 SystemMessage
- **Metadata**：在写入时立即持久化（如 session_config）
- **AgentEvent delta**（TextDelta、ReasoningDelta 等）：仅通过事件流推送，不持久化

**唯一账本规则：** 不存在"不落盘的 Message::System"，也不存在"恢复时从其他事件派生的消息"。所有进入 ContextManager 的消息都有且只有一个来源：对应的 `SessionEvent`。恢复时严格按 `SessionEvent` 序列 replay，不做任何推导或合成。这保证了实时路径和恢复路径的上下文完全一致。

若 SessionStorage 未注册，所有持久化操作静默跳过。

**持久化失败处理：** 写入失败不终止 Turn，但必须 emit `Error(StorageError)` 到事件流，使调用方可感知持久化问题。同时记录 warn 日志。

**关键设计点：** assistant message 在步骤 5e 中无条件追加到上下文——无论模型是返回纯文本还是带 ToolCall。这保证了：

- 多轮对话中后续 Turn 可以看到上一轮回答
- SessionStorage 在 Turn 结束时能拿到完整 assistant message
- 取消导致的 incomplete message 也能被正确记录

**上下文消息规范（唯一账本）：** 实时运行路径和恢复重建路径使用完全相同的消息序列规范：

- **核心规则**：凡是进入 `ContextManager.messages()` 的消息，都有对应的 `SessionEvent` 持久化。恢复时从 `SessionEvent` 序列重建完全相同的 `Message` 列表。
- 当模型返回带 ToolCall 的响应时，上下文中的消息序列为：`Assistant(text) → ToolCall(call_1) → ToolCall(call_2) → ... → ToolResult(call_1) → ToolResult(call_2) → ...`。实时路径在步骤 5e 追加 Assistant、步骤 5f' 追加 ToolCall、步骤 5h 追加 ToolResult。
- **SystemMessage 同样遵守此规则**：Skill 注入、compact 摘要、tool 超限提示等 `Message::System` 在进入上下文时立即持久化为 `SessionEventPayload::SystemMessage`，恢复时 replay 回上下文。
- 两条路径对齐后，模型在任何情况下都看到一致的上下文。

### 9.2 取消处理

取消采用两层令牌模型：

- **`session_close_token`**：Session 级令牌，**唯一来源是 `ActiveSessionInfo`**（`claim_session_slot()` 时创建）。`SessionState` 不持有此 token。仅在 `Session::close()` 或 `Agent::shutdown()` 时通过 `close_internal()` 触发 cancel。取消后自动级联取消所有活跃的 `turn_cancel_token`
- **`turn_cancel_token`**：`send_message()` 步骤 2 在 `active_session` 临界区内，从 `session_close_token` 调用 `.child_token()` 原子创建并安装到 `RunningTurnHandle` 中。调用方通过 `RunningTurn::cancel()` 触发，仅取消当前 Turn，Session 可继续使用

TurnLoop 使用 `turn_cancel_token` 在以下 yield 点检查：

- 模型流式接收的每个 event 之间
- Tool 批次中每个 Tool 执行前（尚未开始的 Tool 跳过）
- 压缩请求发起前

已开始的 Tool 执行不中断（等待完成）。取消后 TurnLoop 发出 `TurnCancelled` 事件并退出。`turn_cancel_token` 在 Turn 结束后自动销毁，不影响后续 `send_message()` 创建的新 Turn。

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
    context_window: usize,
    compact_threshold: f64,
}
```

**System Prompt 每轮重建**：System Prompt 在每轮 Turn 开始时由 PromptBuilder 完整拼接，不做缓存。字符串拼接的开销相对 LLM 调用可忽略不计。

**拼接顺序与需求文档 0005 §5.3 一致：** System Instructions → System Prompt Override（若有，使用 `<system_prompt_override>` 标签）→ Personality → (Tool Definitions via API) → Skill List → Plugin List → Memories → Environment Context → Dynamic Sections。

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
    /// compact_model_id 从 AgentConfig.compact_model 获取（未配置则使用当前对话模型）。
    /// compact_prompt 从 AgentConfig.compact_prompt 获取（未配置则使用 DEFAULT_COMPACT_PROMPT）。
    /// 通过 ModelRouter 解析 compact_model_id 到对应的 Provider，支持跨 Provider 选择压缩模型。
    pub async fn compact(
        &mut self,
        model_router: &ModelRouter,
        compact_model_id: &str,
        compact_prompt: &str,
        prefix: &str,
    ) {
        match self.summarize_compact(model_router, compact_model_id, compact_prompt, prefix).await {
            Ok(()) => {}
            Err(_) => self.truncate_compact(prefix),
        }
    }

    /// 摘要压缩：调用模型生成早期消息的摘要，替换原消息
    async fn summarize_compact(
        &mut self,
        model_router: &ModelRouter,
        compact_model_id: &str,
        compact_prompt: &str,
        prefix: &str,
    ) -> Result<()> {
        // 1. 用 compact_prompt 构建压缩请求
        // 2. 通过 model_router.resolve(compact_model_id) 获取 Provider
        // 3. 调用 provider.chat(...) 生成摘要
        // 4. 保留最近的消息（约占窗口 20%）
        // 5. 用 System { content: prefix + summary } 替换早期消息
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

PromptBuilder 是无状态的，提供单一的 `build()` 方法，每轮 Turn 完整拼接 System Prompt。

```rust
pub(crate) struct PromptBuilder;

impl PromptBuilder {
    /// 构建完整 System Prompt（每轮 Turn 调用一次）
    /// memories 参数为 bootstrap 记忆与 Turn 级 query 记忆合并后的最终集合
    pub fn build(
        config: &AgentConfig,
        system_prompt_override: Option<&str>,
        skills: &[SkillDefinition],
        plugins: &[(String, String, String)], // (id, display_name, description)
        memories: &[Memory], // bootstrap ∪ query memories，已去重
        dynamic_sections: &[String], // 来自 TurnStart Hook 的 ContinueWith
    ) -> String {
        let mut parts = Vec::new();

        // 1. System Instructions
        if !config.system_instructions.is_empty() {
            parts.push(wrap_tag("system_instructions",
                &config.system_instructions.join("\n\n")));
        }

        // 2. System Prompt Override
        if let Some(override_prompt) = system_prompt_override {
            parts.push(wrap_tag("system_prompt_override", override_prompt));
        }

        // 3. Personality
        if let Some(p) = &config.personality {
            parts.push(Self::render_personality(p));
        }

        // 4. Tool Definitions → 通过 ChatRequest.tools 传递，不在这里

        // 5. Skill List
        if let Some(s) = Self::render_skills(skills) { parts.push(s); }

        // 6. Plugin List
        if let Some(p) = Self::render_plugins(plugins) { parts.push(p); }

        // 7. Memories
        if !memories.is_empty() { parts.push(Self::render_memories(memories)); }

        // 8. Environment Context
        if let Some(env) = &config.environment_context {
            parts.push(Self::render_environment(env));
        }

        // 9. Dynamic Sections（来自 Plugin 的 TurnStart Hook）
        for section in dynamic_sections { parts.push(section.clone()); }

        parts.join("\n\n")
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
// B.10 Memory 提取 prompt（单阶段：同时完成提取 + 整合判断）
pub(crate) const MEMORY_EXTRACTION_PROMPT: &str = "You are a Memory Writing Agent...";
// B.11 Memory 提取输入模板
pub(crate) const MEMORY_EXTRACTION_INPUT_TEMPLATE: &str = "Analyze this session and produce JSON...";
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
        // 未知 Tool 处理：模型 hallucinate 了未注册 tool 时，生成 synthetic error result。
        // 不终止 Turn，让模型看到错误后自行调整。
        // 在判断 mutating 前先处理未知 tool（未知 tool 按 non-mutating 处理）。
        let has_mutating = calls.iter().any(|c| {
            self.handlers.get(&c.tool_name)
                .map_or(false, |h| h.is_mutating())
        });

        if has_mutating {
            self.execute_serial(calls, hooks, cancel, event_tx).await
        } else {
            self.execute_concurrent(calls, hooks, cancel, event_tx).await
        }
    }
}
```

**单个 Tool 执行流程：**

```
┌─ Resolve handler: self.handlers.get(tool_name)
│   └─ 未找到? → 生成 synthetic ToolResult:
│               ToolResult { is_error: true, content: "Tool not found: {tool_name}" }
│               emit ToolCallStart + ToolCallEnd(success=false)
│               emit Error(AgentError::ToolNotFound(tool_name))
│               → 继续下一个 Tool（不终止 Turn，让模型看到错误后调整）
├─ Hook: BeforeToolUse
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
- **顺序保证**：无论并发还是串行执行，`execute_batch()` 返回的 `Vec<ToolResult>` 始终按输入 `calls` 的原始顺序排列。并发执行时使用 `JoinSet` + index 映射，完成后按 index 重排。事件（`ToolCallStart` / `ToolCallEnd`）也按原始顺序发送，统一一套排序逻辑，避免事件流和上下文顺序不一致的 bug

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
                HookResult::ContinueWith(patch) => {
                    if !event.supports_continue_with() {
                        warn!(plugin_id = %entry.plugin_id, event = ?event,
                            "ContinueWith returned for unsupported hook, ignoring");
                        continue;
                    }
                    // 应用 patch：按 event 类型匹配 patch 变体，验证一致性
                    match (&mut payload.data, patch) {
                        (HookData::TurnStart { dynamic_sections, .. },
                         HookPatch::TurnStart { append_dynamic_sections }) => {
                            dynamic_sections.extend(append_dynamic_sections);
                        }
                        (HookData::BeforeToolUse { arguments, .. },
                         HookPatch::BeforeToolUse { arguments: new_args }) => {
                            *arguments = new_args;
                        }
                        (data, patch) => {
                            // event/patch 变体不匹配：这是 Plugin 的编程错误
                            warn!(plugin_id = %entry.plugin_id, event = ?event,
                                "HookPatch variant mismatch, ignoring");
                        }
                    }
                }
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
    /// Session 启动时加载通用记忆（bootstrap），按最近更新时间返回。
    /// 结果保存到 SessionState.bootstrap_memories，每轮 Turn 复用。
    pub async fn load_bootstrap_memories(&self) -> Result<Vec<Memory>> {
        self.storage.list_recent(&self.namespace, self.max_items).await
    }

    /// 每轮 Turn 开始时加载记忆并与 bootstrap 记忆合并。
    /// query 为用户输入的文本内容，可能为空（纯图片/文件输入时）。
    pub async fn load_turn_memories(
        &self,
        query: &str,
        bootstrap_memories: &[Memory],
    ) -> Result<Vec<Memory>> {
        // query 归一化：有文本时检索，无文本时仅返回 bootstrap
        let query_memories = if query.is_empty() {
            Vec::new()
        } else {
            self.storage.search(&self.namespace, query, self.max_items).await?
        };

        // 合并去重：bootstrap ∪ query，按 id 去重
        let mut seen = HashSet::new();
        let mut merged = Vec::with_capacity(self.max_items);
        // bootstrap 优先（保证通用记忆始终在场）
        for m in bootstrap_memories {
            if seen.insert(&m.id) && merged.len() < self.max_items {
                merged.push(m.clone());
            }
        }
        // 补充 query 记忆
        for m in query_memories {
            if seen.insert(&m.id) && merged.len() < self.max_items {
                merged.push(m);
            }
        }
        Ok(merged)
    }

    /// 提取记忆（Session 结束或 checkpoint 时调用）
    /// 单阶段流程：一次 LLM 调用同时完成提取 + 与已有记忆的整合判断
    ///
    /// `messages` 参数为增量消息切片 `&context.messages()[last_checkpoint_message_index..]`，
    /// 由调用方根据 SessionState.last_checkpoint_message_index 截取。
    /// 提取完成后调用方负责更新 last_checkpoint_message_index 为当前 messages().len()。
    pub async fn extract_memories(
        &self,
        messages: &[Message], // 增量消息，非全量
        model_router: &ModelRouter,
        memory_model_id: &str,
    ) -> Result<()> {
        // 1. 将增量 messages 渲染为文本
        // 2. 从 storage 加载当前 namespace 下的已有记忆（storage.list_by_namespace()）
        // 3. 使用 MEMORY_EXTRACTION_PROMPT 构建请求，输入包含：
        //    - 增量会话内容
        //    - 已有记忆列表（供模型判断是更新还是新增）
        // 4. 通过 model_router.resolve(memory_model_id) 获取 Provider
        // 5. 调用 provider.chat() 获取提取结果
        // 6. 解析 JSON → 操作列表，每个操作为以下之一：
        //    - create: 新建记忆 → storage.upsert(new_memory)
        //    - update: 更新已有记忆 → storage.upsert(updated_memory)
        //    - delete: 删除过时记忆 → storage.delete(id)
        //    - skip: 重复记忆，不操作
        Ok(())
    }
}
```

**记忆加载时机与 query 来源：**
- **Session 开始时**：调用 `load_bootstrap_memories()` 加载 namespace 下的通用记忆（使用 `MemoryStorage::list_recent()`，按 `updated_at` 降序返回最近记忆），结果保存到 `SessionState.bootstrap_memories`
- **每轮 Turn 开始时**：调用 `load_turn_memories(query, bootstrap_memories)` 合并 bootstrap 记忆与当前输入相关的记忆
  - 提取 `UserInput` 中所有 `ContentBlock::Text` 的文本拼接为 query
  - **query 归一化**：若 query 为空（纯图片/文件输入），跳过 `search()`，仅使用 bootstrap 记忆
  - **合并策略**：bootstrap ∪ query memories，按 `id` 去重，bootstrap 优先级高于 query，总数受 `max_items` 限制
- `search()` 按相关度排序返回 top-k 结果；`list_recent()` 按时间倒序返回 top-k 结果

**resume 时的 bootstrap 记忆：** `resume_session()` 也会重新调用 `load_bootstrap_memories()` 获取最新记忆视图。此时 SessionStorage 中可能包含旧的对话历史（其中的记忆已过时），bootstrap 记忆以最新的 MemoryStorage 状态为准。

**单阶段提取（v1 策略，已与需求文档 0005 §5.7 对齐）：**

v1 采用单阶段提取：将已有记忆列表作为上下文传给提取模型，一次 LLM 调用同时完成提取和整合判断。0005 的 B.10 + B.11 合并为本文档的 `MEMORY_EXTRACTION_PROMPT` 和 `MEMORY_EXTRACTION_INPUT_TEMPLATE`；B.12（Phase 2 整合）在 v1 中不实现。

- **选择理由：** codex-rs 的两阶段流程面向大量存量记忆的场景（需要复杂的去重和渐进展开逻辑）。YouYou v1 的记忆规模有限，单阶段足以满足需求，且减少了一次 LLM 调用和一套 prompt 模板
- **向后兼容路径：** 若 v2 需要升级为两阶段，只需拆分 `extract_memories()` 为两步调用，不影响 `MemoryStorage` trait 接口

**提取结果 JSON Schema：**

```json
{
  "type": "object",
  "properties": {
    "operations": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "action": { "enum": ["create", "update", "delete", "skip"] },
          "id": { "type": "string", "description": "update/delete/skip 时必填，create 时由框架生成" },
          "content": { "type": "string", "description": "create/update 时必填" },
          "tags": { "type": "array", "items": { "type": "string" }, "description": "create/update 时可选" },
          "reason": { "type": "string", "description": "操作理由，用于调试和审计" }
        },
        "required": ["action"]
      }
    }
  },
  "required": ["operations"]
}
```

**幂等规则：** 模型对同一段对话内容多次提取时，应生成相同的操作列表。`MemoryStorage::upsert()` 按 id 匹配：内容相同则仅更新 `updated_at`，内容不同则更新内容和时间戳。重复 `create` 同一内容会被模型判定为 `skip`（因已有记忆列表中已存在）。

**Checkpoint 计数器语义（`turns_since_last_checkpoint`）：** 使用 remainder 语义——表示"自上次 checkpoint 以来经过的轮次数"。每轮 Turn 结束后 +1，达到 `memory_checkpoint_interval` 时归零并执行 checkpoint 提取。恢复时通过 `turn_counter % memory_checkpoint_interval` 计算初始值，保证恢复后 checkpoint 间隔不漂移。

**Checkpoint 增量边界游标（`last_checkpoint_message_index`）：** 标记上次 checkpoint 截止到 `ContextManager.messages()` 中的哪个下标。checkpoint 提取时取 `messages[last_checkpoint_message_index..]` 作为增量输入，提取完成后更新游标为当前 `messages().len()`。恢复时从持久化的 Metadata 事件（key=`last_checkpoint_message_index`）中还原。

**Checkpoint 触发频率与增量范围分离：** `turns_since_last_checkpoint` 只负责"何时触发"，`last_checkpoint_message_index` 只负责"提取哪些消息"。两者独立运作，即便中间发生 compact（摘要替换早期消息），游标仍指向 compact 后的有效位置——compact 将早期消息替换为 System 摘要消息（索引 0），不影响后续新增消息的索引偏移量（新消息始终 append，索引只增不减）。

**Checkpoint 的幂等性：** 每次 checkpoint 提取的是 `[last_checkpoint_message_index..]` 范围内的增量消息。若 checkpoint 执行中途失败（游标未更新），下次 checkpoint 会重新覆盖相同范围，由模型的整合判断保证幂等。

**Checkpoint 游标持久化：** checkpoint 成功后立即通过 `SessionStorage::save_event()` 持久化 `Metadata { key: "last_checkpoint_message_index", value: new_index }` 事件。恢复时从 Metadata 事件中取最后一个 `last_checkpoint_message_index` 值。若无此 Metadata，默认为 0（首次 checkpoint 覆盖全量）。

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
    #[error("[INVALID_DEFAULT_MODEL] default model '{0}' is not registered")]
    InvalidDefaultModel(String),
    #[error("[INVALID_COMPACT_MODEL] compact model '{0}' is not registered")]
    InvalidCompactModel(String),
    #[error("[INVALID_MEMORY_MODEL] memory model '{0}' is not registered")]
    InvalidMemoryModel(String),

    // 运行阶段
    #[error("[INPUT_VALIDATION] {message}")]
    InputValidation { message: String },
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
    #[error("[SESSION_NOT_FOUND] session '{0}'")]
    SessionNotFound(String),
    #[error("[STORAGE_ERROR] {0}")]
    StorageError(anyhow::Error),
    #[error("[PLUGIN_ABORTED] hook '{hook}' aborted: {reason}")]
    PluginAborted { hook: &'static str, reason: String },
    #[error("[REQUEST_CANCELLED]")]
    RequestCancelled,
    #[error("[INTERNAL_PANIC] background task panicked: {message}")]
    InternalPanic { message: String },
    #[error("[AGENT_SHUTDOWN] agent has been shut down")]
    AgentShutdown,
}

impl AgentError {
    pub fn code(&self) -> &'static str { /* match self => "NO_MODEL_PROVIDER" etc */ }
    pub fn retryable(&self) -> bool { matches!(self, Self::ProviderError { retryable: true, .. }) }
    pub fn source_component(&self) -> &'static str {
        match self {
            Self::ProviderError { .. } => "provider",
            Self::ToolExecutionError { .. } | Self::ToolTimeout { .. }
                | Self::ToolNotFound(_) => "tool",
            Self::PluginInitFailed { .. } | Self::PluginAborted { .. } => "plugin",
            Self::StorageError(_) => "storage",
            Self::InvalidCompactModel(_) | Self::InvalidMemoryModel(_) | Self::InvalidDefaultModel(_) => "config",
            _ => "agent",
        }
    }
}
```

**错误码触发位置与对外暴露方式：**

| 错误码 | 触发位置 | 对外暴露方式 |
|--------|----------|-------------|
| `NO_MODEL_PROVIDER` | `build()` 校验 | `build()` 返回 `Err` |
| `NAME_CONFLICT` | `build()` 校验（Tool/Skill/Plugin 名称重复） | `build()` 返回 `Err` |
| `SKILL_DEPENDENCY_NOT_MET` | `build()` 校验（Skill 依赖的 Tool 未注册） | `build()` 返回 `Err` |
| `PLUGIN_INIT_FAILED` | `build()` 中调用 `Plugin::initialize()` 失败 | `build()` 返回 `Err` |
| `STORAGE_DUPLICATE` | `build()` 校验（Storage 注册超过一次） | `build()` 返回 `Err` |
| `INVALID_DEFAULT_MODEL` | `build()` 校验 | `build()` 返回 `Err` |
| `INVALID_COMPACT_MODEL` | `build()` 校验 | `build()` 返回 `Err` |
| `INVALID_MEMORY_MODEL` | `build()` 校验 | `build()` 返回 `Err` |
| `INPUT_VALIDATION` | `send_message()` 步骤 1（输入校验） | `send_message()` 返回 `Err` |
| `SESSION_BUSY` | `claim_session_slot()`（已有活跃 Session） | `new_session()` / `resume_session()` 返回 `Err` |
| `TURN_BUSY` | `send_message()` 步骤 2（`phase` 为 `Running`） | `send_message()` 返回 `Err` |
| `MODEL_NOT_SUPPORTED` | `ModelRouter::resolve()` 找不到 model_id | `send_message()` 返回 `Err`（通过 `run_turn` 传播） |
| `PROVIDER_ERROR` | `ModelProvider::chat()` 返回错误 | `AgentEvent::Error` |
| `TOOL_EXECUTION_ERROR` | `ToolHandler::execute()` 返回错误 | `AgentEvent::Error` + synthetic `ToolResult(is_error=true)` |
| `TOOL_TIMEOUT` | `ToolHandler::execute()` 超时 | `AgentEvent::Error` + synthetic `ToolResult(is_error=true)` |
| `TOOL_NOT_FOUND` | `ToolDispatcher` resolve handler 失败 | `AgentEvent::Error` + synthetic `ToolResult(is_error=true)` |
| `SESSION_NOT_FOUND` | `resume_session()` 加载不到会话 / `close_internal()` slot 为空 | `resume_session()` / `close_internal()` 返回 `Err` |
| `STORAGE_ERROR` | `SessionStorage` / `MemoryStorage` 任意方法返回错误 | `AgentEvent::Error`（持久化失败不终止 Turn） |
| `PLUGIN_ABORTED` | `HookRegistry::dispatch()` 中 Plugin 返回 `Abort` | `AgentEvent::Error`（TurnStart Abort）/ `new_session()` 返回 `Err`（SessionStart Abort） |
| `REQUEST_CANCELLED` | `RunningTurn::join()` 检测到 Turn 因取消终止 | `RunningTurn::join()` 返回 `Err` |
| `INTERNAL_PANIC` | supervisor task 检测到 `run_turn` panic | `AgentEvent::Error` + `RunningTurn::join()` 返回 `Err` |
| `AGENT_SHUTDOWN` | `is_shutdown` 为 `true` 时的任何操作 | `new_session()` / `resume_session()` / `send_message()` 返回 `Err` |

**已移除的错误码：**
- `SKILL_NOT_FOUND`：`SkillManager::detect_invocations()` 对未匹配的 `/command` 静默跳过（不视为错误，用户可能输入了非 Skill 的斜杠文本）。无对外暴露需求。
- `MAX_TOOL_CALLS_EXCEEDED`：Tool 超限走"两段式收尾流程"（注入系统提示 + 最终模型调用），是正常控制流而非错误。超限事实通过 `[TOOL_LIMIT_REACHED]` 系统消息体现在上下文中，不需要错误码。
- `COMPACT_ERROR`：compact 路径内建 summarize → truncate 降级，truncate 为纯内存操作不会失败。若 summarize 模型调用失败，错误由 `ProviderError` 表达。独立的 `CompactError` 无触发点。

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
HookEvent, HookPayload, HookData, HookResult, HookPatch
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

需求是单会话模型，不存在多个并发 Actor 的场景。Agent 的互斥只需要一个 `Mutex<Option<ActiveSessionInfo>>` 守卫。Actor 模型引入的 channel/mailbox 机制对这个场景过重。

### D3: 为什么 AgentBuilder 用 typestate？

编译期保证至少注册一个 ModelProvider，比运行时检查更安全。只用了一个 typestate 参数（NoProvider/HasProvider），没有过度泛型化。

### D4: 为什么 Hook handler 是 Fn 而非独立 trait？

Plugin trait 已经是注册入口。Hook handler 只是 Plugin 内部注册的回调。用 `Fn` 闭包比再定义一个 `HookHandler` trait 更轻量。`apply(self: Arc<Self>, ...)` 接收 `Arc<Self>`，handler 闭包通过 `let me = Arc::clone(&self);` 捕获 Plugin 的 `Arc` 引用，满足 `'static` 要求。

### D5: 为什么 ContextManager 用 Vec<Message> 而非自定义数据结构？

KISS。消息历史本质是有序列表，Vec 完全够用。不需要 ring buffer（压缩不是删除头部而是替换为摘要）、不需要 B-tree（不按 key 查找）。

### D6: 为什么 Session 的 close() 是必须显式调用的？

Agent 是库，调用方应对 Session 生命周期负责。Drop 时的后台异步关闭引入了 Closing 中间态状态机、后台 task、channel 通知等大量复杂度，且在 Drop 中无法可靠等待异步操作完成。显式 `close()` 更简单、更可预测。

### D7: 为什么记忆提取是单阶段而非 codex 的两阶段？

v1 采用单阶段提取（已与 0005 §5.7 对齐）。codex-rs 使用 Phase 1（提取）+ Phase 2（整合）因为它有大量存量记忆需要复杂的分类和渐进展开。YouYou v1 的记忆规模有限，单阶段提取足够满足需求，且减少了一次 LLM 调用和一套 prompt 模板。若 v2 需要升级为两阶段，只需拆分 `extract_memories()` 为两步调用，不影响 `MemoryStorage` trait 接口。

### D8: 为什么 close/shutdown 使用 close_internal() 统一关闭流程？

`Session::close()` 和 `Agent::shutdown()` 都需要执行相同的关闭序列（取消 Turn → 等待 Turn 结束 → SessionEnd Hook → 记忆提取 → 释放槽位）。将全部关闭状态（`SessionState`、`SessionPhase`、`session_close_token`）存入 `ActiveSessionInfo`，使得 `close_internal()` 无需 `Session` handle 即可驱动完整关闭流程。

### D9: 为什么 send_message() 使用两层 task spawn？

单层 spawn 中如果 supervisor 直接 `await run_turn()`，`run_turn()` panic 会击穿 supervisor，导致 `SessionPhase` 无法归位。两层 task（supervisor spawn 内层 run_turn task）确保 `run_turn()` 的 panic 被 tokio 捕获为 `JoinError`，supervisor 可以安全执行清理（将 `phase` 归位为 `Idle`）。

### D10: 为什么 ContinueWith 使用 HookPatch 而非复用 HookData？

`ContinueWith(HookData)` 允许 Plugin 返回任意 `HookData` 变体，"只能追加 dynamic sections / 只能改 arguments"仅是文档约定。使用事件特化的 `HookPatch` 类型约束每种 Hook 允许修改的字段范围——Plugin 无法构造出越权修改 `user_input` 或 `tool_name` 的值。但 `HookPatch` 仍是总枚举，event/patch 变体的匹配在 runtime 完成（不匹配时 warn + 降级为 Continue），未做到完全的编译期保证。

### D11: 为什么用 SessionPhase 状态机替代 turn_in_progress bool？

原设计中 `send_message()` 先设置 `turn_in_progress = true`（在 `SessionState` 锁中），然后再安装 `turn_finished_rx`（在 `active_session` 锁中）。这两步使用不同的锁，`close_internal()` 可能在两步之间介入，读到 `turn_finished_rx = None` 而跳过等待。新设计将 Turn 运行状态从 `SessionState` 迁移至 `ActiveSessionInfo`，并引入 `SessionPhase` 枚举：`send_message()` 在单次 `active_session` 临界区内原子完成"检查 Idle → 创建 handle → 切换为 Running"，消除了竞态窗口。输入校验在获取任何锁之前完成，避免校验失败导致 Session 卡在 Running 状态。

### D12: 为什么 checkpoint 需要 last_checkpoint_message_index 游标？

原设计只有 `turns_since_last_checkpoint` 做触发频率控制，但 `extract_memories()` 接收全量 `session_messages`，无法区分增量。`last_checkpoint_message_index` 记录上次 checkpoint 截止的消息位置，使得每次 checkpoint 只提取新增消息，避免重复提取。该游标持久化为 Metadata 事件，恢复时可准确还原。触发频率（轮次计数）和提取范围（消息索引）分离，职责清晰。
