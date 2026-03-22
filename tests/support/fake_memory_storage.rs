//! 集成测试使用的伪造记忆存储。

use std::sync::{Arc, Mutex};

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use youyou_agent::{Memory, MemoryStorage};

/// 基于内存的伪造记忆存储。
#[derive(Debug, Clone, Default)]
pub struct FakeMemoryStorage {
    memories: Arc<Mutex<Vec<Memory>>>,
}

#[async_trait]
impl MemoryStorage for FakeMemoryStorage {
    async fn search(&self, namespace: &str, _query: &str, limit: usize) -> AnyResult<Vec<Memory>> {
        Ok(self.memories.lock().map_or_else(
            |_| Vec::new(),
            |memories| {
                memories
                    .iter()
                    .filter(|memory| memory.namespace == namespace)
                    .take(limit)
                    .cloned()
                    .collect()
            },
        ))
    }

    async fn list_recent(&self, namespace: &str, limit: usize) -> AnyResult<Vec<Memory>> {
        self.search(namespace, "", limit).await
    }

    async fn list_by_namespace(&self, namespace: &str) -> AnyResult<Vec<Memory>> {
        self.search(namespace, "", usize::MAX).await
    }

    async fn upsert(&self, memory: Memory) -> AnyResult<()> {
        if let Ok(mut memories) = self.memories.lock() {
            if let Some(existing) = memories
                .iter_mut()
                .find(|existing| existing.id == memory.id)
            {
                *existing = memory;
            } else {
                memories.push(memory);
            }
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> AnyResult<()> {
        if let Ok(mut memories) = self.memories.lock() {
            memories.retain(|memory| memory.id != id);
        }
        Ok(())
    }
}
