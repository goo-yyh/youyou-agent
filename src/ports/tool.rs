//! Tool 执行抽象。

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::ToolOutput;

/// Tool 执行契约。
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// 返回唯一的 Tool 名称。
    fn name(&self) -> &str;

    /// 返回面向用户的 Tool 描述。
    fn description(&self) -> &str;

    /// 返回描述 Tool 参数的 JSON Schema。
    fn parameters_schema(&self) -> serde_json::Value;

    /// 返回该 Tool 是否会修改外部状态。
    fn is_mutating(&self) -> bool;

    /// 执行一次 Tool 调用。
    ///
    /// # 错误
    ///
    /// 当 Tool 实现在生成结构化 [`ToolOutput`] 之前失败时返回错误。
    async fn execute(
        &self,
        input: ToolInput,
        timeout_cancel: CancellationToken,
    ) -> AnyResult<ToolOutput>;
}

/// Tool 调用负载。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolInput {
    /// Provider 生成的 Tool 调用 id。
    pub call_id: String,
    /// Tool 名称。
    pub tool_name: String,
    /// 模型提供的 JSON 参数。
    pub arguments: serde_json::Value,
}
