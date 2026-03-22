//! 会话与记忆的持久化抽象。

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{LedgerEvent, Memory};

/// 会话持久化契约。
#[async_trait]
pub trait SessionStorage: Send + Sync {
    /// 追加一条会话账本事件。
    ///
    /// # 错误
    ///
    /// 当事件无法持久化时返回错误。
    async fn save_event(&self, session_id: &str, event: LedgerEvent) -> AnyResult<()>;

    /// 按序号顺序加载完整会话账本。
    ///
    /// # 错误
    ///
    /// 当存储后端无法加载会话时返回错误。
    async fn load_session(&self, session_id: &str) -> AnyResult<Option<Vec<LedgerEvent>>>;

    /// 按最后更新时间列出持久化会话。
    ///
    /// # 错误
    ///
    /// 当存储后端无法枚举会话时返回错误。
    async fn list_sessions(&self, cursor: Option<&str>, limit: usize) -> AnyResult<SessionPage>;

    /// 搜索持久化会话。
    ///
    /// # 错误
    ///
    /// 当存储后端无法执行搜索时返回错误。
    async fn find_sessions(&self, query: &SessionSearchQuery) -> AnyResult<Vec<SessionSummary>>;

    /// 删除一个持久化会话。
    ///
    /// # 错误
    ///
    /// 当会话无法删除时返回错误。
    async fn delete_session(&self, session_id: &str) -> AnyResult<()>;
}

/// 会话发现查询条件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionSearchQuery {
    /// 按 session id 前缀匹配。
    IdPrefix(String),
    /// 按标题子串匹配。
    TitleContains(String),
}

/// 持久化会话的摘要信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    /// 会话标识。
    pub session_id: String,
    /// 可选的人类可读标题。
    pub title: Option<String>,
    /// 首条事件时间戳。
    pub created_at: DateTime<Utc>,
    /// 最后一条事件时间戳。
    pub updated_at: DateTime<Utc>,
    /// 用户与 assistant 消息数量。
    pub message_count: usize,
}

/// 分页会话列表响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    /// 当前页条目。
    pub sessions: Vec<SessionSummary>,
    /// 下一页游标。
    pub next_cursor: Option<String>,
}

/// 记忆持久化契约。
#[async_trait]
pub trait MemoryStorage: Send + Sync {
    /// 搜索记忆存储。
    ///
    /// # 错误
    ///
    /// 当存储后端无法执行搜索时返回错误。
    async fn search(&self, namespace: &str, query: &str, limit: usize) -> AnyResult<Vec<Memory>>;

    /// 列出某个命名空间下最近的记忆。
    ///
    /// # 错误
    ///
    /// 当存储后端无法加载记忆时返回错误。
    async fn list_recent(&self, namespace: &str, limit: usize) -> AnyResult<Vec<Memory>>;

    /// 列出某个命名空间下的全部记忆。
    ///
    /// # 错误
    ///
    /// 当存储后端无法枚举记忆时返回错误。
    async fn list_by_namespace(&self, namespace: &str) -> AnyResult<Vec<Memory>>;

    /// Upsert 一条记忆。
    ///
    /// # 错误
    ///
    /// 当记忆无法写入时返回错误。
    async fn upsert(&self, memory: Memory) -> AnyResult<()>;

    /// 按 id 删除一条记忆。
    ///
    /// # 错误
    ///
    /// 当记忆无法删除时返回错误。
    async fn delete(&self, id: &str) -> AnyResult<()>;
}
