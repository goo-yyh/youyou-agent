//! 会话上下文投影管理器。

use anyhow::anyhow;

use crate::domain::{
    CompactionMarker, LedgerEventPayload, Message, MetadataKey, Result, SessionLedger,
};

/// 基于账本重建出的模型可见上下文投影。
#[allow(
    dead_code,
    reason = "Phase 2 先固定上下文投影形状，后续 phase 会消费压缩阈值和可见消息访问器。"
)]
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

#[allow(
    dead_code,
    reason = "Phase 2 先补齐恢复协议，Phase 3-8 会逐步使用这些上下文访问方法。"
)]
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
    /// # 错误
    ///
    /// 当账本中的压缩 metadata 无法反序列化时返回错误。
    pub(crate) fn rebuild_from_ledger(
        ledger: &SessionLedger,
        context_window: usize,
        compact_threshold: f64,
    ) -> Result<Self> {
        let latest_compaction = latest_compaction_marker(ledger)?;
        let mut visible_messages = Vec::new();
        let mut min_visible_seq = 0;

        if let Some(marker) = &latest_compaction {
            visible_messages.push(Message::System {
                content: marker.rendered_summary.clone(),
            });
            min_visible_seq = marker.replaces_through_seq;
        }

        for event in ledger.events() {
            if event.seq <= min_visible_seq {
                continue;
            }

            if let Some(message) = payload_to_message(&event.payload) {
                visible_messages.push(message);
            }
        }

        let estimated_tokens = estimate_messages_tokens(&visible_messages);

        Ok(Self {
            latest_compaction,
            visible_messages,
            estimated_tokens,
            context_window,
            compact_threshold,
        })
    }

    /// 追加一条已确认落盘的消息到投影中。
    pub(crate) fn push(&mut self, message: Message) {
        self.visible_messages.push(message);
        self.recalculate_tokens();
    }

    /// 应用一条已经持久化成功的压缩标记。
    pub(crate) fn apply_compaction_marker(
        &mut self,
        ledger: &SessionLedger,
        marker: CompactionMarker,
    ) {
        self.latest_compaction = Some(marker.clone());
        self.visible_messages.clear();
        self.visible_messages.push(Message::System {
            content: marker.rendered_summary,
        });

        for event in ledger.events() {
            if event.seq <= marker.replaces_through_seq {
                continue;
            }

            if let Some(message) = payload_to_message(&event.payload) {
                self.visible_messages.push(message);
            }
        }

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

    /// 返回最新的压缩标记。
    #[must_use]
    pub(crate) fn latest_compaction(&self) -> Option<&CompactionMarker> {
        self.latest_compaction.as_ref()
    }

    /// 判断在附加 prompt 和 tools 开销后是否需要压缩。
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "compact_threshold 是 (0,1) 区间内的比例配置，比较时需要与 usize 上下文窗口做换算。"
    )]
    pub(crate) fn needs_compaction(&self, prompt_chars: usize, tools_chars: usize) -> bool {
        let total_tokens = self
            .estimated_tokens
            .saturating_add(prompt_chars / 4)
            .saturating_add(tools_chars / 4);
        total_tokens > (self.context_window as f64 * self.compact_threshold) as usize
    }

    /// 重新计算粗略 token 数。
    fn recalculate_tokens(&mut self) {
        self.estimated_tokens = estimate_messages_tokens(&self.visible_messages);
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
                crate::domain::AgentError::StorageError(
                    anyhow!(error).context("failed to deserialize context compaction metadata"),
                )
            })
        })
        .transpose()
}

/// 对消息进行粗略 token 估算。
fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages.iter().map(message_chars).sum::<usize>() / 4
}

/// 估算单条消息的字符数。
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
