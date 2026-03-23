//! 单轮对话的最小编排实现。

use std::sync::Arc;

use anyhow::anyhow;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::application::prompt_builder::{PromptBuildContext, PromptBuilder};
use crate::application::request_builder::{
    ChatRequestBuilder, RequestBuildOptions, RequestContext, ResolvedSessionConfig,
};
use crate::application::session_service::{SessionRuntime, persist_and_project};
use crate::domain::{
    AgentConfig, AgentError, AgentEvent, AgentEventPayload, ContentBlock, HookData, HookPatch,
    LedgerEventPayload, Message, MessageStatus, Result, SkillDefinition, TurnOutcome, UserInput,
};
use crate::ports::{ModelCapabilities, ModelProvider, PluginDescriptor, SessionStorage};

/// 单轮执行所需的静态依赖快照。
pub(crate) struct TurnEngineDeps {
    /// Agent 的静态配置快照。
    pub(crate) agent_config: AgentConfig,
    /// 当前 turn 使用的模型提供方。
    pub(crate) provider: Arc<dyn ModelProvider>,
    /// 当前模型声明的能力。
    pub(crate) model_capabilities: ModelCapabilities,
    /// 已启用的插件描述。
    pub(crate) plugins: Vec<PluginDescriptor>,
    /// 允许隐式展示的 Skill 列表。
    pub(crate) implicit_skills: Vec<SkillDefinition>,
    /// Hook 注册表快照。
    pub(crate) hook_registry: crate::application::hook_registry::HookRegistry,
    /// 可选的会话存储。
    pub(crate) session_storage: Option<Arc<dyn SessionStorage>>,
}

/// Provider 流消费后的阶段结果。
#[derive(Debug)]
struct StreamOutcome {
    /// 当前已累积的 assistant 文本。
    assistant_output: String,
    /// 本轮是否因为取消而结束。
    cancelled: bool,
}

/// 最小 turn loop 的统一入口。
pub(crate) async fn run_turn(
    runtime: Arc<AsyncMutex<SessionRuntime>>,
    deps: TurnEngineDeps,
    turn_id: String,
    input: UserInput,
    skill_injections: Vec<Message>,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) -> TurnOutcome {
    match run_turn_inner(
        runtime,
        deps,
        turn_id,
        input,
        skill_injections,
        event_tx,
        cancel,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => TurnOutcome::Failed(error),
    }
}

/// 执行单轮 turn 的主体流程。
async fn run_turn_inner(
    runtime: Arc<AsyncMutex<SessionRuntime>>,
    deps: TurnEngineDeps,
    turn_id: String,
    input: UserInput,
    skill_injections: Vec<Message>,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) -> Result<TurnOutcome> {
    let mut runtime = runtime.lock().await;
    let dynamic_sections =
        dispatch_turn_start(&deps.hook_registry, &runtime.session_id, &turn_id, &input).await?;

    persist_skill_injections(
        &mut runtime,
        deps.session_storage.as_deref(),
        &skill_injections,
    )
    .await?;
    persist_user_message(&mut runtime, deps.session_storage.as_deref(), &input).await?;

    let stream_outcome = request_assistant_reply(
        &mut runtime,
        &deps,
        &dynamic_sections,
        &event_tx,
        &turn_id,
        &cancel,
    )
    .await?;

    finalize_turn(
        &mut runtime,
        deps.session_storage.as_deref(),
        &deps.hook_registry,
        &event_tx,
        &turn_id,
        stream_outcome,
    )
    .await
}

/// 触发 `TurnStart` hook 并汇总动态段落。
async fn dispatch_turn_start(
    hook_registry: &crate::application::hook_registry::HookRegistry,
    session_id: &str,
    turn_id: &str,
    input: &UserInput,
) -> Result<Vec<String>> {
    let patches = hook_registry
        .dispatch(
            crate::domain::HookEvent::TurnStart,
            session_id,
            Some(turn_id),
            HookData::TurnStart {
                user_input: input.clone(),
                dynamic_sections: Vec::new(),
            },
        )
        .await?;

    Ok(collect_dynamic_sections(&patches))
}

/// 将显式触发的 Skill 注入写入账本与上下文投影。
async fn persist_skill_injections(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    skill_injections: &[Message],
) -> Result<()> {
    for message in skill_injections {
        if let Message::System { content } = message {
            persist_and_project(
                runtime,
                storage,
                LedgerEventPayload::SystemMessage {
                    content: content.clone(),
                },
            )
            .await?;
        }
    }

    Ok(())
}

/// 将用户输入作为关键事件持久化。
async fn persist_user_message(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    input: &UserInput,
) -> Result<()> {
    persist_and_project(
        runtime,
        storage,
        LedgerEventPayload::UserMessage {
            content: input.content.clone(),
        },
    )
    .await?;

    Ok(())
}

/// 构造请求并消费 provider 流。
async fn request_assistant_reply(
    runtime: &mut SessionRuntime,
    deps: &TurnEngineDeps,
    dynamic_sections: &[String],
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
) -> Result<StreamOutcome> {
    let prompt = PromptBuilder::new().build(
        &deps.agent_config,
        runtime.system_prompt_override.as_deref(),
        &PromptBuildContext {
            implicit_skills: deps.implicit_skills.clone(),
            plugins: deps.plugins.clone(),
            memories: runtime.bootstrap_memories.clone(),
            dynamic_sections: dynamic_sections.to_vec(),
        },
    );
    let request = ChatRequestBuilder::new().build(
        &prompt,
        &RequestContext {
            messages: runtime.context_manager.visible_messages().to_vec(),
            model_capabilities: deps.model_capabilities,
            tool_definitions: Vec::new(),
        },
        &ResolvedSessionConfig {
            model_id: runtime.model_id.clone(),
            system_prompt_override: runtime.system_prompt_override.clone(),
        },
        &RequestBuildOptions { allow_tools: false },
    )?;

    let stream = match deps.provider.chat(request, cancel.clone()).await {
        Ok(stream) => stream,
        Err(_error) if cancel.is_cancelled() => {
            return Ok(StreamOutcome {
                assistant_output: String::new(),
                cancelled: true,
            });
        }
        Err(error) => {
            return Err(AgentError::ProviderError {
                message: "failed to start provider chat request".to_string(),
                source: error.context("model provider failed to start chat request"),
                retryable: false,
            });
        }
    };

    consume_provider_stream(runtime, event_tx, turn_id, cancel, stream).await
}

/// 消费 provider 的流式输出。
async fn consume_provider_stream(
    runtime: &mut SessionRuntime,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    mut stream: crate::ports::ChatEventStream,
) -> Result<StreamOutcome> {
    use futures::StreamExt;

    let mut assistant_output = String::new();
    let mut saw_done = false;

    while let Some(item) = stream.next().await {
        let event = match item {
            Ok(event) => event,
            Err(_error) if cancel.is_cancelled() => {
                return Ok(StreamOutcome {
                    assistant_output,
                    cancelled: true,
                });
            }
            Err(error) => {
                return Err(AgentError::ProviderError {
                    message: "provider stream failed".to_string(),
                    source: error.context("model provider stream returned an error"),
                    retryable: false,
                });
            }
        };

        match event {
            crate::ports::ChatEvent::TextDelta(text) => {
                assistant_output.push_str(text.as_str());
                emit_event(
                    runtime,
                    event_tx,
                    turn_id,
                    AgentEventPayload::TextDelta(text),
                )
                .await;
            }
            crate::ports::ChatEvent::ReasoningDelta(reasoning) => {
                emit_event(
                    runtime,
                    event_tx,
                    turn_id,
                    AgentEventPayload::ReasoningDelta(reasoning),
                )
                .await;
            }
            crate::ports::ChatEvent::ToolCall { .. } => {
                return Err(AgentError::ProviderError {
                    message: "tool calls are not supported before phase 5".to_string(),
                    source: anyhow!("unexpected tool call emitted by provider"),
                    retryable: false,
                });
            }
            crate::ports::ChatEvent::Done { .. } => {
                saw_done = true;
                break;
            }
            crate::ports::ChatEvent::Error(_error) if cancel.is_cancelled() => {
                return Ok(StreamOutcome {
                    assistant_output,
                    cancelled: true,
                });
            }
            crate::ports::ChatEvent::Error(error) => {
                let retryable = error.retryable;
                return Err(AgentError::ProviderError {
                    message: error.message.clone(),
                    source: anyhow!(error),
                    retryable,
                });
            }
        }
    }

    if saw_done {
        return Ok(StreamOutcome {
            assistant_output,
            cancelled: false,
        });
    }

    if cancel.is_cancelled() {
        return Ok(StreamOutcome {
            assistant_output,
            cancelled: true,
        });
    }

    Err(AgentError::ProviderError {
        message: "provider stream ended before completion".to_string(),
        source: anyhow!("provider stream ended without a done event"),
        retryable: false,
    })
}

/// 根据 provider 输出完成 turn 的持久化与终态事件。
async fn finalize_turn(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    hook_registry: &crate::application::hook_registry::HookRegistry,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    stream_outcome: StreamOutcome,
) -> Result<TurnOutcome> {
    if stream_outcome.cancelled {
        persist_incomplete_assistant_message(runtime, storage, &stream_outcome.assistant_output)
            .await?;
        dispatch_turn_end(
            hook_registry,
            &runtime.session_id,
            turn_id,
            stream_outcome.assistant_output,
            true,
        )
        .await?;
        emit_event(runtime, event_tx, turn_id, AgentEventPayload::TurnCancelled).await;
        return Ok(TurnOutcome::Cancelled);
    }

    persist_complete_assistant_message(runtime, storage, &stream_outcome.assistant_output).await?;
    dispatch_turn_end(
        hook_registry,
        &runtime.session_id,
        turn_id,
        stream_outcome.assistant_output,
        false,
    )
    .await?;
    emit_event(runtime, event_tx, turn_id, AgentEventPayload::TurnComplete).await;

    Ok(TurnOutcome::Completed)
}

/// 持久化完整 assistant 消息。
async fn persist_complete_assistant_message(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    assistant_output: &str,
) -> Result<()> {
    persist_assistant_message(runtime, storage, assistant_output, MessageStatus::Complete).await
}

/// 持久化取消后的不完整 assistant 消息。
async fn persist_incomplete_assistant_message(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    assistant_output: &str,
) -> Result<()> {
    if assistant_output.is_empty() {
        return Ok(());
    }

    persist_assistant_message(
        runtime,
        storage,
        assistant_output,
        MessageStatus::Incomplete,
    )
    .await
}

/// 按指定状态持久化 assistant 消息。
async fn persist_assistant_message(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    assistant_output: &str,
    status: MessageStatus,
) -> Result<()> {
    persist_and_project(
        runtime,
        storage,
        LedgerEventPayload::AssistantMessage {
            content: assistant_content(assistant_output),
            status,
        },
    )
    .await?;

    Ok(())
}

/// 触发 `TurnEnd` hook。
async fn dispatch_turn_end(
    hook_registry: &crate::application::hook_registry::HookRegistry,
    session_id: &str,
    turn_id: &str,
    assistant_output: String,
    cancelled: bool,
) -> Result<()> {
    hook_registry
        .dispatch(
            crate::domain::HookEvent::TurnEnd,
            session_id,
            Some(turn_id),
            HookData::TurnEnd {
                assistant_output,
                tool_calls_count: 0,
                cancelled,
            },
        )
        .await?;

    Ok(())
}

/// 发出一条按序编号的对外事件。
async fn emit_event(
    runtime: &mut SessionRuntime,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    payload: AgentEventPayload,
) {
    let event = AgentEvent {
        session_id: runtime.session_id.clone(),
        turn_id: turn_id.to_string(),
        timestamp: chrono::Utc::now(),
        sequence: runtime.event_sequence,
        payload,
    };
    runtime.event_sequence = runtime.event_sequence.saturating_add(1);
    let _ = event_tx.send(event).await;
}

/// 将 hook 返回的 patch 汇总成动态段落列表。
fn collect_dynamic_sections(patches: &[HookPatch]) -> Vec<String> {
    let mut sections = Vec::new();

    for patch in patches {
        if let HookPatch::TurnStart {
            append_dynamic_sections,
        } = patch
        {
            sections.extend(
                append_dynamic_sections
                    .iter()
                    .filter(|section| !section.trim().is_empty())
                    .cloned(),
            );
        }
    }

    sections
}

/// 将 assistant 文本转成标准内容块列表。
fn assistant_content(text: &str) -> Vec<ContentBlock> {
    if text.is_empty() {
        return Vec::new();
    }

    vec![ContentBlock::Text(text.to_string())]
}
