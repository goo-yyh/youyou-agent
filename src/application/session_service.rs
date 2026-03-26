//! 会话生命周期应用服务。

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::anyhow;
use chrono::Utc;
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::application::context_manager::{ContextManager, payload_to_message};
use crate::application::hook_registry::HookRegistry;
use crate::application::memory_manager::{IncrementalExtraction, MemoryManager};
use crate::domain::{
    AgentConfig, AgentError, CompactionMarker, HookData, LedgerEvent, LedgerEventPayload, Memory,
    MetadataKey, Result, SessionConfig, SessionLedger,
};
use crate::ports::{
    MemoryStorage, ModelProvider, Plugin, SessionPage, SessionSearchQuery, SessionStorage,
    SessionSummary,
};

/// 运行期模型目录需要提供的最小能力。
pub(crate) trait ModelCatalog {
    /// 根据模型标识解析上下文窗口大小。
    fn resolve_context_window(&self, model_id: &str) -> Result<usize>;

    /// 根据模型标识解析其所属 provider。
    fn resolve_provider(&self, model_id: &str) -> Result<Arc<dyn ModelProvider>>;
}

/// Agent 生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleState {
    /// Agent 可以接收外部操作。
    Running,
    /// Agent 正在关闭过程中。
    ShuttingDown,
    /// Agent 已完全关闭。
    Shutdown,
}

/// 运行中 turn 的内部句柄。
pub(crate) struct RunningTurnHandle {
    /// 当前 turn 的取消令牌。
    pub(crate) turn_cancel_token: CancellationToken,
    /// turn 结束通知。
    pub(crate) turn_finished_rx: Option<oneshot::Receiver<()>>,
}

impl fmt::Debug for RunningTurnHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunningTurnHandle")
            .field("has_turn_finished_rx", &self.turn_finished_rx.is_some())
            .finish_non_exhaustive()
    }
}

/// 当前会话的 turn 运行状态。
#[allow(
    dead_code,
    reason = "Phase 2 预留了运行中 turn 状态，Phase 4 才会真正启动 turn loop。"
)]
#[derive(Debug)]
pub(crate) enum TurnState {
    /// 当前没有正在运行的 turn。
    Idle,
    /// 当前有一个正在运行的 turn。
    Running(RunningTurnHandle),
    /// 当前 session 正在关闭，禁止启动新的 turn。
    Closing,
}

/// 活跃 session 的控制面状态。
pub(crate) struct ActiveSessionState {
    /// 活跃会话标识。
    pub(crate) session_id: String,
    /// 会话级取消令牌。
    pub(crate) session_cancel_token: CancellationToken,
    /// 当前 turn 状态。
    pub(crate) turn_state: TurnState,
    /// 会话运行时状态。
    pub(crate) runtime: Arc<AsyncMutex<SessionRuntime>>,
}

impl fmt::Debug for ActiveSessionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveSessionState")
            .field("session_id", &self.session_id)
            .field("turn_state", &self.turn_state)
            .finish_non_exhaustive()
    }
}

/// 单会话槽位状态。
#[derive(Debug)]
pub(crate) enum SessionSlotState {
    /// 当前没有会话。
    Empty,
    /// 某个会话正在创建或恢复过程中。
    Reserved {
        /// 本次预留的唯一标识。
        reservation_id: String,
        /// 预留给哪个 session id。
        session_id: String,
    },
    /// 当前活跃的会话。
    Active(ActiveSessionState),
}

/// Agent 控制平面状态。
#[derive(Debug)]
pub(crate) struct AgentControl {
    /// Agent 生命周期。
    pub(crate) lifecycle: LifecycleState,
    /// 单会话槽位。
    pub(crate) slot: SessionSlotState,
}

impl Default for AgentControl {
    fn default() -> Self {
        Self {
            lifecycle: LifecycleState::Running,
            slot: SessionSlotState::Empty,
        }
    }
}

/// 会话运行时状态。
#[allow(
    dead_code,
    reason = "Phase 2 先固化可恢复的运行时字段，后续 phase 会逐步读写这些状态。"
)]
#[derive(Debug)]
pub(crate) struct SessionRuntime {
    /// 会话标识。
    pub(crate) session_id: String,
    /// 当前锁定的模型标识。
    pub(crate) model_id: String,
    /// 会话级 system prompt 覆盖文本。
    pub(crate) system_prompt_override: Option<String>,
    /// 会话绑定的记忆命名空间。
    pub(crate) memory_namespace: String,
    /// 内存账本视图。
    pub(crate) ledger: SessionLedger,
    /// 模型可见上下文投影。
    pub(crate) context_manager: ContextManager,
    /// 对外事件序号计数器。
    pub(crate) event_sequence: u64,
    /// 当前 turn 编号。
    pub(crate) turn_index: u64,
    /// 最近一次记忆 checkpoint 覆盖到的 ledger seq。
    pub(crate) last_memory_checkpoint_seq: u64,
    /// 启动时加载的 bootstrap 记忆。
    pub(crate) bootstrap_memories: Vec<Memory>,
}

impl SessionRuntime {
    /// 创建一个新会话的运行时状态。
    pub(crate) fn new(
        session_id: String,
        model_id: String,
        system_prompt_override: Option<String>,
        memory_namespace: String,
        context_window: usize,
        compact_threshold: f64,
        bootstrap_memories: Vec<Memory>,
    ) -> Self {
        Self {
            session_id,
            model_id,
            system_prompt_override,
            memory_namespace,
            ledger: SessionLedger::new(),
            context_manager: ContextManager::new(context_window, compact_threshold),
            event_sequence: 1,
            turn_index: 0,
            last_memory_checkpoint_seq: 0,
            bootstrap_memories,
        }
    }

    /// 从已持久化账本恢复运行时状态。
    ///
    /// # Errors
    ///
    /// 当账本中的上下文相关 metadata 无法解析时返回错误。
    fn from_ledger(restore: SessionRuntimeRestore) -> Result<Self> {
        let context_manager = ContextManager::rebuild_from_ledger(
            &restore.ledger,
            restore.context_window,
            restore.compact_threshold,
        )?;

        Ok(Self {
            session_id: restore.session_id,
            model_id: restore.model_id,
            system_prompt_override: restore.system_prompt_override,
            memory_namespace: restore.memory_namespace,
            ledger: restore.ledger,
            context_manager,
            event_sequence: 1,
            turn_index: restore.turn_index,
            last_memory_checkpoint_seq: restore.last_memory_checkpoint_seq,
            bootstrap_memories: restore.bootstrap_memories,
        })
    }

    /// 为新账本事件分配序号和时间戳。
    #[must_use]
    pub(crate) fn allocate_event(&mut self, payload: LedgerEventPayload) -> LedgerEvent {
        LedgerEvent {
            seq: self.ledger.next_seq(),
            timestamp: Utc::now(),
            payload,
        }
    }

    /// 统计当前消息事件数量。
    #[must_use]
    pub(crate) fn message_count(&self) -> usize {
        self.ledger
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.payload,
                    LedgerEventPayload::UserMessage { .. }
                        | LedgerEventPayload::AssistantMessage { .. }
                )
            })
            .count()
    }
}

/// 创建或恢复完成后返回给 API 层的会话快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionDescriptor {
    /// 会话标识。
    pub(crate) session_id: String,
    /// 当前模型标识。
    pub(crate) model_id: String,
    /// 会话绑定的记忆命名空间。
    pub(crate) memory_namespace: String,
    /// 会话级 system prompt 覆盖文本。
    pub(crate) system_prompt_override: Option<String>,
}

/// 会话生命周期服务。
pub(crate) struct SessionService<'a, M> {
    /// 全局静态配置。
    pub(crate) agent_config: &'a AgentConfig,
    /// 模型目录。
    pub(crate) models: &'a M,
    /// Hook 注册表。
    pub(crate) hook_registry: &'a HookRegistry,
    /// Agent 控制平面。
    pub(crate) control: Arc<Mutex<AgentControl>>,
    /// 可选会话存储。
    pub(crate) session_storage: Option<Arc<dyn SessionStorage>>,
    /// 可选记忆存储。
    pub(crate) memory_storage: Option<Arc<dyn MemoryStorage>>,
}

impl<'a, M> SessionService<'a, M>
where
    M: ModelCatalog,
{
    /// 创建会话生命周期服务。
    #[must_use]
    pub(crate) fn new(
        agent_config: &'a AgentConfig,
        models: &'a M,
        hook_registry: &'a HookRegistry,
        control: Arc<Mutex<AgentControl>>,
        session_storage: Option<Arc<dyn SessionStorage>>,
        memory_storage: Option<Arc<dyn MemoryStorage>>,
    ) -> Self {
        Self {
            agent_config,
            models,
            hook_registry,
            control,
            session_storage,
            memory_storage,
        }
    }

    /// 新建一个会话。
    ///
    /// # 错误
    ///
    /// 当会话槽位已占用、模型非法、hook 中止或持久化失败时返回错误。
    pub(crate) async fn new_session(&self, config: SessionConfig) -> Result<SessionDescriptor> {
        self.ensure_running()?;

        let session_id = Uuid::new_v4().to_string();
        let reservation_id = self.claim_session_slot(session_id.clone())?;
        let result = self.new_session_inner(session_id, config).await;

        if result.is_err() {
            self.rollback_reservation(&reservation_id)?;
        }

        let (runtime, descriptor) = result?;
        self.activate_session(&reservation_id, runtime)?;
        Ok(descriptor)
    }

    /// 恢复一个已持久化会话。
    ///
    /// # 错误
    ///
    /// 当未注册存储、会话不存在、账本损坏或槽位已占用时返回错误。
    pub(crate) async fn resume_session(&self, session_id: &str) -> Result<SessionDescriptor> {
        self.ensure_running()?;

        let storage = self.require_session_storage()?;
        let reservation_id = self.claim_session_slot(session_id.to_string())?;
        let result = self.resume_session_inner(storage, session_id).await;

        if result.is_err() {
            self.rollback_reservation(&reservation_id)?;
        }

        let (runtime, descriptor) = result?;
        self.activate_session(&reservation_id, runtime)?;
        Ok(descriptor)
    }

    /// 关闭指定会话。
    ///
    /// # 错误
    ///
    /// 当句柄对应的会话已失效，或 `SessionEnd` hook 中止关闭时返回错误。
    pub(crate) async fn close_session(&self, session_id: &str) -> Result<()> {
        let (runtime, running_turn) = self.begin_close(session_id)?;

        if let Some(mut turn_handle) = running_turn {
            turn_handle.turn_cancel_token.cancel();
            if let Some(turn_finished_rx) = turn_handle.turn_finished_rx.take() {
                let _ = turn_finished_rx.await;
            }
        }

        let message_count = runtime.lock().await.message_count();
        self.hook_registry
            .dispatch(
                crate::domain::HookEvent::SessionEnd,
                session_id,
                None,
                HookData::SessionEnd { message_count },
            )
            .await
            .inspect_err(|_error| {
                let _ = self.abort_close(session_id);
            })?;

        self.run_close_memory_extraction(&runtime, session_id).await;
        self.finish_close(session_id)
    }

    /// 关闭 agent。
    ///
    /// # 错误
    ///
    /// 当活跃会话在关闭期间被 hook 中止时返回错误。
    pub(crate) async fn shutdown(&self, plugins: &[Arc<dyn Plugin>]) -> Result<()> {
        let active_session_id = {
            let mut control = self.lock_control()?;
            match control.lifecycle {
                LifecycleState::Shutdown | LifecycleState::ShuttingDown => return Ok(()),
                LifecycleState::Running => {
                    control.lifecycle = LifecycleState::ShuttingDown;
                }
            }

            match &control.slot {
                SessionSlotState::Active(active) => Some(active.session_id.clone()),
                SessionSlotState::Empty | SessionSlotState::Reserved { .. } => None,
            }
        };

        if let Some(session_id) = active_session_id
            && let Err(error) = self.close_session(&session_id).await
        {
            let mut control = self.lock_control()?;
            control.lifecycle = LifecycleState::Running;
            return Err(error);
        }

        for plugin in plugins.iter().rev() {
            if let Err(error) = plugin.shutdown().await {
                let descriptor = plugin.descriptor();
                warn!(
                    plugin_id = %descriptor.id,
                    error = %error,
                    "plugin shutdown failed during agent shutdown",
                );
            }
        }

        let mut control = self.lock_control()?;
        control.lifecycle = LifecycleState::Shutdown;
        control.slot = SessionSlotState::Empty;
        Ok(())
    }

    /// 列出会话。
    ///
    /// # 错误
    ///
    /// 当未注册会话存储或存储访问失败时返回错误。
    pub(crate) async fn list_sessions(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<SessionPage> {
        self.ensure_running()?;
        let storage = self.require_session_storage()?;
        storage.list_sessions(cursor, limit).await.map_err(|error| {
            AgentError::StorageError(error.context("failed to list sessions from session storage"))
        })
    }

    /// 搜索会话。
    ///
    /// # 错误
    ///
    /// 当未注册会话存储或存储访问失败时返回错误。
    pub(crate) async fn find_sessions(
        &self,
        query: &SessionSearchQuery,
    ) -> Result<Vec<SessionSummary>> {
        self.ensure_running()?;
        let storage = self.require_session_storage()?;
        storage.find_sessions(query).await.map_err(|error| {
            AgentError::StorageError(
                error.context("failed to search sessions from session storage"),
            )
        })
    }

    /// 删除指定会话。
    ///
    /// # 错误
    ///
    /// 当目标会话当前仍处于活跃状态，或未注册会话存储时返回错误。
    pub(crate) async fn delete_session(&self, session_id: &str) -> Result<()> {
        self.ensure_running()?;
        self.ensure_session_not_active(session_id)?;

        let storage = self.require_session_storage()?;
        storage.delete_session(session_id).await.map_err(|error| {
            AgentError::StorageError(
                error.context(format!("failed to delete session '{session_id}'")),
            )
        })
    }

    /// 执行 `new_session` 的核心逻辑。
    async fn new_session_inner(
        &self,
        session_id: String,
        config: SessionConfig,
    ) -> Result<(Arc<AsyncMutex<SessionRuntime>>, SessionDescriptor)> {
        let model_id = config
            .model_id
            .clone()
            .unwrap_or_else(|| self.agent_config.default_model.clone());
        let context_window = self.models.resolve_context_window(&model_id)?;
        let memory_namespace = self.agent_config.memory_namespace.clone();
        let bootstrap_memories = self.load_bootstrap_memories(&memory_namespace).await?;
        let mut runtime = SessionRuntime::new(
            session_id.clone(),
            model_id.clone(),
            config.system_prompt_override.clone(),
            memory_namespace.clone(),
            context_window,
            self.agent_config.compact_threshold,
            bootstrap_memories,
        );

        self.hook_registry
            .dispatch(
                crate::domain::HookEvent::SessionStart,
                &session_id,
                None,
                HookData::SessionStart {
                    model_id: model_id.clone(),
                },
            )
            .await?;

        let resolved_config = SessionConfig {
            model_id: Some(model_id.clone()),
            system_prompt_override: config.system_prompt_override.clone(),
        };
        self.persist_session_bootstrap_metadata(&mut runtime, &resolved_config, &memory_namespace)
            .await?;

        let descriptor = SessionDescriptor {
            session_id,
            model_id,
            memory_namespace,
            system_prompt_override: config.system_prompt_override,
        };

        Ok((Arc::new(AsyncMutex::new(runtime)), descriptor))
    }

    /// 执行 `resume_session` 的核心逻辑。
    async fn resume_session_inner(
        &self,
        storage: Arc<dyn SessionStorage>,
        session_id: &str,
    ) -> Result<(Arc<AsyncMutex<SessionRuntime>>, SessionDescriptor)> {
        let events = storage
            .load_session(session_id)
            .await
            .map_err(|error| {
                AgentError::StorageError(
                    error.context(format!("failed to load session '{session_id}'")),
                )
            })?
            .ok_or_else(|| AgentError::SessionNotFound(session_id.to_string()))?;
        let ledger = SessionLedger::from_events(events);
        let restored = self.restore_session_state(&ledger)?;
        let context_window = self.models.resolve_context_window(&restored.model_id)?;
        let bootstrap_memories = self
            .load_bootstrap_memories(&restored.memory_namespace)
            .await?;
        let runtime = SessionRuntime::from_ledger(SessionRuntimeRestore {
            session_id: session_id.to_string(),
            model_id: restored.model_id.clone(),
            system_prompt_override: restored.system_prompt_override.clone(),
            memory_namespace: restored.memory_namespace.clone(),
            ledger,
            context_window,
            compact_threshold: self.agent_config.compact_threshold,
            last_memory_checkpoint_seq: restored.last_memory_checkpoint_seq,
            turn_index: restored.turn_index,
            bootstrap_memories,
        })?;

        let descriptor = SessionDescriptor {
            session_id: session_id.to_string(),
            model_id: restored.model_id,
            memory_namespace: restored.memory_namespace,
            system_prompt_override: restored.system_prompt_override,
        };

        Ok((Arc::new(AsyncMutex::new(runtime)), descriptor))
    }

    /// 将新会话必需的 metadata 持久化到账本。
    async fn persist_session_bootstrap_metadata(
        &self,
        runtime: &mut SessionRuntime,
        session_config: &SessionConfig,
        memory_namespace: &str,
    ) -> Result<()> {
        let session_config_value = serde_json::to_value(session_config).map_err(|error| {
            AgentError::StorageError(anyhow!(error).context("failed to serialize session config"))
        })?;
        persist_and_project(
            runtime,
            self.session_storage.as_deref(),
            LedgerEventPayload::Metadata {
                key: MetadataKey::SessionConfig,
                value: session_config_value,
            },
        )
        .await?;

        persist_and_project(
            runtime,
            self.session_storage.as_deref(),
            LedgerEventPayload::Metadata {
                key: MetadataKey::MemoryNamespace,
                value: serde_json::Value::String(memory_namespace.to_string()),
            },
        )
        .await?;

        Ok(())
    }

    /// 加载 session 启动时需要注入的 bootstrap 记忆。
    async fn load_bootstrap_memories(&self, namespace: &str) -> Result<Vec<Memory>> {
        let Some(memory_manager) = self.memory_manager() else {
            return Ok(Vec::new());
        };

        memory_manager.list_recent(namespace).await
    }

    /// 在 close 路径上执行收尾提取，失败只记录 warn。
    async fn run_close_memory_extraction(
        &self,
        runtime: &Arc<AsyncMutex<SessionRuntime>>,
        session_id: &str,
    ) {
        let Some(memory_manager) = self.memory_manager() else {
            return;
        };

        let mut runtime = runtime.lock().await;
        let memory_model_id = self.resolve_memory_model_id(&runtime.model_id);
        let provider = match self.models.resolve_provider(&memory_model_id) {
            Ok(provider) => provider,
            Err(error) => {
                warn!(
                    session_id = %session_id,
                    error = %error,
                    "failed to resolve memory provider during session close"
                );
                return;
            }
        };
        let source = format!("session:{session_id}");
        let close_cancel = CancellationToken::new();
        let close_timeout = Duration::from_millis(self.agent_config.tool_timeout_ms);

        match tokio::time::timeout(
            close_timeout,
            memory_manager.extract_incremental(IncrementalExtraction {
                namespace: &runtime.memory_namespace,
                ledger: &runtime.ledger,
                last_checkpoint_seq: runtime.last_memory_checkpoint_seq,
                source: &source,
                provider: provider.as_ref(),
                model_id: &memory_model_id,
                cancel: &close_cancel,
            }),
        )
        .await
        {
            Ok(Ok(Some(last_seq))) => {
                if let Err(error) = persist_memory_checkpoint_metadata(
                    &mut runtime,
                    self.session_storage.as_deref(),
                    last_seq,
                )
                .await
                {
                    warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to persist memory checkpoint during session close"
                    );
                }
            }
            Ok(Ok(None) | Err(AgentError::RequestCancelled)) => {}
            Ok(Err(error)) => {
                warn!(
                    session_id = %session_id,
                    error = %error,
                    "memory close extraction failed; skipping without blocking close"
                );
            }
            Err(_elapsed) => {
                close_cancel.cancel();
                warn!(
                    session_id = %session_id,
                    timeout_ms = self.agent_config.tool_timeout_ms,
                    "memory close extraction timed out; skipping without blocking close"
                );
            }
        }
    }

    /// 从账本中恢复会话关键配置。
    fn restore_session_state(&self, ledger: &SessionLedger) -> Result<RestoredSessionState> {
        let session_config = ledger
            .latest_metadata(&MetadataKey::SessionConfig)
            .map(|value| {
                serde_json::from_value::<SessionConfig>(value.clone()).map_err(|error| {
                    AgentError::StorageError(
                        anyhow!(error).context("failed to deserialize session config metadata"),
                    )
                })
            })
            .transpose()?
            .unwrap_or_default();

        let memory_namespace =
            if let Some(value) = ledger.latest_metadata(&MetadataKey::MemoryNamespace) {
                serde_json::from_value::<String>(value.clone()).map_err(|error| {
                    AgentError::StorageError(
                        anyhow!(error).context("failed to deserialize memory namespace metadata"),
                    )
                })?
            } else {
                warn!("memory namespace metadata missing, falling back to agent config");
                self.agent_config.memory_namespace.clone()
            };

        let last_memory_checkpoint_seq = ledger
            .latest_metadata(&MetadataKey::MemoryCheckpoint)
            .map(|value| {
                serde_json::from_value::<MemoryCheckpointMetadata>(value.clone()).map_err(|error| {
                    AgentError::StorageError(
                        anyhow!(error).context("failed to deserialize memory checkpoint metadata"),
                    )
                })
            })
            .transpose()?
            .map_or(0, |metadata| metadata.last_seq);

        let model_id = session_config
            .model_id
            .clone()
            .unwrap_or_else(|| self.agent_config.default_model.clone());
        let turn_index = ledger
            .events()
            .iter()
            .filter(|event| matches!(event.payload, LedgerEventPayload::UserMessage { .. }))
            .count() as u64;

        Ok(RestoredSessionState {
            model_id,
            system_prompt_override: session_config.system_prompt_override,
            memory_namespace,
            last_memory_checkpoint_seq,
            turn_index,
        })
    }

    /// 在控制面中预留 session 槽位。
    fn claim_session_slot(&self, session_id: String) -> Result<String> {
        let mut control = self.lock_control()?;
        match control.lifecycle {
            LifecycleState::Shutdown | LifecycleState::ShuttingDown => {
                return Err(AgentError::AgentShutdown);
            }
            LifecycleState::Running => {}
        }

        match control.slot {
            SessionSlotState::Empty => {
                let reservation_id = Uuid::new_v4().to_string();
                control.slot = SessionSlotState::Reserved {
                    reservation_id: reservation_id.clone(),
                    session_id,
                };
                Ok(reservation_id)
            }
            SessionSlotState::Reserved { .. } | SessionSlotState::Active(_) => {
                Err(AgentError::SessionBusy)
            }
        }
    }

    /// 失败时按 reservation id 回滚预留槽位。
    fn rollback_reservation(&self, reservation_id: &str) -> Result<()> {
        let mut control = self.lock_control()?;
        if matches!(
            &control.slot,
            SessionSlotState::Reserved {
                reservation_id: current_id,
                ..
            } if current_id == reservation_id
        ) {
            control.slot = SessionSlotState::Empty;
        }

        Ok(())
    }

    /// 将预留槽位提交为活跃会话。
    fn activate_session(
        &self,
        reservation_id: &str,
        runtime: Arc<AsyncMutex<SessionRuntime>>,
    ) -> Result<()> {
        let mut control = self.lock_control()?;
        let session_id = {
            let SessionSlotState::Reserved {
                reservation_id: current_id,
                session_id,
            } = &control.slot
            else {
                return Err(AgentError::SessionBusy);
            };

            if current_id != reservation_id {
                return Err(AgentError::SessionBusy);
            }

            session_id.clone()
        };

        control.slot = SessionSlotState::Active(ActiveSessionState {
            session_id,
            session_cancel_token: CancellationToken::new(),
            turn_state: TurnState::Idle,
            runtime,
        });

        Ok(())
    }

    /// 将当前活跃 session 切换到 closing 状态，并返回 close 所需上下文。
    fn begin_close(
        &self,
        session_id: &str,
    ) -> Result<(Arc<AsyncMutex<SessionRuntime>>, Option<RunningTurnHandle>)> {
        let mut control = self.lock_control()?;
        let SessionSlotState::Active(active) = &mut control.slot else {
            return Err(AgentError::SessionNotFound(session_id.to_string()));
        };

        if active.session_id != session_id {
            return Err(AgentError::SessionNotFound(session_id.to_string()));
        }

        let runtime = Arc::clone(&active.runtime);
        let running_turn = match std::mem::replace(&mut active.turn_state, TurnState::Closing) {
            TurnState::Idle => None,
            TurnState::Running(handle) => Some(handle),
            TurnState::Closing => return Err(AgentError::SessionBusy),
        };

        Ok((runtime, running_turn))
    }

    /// 在 close 流程被 hook 中止后恢复 session 的可运行状态。
    fn abort_close(&self, session_id: &str) -> Result<()> {
        let mut control = self.lock_control()?;
        let SessionSlotState::Active(active) = &mut control.slot else {
            return Err(AgentError::SessionNotFound(session_id.to_string()));
        };

        if active.session_id != session_id {
            return Err(AgentError::SessionNotFound(session_id.to_string()));
        }

        if matches!(active.turn_state, TurnState::Closing) {
            active.turn_state = TurnState::Idle;
        }

        Ok(())
    }

    /// close 成功后提交槽位释放。
    fn finish_close(&self, session_id: &str) -> Result<()> {
        let mut control = self.lock_control()?;
        match &control.slot {
            SessionSlotState::Active(active)
                if active.session_id == session_id
                    && matches!(active.turn_state, TurnState::Closing) =>
            {
                control.slot = SessionSlotState::Empty;
                Ok(())
            }
            SessionSlotState::Active(active) if active.session_id == session_id => {
                Err(AgentError::SessionBusy)
            }
            _ => Err(AgentError::SessionNotFound(session_id.to_string())),
        }
    }

    /// 确保外部可见操作只能在运行态使用。
    fn ensure_running(&self) -> Result<()> {
        let control = self.lock_control()?;
        match control.lifecycle {
            LifecycleState::Running => Ok(()),
            LifecycleState::ShuttingDown | LifecycleState::Shutdown => {
                Err(AgentError::AgentShutdown)
            }
        }
    }

    /// 确保指定 session 当前不是活跃会话。
    fn ensure_session_not_active(&self, session_id: &str) -> Result<()> {
        let control = self.lock_control()?;
        match &control.slot {
            SessionSlotState::Active(active) if active.session_id == session_id => {
                Err(AgentError::SessionBusy)
            }
            _ => Ok(()),
        }
    }

    /// 获取会话存储，未注册时返回统一错误。
    fn require_session_storage(&self) -> Result<Arc<dyn SessionStorage>> {
        self.session_storage
            .clone()
            .ok_or_else(|| AgentError::StorageError(anyhow!("SessionStorage not registered")))
    }

    /// 获取控制面锁，并将 poison 统一转换为结构化错误。
    fn lock_control(&self) -> Result<MutexGuard<'_, AgentControl>> {
        self.control
            .lock()
            .map_err(|error| AgentError::InternalPanic {
                message: format!("agent control mutex poisoned: {error}"),
            })
    }

    /// 按当前配置构造一个记忆管理器。
    fn memory_manager(&self) -> Option<MemoryManager> {
        self.memory_storage
            .clone()
            .map(|storage| MemoryManager::new(storage, self.agent_config.memory_max_items))
    }

    /// 解析当前会话应使用的记忆模型标识。
    fn resolve_memory_model_id(&self, session_model_id: &str) -> String {
        self.agent_config
            .memory_model
            .clone()
            .unwrap_or_else(|| session_model_id.to_string())
    }
}

/// 统一的关键事件写入路径。
///
/// # 错误
///
/// 当持久化失败或 metadata 投影无法更新时返回错误。
pub(crate) async fn persist_and_project(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    payload: LedgerEventPayload,
) -> Result<LedgerEvent> {
    let event = runtime.allocate_event(payload);

    if let Some(storage) = storage {
        storage
            .save_event(&runtime.session_id, event.clone())
            .await
            .map_err(|error| {
                AgentError::StorageError(error.context(format!(
                    "failed to save ledger event {} for session '{}'",
                    event.seq, runtime.session_id
                )))
            })?;
    }

    runtime.ledger.append(event.clone());
    project_ledger_payload(runtime, &event.payload)?;
    Ok(event)
}

/// 持久化一条记忆 checkpoint metadata，并同步更新运行时边界。
///
/// # 错误
///
/// 当 metadata 无法持久化时返回错误。
pub(crate) async fn persist_memory_checkpoint_metadata(
    runtime: &mut SessionRuntime,
    storage: Option<&dyn SessionStorage>,
    last_seq: u64,
) -> Result<LedgerEvent> {
    persist_and_project(
        runtime,
        storage,
        LedgerEventPayload::Metadata {
            key: MetadataKey::MemoryCheckpoint,
            value: serde_json::json!({ "lastSeq": last_seq }),
        },
    )
    .await
}

/// 将已落盘事件投影到运行时状态。
fn project_ledger_payload(
    runtime: &mut SessionRuntime,
    payload: &LedgerEventPayload,
) -> Result<()> {
    if let Some(message) = payload_to_message(payload) {
        runtime.context_manager.push(message);
        return Ok(());
    }

    if let LedgerEventPayload::Metadata { key, value } = payload {
        match key {
            MetadataKey::ContextCompaction => {
                let marker =
                    serde_json::from_value::<CompactionMarker>(value.clone()).map_err(|error| {
                        AgentError::StorageError(
                            anyhow!(error)
                                .context("failed to deserialize context compaction metadata"),
                        )
                    })?;
                runtime
                    .context_manager
                    .apply_compaction_marker(&runtime.ledger, marker);
            }
            MetadataKey::MemoryCheckpoint => {
                let checkpoint = serde_json::from_value::<MemoryCheckpointMetadata>(value.clone())
                    .map_err(|error| {
                        AgentError::StorageError(
                            anyhow!(error)
                                .context("failed to deserialize memory checkpoint metadata"),
                        )
                    })?;
                runtime.last_memory_checkpoint_seq = checkpoint.last_seq;
            }
            MetadataKey::SessionConfig | MetadataKey::MemoryNamespace => {}
        }
    }

    Ok(())
}

/// 记忆 checkpoint metadata 结构。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoryCheckpointMetadata {
    /// 最近一次 checkpoint 覆盖到的 ledger seq。
    last_seq: u64,
}

/// 从账本中恢复出的会话关键状态。
#[derive(Debug)]
struct RestoredSessionState {
    /// 恢复出的模型标识。
    model_id: String,
    /// 恢复出的会话级 system prompt 覆盖文本。
    system_prompt_override: Option<String>,
    /// 恢复出的记忆命名空间。
    memory_namespace: String,
    /// 最近一次记忆 checkpoint 边界。
    last_memory_checkpoint_seq: u64,
    /// 通过用户消息数恢复的 turn 编号。
    turn_index: u64,
}

/// 从账本恢复 `SessionRuntime` 时使用的完整输入。
#[derive(Debug)]
struct SessionRuntimeRestore {
    /// 会话标识。
    session_id: String,
    /// 当前锁定的模型标识。
    model_id: String,
    /// 会话级 system prompt 覆盖文本。
    system_prompt_override: Option<String>,
    /// 会话绑定的记忆命名空间。
    memory_namespace: String,
    /// 已加载的完整账本。
    ledger: SessionLedger,
    /// 当前模型的上下文窗口。
    context_window: usize,
    /// 压缩阈值配置。
    compact_threshold: f64,
    /// 最近一次记忆 checkpoint 边界。
    last_memory_checkpoint_seq: u64,
    /// 通过账本恢复出的 turn 编号。
    turn_index: u64,
    /// 恢复时重新加载的 bootstrap 记忆。
    bootstrap_memories: Vec<Memory>,
}
