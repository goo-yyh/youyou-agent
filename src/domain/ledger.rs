//! 作为持久化会话事实源的账本类型。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{ContentBlock, MessageStatus, ToolOutput};

/// 持久化到会话账本中的标准 metadata key。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MetadataKey {
    /// 序列化后的会话配置。
    SessionConfig,
    /// 绑定到会话的记忆命名空间。
    MemoryNamespace,
    /// 最新的上下文压缩标记。
    ContextCompaction,
    /// 最新的记忆 checkpoint。
    MemoryCheckpoint,
}

/// 一条持久化账本事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEvent {
    /// 单调递增的序号。
    pub seq: u64,
    /// 事件时间戳。
    pub timestamp: DateTime<Utc>,
    /// 事件负载。
    pub payload: LedgerEventPayload,
}

/// 存储在账本中的负载类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LedgerEventPayload {
    /// 持久化的用户消息。
    UserMessage {
        /// 用户内容块。
        content: Vec<ContentBlock>,
    },
    /// 持久化的 assistant 消息。
    AssistantMessage {
        /// Assistant 内容块。
        content: Vec<ContentBlock>,
        /// Assistant 消息的完成状态。
        status: MessageStatus,
    },
    /// 持久化的 Tool 调用。
    ToolCall {
        /// Provider 生成的 Tool 调用 id。
        call_id: String,
        /// Tool 名称。
        tool_name: String,
        /// JSON 参数。
        arguments: serde_json::Value,
    },
    /// 持久化的 Tool 结果。
    ToolResult {
        /// Provider 生成的 Tool 调用 id。
        call_id: String,
        /// Tool 输出。
        output: ToolOutput,
    },
    /// 持久化的 synthetic 或 system 消息。
    SystemMessage {
        /// System 文本。
        content: String,
    },
    /// 持久化的 metadata 值。
    Metadata {
        /// Metadata key。
        key: MetadataKey,
        /// Metadata 值。
        value: serde_json::Value,
    },
}

/// 以 metadata 形式持久化的压缩状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionMarker {
    /// 摘要覆盖到的最大 ledger 序号。
    pub replaces_through_seq: u64,
    /// 恢复时直接注入的完整渲染摘要文本。
    pub rendered_summary: String,
}

/// 会话账本的内存追加视图。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLedger {
    events: Vec<LedgerEvent>,
    next_seq: u64,
}

impl SessionLedger {
    /// 创建一个空账本。
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            next_seq: 1,
        }
    }

    /// 从持久化事件重建账本。
    #[must_use]
    pub fn from_events(events: Vec<LedgerEvent>) -> Self {
        let next_seq = events.last().map_or(1, |event| event.seq.saturating_add(1));
        Self { events, next_seq }
    }

    /// 向内存账本追加一条事件。
    pub fn append(&mut self, event: LedgerEvent) {
        self.next_seq = event.seq.saturating_add(1);
        self.events.push(event);
    }

    /// 返回下一条应分配的序号。
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// 返回完整且有序的事件切片。
    #[must_use]
    pub fn events(&self) -> &[LedgerEvent] {
        &self.events
    }

    /// 返回给定 key 的最新 metadata 值。
    #[must_use]
    pub fn latest_metadata(&self, key: &MetadataKey) -> Option<&serde_json::Value> {
        self.events
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                LedgerEventPayload::Metadata {
                    key: event_key,
                    value,
                } if event_key == key => Some(value),
                _ => None,
            })
    }
}
