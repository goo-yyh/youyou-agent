//! 活跃会话句柄。

use std::fmt;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::api::RunningTurn;
use crate::application::memory_manager::MemoryManager;
use crate::application::session_service::{AgentControl, SessionDescriptor, SessionService};
use crate::application::skill_manager::SkillManager;
use crate::application::tool_dispatcher::ToolDispatcher;
use crate::application::turn_engine::{self, MemoryTurnDeps, TurnEngineDeps};
use crate::domain::{AgentError, ContentBlock, Message, Result, UserInput};
use crate::ports::ModelCapabilities;
use crate::prompt::templates::DEFAULT_COMPACT_PROMPT;

use super::agent::{AgentKernel, ModelRegistry};

/// 活跃会话句柄。
#[derive(Clone)]
pub struct SessionHandle {
    pub(crate) kernel: Arc<AgentKernel>,
    pub(crate) control: Arc<Mutex<AgentControl>>,
    session_id: String,
    model_id: String,
    memory_namespace: String,
    system_prompt_override: Option<String>,
}

impl SessionHandle {
    /// 创建一个新的会话句柄。
    pub(crate) fn new(
        kernel: Arc<AgentKernel>,
        control: Arc<Mutex<AgentControl>>,
        descriptor: SessionDescriptor,
    ) -> Self {
        Self {
            kernel,
            control,
            session_id: descriptor.session_id,
            model_id: descriptor.model_id,
            memory_namespace: descriptor.memory_namespace,
            system_prompt_override: descriptor.system_prompt_override,
        }
    }

    /// 返回会话标识。
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 返回当前会话锁定的模型标识。
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// 返回当前会话绑定的记忆命名空间。
    #[must_use]
    pub fn memory_namespace(&self) -> &str {
        &self.memory_namespace
    }

    /// 返回会话级 system prompt 覆盖文本。
    #[must_use]
    pub fn system_prompt_override(&self) -> Option<&str> {
        self.system_prompt_override.as_deref()
    }

    /// 关闭当前会话。
    ///
    /// # Errors
    ///
    /// 当该句柄已经失效，或 `SessionEnd` hook 中止关闭时返回错误。
    pub async fn close(&self) -> Result<()> {
        self.session_service().close_session(&self.session_id).await
    }

    /// 发送一条消息并启动新的 turn。
    ///
    /// # Errors
    ///
    /// 当输入非法、Session 已失效、当前已有运行中 turn，或 Skill 解析失败时返回错误。
    pub async fn send_message(
        &self,
        input: UserInput,
        external_cancel: Option<CancellationToken>,
    ) -> Result<RunningTurn> {
        let registered_model = self.kernel.models.resolve_model(&self.model_id)?;
        validate_user_input(&input, registered_model.info.capabilities)?;

        let skill_manager = SkillManager::new(
            self.kernel
                .skills
                .skills
                .values()
                .cloned()
                .collect::<Vec<_>>(),
        );
        let skill_injections = skill_manager
            .resolve_invocations(&input)?
            .into_iter()
            .map(|skill| skill_manager.render_injection(skill))
            .collect::<Vec<Message>>();

        let (event_tx, event_rx) = mpsc::channel(64);
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let (turn_finished_tx, turn_finished_rx) = oneshot::channel();
        let turn_start = self.start_turn(turn_finished_rx)?;
        let turn_id = allocate_turn_id(&turn_start.runtime).await;
        let bridge_shutdown = CancellationToken::new();

        if let Some(external_cancel) = external_cancel {
            spawn_external_cancel_bridge(
                external_cancel,
                turn_start.turn_cancel_token.clone(),
                bridge_shutdown.clone(),
            );
        }

        let deps = self.build_turn_engine_deps(registered_model.info.capabilities)?;
        spawn_turn_supervisor(
            Arc::clone(&self.control),
            Arc::clone(&turn_start.runtime),
            self.session_id.clone(),
            turn_id,
            input,
            skill_injections,
            deps,
            event_tx,
            outcome_tx,
            turn_finished_tx,
            turn_start.turn_cancel_token.clone(),
            bridge_shutdown,
        );

        Ok(RunningTurn::new(
            ReceiverStream::new(event_rx),
            turn_start.turn_cancel_token,
            outcome_rx,
        ))
    }

    /// 构造当前句柄使用的会话生命周期服务。
    fn session_service(&self) -> SessionService<'_, ModelRegistry> {
        SessionService::new(
            &self.kernel.config,
            &self.kernel.models,
            &self.kernel.hook_registry,
            Arc::clone(&self.control),
            self.kernel.session_storage.clone(),
            self.kernel.memory_storage.clone(),
        )
    }

    /// 原子地切换 turn 状态，并拿到运行所需的运行时引用。
    fn start_turn(&self, turn_finished_rx: oneshot::Receiver<()>) -> Result<TurnStartContext> {
        let mut control = self
            .control
            .lock()
            .map_err(|error| AgentError::InternalPanic {
                message: format!("agent control mutex poisoned: {error}"),
            })?;
        let crate::application::session_service::SessionSlotState::Active(active) =
            &mut control.slot
        else {
            return Err(AgentError::SessionNotFound(self.session_id.clone()));
        };

        if active.session_id != self.session_id {
            return Err(AgentError::SessionNotFound(self.session_id.clone()));
        }

        if matches!(
            active.turn_state,
            crate::application::session_service::TurnState::Running(_)
        ) {
            return Err(AgentError::TurnBusy);
        }

        let turn_cancel_token = active.session_cancel_token.child_token();
        active.turn_state = crate::application::session_service::TurnState::Running(
            crate::application::session_service::RunningTurnHandle {
                turn_cancel_token: turn_cancel_token.clone(),
                turn_finished_rx: Some(turn_finished_rx),
            },
        );

        Ok(TurnStartContext {
            runtime: Arc::clone(&active.runtime),
            turn_cancel_token,
        })
    }

    /// 构造 turn engine 需要的静态依赖快照。
    fn build_turn_engine_deps(
        &self,
        model_capabilities: ModelCapabilities,
    ) -> Result<TurnEngineDeps> {
        let provider = self.resolve_provider_for_model(&self.model_id)?;
        let compact_model_id = self
            .kernel
            .config
            .compact_model
            .clone()
            .unwrap_or_else(|| self.model_id.clone());
        let compact_registered_model = self.kernel.models.resolve_model(&compact_model_id)?;
        let compact_provider = self.resolve_provider_for_model(&compact_model_id)?;
        let memory = self.build_memory_turn_deps()?;

        Ok(TurnEngineDeps {
            agent_config: self.kernel.config.clone(),
            provider,
            model_capabilities,
            compact_provider,
            compact_model_id,
            compact_model_capabilities: compact_registered_model.info.capabilities,
            compact_prompt: self
                .kernel
                .config
                .compact_prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_COMPACT_PROMPT.to_string()),
            memory,
            plugins: self.kernel.plugins.values().cloned().collect(),
            implicit_skills: self
                .kernel
                .skills
                .skills
                .values()
                .filter(|skill| skill.allow_implicit_invocation)
                .cloned()
                .collect(),
            tool_definitions: self.kernel.tools.definitions.values().cloned().collect(),
            tool_dispatcher: ToolDispatcher::new(
                self.kernel.tools.handlers.clone(),
                self.kernel.config.tool_timeout_ms,
                self.kernel.config.tool_output_max_bytes,
                self.kernel.config.tool_output_metadata_max_bytes,
            ),
            hook_registry: self.kernel.hook_registry.clone(),
            session_storage: self.kernel.session_storage.clone(),
        })
    }

    /// 构造 turn 期间使用的记忆依赖。
    fn build_memory_turn_deps(&self) -> Result<Option<MemoryTurnDeps>> {
        let Some(storage) = self.kernel.memory_storage.clone() else {
            return Ok(None);
        };

        let model_id = self
            .kernel
            .config
            .memory_model
            .clone()
            .unwrap_or_else(|| self.model_id.clone());
        let provider = self.resolve_provider_for_model(&model_id)?;

        Ok(Some(MemoryTurnDeps {
            manager: MemoryManager::new(storage, self.kernel.config.memory_max_items),
            provider,
            model_id,
        }))
    }

    /// 为指定模型解析其所属的 provider。
    fn resolve_provider_for_model(
        &self,
        model_id: &str,
    ) -> Result<std::sync::Arc<dyn crate::ports::ModelProvider>> {
        let registered_model = self.kernel.models.resolve_model(model_id)?;

        self.kernel
            .models
            .providers
            .get(&registered_model.provider_id)
            .cloned()
            .ok_or_else(|| AgentError::ProviderError {
                message: format!(
                    "provider '{}' for model '{}' is not registered",
                    registered_model.provider_id, model_id
                ),
                source: anyhow!("model provider missing from registry"),
                retryable: false,
            })
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("session_id", &self.session_id)
            .field("model_id", &self.model_id)
            .field("memory_namespace", &self.memory_namespace)
            .field(
                "has_system_prompt_override",
                &self.system_prompt_override.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl PartialEq for SessionHandle {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
    }
}

impl Eq for SessionHandle {}

/// 启动 turn 后返回给 API 层的运行上下文。
struct TurnStartContext {
    /// 当前活跃 session 的运行时引用。
    runtime: Arc<AsyncMutex<crate::application::session_service::SessionRuntime>>,
    /// 本轮 turn 的取消令牌。
    turn_cancel_token: CancellationToken,
}

/// 为一个新 turn 分配稳定的 turn 标识。
async fn allocate_turn_id(
    runtime: &Arc<AsyncMutex<crate::application::session_service::SessionRuntime>>,
) -> String {
    let mut runtime = runtime.lock().await;
    runtime.turn_index = runtime.turn_index.saturating_add(1);
    format!("turn-{}", runtime.turn_index)
}

/// 将外部取消令牌桥接到 turn 取消令牌。
fn spawn_external_cancel_bridge(
    external_cancel: CancellationToken,
    turn_cancel_token: CancellationToken,
    bridge_shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        tokio::select! {
            () = external_cancel.cancelled() => turn_cancel_token.cancel(),
            () = bridge_shutdown.cancelled() => {}
        }
    });
}

/// 启动两层 task supervisor，并在退出时统一回收 turn 状态。
#[allow(
    clippy::too_many_arguments,
    reason = "Turn 启动阶段需要把状态、通道和依赖一次性交给 supervisor。"
)]
fn spawn_turn_supervisor(
    control: Arc<Mutex<AgentControl>>,
    runtime: Arc<AsyncMutex<crate::application::session_service::SessionRuntime>>,
    session_id: String,
    turn_id: String,
    input: UserInput,
    skill_injections: Vec<Message>,
    deps: TurnEngineDeps,
    event_tx: mpsc::Sender<crate::domain::AgentEvent>,
    outcome_tx: oneshot::Sender<crate::domain::TurnOutcome>,
    turn_finished_tx: oneshot::Sender<()>,
    turn_cancel_token: CancellationToken,
    bridge_shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let panic_event_tx = event_tx.clone();
        let inner_handle = tokio::spawn(turn_engine::run_turn(
            Arc::clone(&runtime),
            deps,
            turn_id.clone(),
            input,
            skill_injections,
            event_tx,
            turn_cancel_token.clone(),
        ));

        let outcome = match inner_handle.await {
            Ok(outcome) => outcome,
            Err(join_error) if join_error.is_panic() => {
                emit_panic_event(&runtime, &panic_event_tx, &turn_id).await;
                crate::domain::TurnOutcome::Panicked
            }
            Err(join_error) => crate::domain::TurnOutcome::Failed(AgentError::InternalPanic {
                message: format!("turn task cancelled unexpectedly: {join_error}"),
            }),
        };

        bridge_shutdown.cancel();
        let _ = outcome_tx.send(outcome);
        clear_running_turn(&control, &session_id);
        let _ = turn_finished_tx.send(());
    });
}

/// 在 panic 场景下尽力发出一条内部错误事件。
async fn emit_panic_event(
    runtime: &Arc<AsyncMutex<crate::application::session_service::SessionRuntime>>,
    event_tx: &mpsc::Sender<crate::domain::AgentEvent>,
    turn_id: &str,
) {
    let mut runtime = runtime.lock().await;
    let event = crate::domain::AgentEvent {
        session_id: runtime.session_id.clone(),
        turn_id: turn_id.to_string(),
        timestamp: chrono::Utc::now(),
        sequence: runtime.event_sequence,
        payload: crate::domain::AgentEventPayload::Error(AgentError::InternalPanic {
            message: "background turn task panicked".to_string(),
        }),
    };
    runtime.event_sequence = runtime.event_sequence.saturating_add(1);
    let _ = event_tx.send(event).await;
}

/// 在 supervisor 退出后统一归位 turn 状态。
fn clear_running_turn(control: &Arc<Mutex<AgentControl>>, session_id: &str) {
    let Ok(mut control) = control.lock() else {
        return;
    };

    let crate::application::session_service::SessionSlotState::Active(active) = &mut control.slot
    else {
        return;
    };

    if active.session_id == session_id {
        active.turn_state = crate::application::session_service::TurnState::Idle;
    }
}

/// 对外输入的同步校验。
fn validate_user_input(input: &UserInput, capabilities: ModelCapabilities) -> Result<()> {
    if input.content.is_empty() {
        return Err(AgentError::InputValidation {
            message: "user input must contain at least one content block".to_string(),
        });
    }

    let mut has_meaningful_content = false;

    for block in &input.content {
        match block {
            ContentBlock::Text(text) if !text.trim().is_empty() => {
                has_meaningful_content = true;
            }
            ContentBlock::Image { data, media_type } => {
                if !capabilities.vision {
                    return Err(AgentError::InputValidation {
                        message: "current model does not support image input".to_string(),
                    });
                }
                if data.trim().is_empty() || media_type.trim().is_empty() {
                    return Err(AgentError::InputValidation {
                        message: "image input must contain non-empty data and media_type"
                            .to_string(),
                    });
                }
                has_meaningful_content = true;
            }
            ContentBlock::File {
                name,
                media_type,
                text,
            } => {
                if name.trim().is_empty() || media_type.trim().is_empty() || text.trim().is_empty()
                {
                    return Err(AgentError::InputValidation {
                        message: "file input must contain non-empty name, media_type and text"
                            .to_string(),
                    });
                }
                has_meaningful_content = true;
            }
            ContentBlock::Text(_) => {}
        }
    }

    if !has_meaningful_content {
        return Err(AgentError::InputValidation {
            message: "user input must contain non-empty content".to_string(),
        });
    }

    Ok(())
}
