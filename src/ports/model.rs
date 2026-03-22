//! 模型提供方抽象。

use std::pin::Pin;

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::Message;

/// Provider 发出的聊天事件装箱流。
pub type ChatEventStream = Pin<Box<dyn Stream<Item = AnyResult<ChatEvent>> + Send>>;

/// 外部模型提供方接口。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// 返回唯一的 provider 标识。
    fn id(&self) -> &str;

    /// 返回该 provider 暴露的全部模型。
    fn models(&self) -> &[ModelInfo];

    /// 发起一次聊天请求。
    ///
    /// # 错误
    ///
    /// 当 provider 无法启动请求时返回错误。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> AnyResult<ChatEventStream>;
}

/// Provider 暴露的模型元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    /// 模型标识。
    pub id: String,
    /// 面向用户的显示名称。
    pub display_name: String,
    /// 用于压缩启发式判断的上下文窗口大小。
    pub context_window: usize,
    /// Provider 声明的能力标记。
    pub capabilities: ModelCapabilities,
}

/// 模型声明的能力标记。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    /// 模型是否支持发出 Tool 调用。
    pub tool_use: bool,
    /// 模型是否支持图片输入。
    pub vision: bool,
    /// Provider 是否支持流式响应。
    pub streaming: bool,
}

/// Provider 请求负载。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    /// 目标模型标识。
    pub model_id: String,
    /// 完整且有序的请求消息列表。
    pub messages: Vec<Message>,
    /// 暴露给模型的 Tool 定义列表。
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// 可选的 temperature 覆盖值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// 可选的输出 token 上限。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 可选的推理强度提示。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// 序列化进 provider 请求的 Tool 定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    /// 暴露给模型的 Tool 名称。
    pub name: String,
    /// 面向用户的 Tool 描述。
    pub description: String,
    /// 描述参数结构的 JSON Schema。
    pub parameters: serde_json::Value,
}

/// Provider 流式返回的响应事件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatEvent {
    /// 来自模型的文本增量。
    TextDelta(String),
    /// 来自模型的推理增量。
    ReasoningDelta(String),
    /// 模型请求的 Tool 调用。
    ToolCall {
        /// Provider 生成的调用 id。
        call_id: String,
        /// Tool 名称。
        tool_name: String,
        /// JSON 参数。
        arguments: serde_json::Value,
    },
    /// 流结束标记。
    Done {
        /// Token 使用统计。
        usage: TokenUsage,
    },
    /// Provider 上报的错误事件。
    Error(ChatError),
}

/// Provider 上报的 Token 使用量。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// 输入 token 数。
    pub input_tokens: u64,
    /// 输出 token 数。
    pub output_tokens: u64,
}

/// Provider 层面的聊天错误。
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
#[error("{message}")]
#[serde(rename_all = "camelCase")]
pub struct ChatError {
    /// 面向用户的错误信息。
    pub message: String,
    /// 重试是否可能成功。
    pub retryable: bool,
    /// 是否表示上下文窗口已超限。
    pub is_context_length_exceeded: bool,
}
