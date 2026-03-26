//! 单轮对话的核心编排器。

use std::sync::Arc;

use anyhow::anyhow;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::application::prompt_builder::{PromptBuildContext, PromptBuilder, RenderedPrompt};
use crate::application::request_builder::{
    ChatRequestBuilder, RequestBuildOptions, ResolvedSessionConfig,
};
use crate::application::session_service::{SessionRuntime, persist_and_project};
use crate::application::tool_dispatcher::{RequestedToolCall, ToolDispatcher};
use crate::domain::{
    AgentConfig, AgentError, AgentEvent, AgentEventPayload, ContentBlock, HookData, HookPatch,
    LedgerEventPayload, Message, MessageStatus, MetadataKey, Result, SkillDefinition, TurnOutcome,
    UserInput,
};
use crate::ports::{
    ChatEvent, ModelCapabilities, ModelProvider, PluginDescriptor, SessionStorage, ToolDefinition,
};

/// 单轮执行依赖的静态快照。
pub(crate) struct TurnEngineDeps {
    /// Agent 全局配置快照。
    pub(crate) agent_config: AgentConfig,
    /// 当前 turn 使用的主 provider。
    pub(crate) provider: Arc<dyn ModelProvider>,
    /// 当前模型能力声明。
    pub(crate) model_capabilities: ModelCapabilities,
    /// compact 使用的 provider。
    pub(crate) compact_provider: Arc<dyn ModelProvider>,
    /// compact 使用的模型标识。
    pub(crate) compact_model_id: String,
    /// compact 模型能力声明。
    pub(crate) compact_model_capabilities: ModelCapabilities,
    /// compact 使用的 prompt 文本。
    pub(crate) compact_prompt: String,
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

/// 模型请求阶段的错误分类。
#[derive(Debug)]
enum RequestIterationError {
    /// 常规 Agent 错误。
    Agent(AgentError),
    /// Provider 明确报告上下文窗口超限。
    ContextLengthExceeded(String),
}

/// compact 的触发来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionTrigger {
    /// 在请求前根据估算主动触发。
    Estimate,
    /// Provider 返回 `context_length_exceeded` 后兜底触发。
    ContextLengthExceededFallback,
}

/// 本次 compact 编排的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionOutcome {
    /// 成功执行并持久化了 compact marker。
    Applied,
    /// 本次 compact 被 `BeforeCompact` 中止，仅对预估触发有效。
    Skipped,
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
    reason = "turn loop 需要把 provider、compact、tool loop 和取消语义放在同一个状态机里。"
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
        if cancel.is_cancelled() {
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

        let prompt = build_turn_prompt(&deps, &runtime, &dynamic_sections);
        match maybe_compact_before_request(
            &mut runtime,
            &deps,
            &event_tx,
            &turn_id,
            &cancel,
            &prompt,
            allow_tools,
        )
        .await
        {
            Ok(()) => {}
            Err(AgentError::RequestCancelled) => {
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
            Err(error) => return Err(error),
        }

        let iteration = match request_model_iteration_with_fallback(
            &mut runtime,
            &deps,
            &prompt,
            &event_tx,
            &turn_id,
            &cancel,
            allow_tools,
        )
        .await
        {
            Ok(iteration) => iteration,
            Err(AgentError::RequestCancelled) => {
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
            Err(error) => return Err(error),
        };

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

/// 构造当前迭代的 system prompt。
#[must_use]
fn build_turn_prompt(
    deps: &TurnEngineDeps,
    runtime: &SessionRuntime,
    dynamic_sections: &[String],
) -> RenderedPrompt {
    PromptBuilder::new().build(
        &deps.agent_config,
        runtime.system_prompt_override.as_deref(),
        &PromptBuildContext {
            implicit_skills: deps.implicit_skills.clone(),
            plugins: deps.plugins.clone(),
            memories: runtime.bootstrap_memories.clone(),
            dynamic_sections: dynamic_sections.to_vec(),
        },
    )
}

/// 在请求前根据估算决定是否主动 compact。
async fn maybe_compact_before_request(
    runtime: &mut SessionRuntime,
    deps: &TurnEngineDeps,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    prompt: &RenderedPrompt,
    allow_tools: bool,
) -> Result<()> {
    let tools_chars = if allow_tools {
        tool_definitions_chars(&deps.tool_definitions)
    } else {
        0
    };

    if !runtime
        .context_manager
        .needs_compaction(prompt.text.len(), tools_chars)
    {
        return Ok(());
    }

    let _ = compact_context(
        runtime,
        deps,
        event_tx,
        turn_id,
        cancel,
        CompactionTrigger::Estimate,
    )
    .await?;
    Ok(())
}

/// 执行一次模型请求，并在需要时只做一次 `context_length_exceeded` 兜底重试。
async fn request_model_iteration_with_fallback(
    runtime: &mut SessionRuntime,
    deps: &TurnEngineDeps,
    prompt: &RenderedPrompt,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    allow_tools: bool,
) -> Result<ModelIteration> {
    match request_model_iteration_once(
        runtime,
        deps,
        prompt,
        event_tx,
        turn_id,
        cancel,
        allow_tools,
    )
    .await
    {
        Ok(iteration) => Ok(iteration),
        Err(RequestIterationError::Agent(error)) => Err(error),
        Err(RequestIterationError::ContextLengthExceeded(message)) => {
            let _ = compact_context(
                runtime,
                deps,
                event_tx,
                turn_id,
                cancel,
                CompactionTrigger::ContextLengthExceededFallback,
            )
            .await?;

            match request_model_iteration_once(
                runtime,
                deps,
                prompt,
                event_tx,
                turn_id,
                cancel,
                allow_tools,
            )
            .await
            {
                Ok(iteration) => Ok(iteration),
                Err(RequestIterationError::Agent(error)) => Err(error),
                Err(RequestIterationError::ContextLengthExceeded(retry_message)) => {
                    Err(AgentError::CompactError {
                        message: format!(
                            "provider still rejected the request after fallback compaction: {message}; retry: {retry_message}"
                        ),
                    })
                }
            }
        }
    }
}

/// 发起单次模型请求迭代。
async fn request_model_iteration_once(
    runtime: &mut SessionRuntime,
    deps: &TurnEngineDeps,
    prompt: &RenderedPrompt,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    allow_tools: bool,
) -> std::result::Result<ModelIteration, RequestIterationError> {
    if cancel.is_cancelled() {
        return Ok(ModelIteration {
            assistant_output: String::new(),
            tool_calls: Vec::new(),
            cancelled: true,
        });
    }

    let request = ChatRequestBuilder::new()
        .build(
            prompt,
            &runtime
                .context_manager
                .build_request_context(deps.model_capabilities, deps.tool_definitions.clone()),
            &ResolvedSessionConfig {
                model_id: runtime.model_id.clone(),
                system_prompt_override: runtime.system_prompt_override.clone(),
            },
            &RequestBuildOptions { allow_tools },
        )
        .map_err(RequestIterationError::Agent)?;
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
            return Err(RequestIterationError::Agent(AgentError::ProviderError {
                message: "failed to start provider chat request".to_string(),
                source: error.context("model provider failed to start chat request"),
                retryable: false,
            }));
        }
    };

    consume_provider_stream(runtime, event_tx, turn_id, cancel, stream, allow_tools).await
}

/// 消费 provider 流，收集 assistant 输出和 `ToolCall`。
#[allow(
    clippy::too_many_lines,
    reason = "provider 流需要同时处理文本增量、tool call、取消和上下文超限分支。"
)]
async fn consume_provider_stream(
    runtime: &mut SessionRuntime,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    mut stream: crate::ports::ChatEventStream,
    allow_tools: bool,
) -> std::result::Result<ModelIteration, RequestIterationError> {
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
                return Err(RequestIterationError::Agent(AgentError::ProviderError {
                    message: "provider stream failed".to_string(),
                    source: error.context("model provider stream returned an error"),
                    retryable: false,
                }));
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
                    return Err(RequestIterationError::Agent(AgentError::ProviderError {
                        message: "provider emitted a tool call after tools were disabled"
                            .to_string(),
                        source: anyhow!("provider emitted tool call in final no-tools request"),
                        retryable: false,
                    }));
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
            ChatEvent::Error(error) if error.is_context_length_exceeded => {
                return Err(RequestIterationError::ContextLengthExceeded(error.message));
            }
            ChatEvent::Error(error) => {
                let retryable = error.retryable;
                return Err(RequestIterationError::Agent(AgentError::ProviderError {
                    message: error.message.clone(),
                    source: anyhow!(error),
                    retryable,
                }));
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

    Err(RequestIterationError::Agent(AgentError::ProviderError {
        message: "provider stream ended before completion".to_string(),
        source: anyhow!("provider stream ended without a done event"),
        retryable: false,
    }))
}

/// 执行一次完整的 compact 编排。
async fn compact_context(
    runtime: &mut SessionRuntime,
    deps: &TurnEngineDeps,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    cancel: &CancellationToken,
    trigger: CompactionTrigger,
) -> Result<CompactionOutcome> {
    match dispatch_before_compact(
        &deps.hook_registry,
        &runtime.session_id,
        turn_id,
        runtime.context_manager.visible_messages().len(),
        runtime.context_manager.estimated_tokens(),
    )
    .await
    {
        Ok(()) => {}
        Err(AgentError::PluginAborted {
            hook: "BeforeCompact",
            reason,
        }) => {
            return match trigger {
                CompactionTrigger::Estimate => Ok(CompactionOutcome::Skipped),
                CompactionTrigger::ContextLengthExceededFallback => Err(AgentError::CompactError {
                    message: format!("BeforeCompact hook aborted fallback compaction: {reason}"),
                }),
            };
        }
        Err(error) => return Err(error),
    }

    let marker = runtime
        .context_manager
        .generate_compaction_marker(
            &runtime.ledger,
            deps.compact_provider.as_ref(),
            &deps.compact_model_id,
            deps.compact_model_capabilities,
            &deps.compact_prompt,
            cancel,
        )
        .await?;
    persist_compaction_marker(runtime, deps.session_storage.as_deref(), marker).await?;
    emit_event(
        runtime,
        event_tx,
        turn_id,
        AgentEventPayload::ContextCompacted,
    )
    .await;

    Ok(CompactionOutcome::Applied)
}

/// 触发 `BeforeCompact` hook。
async fn dispatch_before_compact(
    hook_registry: &crate::application::hook_registry::HookRegistry,
    session_id: &str,
    turn_id: &str,
    message_count: usize,
    estimated_tokens: usize,
) -> Result<()> {
    let _ = hook_registry
        .dispatch(
            crate::domain::HookEvent::BeforeCompact,
            session_id,
            Some(turn_id),
            HookData::BeforeCompact {
                message_count,
                estimated_tokens,
            },
        )
        .await?;

    Ok(())
}

/// 将 compact marker 作为关键 metadata 持久化到账本。
async fn persist_compaction_marker(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    marker: crate::domain::CompactionMarker,
) -> Result<()> {
    let value = serde_json::to_value(&marker).map_err(|error| {
        AgentError::StorageError(anyhow!(error).context("failed to serialize compact marker"))
    })?;

    let _ = persist_and_project(
        runtime,
        storage,
        LedgerEventPayload::Metadata {
            key: MetadataKey::ContextCompaction,
            value,
        },
    )
    .await?;

    Ok(())
}

/// 计算当前请求中 tool 定义的大致字符数。
#[must_use]
fn tool_definitions_chars(definitions: &[ToolDefinition]) -> usize {
    definitions
        .iter()
        .map(|definition| {
            definition.name.len()
                + definition.description.len()
                + definition.parameters.to_string().len()
        })
        .sum()
}

/// 将显式触发的 skill 注入记到账本。
async fn persist_skill_injections(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    skill_injections: &[Message],
) -> Result<()> {
    for message in skill_injections {
        if let Message::System { content } = message {
            let _ = persist_and_project(
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
    let _ = persist_and_project(
        runtime,
        storage,
        LedgerEventPayload::UserMessage {
            content: input.content.clone(),
        },
    )
    .await?;

    Ok(())
}

/// 将当前批次的 `ToolCall` 写入账本。
async fn persist_tool_calls(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    batch: &crate::application::tool_dispatcher::PreparedToolBatch,
) -> Result<()> {
    for call in &batch.calls {
        let _ = persist_and_project(
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
        let _ = persist_and_project(
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
    let _ = persist_and_project(
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

    let _ = persist_and_project(
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
    let _ = hook_registry
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
#[must_use]
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
#[must_use]
fn assistant_content(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(text.to_string())]
}
