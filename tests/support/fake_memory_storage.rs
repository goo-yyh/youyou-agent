//! 集成测试使用的伪造记忆存储。
#![allow(
    dead_code,
    reason = "测试支撑会被多个集成测试按需复用，单个测试目标不一定覆盖全部辅助方法。"
)]

use std::sync::{Arc, Mutex};

use anyhow::{Result as AnyResult, anyhow};
use async_trait::async_trait;
use youyou_agent::{Memory, MemoryStorage};

/// 一次 `search()` 调用的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSearch {
    /// 传入的 namespace。
    pub namespace: String,
    /// 传入的 query。
    pub query: String,
    /// 传入的 limit。
    pub limit: usize,
}

/// 伪造记忆存储的内部状态。
#[derive(Debug, Default)]
struct FakeMemoryStorageState {
    memories: Vec<Memory>,
    list_recent_namespaces: Vec<String>,
    search_calls: Vec<RecordedSearch>,
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

    /// 返回收到过的 `search()` 调用记录。
    #[must_use]
    pub fn search_calls(&self) -> Vec<RecordedSearch> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |state| state.search_calls.clone())
    }

    /// 返回当前全部记忆副本。
    #[must_use]
    pub fn memories(&self) -> Vec<Memory> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |state| state.memories.clone())
    }

    /// 按 namespace 与更新时间查询记忆，不记录 search 调用。
    fn query_memories(
        state: &FakeMemoryStorageState,
        namespace: &str,
        limit: usize,
    ) -> Vec<Memory> {
        let mut memories: Vec<_> = state
            .memories
            .iter()
            .filter(|memory| memory.namespace == namespace)
            .cloned()
            .collect();
        memories.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        memories.into_iter().take(limit).collect()
    }
}

#[async_trait]
impl MemoryStorage for FakeMemoryStorage {
    async fn search(&self, namespace: &str, query: &str, limit: usize) -> AnyResult<Vec<Memory>> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake memory storage poisoned: {error}"))?;
        state.search_calls.push(RecordedSearch {
            namespace: namespace.to_string(),
            query: query.to_string(),
            limit,
        });
        Ok(Self::query_memories(&state, namespace, limit))
    }

    async fn list_recent(&self, namespace: &str, limit: usize) -> AnyResult<Vec<Memory>> {
        if let Ok(mut state) = self.state.lock() {
            state.list_recent_namespaces.push(namespace.to_string());
        }
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake memory storage poisoned: {error}"))?;
        Ok(Self::query_memories(&state, namespace, limit))
    }

    async fn list_by_namespace(&self, namespace: &str) -> AnyResult<Vec<Memory>> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("fake memory storage poisoned: {error}"))?;
        Ok(Self::query_memories(&state, namespace, usize::MAX))
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
