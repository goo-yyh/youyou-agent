use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use futures::Stream;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use youyou_agent::{
    AgentBuilder, AgentConfig, AgentEventPayload, ChatEvent, ChatEventStream, ChatRequest,
    ContentBlock, LedgerEvent, LedgerEventPayload, Memory, MemoryStorage, ModelCapabilities,
    ModelInfo, ModelProvider, RunningTurn, SessionConfig, SessionPage, SessionSearchQuery,
    SessionStorage, SessionSummary, TokenUsage, ToolHandler, ToolInput, ToolOutput, TurnOutcome,
    UserInput,
};

/// 最小示例入口。
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let chat_provider = ScriptedProvider::new("chat-provider", vec![model_info("demo-model")]);
    let memory_provider =
        ScriptedProvider::new("memory-provider", vec![model_info("memory-model")]);
    chat_provider.enqueue_script(vec![
        ProviderStep::Emit(ChatEvent::TextDelta("正在生成响应……".to_string())),
        ProviderStep::WaitForCancel,
    ]);
    chat_provider.enqueue_script(vec![
        ProviderStep::Emit(ChatEvent::ToolCall {
            call_id: "call-1".to_string(),
            tool_name: "echo".to_string(),
            arguments: json!({"value": "world"}),
        }),
        ProviderStep::Emit(ChatEvent::Done {
            usage: TokenUsage::default(),
        }),
    ]);
    chat_provider.enqueue_script(vec![
        ProviderStep::Emit(ChatEvent::TextDelta(
            "工具执行完成，恢复后的会话继续结束。".to_string(),
        )),
        ProviderStep::Emit(ChatEvent::Done {
            usage: TokenUsage::default(),
        }),
    ]);
    memory_provider.enqueue_script(json_text_script(
        r#"{"memoryOperations":[],"rolloutSummary":"","rolloutSlug":""}"#,
    ));
    memory_provider.enqueue_script(json_text_script(
        r#"{"memoryOperations":[],"rolloutSummary":"","rolloutSlug":""}"#,
    ));

    let mut config = AgentConfig::new("demo-model", "example/demo");
    config.memory_model = Some("memory-model".to_string());

    let agent = AgentBuilder::new(config)
        .register_model_provider(chat_provider)
        .register_model_provider(memory_provider)
        .register_tool(EchoTool::new())
        .register_session_storage(InMemorySessionStorage::default())
        .register_memory_storage(InMemoryMemoryStorage::default())
        .build()
        .await?;

    let session = agent.new_session(SessionConfig::default()).await?;
    let session_id = session.session_id().to_string();
    println!("created session: {session_id}");

    let cancelled = cancel_after_first_text_delta(
        session
            .send_message(text_input("第一轮先开始，再由调用方取消"), None)
            .await?,
    )
    .await?;
    println!("first turn outcome: {cancelled:?}");

    session.close().await?;
    println!("session closed, preparing to resume");

    let resumed = agent.resume_session(&session_id).await?;
    let completed = finish_turn(
        resumed
            .send_message(text_input("第二轮请调用 echo 工具"), None)
            .await?,
    )
    .await?;
    println!("resumed turn outcome: {completed:?}");

    resumed.close().await?;
    println!("resumed session closed");
    Ok(())
}

/// 构造一个支持流式与 tool use 的模型元数据。
fn model_info(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        display_name: id.to_string(),
        context_window: 8_192,
        capabilities: ModelCapabilities {
            tool_use: true,
            vision: false,
            streaming: true,
        },
    }
}

/// 构造最简单的文本输入。
fn text_input(text: &str) -> UserInput {
    UserInput {
        content: vec![ContentBlock::Text(text.to_string())],
    }
}

/// 构造一轮仅输出 JSON 文本的脚本。
fn json_text_script(text: &str) -> Vec<ProviderStep> {
    vec![
        ProviderStep::Emit(ChatEvent::TextDelta(text.to_string())),
        ProviderStep::Emit(ChatEvent::Done {
            usage: TokenUsage::default(),
        }),
    ]
}

/// 消费一个 turn 到结束，并打印事件。
async fn finish_turn(mut turn: RunningTurn) -> Result<TurnOutcome> {
    while let Some(event) = turn.events.next().await {
        print_event(&event.payload);
    }

    turn.join().await.map_err(Into::into)
}

/// 在收到首个文本增量后取消 turn，并打印剩余事件。
async fn cancel_after_first_text_delta(mut turn: RunningTurn) -> Result<TurnOutcome> {
    while let Some(event) = turn.events.next().await {
        print_event(&event.payload);
        if matches!(event.payload, AgentEventPayload::TextDelta(_)) {
            turn.cancel();
            break;
        }
    }

    while let Some(event) = turn.events.next().await {
        print_event(&event.payload);
    }

    turn.join().await.map_err(Into::into)
}

/// 将事件负载打印到控制台，便于观察 turn 生命周期。
fn print_event(payload: &AgentEventPayload) {
    match payload {
        AgentEventPayload::TextDelta(text) => println!("text delta: {text}"),
        AgentEventPayload::ReasoningDelta(text) => println!("reasoning delta: {text}"),
        AgentEventPayload::ToolCallStart {
            call_id,
            tool_name,
            arguments,
        } => println!("tool start: {call_id} {tool_name} {arguments}"),
        AgentEventPayload::ToolCallEnd {
            call_id,
            tool_name,
            output,
            duration_ms,
            success,
        } => println!(
            "tool end: {call_id} {tool_name} success={success} duration_ms={duration_ms} output={}",
            output.content
        ),
        AgentEventPayload::ContextCompacted => println!("context compacted"),
        AgentEventPayload::TurnComplete => println!("turn complete"),
        AgentEventPayload::TurnCancelled => println!("turn cancelled"),
        AgentEventPayload::Error(error) => println!("error: {error}"),
    }
}

/// 示例 provider 的脚本步骤。
#[derive(Debug, Clone)]
enum ProviderStep {
    /// 发送一条固定的聊天事件。
    Emit(ChatEvent),
    /// 一直等待到外部取消请求到来。
    WaitForCancel,
}

/// 脚本化 provider 的内部状态。
#[derive(Debug, Default)]
struct ScriptedProviderState {
    scripts: VecDeque<Vec<ProviderStep>>,
}

/// 一个最小的脚本化 provider。
#[derive(Debug, Clone)]
struct ScriptedProvider {
    id: String,
    models: Vec<ModelInfo>,
    state: Arc<Mutex<ScriptedProviderState>>,
}

impl ScriptedProvider {
    /// 创建脚本化 provider。
    fn new(id: impl Into<String>, models: Vec<ModelInfo>) -> Self {
        Self {
            id: id.into(),
            models,
            state: Arc::new(Mutex::new(ScriptedProviderState::default())),
        }
    }

    /// 追加一轮脚本输出。
    fn enqueue_script(&self, script: Vec<ProviderStep>) {
        if let Ok(mut state) = self.state.lock() {
            state.scripts.push_back(script);
        }
    }

    /// 返回一轮默认的完成脚本。
    fn default_script() -> Vec<ProviderStep> {
        vec![ProviderStep::Emit(ChatEvent::Done {
            usage: TokenUsage::default(),
        })]
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    async fn chat(
        &self,
        _request: ChatRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<ChatEventStream> {
        let script = {
            let mut state = self
                .state
                .lock()
                .map_err(|error| anyhow!("scripted provider poisoned: {error}"))?;
            state
                .scripts
                .pop_front()
                .unwrap_or_else(Self::default_script)
        };
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            for step in script {
                if execute_provider_step(&tx, &cancel, step).await.is_err() {
                    return;
                }
            }
        });

        let stream: Pin<Box<dyn Stream<Item = anyhow::Result<ChatEvent>> + Send>> =
            Box::pin(ReceiverStream::new(rx));
        Ok(stream)
    }
}

/// 执行一条 provider 步骤。
async fn execute_provider_step(
    tx: &mpsc::Sender<anyhow::Result<ChatEvent>>,
    cancel: &tokio_util::sync::CancellationToken,
    step: ProviderStep,
) -> std::result::Result<(), ()> {
    match step {
        ProviderStep::Emit(event) => tx.send(Ok(event)).await.map_err(|_| ()),
        ProviderStep::WaitForCancel => {
            cancel.cancelled().await;
            Ok(())
        }
    }
}

/// 一个最小的 echo tool。
#[derive(Debug, Default)]
struct EchoTool;

impl EchoTool {
    /// 创建 echo tool。
    fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolHandler for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "返回调用参数中的 value 字段"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"],
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        input: ToolInput,
        _timeout_cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<ToolOutput> {
        let value = input
            .arguments
            .get("value")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("missing string field 'value'"))?;

        Ok(ToolOutput {
            content: format!("echo: {value}"),
            is_error: false,
            metadata: json!({ "toolName": input.tool_name }),
        })
    }
}

/// 内存里的会话存储状态。
#[derive(Debug, Default)]
struct InMemorySessionStorageState {
    sessions: BTreeMap<String, Vec<LedgerEvent>>,
    summaries: BTreeMap<String, SessionSummary>,
}

/// 一个最小的内存会话存储实现。
#[derive(Debug, Clone, Default)]
struct InMemorySessionStorage {
    state: Arc<Mutex<InMemorySessionStorageState>>,
}

#[async_trait]
impl SessionStorage for InMemorySessionStorage {
    async fn save_event(&self, session_id: &str, event: LedgerEvent) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("in-memory session storage poisoned: {error}"))?;
        state
            .sessions
            .entry(session_id.to_string())
            .or_default()
            .push(event.clone());
        update_summary(&mut state.summaries, session_id, &event);
        Ok(())
    }

    async fn load_session(&self, session_id: &str) -> anyhow::Result<Option<Vec<LedgerEvent>>> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("in-memory session storage poisoned: {error}"))?;
        Ok(state.sessions.get(session_id).cloned())
    }

    async fn list_sessions(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<SessionPage> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("in-memory session storage poisoned: {error}"))?;
        let mut sessions: Vec<_> = state.summaries.values().cloned().collect();
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });

        if let Some(cursor) = cursor
            && let Some(position) = sessions
                .iter()
                .position(|summary| summary.session_id == cursor)
        {
            sessions = sessions
                .into_iter()
                .skip(position.saturating_add(1))
                .collect();
        }

        let limited = limit.max(1);
        let next_cursor = sessions
            .get(limited)
            .map(|summary| summary.session_id.clone());

        Ok(SessionPage {
            sessions: sessions.into_iter().take(limited).collect(),
            next_cursor,
        })
    }

    async fn find_sessions(
        &self,
        query: &SessionSearchQuery,
    ) -> anyhow::Result<Vec<SessionSummary>> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("in-memory session storage poisoned: {error}"))?;
        let mut sessions: Vec<_> = state.summaries.values().cloned().collect();
        sessions.retain(|summary| match query {
            SessionSearchQuery::IdPrefix(prefix) => summary.session_id.starts_with(prefix),
            SessionSearchQuery::TitleContains(keyword) => summary
                .title
                .as_ref()
                .is_some_and(|title| title.contains(keyword)),
        });
        Ok(sessions)
    }

    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("in-memory session storage poisoned: {error}"))?;
        state.sessions.remove(session_id);
        state.summaries.remove(session_id);
        Ok(())
    }
}

/// 根据新事件更新 session summary。
fn update_summary(
    summaries: &mut BTreeMap<String, SessionSummary>,
    session_id: &str,
    event: &LedgerEvent,
) {
    let summary = summaries
        .entry(session_id.to_string())
        .or_insert_with(|| SessionSummary {
            session_id: session_id.to_string(),
            title: None,
            created_at: event.timestamp,
            updated_at: event.timestamp,
            message_count: 0,
        });
    summary.updated_at = event.timestamp;

    match &event.payload {
        LedgerEventPayload::UserMessage { content } => {
            summary.message_count = summary.message_count.saturating_add(1);
            if summary.title.is_none() {
                summary.title = first_text_preview(content);
            }
        }
        LedgerEventPayload::AssistantMessage { .. } => {
            summary.message_count = summary.message_count.saturating_add(1);
        }
        LedgerEventPayload::ToolCall { .. }
        | LedgerEventPayload::ToolResult { .. }
        | LedgerEventPayload::SystemMessage { .. }
        | LedgerEventPayload::Metadata { .. } => {}
    }
}

/// 从用户消息里提取一个简单标题。
fn first_text_preview(content: &[ContentBlock]) -> Option<String> {
    content.iter().find_map(|block| match block {
        ContentBlock::Text(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.chars().take(32).collect())
            }
        }
        ContentBlock::Image { .. } | ContentBlock::File { .. } => None,
    })
}

/// 内存里的记忆存储状态。
#[derive(Debug, Default)]
struct InMemoryMemoryStorageState {
    memories: Vec<Memory>,
}

/// 一个最小的内存记忆存储实现。
#[derive(Debug, Clone, Default)]
struct InMemoryMemoryStorage {
    state: Arc<Mutex<InMemoryMemoryStorageState>>,
}

#[async_trait]
impl MemoryStorage for InMemoryMemoryStorage {
    async fn search(
        &self,
        namespace: &str,
        _query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        query_memories(&self.state, namespace, limit)
    }

    async fn list_recent(&self, namespace: &str, limit: usize) -> anyhow::Result<Vec<Memory>> {
        query_memories(&self.state, namespace, limit)
    }

    async fn list_by_namespace(&self, namespace: &str) -> anyhow::Result<Vec<Memory>> {
        query_memories(&self.state, namespace, usize::MAX)
    }

    async fn upsert(&self, memory: Memory) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("in-memory memory storage poisoned: {error}"))?;
        if let Some(existing) = state
            .memories
            .iter_mut()
            .find(|existing| existing.id == memory.id)
        {
            *existing = memory;
        } else {
            state.memories.push(memory);
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("in-memory memory storage poisoned: {error}"))?;
        state.memories.retain(|memory| memory.id != id);
        Ok(())
    }
}

/// 查询指定 namespace 的记忆。
fn query_memories(
    state: &Arc<Mutex<InMemoryMemoryStorageState>>,
    namespace: &str,
    limit: usize,
) -> Result<Vec<Memory>> {
    let state = state
        .lock()
        .map_err(|error| anyhow!("in-memory memory storage poisoned: {error}"))?;
    let mut memories: Vec<_> = state
        .memories
        .iter()
        .filter(|memory| memory.namespace == namespace)
        .cloned()
        .collect();
    memories.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(memories.into_iter().take(limit).collect())
}

/// 生成一条空白记忆，方便外部观察内存存储结构。
#[allow(dead_code, reason = "示例保留该辅助函数，便于二次改造成真实 demo。")]
fn sample_memory(id: &str, namespace: &str, content: &str) -> Memory {
    let now = Utc::now();
    Memory {
        id: id.to_string(),
        namespace: namespace.to_string(),
        content: content.to_string(),
        source: "example".to_string(),
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
    }
}
