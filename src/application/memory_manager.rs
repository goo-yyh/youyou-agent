//! 记忆加载、搜索与提取管理器。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::anyhow;
use chrono::Utc;
use futures::StreamExt;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::domain::{
    AgentError, ContentBlock, LedgerEvent, LedgerEventPayload, Memory, Result, SessionLedger,
    UserInput,
};
use crate::ports::{ChatEvent, ChatRequest, MemoryStorage, ModelProvider};
use crate::prompt::templates::{MEMORY_STAGE_ONE_INPUT, MEMORY_STAGE_ONE_SYSTEM};

/// 管理 bootstrap、turn search 与 extraction 的记忆管理器。
#[derive(Clone)]
pub(crate) struct MemoryManager {
    /// 记忆存储后端。
    storage: Arc<dyn MemoryStorage>,
    /// 单次注入或查询允许返回的最大条数。
    max_items: usize,
}

impl std::fmt::Debug for MemoryManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryManager")
            .field("max_items", &self.max_items)
            .finish_non_exhaustive()
    }
}

/// 提取模型输出的结构化结果。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExtractionResult {
    /// 模型输出的记忆操作列表。
    #[serde(default)]
    pub(crate) memory_operations: Vec<MemoryOperation>,
    /// 保留给后续 phase 的 rollout 摘要。
    #[serde(default)]
    pub(crate) rollout_summary: String,
    /// 保留给后续 phase 的 rollout slug。
    #[serde(default)]
    pub(crate) rollout_slug: String,
}

/// 模型输出的单条记忆操作。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub(crate) enum MemoryOperation {
    /// 新建记忆。
    Create {
        /// 记忆内容。
        content: String,
        /// 可选标签列表。
        #[serde(default)]
        tags: Vec<String>,
    },
    /// 更新已有记忆。
    Update {
        /// 目标记忆 ID。
        #[serde(rename = "targetId")]
        target_id: String,
        /// 更新后的记忆内容。
        content: String,
        /// 可选标签列表。
        #[serde(default)]
        tags: Vec<String>,
    },
    /// 删除已有记忆。
    Delete {
        /// 目标记忆 ID。
        #[serde(rename = "targetId")]
        target_id: String,
    },
}

/// 一次增量记忆提取所需的上下文快照。
pub(crate) struct IncrementalExtraction<'a> {
    /// 当前会话绑定的 memory namespace。
    pub(crate) namespace: &'a str,
    /// 当前会话 ledger 的只读快照。
    pub(crate) ledger: &'a SessionLedger,
    /// 上次 checkpoint 已处理到的 ledger seq。
    pub(crate) last_checkpoint_seq: u64,
    /// 本次提取写入到 memory source 的来源标识。
    pub(crate) source: &'a str,
    /// 负责执行提取请求的模型 provider。
    pub(crate) provider: &'a dyn ModelProvider,
    /// 提取请求使用的模型标识。
    pub(crate) model_id: &'a str,
    /// 当前提取请求的取消令牌。
    pub(crate) cancel: &'a CancellationToken,
}

impl MemoryManager {
    /// 创建一个新的记忆管理器。
    #[must_use]
    pub(crate) fn new(storage: Arc<dyn MemoryStorage>, max_items: usize) -> Self {
        Self { storage, max_items }
    }

    /// 加载会话启动时需要注入的 bootstrap 记忆。
    ///
    /// # Errors
    ///
    /// 当记忆存储无法加载最近记忆时返回错误。
    pub(crate) async fn list_recent(&self, namespace: &str) -> Result<Vec<Memory>> {
        self.storage
            .list_recent(namespace, self.max_items)
            .await
            .map_err(|error| {
                AgentError::StorageError(error.context(format!(
                    "failed to load bootstrap memories for namespace '{namespace}'"
                )))
            })
    }

    /// 根据当前用户输入加载本轮需要暴露给模型的记忆。
    ///
    /// # Errors
    ///
    /// 当记忆搜索失败时返回错误。
    pub(crate) async fn load_turn_memories(
        &self,
        namespace: &str,
        bootstrap_memories: &[Memory],
        input: &UserInput,
    ) -> Result<Vec<Memory>> {
        let Some(query) = explicit_text_query(input) else {
            return Ok(limit_memories(bootstrap_memories, self.max_items));
        };

        let searched_memories = self
            .storage
            .search(namespace, &query, self.max_items)
            .await
            .map_err(|error| {
                AgentError::StorageError(error.context(format!(
                    "failed to search memories for namespace '{namespace}'"
                )))
            })?;

        Ok(merge_memories(
            bootstrap_memories,
            &searched_memories,
            self.max_items,
        ))
    }

    /// 基于 ledger seq 边界执行一次增量提取。
    ///
    /// 返回值表示新的 checkpoint 边界；`None` 表示没有需要推进的内容。
    ///
    /// # Errors
    ///
    /// 当模型调用失败或记忆存储写入失败时返回错误。
    pub(crate) async fn extract_incremental(
        &self,
        request: IncrementalExtraction<'_>,
    ) -> Result<Option<u64>> {
        let IncrementalExtraction {
            namespace,
            ledger,
            last_checkpoint_seq,
            source,
            provider,
            model_id,
            cancel,
        } = request;
        let incremental_events = incremental_events_after(ledger, last_checkpoint_seq);
        let Some(last_seq) = incremental_events.last().map(|event| event.seq) else {
            return Ok(None);
        };

        let rendered_events = render_incremental_events(&incremental_events);
        if rendered_events.trim().is_empty() {
            return Ok(Some(last_seq));
        }

        let existing_memories =
            self.storage
                .list_by_namespace(namespace)
                .await
                .map_err(|error| {
                    AgentError::StorageError(error.context(format!(
                        "failed to list memories for namespace '{namespace}'"
                    )))
                })?;
        let request = build_extraction_request(
            namespace,
            source,
            &existing_memories,
            &rendered_events,
            model_id,
        )?;
        let output = collect_model_output(provider, request, cancel).await?;
        let Some(extraction) = parse_extraction_result(&output) else {
            return Err(AgentError::ProviderError {
                message: "memory extraction output is not valid JSON".to_string(),
                source: anyhow!(
                    "memory extraction model returned invalid JSON for namespace '{namespace}' from source '{source}'"
                ),
                retryable: false,
            });
        };
        let _ = (&extraction.rollout_summary, &extraction.rollout_slug);

        self.apply_memory_operations(
            namespace,
            source,
            &existing_memories,
            &extraction.memory_operations,
        )
        .await?;

        Ok(Some(last_seq))
    }

    /// 根据模型操作执行写入、更新或删除。
    async fn apply_memory_operations(
        &self,
        namespace: &str,
        source: &str,
        existing_memories: &[Memory],
        operations: &[MemoryOperation],
    ) -> Result<()> {
        let mut memory_index = existing_memories
            .iter()
            .cloned()
            .map(|memory| (memory.id.clone(), memory))
            .collect::<HashMap<_, _>>();

        for operation in operations {
            match operation {
                MemoryOperation::Create { content, tags } => {
                    let memory = build_new_memory(namespace, source, content, tags);
                    memory_index.insert(memory.id.clone(), memory.clone());
                    self.upsert_memory(memory).await?;
                }
                MemoryOperation::Update {
                    target_id,
                    content,
                    tags,
                } => {
                    if let Some(existing) = memory_index.get(target_id).cloned() {
                        let memory = build_updated_memory(existing, source, content, tags);
                        memory_index.insert(memory.id.clone(), memory.clone());
                        self.upsert_memory(memory).await?;
                    } else {
                        warn!(
                            namespace = %namespace,
                            target_id = %target_id,
                            "memory update target is missing; degrading to create",
                        );
                        let memory = build_new_memory(namespace, source, content, tags);
                        memory_index.insert(memory.id.clone(), memory.clone());
                        self.upsert_memory(memory).await?;
                    }
                }
                MemoryOperation::Delete { target_id } => {
                    if memory_index.remove(target_id).is_some() {
                        self.storage.delete(target_id).await.map_err(|error| {
                            AgentError::StorageError(
                                error.context(format!("failed to delete memory '{target_id}'")),
                            )
                        })?;
                    } else {
                        warn!(
                            namespace = %namespace,
                            target_id = %target_id,
                            "memory delete target is missing; skipping",
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// 执行一次 upsert，并统一错误上下文。
    async fn upsert_memory(&self, memory: Memory) -> Result<()> {
        let memory_id = memory.id.clone();
        self.storage.upsert(memory).await.map_err(|error| {
            AgentError::StorageError(
                error.context(format!("failed to upsert memory '{memory_id}'")),
            )
        })
    }
}

/// 从用户输入中提取显式文本 query。
#[must_use]
fn explicit_text_query(input: &UserInput) -> Option<String> {
    let parts = input
        .content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text(text) = block {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// 按原顺序截断到允许的最大记忆数。
#[must_use]
fn limit_memories(memories: &[Memory], max_items: usize) -> Vec<Memory> {
    memories.iter().take(max_items).cloned().collect()
}

/// 合并 bootstrap 和记忆搜索结果，并按 id 去重。
#[must_use]
fn merge_memories(
    bootstrap_memories: &[Memory],
    searched_memories: &[Memory],
    max_items: usize,
) -> Vec<Memory> {
    let mut merged = Vec::new();
    let mut seen_ids = HashSet::new();

    for memory in bootstrap_memories.iter().chain(searched_memories.iter()) {
        if seen_ids.insert(memory.id.clone()) {
            merged.push(memory.clone());
        }
        if merged.len() >= max_items {
            break;
        }
    }

    merged
}

/// 获取 checkpoint 边界之后的增量事件。
#[must_use]
fn incremental_events_after(
    ledger: &crate::domain::SessionLedger,
    last_checkpoint_seq: u64,
) -> Vec<LedgerEvent> {
    ledger
        .events()
        .iter()
        .filter(|event| event.seq > last_checkpoint_seq)
        .cloned()
        .collect()
}

/// 将增量事件渲染为 extraction prompt 可消费的文本。
#[must_use]
fn render_incremental_events(events: &[LedgerEvent]) -> String {
    events
        .iter()
        .filter_map(render_ledger_event)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 将单条 ledger 事件渲染为 extraction 文本。
#[must_use]
fn render_ledger_event(event: &LedgerEvent) -> Option<String> {
    match &event.payload {
        LedgerEventPayload::UserMessage { content } => Some(format!(
            "[seq:{}] user: {}",
            event.seq,
            render_content_blocks(content),
        )),
        LedgerEventPayload::AssistantMessage { content, status } => Some(format!(
            "[seq:{}] assistant({}): {}",
            event.seq,
            render_message_status(*status),
            render_content_blocks(content),
        )),
        LedgerEventPayload::ToolCall {
            call_id,
            tool_name,
            arguments,
        } => Some(format!(
            "[seq:{}] tool_call {}#{}: {}",
            event.seq, tool_name, call_id, arguments,
        )),
        LedgerEventPayload::ToolResult { call_id, output } => Some(format!(
            "[seq:{}] tool_result {} (is_error={}): {}",
            event.seq, call_id, output.is_error, output.content,
        )),
        LedgerEventPayload::SystemMessage { content } => {
            Some(format!("[seq:{}] system: {}", event.seq, content))
        }
        LedgerEventPayload::Metadata { .. } => None,
    }
}

/// 渲染内容块列表。
#[must_use]
fn render_content_blocks(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(render_content_block)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 渲染单个内容块。
#[must_use]
fn render_content_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.clone(),
        ContentBlock::Image { media_type, .. } => format!("<image media_type=\"{media_type}\" />"),
        ContentBlock::File {
            name,
            media_type,
            text,
        } => format!("<file name=\"{name}\" media_type=\"{media_type}\">\n{text}\n</file>"),
    }
}

/// 渲染 assistant 完成状态。
#[must_use]
fn render_message_status(status: crate::domain::MessageStatus) -> &'static str {
    match status {
        crate::domain::MessageStatus::Complete => "complete",
        crate::domain::MessageStatus::Incomplete => "incomplete",
    }
}

/// 构造 extraction 请求。
fn build_extraction_request(
    namespace: &str,
    source: &str,
    existing_memories: &[Memory],
    rendered_events: &str,
    model_id: &str,
) -> Result<ChatRequest> {
    let existing_memories_json =
        serde_json::to_string_pretty(existing_memories).map_err(|error| {
            AgentError::StorageError(
                anyhow!(error).context("failed to serialize existing memories"),
            )
        })?;
    let rollout_contents = format!(
        "namespace: {namespace}\nsource: {source}\n\nexisting memories json:\n{existing_memories_json}\n\nincremental ledger events:\n{rendered_events}",
    );
    let user_text = MEMORY_STAGE_ONE_INPUT
        .replace("{rollout_path}", source)
        .replace("{rollout_cwd}", namespace)
        .replace("{rollout_contents}", &rollout_contents);

    Ok(ChatRequest {
        model_id: model_id.to_string(),
        messages: vec![
            crate::domain::Message::System {
                content: MEMORY_STAGE_ONE_SYSTEM.to_string(),
            },
            crate::domain::Message::User {
                content: vec![ContentBlock::Text(user_text)],
            },
        ],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
    })
}

/// 收集 extraction 模型的纯文本输出。
async fn collect_model_output(
    provider: &dyn ModelProvider,
    request: ChatRequest,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut stream = provider
        .chat(request, cancel.clone())
        .await
        .map_err(|error| AgentError::ProviderError {
            message: "failed to start memory extraction request".to_string(),
            source: error.context("memory extraction provider failed to start request"),
            retryable: false,
        })?;
    let mut output = String::new();

    while let Some(item) = stream.next().await {
        let event = match item {
            Ok(event) => event,
            Err(_error) if cancel.is_cancelled() => return Err(AgentError::RequestCancelled),
            Err(error) => {
                return Err(AgentError::ProviderError {
                    message: "memory extraction stream failed".to_string(),
                    source: error.context("memory extraction stream returned an error"),
                    retryable: false,
                });
            }
        };

        match event {
            ChatEvent::TextDelta(text) => output.push_str(text.as_str()),
            ChatEvent::ReasoningDelta(_) => {}
            ChatEvent::Done { .. } => return Ok(output),
            ChatEvent::ToolCall { .. } => {
                return Err(AgentError::ProviderError {
                    message: "memory extraction emitted an unexpected tool call".to_string(),
                    source: anyhow!("memory extraction request must not use tools"),
                    retryable: false,
                });
            }
            ChatEvent::Error(_error) if cancel.is_cancelled() => {
                return Err(AgentError::RequestCancelled);
            }
            ChatEvent::Error(error) => {
                let retryable = error.retryable;
                return Err(AgentError::ProviderError {
                    message: error.message.clone(),
                    source: anyhow!(error),
                    retryable,
                });
            }
        }
    }

    if cancel.is_cancelled() {
        return Err(AgentError::RequestCancelled);
    }

    Err(AgentError::ProviderError {
        message: "memory extraction stream ended before completion".to_string(),
        source: anyhow!("memory extraction stream ended without a done event"),
        retryable: false,
    })
}

/// 解析模型返回的 extraction JSON。
#[must_use]
fn parse_extraction_result(output: &str) -> Option<ExtractionResult> {
    serde_json::from_str::<ExtractionResult>(output.trim()).ok()
}

/// 构造一条新的记忆记录。
#[must_use]
fn build_new_memory(namespace: &str, source: &str, content: &str, tags: &[String]) -> Memory {
    let now = Utc::now();
    Memory {
        id: Uuid::new_v4().to_string(),
        namespace: namespace.to_string(),
        content: content.to_string(),
        source: source.to_string(),
        tags: tags.to_vec(),
        created_at: now,
        updated_at: now,
    }
}

/// 基于现有记忆构造更新后的记录。
#[must_use]
fn build_updated_memory(existing: Memory, source: &str, content: &str, tags: &[String]) -> Memory {
    Memory {
        id: existing.id,
        namespace: existing.namespace,
        content: content.to_string(),
        source: source.to_string(),
        tags: tags.to_vec(),
        created_at: existing.created_at,
        updated_at: Utc::now(),
    }
}
