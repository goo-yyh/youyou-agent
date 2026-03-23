//! 集成测试使用的伪造记忆存储。
#![allow(
    dead_code,
    reason = "测试支撑会被多个集成测试按需复用，单个测试目标不一定覆盖全部辅助方法。"
)]

use std::sync::{Arc, Mutex};

use anyhow::{Result as AnyResult, anyhow};
use async_trait::async_trait;
use youyou_agent::{Memory, MemoryStorage};

/// 伪造记忆存储的内部状态。
#[derive(Debug, Default)]
struct FakeMemoryStorageState {
    memories: Vec<Memory>,
    list_recent_namespaces: Vec<String>,
}

/// 基于内存的伪造记忆存储。
#[derive(Debug, Clone, Default)]
pub struct FakeMemoryStorage {
    state: Arc<Mutex<FakeMemoryStorageState>>,
}

impl FakeMemoryStorage {
    /// 预置一条记忆。
    pub fn insert(&self, memory: Memory) {
        if let Ok(mut state) = self.state.lock() {
            state.memories.push(memory);
        }
    }

    /// 返回 `list_recent()` 收到过的 namespace 列表。
    #[must_use]
    pub fn list_recent_namespaces(&self) -> Vec<String> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |state| state.list_recent_namespaces.clone())
    }
}

#[async_trait]
impl MemoryStorage for FakeMemoryStorage {
    async fn search(&self, namespace: &str, _query: &str, limit: usize) -> AnyResult<Vec<Memory>> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake memory storage poisoned: {error}"))?;
        let mut memories: Vec<_> = state
            .memories
            .iter()
            .filter(|memory| memory.namespace == namespace)
            .cloned()
            .collect();
        memories.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(memories.into_iter().take(limit).collect())
    }

    async fn list_recent(&self, namespace: &str, limit: usize) -> AnyResult<Vec<Memory>> {
        if let Ok(mut state) = self.state.lock() {
            state.list_recent_namespaces.push(namespace.to_string());
        }
        self.search(namespace, "", limit).await
    }

    async fn list_by_namespace(&self, namespace: &str) -> AnyResult<Vec<Memory>> {
        self.search(namespace, "", usize::MAX).await
    }

    async fn upsert(&self, memory: Memory) -> AnyResult<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake memory storage poisoned: {error}"))?;
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

    async fn delete(&self, id: &str) -> AnyResult<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake memory storage poisoned: {error}"))?;
        state.memories.retain(|memory| memory.id != id);
        Ok(())
    }
}
