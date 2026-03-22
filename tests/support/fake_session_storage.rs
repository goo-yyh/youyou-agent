//! 集成测试使用的伪造会话存储。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use youyou_agent::{LedgerEvent, SessionPage, SessionSearchQuery, SessionStorage, SessionSummary};

/// 基于内存的伪造会话存储。
#[derive(Debug, Clone, Default)]
pub struct FakeSessionStorage {
    sessions: Arc<Mutex<BTreeMap<String, Vec<LedgerEvent>>>>,
}

#[async_trait]
impl SessionStorage for FakeSessionStorage {
    async fn save_event(&self, session_id: &str, event: LedgerEvent) -> AnyResult<()> {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions
                .entry(session_id.to_string())
                .or_default()
                .push(event);
        }
        Ok(())
    }

    async fn load_session(&self, session_id: &str) -> AnyResult<Option<Vec<LedgerEvent>>> {
        Ok(self
            .sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(session_id).cloned()))
    }

    async fn list_sessions(&self, _cursor: Option<&str>, _limit: usize) -> AnyResult<SessionPage> {
        Ok(SessionPage {
            sessions: Vec::<SessionSummary>::new(),
            next_cursor: None,
        })
    }

    async fn find_sessions(&self, _query: &SessionSearchQuery) -> AnyResult<Vec<SessionSummary>> {
        Ok(Vec::new())
    }

    async fn delete_session(&self, session_id: &str) -> AnyResult<()> {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(session_id);
        }
        Ok(())
    }
}
