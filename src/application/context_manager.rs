//! 会话上下文投影与压缩逻辑。

use anyhow::anyhow;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::application::prompt_builder::RenderedPrompt;
use crate::application::request_builder::{
    ChatRequestBuilder, RequestBuildOptions, RequestContext, ResolvedSessionConfig,
};
use crate::domain::{
    AgentError, CompactionMarker, LedgerEventPayload, Message, MessageStatus, MetadataKey, Result,
    SessionLedger,
};
use crate::ports::{ChatEventStream, ModelCapabilities, ModelProvider, ToolDefinition};
use crate::prompt::templates::COMPACT_SUMMARY_PREFIX;

/// 恢复投影时注入的取消提示。
const RESUME_CANCEL_NOTICE: &str = "[此消息因用户取消而中断]";

/// 截断降级模式使用的固定提示。
const TRUNCATION_SUMMARY_SUFFIX: &str = "[System note: Earlier context was truncated because summary compaction was unavailable. Continue from the most recent messages and treat missing earlier details as potentially incomplete.]";

/// 截断降级的目标比例分子。
const TRUNCATION_TARGET_NUMERATOR: usize = 3;

/// 截断降级的目标比例分母。
const TRUNCATION_TARGET_DENOMINATOR: usize = 10;

/// 基于账本重建出的模型可见上下文投影。
#[derive(Debug, Clone)]
pub(crate) struct ContextManager {
    /// 最近一次已生效的压缩标记。
    latest_compaction: Option<CompactionMarker>,
    /// 当前对模型可见的消息序列。
    visible_messages: Vec<Message>,
    /// 粗略 token 估算值。
    estimated_tokens: usize,
    /// 当前模型的上下文窗口。
    context_window: usize,
    /// 触发压缩的阈值比例。
    compact_threshold: f64,
}

impl ContextManager {
    /// 创建一个空的上下文投影。
    #[must_use]
    pub(crate) fn new(context_window: usize, compact_threshold: f64) -> Self {
        Self {
            latest_compaction: None,
            visible_messages: Vec::new(),
            estimated_tokens: 0,
            context_window,
            compact_threshold,
        }
    }

    /// 从完整账本重建当前可见上下文。
    ///
    /// # Errors
    ///
    /// 当账本中的压缩 metadata 无法反序列化时返回错误。
    pub(crate) fn rebuild_from_ledger(
        ledger: &SessionLedger,
        context_window: usize,
        compact_threshold: f64,
    ) -> Result<Self> {
        let latest_compaction = latest_compaction_marker(ledger)?;
        let visible_messages = rebuild_visible_messages(ledger, latest_compaction.as_ref());

        Ok(Self {
            latest_compaction,
            estimated_tokens: estimate_messages_tokens(&visible_messages),
            visible_messages,
            context_window,
            compact_threshold,
        })
    }

    /// 追加一条已确认落盘的消息到投影中。
    pub(crate) fn push(&mut self, message: Message) {
        self.visible_messages.push(message);
        self.recalculate_tokens();
    }

    /// 构造发给请求构建器的上下文快照。
    #[must_use]
    pub(crate) fn build_request_context(
        &self,
        model_capabilities: ModelCapabilities,
        tool_definitions: Vec<ToolDefinition>,
    ) -> RequestContext {
        RequestContext {
            messages: self.visible_messages.clone(),
            model_capabilities,
            tool_definitions,
        }
    }

    /// 应用一条已经持久化成功的压缩标记。
    pub(crate) fn apply_compaction_marker(
        &mut self,
        ledger: &SessionLedger,
        marker: CompactionMarker,
    ) {
        self.latest_compaction = Some(marker);
        self.visible_messages = rebuild_visible_messages(ledger, self.latest_compaction.as_ref());
        self.recalculate_tokens();
    }

    /// 返回当前可见消息。
    #[must_use]
    pub(crate) fn visible_messages(&self) -> &[Message] {
        &self.visible_messages
    }

    /// 返回当前粗略 token 估算值。
    #[must_use]
    pub(crate) fn estimated_tokens(&self) -> usize {
        self.estimated_tokens
    }

    /// 判断在附加 prompt 和 tools 开销后是否需要压缩。
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "compact_threshold 是配置比例，比较时需要转换为 usize 阈值。"
    )]
    pub(crate) fn needs_compaction(&self, prompt_chars: usize, tools_chars: usize) -> bool {
        let total_tokens = self
            .estimated_tokens
            .saturating_add(prompt_chars / 4)
            .saturating_add(tools_chars / 4);
        total_tokens > (self.context_window as f64 * self.compact_threshold) as usize
    }

    /// 生成新的压缩标记。
    ///
    /// 该方法只负责计算压缩结果，不触发 Hook、不持久化，也不发送事件。
    ///
    /// # Errors
    ///
    /// 当没有可压缩历史、截断后仍超出允许范围、或请求被取消时返回错误。
    pub(crate) async fn generate_compaction_marker(
        &self,
        ledger: &SessionLedger,
        provider: &dyn ModelProvider,
        compact_model_id: &str,
        compact_model_capabilities: ModelCapabilities,
        compact_prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<CompactionMarker> {
        let plan = CompactionPlan::build(ledger, self.latest_compaction.as_ref())?;

        match self
            .try_generate_summary_marker(
                &plan,
                provider,
                compact_model_id,
                compact_model_capabilities,
                compact_prompt,
                cancel,
            )
            .await
        {
            Ok(marker) => Ok(marker),
            Err(AgentError::RequestCancelled) => Err(AgentError::RequestCancelled),
            Err(_) => self.generate_truncation_marker(&plan),
        }
    }

    /// 重新计算粗略 token 数。
    fn recalculate_tokens(&mut self) {
        self.estimated_tokens = estimate_messages_tokens(&self.visible_messages);
    }

    /// 使用 compact model 生成摘要压缩标记。
    async fn try_generate_summary_marker(
        &self,
        plan: &CompactionPlan,
        provider: &dyn ModelProvider,
        compact_model_id: &str,
        compact_model_capabilities: ModelCapabilities,
        compact_prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<CompactionMarker> {
        let request = ChatRequestBuilder::new().build(
            &RenderedPrompt {
                text: compact_prompt.to_string(),
            },
            &RequestContext {
                messages: plan.compactable_messages.clone(),
                model_capabilities: compact_model_capabilities,
                tool_definitions: Vec::new(),
            },
            &ResolvedSessionConfig {
                model_id: compact_model_id.to_string(),
                system_prompt_override: None,
            },
            &RequestBuildOptions { allow_tools: false },
        )?;

        let stream = provider
            .chat(request, cancel.clone())
            .await
            .map_err(|error| AgentError::ProviderError {
                message: "failed to start compact chat request".to_string(),
                source: error.context("compact model failed to start chat request"),
                retryable: false,
            })?;
        let summary = collect_summary_text(stream, cancel).await?;
        if summary.trim().is_empty() {
            return Err(AgentError::CompactError {
                message: "compact model returned an empty summary".to_string(),
            });
        }

        let marker = CompactionMarker {
            replaces_through_seq: plan.replaces_through_seq,
            rendered_summary: render_summary_message(summary.trim()),
        };
        Self::ensure_projection_fits(
            &marker.rendered_summary,
            &plan.retained_messages,
            self.context_window,
            "summary compaction output still exceeds the context window",
        )?;

        Ok(marker)
    }

    /// 生成截断降级模式的压缩标记。
    fn generate_truncation_marker(&self, plan: &CompactionPlan) -> Result<CompactionMarker> {
        let marker = CompactionMarker {
            replaces_through_seq: plan.replaces_through_seq,
            rendered_summary: render_truncation_summary(),
        };
        Self::ensure_projection_fits(
            &marker.rendered_summary,
            &plan.retained_messages,
            truncation_target_tokens(self.context_window),
            "truncation fallback still cannot fit the preserved current turn",
        )?;

        Ok(marker)
    }

    /// 校验压缩后的可见消息是否落在允许预算内。
    fn ensure_projection_fits(
        rendered_summary: &str,
        retained_messages: &[Message],
        max_tokens: usize,
        message: &str,
    ) -> Result<()> {
        let projected = build_projected_messages(rendered_summary, retained_messages);
        let estimated_tokens = estimate_messages_tokens(&projected);
        if estimated_tokens > max_tokens {
            return Err(AgentError::CompactError {
                message: format!("{message}: estimated {estimated_tokens} tokens"),
            });
        }

        Ok(())
    }
}

/// 一条投影后的消息及其账本序号。
#[derive(Debug, Clone)]
struct ProjectedMessage {
    /// 对应的账本序号；合成消息没有序号。
    seq: Option<u64>,
    /// 模型可见的消息内容。
    message: Message,
}

/// 一次 compact 生成所需的工作集。
#[derive(Debug, Clone)]
struct CompactionPlan {
    /// 需要被摘要替代的旧消息。
    compactable_messages: Vec<Message>,
    /// compact 后仍需保留的当前 turn 区间。
    retained_messages: Vec<Message>,
    /// 新摘要覆盖到的最大账本序号。
    replaces_through_seq: u64,
}

impl CompactionPlan {
    /// 从当前账本与压缩标记中推导本次 compact 的边界。
    ///
    /// # Errors
    ///
    /// 当不存在当前 turn anchor，或当前 turn 之前没有任何可压缩消息时返回错误。
    fn build(ledger: &SessionLedger, latest_compaction: Option<&CompactionMarker>) -> Result<Self> {
        let projected_messages = project_visible_entries(ledger, latest_compaction);
        let anchor_seq = find_current_turn_anchor_seq(ledger, latest_compaction)?;
        let split_index = projected_messages
            .iter()
            .position(|entry| entry.seq == Some(anchor_seq))
            .ok_or_else(|| AgentError::CompactError {
                message: "current turn anchor is missing from visible history".to_string(),
            })?;

        let (compactable_entries, retained_entries) = projected_messages.split_at(split_index);
        if compactable_entries.is_empty() {
            return Err(AgentError::CompactError {
                message: "no earlier history can be compacted before the current turn".to_string(),
            });
        }

        Ok(Self {
            compactable_messages: compactable_entries
                .iter()
                .map(|entry| entry.message.clone())
                .collect(),
            retained_messages: retained_entries
                .iter()
                .map(|entry| entry.message.clone())
                .collect(),
            replaces_through_seq: anchor_seq.saturating_sub(1),
        })
    }
}

/// 将账本负载映射为可见消息。
#[must_use]
pub(crate) fn payload_to_message(payload: &LedgerEventPayload) -> Option<Message> {
    match payload {
        LedgerEventPayload::UserMessage { content } => Some(Message::User {
            content: content.clone(),
        }),
        LedgerEventPayload::AssistantMessage { content, status } => Some(Message::Assistant {
            content: content.clone(),
            status: *status,
        }),
        LedgerEventPayload::ToolCall {
            call_id,
            tool_name,
            arguments,
        } => Some(Message::ToolCall {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        }),
        LedgerEventPayload::ToolResult { call_id, output } => Some(Message::ToolResult {
            call_id: call_id.clone(),
            output: output.clone(),
        }),
        LedgerEventPayload::SystemMessage { content } => Some(Message::System {
            content: content.clone(),
        }),
        LedgerEventPayload::Metadata { .. } => None,
    }
}

/// 读取账本中的最新压缩标记。
fn latest_compaction_marker(ledger: &SessionLedger) -> Result<Option<CompactionMarker>> {
    ledger
        .latest_metadata(&MetadataKey::ContextCompaction)
        .map(|value| {
            serde_json::from_value::<CompactionMarker>(value.clone()).map_err(|error| {
                AgentError::StorageError(
                    anyhow!(error).context("failed to deserialize context compaction metadata"),
                )
            })
        })
        .transpose()
}

/// 用实时/恢复统一规则重建当前可见消息。
#[must_use]
fn rebuild_visible_messages(
    ledger: &SessionLedger,
    latest_compaction: Option<&CompactionMarker>,
) -> Vec<Message> {
    project_visible_entries(ledger, latest_compaction)
        .into_iter()
        .map(|entry| entry.message)
        .collect()
}

/// 构建带账本边界信息的可见消息列表。
#[must_use]
fn project_visible_entries(
    ledger: &SessionLedger,
    latest_compaction: Option<&CompactionMarker>,
) -> Vec<ProjectedMessage> {
    let mut projected = Vec::new();
    let min_visible_seq = latest_compaction.map_or(0, |marker| marker.replaces_through_seq);

    if let Some(marker) = latest_compaction {
        projected.push(ProjectedMessage {
            seq: None,
            message: Message::System {
                content: marker.rendered_summary.clone(),
            },
        });
    }

    for event in ledger.events() {
        if event.seq <= min_visible_seq {
            continue;
        }

        if let Some(message) = payload_to_message(&event.payload) {
            push_projected_message(&mut projected, Some(event.seq), message);
        }
    }

    projected
}

/// 将一条投影消息及其可能的恢复提示追加到工作列表。
fn push_projected_message(
    projected: &mut Vec<ProjectedMessage>,
    seq: Option<u64>,
    message: Message,
) {
    let should_append_cancel_notice = matches!(
        &message,
        Message::Assistant {
            status: MessageStatus::Incomplete,
            ..
        }
    );

    projected.push(ProjectedMessage { seq, message });

    if should_append_cancel_notice {
        projected.push(ProjectedMessage {
            seq: None,
            message: Message::System {
                content: RESUME_CANCEL_NOTICE.to_string(),
            },
        });
    }
}

/// 找到当前 turn 的起点用户消息序号。
fn find_current_turn_anchor_seq(
    ledger: &SessionLedger,
    latest_compaction: Option<&CompactionMarker>,
) -> Result<u64> {
    let min_visible_seq = latest_compaction.map_or(0, |marker| marker.replaces_through_seq);

    ledger
        .events()
        .iter()
        .rev()
        .find_map(|event| match event.payload {
            LedgerEventPayload::UserMessage { .. } if event.seq > min_visible_seq => {
                Some(event.seq)
            }
            LedgerEventPayload::UserMessage { .. }
            | LedgerEventPayload::AssistantMessage { .. }
            | LedgerEventPayload::ToolCall { .. }
            | LedgerEventPayload::ToolResult { .. }
            | LedgerEventPayload::SystemMessage { .. }
            | LedgerEventPayload::Metadata { .. } => None,
        })
        .ok_or_else(|| AgentError::CompactError {
            message: "no current turn user anchor is available for compaction".to_string(),
        })
}

/// 将 compact provider 的流式结果拼成摘要文本。
async fn collect_summary_text(
    mut stream: ChatEventStream,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut summary = String::new();

    while let Some(item) = stream.next().await {
        let event = match item {
            Ok(event) => event,
            Err(_error) if cancel.is_cancelled() => return Err(AgentError::RequestCancelled),
            Err(error) => {
                return Err(AgentError::ProviderError {
                    message: "compact model stream failed".to_string(),
                    source: error.context("compact model stream returned an error"),
                    retryable: false,
                });
            }
        };

        match event {
            crate::ports::ChatEvent::TextDelta(text) => summary.push_str(text.as_str()),
            crate::ports::ChatEvent::ReasoningDelta(_) => {}
            crate::ports::ChatEvent::Done { .. } => return Ok(summary),
            crate::ports::ChatEvent::ToolCall { .. } => {
                return Err(AgentError::CompactError {
                    message: "compact model emitted an unexpected tool call".to_string(),
                });
            }
            crate::ports::ChatEvent::Error(_error) if cancel.is_cancelled() => {
                return Err(AgentError::RequestCancelled);
            }
            crate::ports::ChatEvent::Error(error) => {
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
        message: "compact model stream ended before completion".to_string(),
        source: anyhow!("compact stream ended without a done event"),
        retryable: false,
    })
}

/// 将摘要正文包裹为最终的 compact marker system 文本。
#[must_use]
fn render_summary_message(summary: &str) -> String {
    format!("{COMPACT_SUMMARY_PREFIX}\n\n{summary}")
}

/// 渲染截断降级模式的摘要文本。
#[must_use]
fn render_truncation_summary() -> String {
    format!("{COMPACT_SUMMARY_PREFIX}\n\n{TRUNCATION_SUMMARY_SUFFIX}")
}

/// 生成压缩后模型可见的消息序列。
#[must_use]
fn build_projected_messages(rendered_summary: &str, retained_messages: &[Message]) -> Vec<Message> {
    let mut projected = Vec::with_capacity(retained_messages.len().saturating_add(1));
    projected.push(Message::System {
        content: rendered_summary.to_string(),
    });
    projected.extend(retained_messages.iter().cloned());
    projected
}

/// 计算截断降级的目标 token 预算。
#[must_use]
fn truncation_target_tokens(context_window: usize) -> usize {
    context_window.saturating_mul(TRUNCATION_TARGET_NUMERATOR) / TRUNCATION_TARGET_DENOMINATOR
}

/// 对消息进行粗略 token 估算。
#[must_use]
fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages.iter().map(message_chars).sum::<usize>() / 4
}

/// 估算单条消息的字符数。
#[must_use]
fn message_chars(message: &Message) -> usize {
    match message {
        Message::User { content } | Message::Assistant { content, .. } => {
            content.iter().map(content_block_chars).sum()
        }
        Message::ToolCall {
            call_id,
            tool_name,
            arguments,
        } => call_id.len() + tool_name.len() + arguments.to_string().len(),
        Message::ToolResult { call_id, output } => {
            call_id.len() + output.content.len() + output.metadata.to_string().len()
        }
        Message::System { content } => content.len(),
    }
}

/// 估算单个内容块的字符数。
#[must_use]
fn content_block_chars(block: &crate::domain::ContentBlock) -> usize {
    match block {
        crate::domain::ContentBlock::Text(text) => text.len(),
        crate::domain::ContentBlock::Image { data, media_type } => data.len() + media_type.len(),
        crate::domain::ContentBlock::File {
            name,
            media_type,
            text,
        } => name.len() + media_type.len() + text.len(),
    }
}
