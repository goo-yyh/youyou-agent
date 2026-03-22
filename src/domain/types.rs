//! 领域层与端口层共享的值对象。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 一条对话消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    /// 用户消息。
    User {
        /// 调用方提供的内容块。
        content: Vec<ContentBlock>,
    },
    /// Assistant 消息。
    Assistant {
        /// 模型生成的内容块。
        content: Vec<ContentBlock>,
        /// 用于恢复语义的完成状态。
        status: MessageStatus,
    },
    /// 模型请求的 Tool 调用。
    ToolCall {
        /// Provider 生成的 Tool 调用 id。
        call_id: String,
        /// Tool 名称。
        tool_name: String,
        /// JSON 参数。
        arguments: serde_json::Value,
    },
    /// 追加到对话中的 Tool 结果。
    ToolResult {
        /// Provider 生成的 Tool 调用 id。
        call_id: String,
        /// Tool 输出负载。
        output: ToolOutput,
    },
    /// Agent 注入的 system 消息。
    System {
        /// System 消息内容。
        content: String,
    },
}

/// 构成单条消息的逻辑内容块。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    /// 纯 UTF-8 文本。
    Text(String),
    /// 内联图片内容。
    Image {
        /// 编码后的图片数据。
        data: String,
        /// 图片的 MIME 类型。
        media_type: String,
    },
    /// 内联文件内容。
    File {
        /// 展示给模型的文件名。
        name: String,
        /// 文件的 MIME 类型。
        media_type: String,
        /// 文件的文本表示。
        text: String,
    },
}

/// Assistant 消息的完成状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageStatus {
    /// 消息正常完成。
    Complete,
    /// 消息被中断，恢复时可能需要提示。
    Incomplete,
}

/// 贯穿整个系统的 Tool 执行输出。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    /// 对模型可见的 Tool 内容。
    pub content: String,
    /// Tool 结果是否代表错误。
    pub is_error: bool,
    /// 保留给审计与恢复链路使用的结构化 metadata。
    pub metadata: serde_json::Value,
}

/// Session API 接受的用户输入。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserInput {
    /// 调用方提供的内容块。
    pub content: Vec<ContentBlock>,
}

/// Agent 构建阶段注册的静态 Skill 定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillDefinition {
    /// 用于 `/name` 调用的唯一 Skill 名称。
    pub name: String,
    /// 面向用户的显示名称。
    pub display_name: String,
    /// 展示在 prompt 中的简短描述。
    pub description: String,
    /// Skill 被触发时注入的 prompt 片段。
    pub prompt_template: String,
    /// Skill 依赖的 Tool 名称列表。
    #[serde(default)]
    pub required_tools: Vec<String>,
    /// 是否出现在隐式 Skill 列表中。
    pub allow_implicit_invocation: bool,
}

/// 跨会话记忆记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    /// 全局唯一的记忆标识。
    pub id: String,
    /// 用于隔离记忆的命名空间。
    pub namespace: String,
    /// 记忆内容。
    pub content: String,
    /// 记忆来源描述。
    pub source: String,
    /// 可选的分类标签。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 创建时间戳。
    pub created_at: DateTime<Utc>,
    /// 最后更新时间戳。
    pub updated_at: DateTime<Utc>,
}
