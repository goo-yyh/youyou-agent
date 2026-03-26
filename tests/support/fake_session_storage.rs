//! 集成测试使用的伪造会话存储。
#![allow(
    dead_code,
    reason = "测试支撑会被多个集成测试按需复用，单个测试目标不一定覆盖全部辅助方法。"
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result as AnyResult, anyhow};
use async_trait::async_trait;
use youyou_agent::{
    LedgerEvent, LedgerEventPayload, MetadataKey, SessionPage, SessionSearchQuery, SessionStorage,
    SessionSummary,
};

/// 会话存储可注入失败的事件类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailingPayload {
    /// 任意 `UserMessage` 事件。
    UserMessage,
    /// 任意 `AssistantMessage` 事件。
    AssistantMessage,
    /// 任意 `ToolCall` 事件。
    ToolCall,
    /// 任意 `ToolResult` 事件。
    ToolResult,
    /// 任意 `SystemMessage` 事件。
    SystemMessage,
    /// 指定 key 的 `Metadata` 事件。
    Metadata(MetadataKey),
}

/// 伪造会话存储的内部状态。
#[derive(Debug, Default)]
struct FakeSessionStorageState {
    sessions: BTreeMap<String, Vec<LedgerEvent>>,
    summaries: BTreeMap<String, SessionSummary>,
    failing_payloads: Vec<FailingPayload>,
}

/// 基于内存的伪造会话存储。
#[derive(Debug, Clone, Default)]
pub struct FakeSessionStorage {
    state: Arc<Mutex<FakeSessionStorageState>>,
}

impl FakeSessionStorage {
    /// 配置在指定 metadata key 上模拟持久化失败。
    pub fn fail_on_metadata_key(&self, key: MetadataKey) {
        self.fail_on_payload(FailingPayload::Metadata(key));
    }

    /// 配置在指定事件类型上模拟持久化失败。
    pub fn fail_on_payload(&self, payload: FailingPayload) {
        if let Ok(mut state) = self.state.lock() {
            state.failing_payloads.push(payload);
        }
    }

    /// 清空之前设置的失败注入规则。
    pub fn clear_failures(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.failing_payloads.clear();
        }
    }

    /// 返回指定会话当前保存的事件副本。
    #[must_use]
    pub fn saved_events(&self, session_id: &str) -> Vec<LedgerEvent> {
        self.state.lock().map_or_else(
            |_| Vec::new(),
            |state| state.sessions.get(session_id).cloned().unwrap_or_default(),
        )
    }

    /// 返回当前全部 summary 副本。
    #[must_use]
    pub fn summaries(&self) -> Vec<SessionSummary> {
        self.state.lock().map_or_else(
            |_| Vec::new(),
            |state| state.summaries.values().cloned().collect(),
        )
    }

    /// 将一条事件同步到 summary 视图。
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
}

#[async_trait]
impl SessionStorage for FakeSessionStorage {
    async fn save_event(&self, session_id: &str, event: LedgerEvent) -> AnyResult<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake session storage poisoned: {error}"))?;

        if let Some(payload) = state
            .failing_payloads
            .iter()
            .find(|payload| payload_matches(payload, &event.payload))
        {
            return Err(anyhow!("simulated persistence failure for {payload:?}"));
        }

        state
            .sessions
            .entry(session_id.to_string())
            .or_default()
            .push(event.clone());
        Self::update_summary(&mut state.summaries, session_id, &event);

        Ok(())
    }

    async fn load_session(&self, session_id: &str) -> AnyResult<Option<Vec<LedgerEvent>>> {
        Ok(self
            .state
            .lock()
            .map_err(|error| anyhow!("fake session storage poisoned: {error}"))?
            .sessions
            .get(session_id)
            .cloned())
    }

    async fn list_sessions(&self, cursor: Option<&str>, limit: usize) -> AnyResult<SessionPage> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake session storage poisoned: {error}"))?;
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

        let limit = limit.max(1);
        let next_cursor = sessions
            .get(limit)
            .map(|summary| summary.session_id.clone());

        Ok(SessionPage {
            sessions: sessions.into_iter().take(limit).collect(),
            next_cursor,
        })
    }

    async fn find_sessions(&self, query: &SessionSearchQuery) -> AnyResult<Vec<SessionSummary>> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake session storage poisoned: {error}"))?;
        let mut sessions: Vec<_> = state.summaries.values().cloned().collect();
        sessions.retain(|summary| match query {
            SessionSearchQuery::IdPrefix(prefix) => summary.session_id.starts_with(prefix),
            SessionSearchQuery::TitleContains(keyword) => summary
                .title
                .as_ref()
                .is_some_and(|title| title.contains(keyword)),
        });
        sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(sessions)
    }

    async fn delete_session(&self, session_id: &str) -> AnyResult<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake session storage poisoned: {error}"))?;
        state.sessions.remove(session_id);
        state.summaries.remove(session_id);

        Ok(())
    }
}

/// 从用户内容块中提取首个文本预览。
fn first_text_preview(content: &[youyou_agent::ContentBlock]) -> Option<String> {
    content.iter().find_map(|block| {
        if let youyou_agent::ContentBlock::Text(text) = block {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.chars().take(32).collect())
            }
        } else {
            None
        }
    })
}

/// 判断失败注入规则是否命中当前事件负载。
fn payload_matches(rule: &FailingPayload, payload: &LedgerEventPayload) -> bool {
    match (rule, payload) {
        (FailingPayload::UserMessage, LedgerEventPayload::UserMessage { .. })
        | (FailingPayload::AssistantMessage, LedgerEventPayload::AssistantMessage { .. })
        | (FailingPayload::ToolCall, LedgerEventPayload::ToolCall { .. })
        | (FailingPayload::ToolResult, LedgerEventPayload::ToolResult { .. })
        | (FailingPayload::SystemMessage, LedgerEventPayload::SystemMessage { .. }) => true,
        (FailingPayload::Metadata(expected_key), LedgerEventPayload::Metadata { key, .. }) => {
            expected_key == key
        }
        _ => false,
    }
}
