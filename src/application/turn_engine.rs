//! 单轮对话的核心编排器。

use std::sync::Arc;

use anyhow::anyhow;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::application::prompt_builder::{PromptBuildContext, PromptBuilder};
use crate::application::request_builder::{
    ChatRequestBuilder, RequestBuildOptions, RequestContext, ResolvedSessionConfig,
};
use crate::application::session_service::{SessionRuntime, persist_and_project};
use crate::application::tool_dispatcher::{RequestedToolCall, ToolDispatcher};
use crate::domain::{
    AgentConfig, AgentError, AgentEvent, AgentEventPayload, ContentBlock, HookData, HookPatch,
    LedgerEventPayload, Message, MessageStatus, Result, SkillDefinition, TurnOutcome, UserInput,
};
use crate::ports::{
    ChatEvent, ModelCapabilities, ModelProvider, PluginDescriptor, SessionStorage, ToolDefinition,
};

/// 单轮执行依赖的静态快照。
pub(crate) struct TurnEngineDeps {
    /// Agent 全局配置快照。
    pub(crate) agent_config: AgentConfig,
    /// 当前 turn 使用的 provider。
    pub(crate) provider: Arc<dyn ModelProvider>,
    /// 当前模型能力声明。
    pub(crate) model_capabilities: ModelCapabilities,
    /// 当前会话激活的 plugin 描述。
    pub(crate) plugins: Vec<PluginDescriptor>,
    /// 允许隐式展示的 skill 列表。
    pub(crate) implicit_skills: Vec<SkillDefinition>,
    /// 当前 turn 可用的 tool 定义。
    pub(crate) tool_definitions: Vec<ToolDefinition>,
    /// 当前 turn 的 tool 执行器。
    pub(crate) tool_dispatcher: ToolDispatcher,
    /// Hook 注册表快照。
    pub(crate) hook_registry: crate::application::hook_registry::HookRegistry,
    /// 可选的会话存储。
    pub(crate) session_storage: Option<Arc<dyn SessionStorage>>,
}

/// 单次模型请求迭代的结果。
#[derive(Debug)]
struct ModelIteration {
    /// 本次模型输出的 assistant 文本。
    assistant_output: String,
    /// 本次模型请求发出的 Tool 调用。
    tool_calls: Vec<RequestedToolCall>,
    /// 本次模型请求是否因为取消结束。
    cancelled: bool,
}

/// 运行中 turn 的额外终态控制。
#[derive(Debug)]
enum TurnContinuation {
    /// 正常继续下一轮 tool loop。
    Continue,
    /// 用指定错误结束 turn，但允许先跑一个无 tool 的收尾请求。
    FinishAfterFinalText(AgentError),
}

/// `run_turn` 的统一对外入口。
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

/// 执行单轮 turn 的完整编排流程。
#[allow(
    clippy::too_many_lines,
    reason = "phase 5 的主循环需要把 provider、tool loop、取消和终态统一编排在一起。"
)]
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

    let mut tool_calls_count = 0usize;
    let mut allow_tools = true;
    let mut continuation = TurnContinuation::Continue;

    loop {
        let iteration = request_model_iteration(
            &mut runtime,
            &deps,
            &dynamic_sections,
            &event_tx,
            &turn_id,
            &cancel,
            allow_tools,
        )
        .await?;

        if iteration.cancelled {
            if !iteration.assistant_output.is_empty() {
                persist_assistant_message(
                    &mut runtime,
                    deps.session_storage.as_deref(),
                    &iteration.assistant_output,
                    MessageStatus::Incomplete,
                )
                .await?;
            }

            return finalize_cancelled_turn(
                &mut runtime,
                &deps.hook_registry,
                &event_tx,
                &turn_id,
                iteration.assistant_output,
                tool_calls_count,
            )
            .await;
        }

        if !iteration.assistant_output.is_empty() {
            persist_assistant_message(
                &mut runtime,
                deps.session_storage.as_deref(),
                &iteration.assistant_output,
                MessageStatus::Complete,
            )
            .await?;
        }

        if iteration.tool_calls.is_empty() {
            return finalize_completed_turn(
                &mut runtime,
                &deps.hook_registry,
                &event_tx,
                &turn_id,
                iteration.assistant_output,
                tool_calls_count,
                continuation,
            )
            .await;
        }

        if !allow_tools {
            return Err(AgentError::ProviderError {
                message: "provider emitted tool calls while tools are disabled".to_string(),
                source: anyhow!("unexpected tool call in allow_tools=false request"),
                retryable: false,
            });
        }

        let prepared_batch = deps
            .tool_dispatcher
            .prepare_batch(
                iteration.tool_calls,
                &deps.hook_registry,
                &runtime.session_id,
                &turn_id,
            )
            .await?;
        persist_tool_calls(
            &mut runtime,
            deps.session_storage.as_deref(),
            &prepared_batch,
        )
        .await?;

        let session_id = runtime.session_id.clone();
        let batch_outcome = deps
            .tool_dispatcher
            .execute_batch(
                prepared_batch,
                &deps.hook_registry,
                &cancel,
                &session_id,
                &turn_id,
                &mut runtime.event_sequence,
                &event_tx,
            )
            .await?;
        persist_tool_results(
            &mut runtime,
            deps.session_storage.as_deref(),
            &batch_outcome.results,
        )
        .await?;

        tool_calls_count = tool_calls_count.saturating_add(batch_outcome.results.len());

        if batch_outcome.cancelled {
            return finalize_cancelled_turn(
                &mut runtime,
                &deps.hook_registry,
                &event_tx,
                &turn_id,
                String::new(),
                tool_calls_count,
            )
            .await;
        }

        if tool_calls_count > deps.agent_config.max_tool_calls_per_turn {
            let limit = deps.agent_config.max_tool_calls_per_turn;
            emit_event(
                &mut runtime,
                &event_tx,
                &turn_id,
                AgentEventPayload::Error(AgentError::MaxToolCallsExceeded { limit }),
            )
            .await;
            persist_tool_limit_message(&mut runtime, deps.session_storage.as_deref(), limit)
                .await?;
            continuation =
                TurnContinuation::FinishAfterFinalText(AgentError::MaxToolCallsExceeded { limit });
            allow_tools = false;
            continue;
        }

        if batch_outcome.stop_after_batch {
            allow_tools = false;
        }
    }
}

/// 触发 `TurnStart` hook，并收集最终动态段落。
async fn dispatch_turn_start(
    hook_registry: &crate::application::hook_registry::HookRegistry,
    session_id: &str,
    turn_id: &str,
    input: &UserInput,
) -> Result<Vec<String>> {
    let patch = hook_registry
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

    Ok(collect_dynamic_sections(patch))
}

/// 将显式触发的 skill 注入记到账本。
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

/// 将用户输入写入账本与上下文投影。
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

/// 发起单次模型请求迭代。
#[allow(
    clippy::too_many_arguments,
    reason = "单次模型请求需要显式接收 prompt、事件和取消上下文。"
)]
async fn request_model_iteration(
    runtime: &mut SessionRuntime,
    deps: &TurnEngineDeps,
    dynamic_sections: &[String],
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    allow_tools: bool,
) -> Result<ModelIteration> {
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
            tool_definitions: deps.tool_definitions.clone(),
        },
        &ResolvedSessionConfig {
            model_id: runtime.model_id.clone(),
            system_prompt_override: runtime.system_prompt_override.clone(),
        },
        &RequestBuildOptions { allow_tools },
    )?;
    let stream = match deps.provider.chat(request, cancel.clone()).await {
        Ok(stream) => stream,
        Err(_error) if cancel.is_cancelled() => {
            return Ok(ModelIteration {
                assistant_output: String::new(),
                tool_calls: Vec::new(),
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

    consume_provider_stream(runtime, event_tx, turn_id, cancel, stream, allow_tools).await
}

/// 消费 provider 流，收集 assistant 输出和 `ToolCall`。
#[allow(
    clippy::too_many_lines,
    reason = "provider 流消费需要同时处理文本增量、tool call、取消和 provider error。"
)]
async fn consume_provider_stream(
    runtime: &mut SessionRuntime,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    mut stream: crate::ports::ChatEventStream,
    allow_tools: bool,
) -> Result<ModelIteration> {
    use futures::StreamExt;

    let mut assistant_output = String::new();
    let mut tool_calls = Vec::new();
    let mut saw_done = false;

    while let Some(item) = stream.next().await {
        let event = match item {
            Ok(event) => event,
            Err(_error) if cancel.is_cancelled() => {
                return Ok(ModelIteration {
                    assistant_output,
                    tool_calls,
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
            ChatEvent::TextDelta(text) => {
                assistant_output.push_str(text.as_str());
                emit_event(
                    runtime,
                    event_tx,
                    turn_id,
                    AgentEventPayload::TextDelta(text),
                )
                .await;
            }
            ChatEvent::ReasoningDelta(reasoning) => {
                emit_event(
                    runtime,
                    event_tx,
                    turn_id,
                    AgentEventPayload::ReasoningDelta(reasoning),
                )
                .await;
            }
            ChatEvent::ToolCall {
                call_id,
                tool_name,
                arguments,
            } => {
                if !allow_tools {
                    return Err(AgentError::ProviderError {
                        message: "provider emitted a tool call after tools were disabled"
                            .to_string(),
                        source: anyhow!("provider emitted tool call in final no-tools request"),
                        retryable: false,
                    });
                }
                tool_calls.push(RequestedToolCall {
                    call_id,
                    tool_name,
                    arguments,
                });
            }
            ChatEvent::Done { .. } => {
                saw_done = true;
                break;
            }
            ChatEvent::Error(_error) if cancel.is_cancelled() => {
                return Ok(ModelIteration {
                    assistant_output,
                    tool_calls,
                    cancelled: true,
                });
            }
            ChatEvent::Error(error) => {
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
        return Ok(ModelIteration {
            assistant_output,
            tool_calls,
            cancelled: false,
        });
    }

    if cancel.is_cancelled() {
        return Ok(ModelIteration {
            assistant_output,
            tool_calls,
            cancelled: true,
        });
    }

    Err(AgentError::ProviderError {
        message: "provider stream ended before completion".to_string(),
        source: anyhow!("provider stream ended without a done event"),
        retryable: false,
    })
}

/// 将当前批次的 `ToolCall` 写入账本。
async fn persist_tool_calls(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    batch: &crate::application::tool_dispatcher::PreparedToolBatch,
) -> Result<()> {
    for call in &batch.calls {
        persist_and_project(
            runtime,
            storage,
            LedgerEventPayload::ToolCall {
                call_id: call.call_id.clone(),
                tool_name: call.tool_name.clone(),
                arguments: call.effective_arguments.clone(),
            },
        )
        .await?;
    }

    Ok(())
}

/// 将当前批次的 `ToolResult` 写入账本。
async fn persist_tool_results(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    results: &[crate::application::tool_dispatcher::ToolExecutionRecord],
) -> Result<()> {
    for result in results {
        persist_and_project(
            runtime,
            storage,
            LedgerEventPayload::ToolResult {
                call_id: result.call_id.clone(),
                output: result.output.clone(),
            },
        )
        .await?;
    }

    Ok(())
}

/// 在 Tool 超限时注入统一的 system 提示。
async fn persist_tool_limit_message(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    limit: usize,
) -> Result<()> {
    persist_and_project(
        runtime,
        storage,
        LedgerEventPayload::SystemMessage {
            content: format!(
                "[TOOL_LIMIT_REACHED] You have reached the maximum number of tool calls ({limit}) for this turn. You MUST NOT call any more tools. Summarize your progress and provide a final response."
            ),
        },
    )
    .await?;

    Ok(())
}

/// 按指定状态持久化 assistant 消息。
async fn persist_assistant_message(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    assistant_output: &str,
    status: MessageStatus,
) -> Result<()> {
    if assistant_output.is_empty() {
        return Ok(());
    }

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

/// 以取消态结束当前 turn。
async fn finalize_cancelled_turn(
    runtime: &mut SessionRuntime,
    hook_registry: &crate::application::hook_registry::HookRegistry,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    assistant_output: String,
    tool_calls_count: usize,
) -> Result<TurnOutcome> {
    dispatch_turn_end(
        hook_registry,
        &runtime.session_id,
        turn_id,
        assistant_output,
        tool_calls_count,
        true,
    )
    .await?;
    emit_event(runtime, event_tx, turn_id, AgentEventPayload::TurnCancelled).await;

    Ok(TurnOutcome::Cancelled)
}

/// 以完成或失败态结束当前 turn。
async fn finalize_completed_turn(
    runtime: &mut SessionRuntime,
    hook_registry: &crate::application::hook_registry::HookRegistry,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    assistant_output: String,
    tool_calls_count: usize,
    continuation: TurnContinuation,
) -> Result<TurnOutcome> {
    dispatch_turn_end(
        hook_registry,
        &runtime.session_id,
        turn_id,
        assistant_output,
        tool_calls_count,
        false,
    )
    .await?;

    match continuation {
        TurnContinuation::Continue => {
            emit_event(runtime, event_tx, turn_id, AgentEventPayload::TurnComplete).await;
            Ok(TurnOutcome::Completed)
        }
        TurnContinuation::FinishAfterFinalText(error) => Ok(TurnOutcome::Failed(error)),
    }
}

/// 触发 `TurnEnd` hook。
async fn dispatch_turn_end(
    hook_registry: &crate::application::hook_registry::HookRegistry,
    session_id: &str,
    turn_id: &str,
    assistant_output: String,
    tool_calls_count: usize,
    cancelled: bool,
) -> Result<()> {
    hook_registry
        .dispatch(
            crate::domain::HookEvent::TurnEnd,
            session_id,
            Some(turn_id),
            HookData::TurnEnd {
                assistant_output,
                tool_calls_count,
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

/// 从 `TurnStart` patch 中提取动态段落。
fn collect_dynamic_sections(patch: Option<HookPatch>) -> Vec<String> {
    match patch {
        Some(HookPatch::TurnStart {
            append_dynamic_sections,
        }) => append_dynamic_sections
            .into_iter()
            .filter(|section| !section.trim().is_empty())
            .collect(),
        Some(HookPatch::BeforeToolUse { .. }) | None => Vec::new(),
    }
}

/// 将 assistant 文本封装为标准内容块。
fn assistant_content(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(text.to_string())]
}
