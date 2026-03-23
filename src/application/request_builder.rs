//! Provider 请求组装器。

use crate::application::prompt_builder::RenderedPrompt;
use crate::domain::{AgentError, ContentBlock, Message, Result, ToolOutput};
use crate::ports::{ChatRequest, ModelCapabilities, ToolDefinition};

/// 已解析完成的会话配置快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSessionConfig {
    /// 当前会话固定使用的模型标识。
    pub model_id: String,
    /// 会话级 system prompt 覆盖文本。
    pub system_prompt_override: Option<String>,
}

/// 构建 provider 请求时的上下文。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestContext {
    /// 当前请求可见的消息历史。
    pub messages: Vec<Message>,
    /// 当前模型的能力声明。
    pub model_capabilities: ModelCapabilities,
    /// 当前会话中可暴露给模型的 Tool 定义。
    pub tool_definitions: Vec<ToolDefinition>,
}

/// 请求级构建选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBuildOptions {
    /// 是否允许本次请求暴露 Tool 定义。
    pub allow_tools: bool,
}

impl Default for RequestBuildOptions {
    fn default() -> Self {
        Self { allow_tools: true }
    }
}

/// 无状态的 provider 请求组装器。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatRequestBuilder;

impl ChatRequestBuilder {
    /// 创建一个新的 `ChatRequestBuilder`。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 将渲染好的 prompt 和请求上下文组装为正式 `ChatRequest`。
    ///
    /// # Errors
    ///
    /// 当模型能力无法满足本次请求时返回 [`AgentError::InputValidation`]。
    pub fn build(
        &self,
        rendered_prompt: &RenderedPrompt,
        request_context: &RequestContext,
        session_config: &ResolvedSessionConfig,
        options: &RequestBuildOptions,
    ) -> Result<ChatRequest> {
        let tools = if options.allow_tools {
            request_context.tool_definitions.clone()
        } else {
            Vec::new()
        };

        validate_request_capabilities(
            request_context.model_capabilities,
            session_config.model_id.as_str(),
            &request_context.messages,
            &tools,
        )?;

        let mut messages = Vec::with_capacity(request_context.messages.len().saturating_add(1));
        messages.push(Message::System {
            content: rendered_prompt.text.clone(),
        });
        messages.extend(
            request_context
                .messages
                .iter()
                .map(map_message_for_provider),
        );

        Ok(ChatRequest {
            model_id: session_config.model_id.clone(),
            messages,
            tools,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        })
    }
}

/// 校验模型能力是否满足当前请求。
fn validate_request_capabilities(
    capabilities: ModelCapabilities,
    model_id: &str,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> Result<()> {
    if !tools.is_empty() && !capabilities.tool_use {
        return Err(AgentError::InputValidation {
            message: format!("model '{model_id}' does not support tool use"),
        });
    }

    if messages_contain_image(messages) && !capabilities.vision {
        return Err(AgentError::InputValidation {
            message: format!("model '{model_id}' does not support image input"),
        });
    }

    Ok(())
}

/// 判断消息列表中是否包含图片输入。
#[must_use]
fn messages_contain_image(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User { content } | Message::Assistant { content, .. } => content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. })),
        Message::ToolCall { .. } | Message::ToolResult { .. } | Message::System { .. } => false,
    })
}

/// 将领域消息转换为 provider 可见消息。
#[must_use]
fn map_message_for_provider(message: &Message) -> Message {
    match message {
        Message::User { content } => Message::User {
            content: map_content_blocks_for_provider(content),
        },
        Message::Assistant { content, status } => Message::Assistant {
            content: map_content_blocks_for_provider(content),
            status: *status,
        },
        Message::ToolCall {
            call_id,
            tool_name,
            arguments,
        } => Message::ToolCall {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        },
        Message::ToolResult { call_id, output } => Message::ToolResult {
            call_id: call_id.clone(),
            output: ToolOutput {
                content: output.content.clone(),
                is_error: output.is_error,
                metadata: serde_json::json!({}),
            },
        },
        Message::System { content } => Message::System {
            content: content.clone(),
        },
    }
}

/// 将内容块映射为 provider 可见格式。
#[must_use]
fn map_content_blocks_for_provider(content: &[ContentBlock]) -> Vec<ContentBlock> {
    content.iter().map(map_content_block_for_provider).collect()
}

/// 将单个内容块映射为 provider 可见格式。
#[must_use]
fn map_content_block_for_provider(block: &ContentBlock) -> ContentBlock {
    match block {
        ContentBlock::Text(text) => ContentBlock::Text(text.clone()),
        ContentBlock::Image { data, media_type } => ContentBlock::Image {
            data: data.clone(),
            media_type: media_type.clone(),
        },
        ContentBlock::File {
            name,
            media_type,
            text,
        } => ContentBlock::Text(format!(
            "<file>\n<name>{name}</name>\n<media_type>{media_type}</media_type>\n{text}\n</file>"
        )),
    }
}
