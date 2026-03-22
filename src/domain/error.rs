//! crate 内统一使用的结构化错误定义。

use anyhow::Error as AnyError;
use thiserror::Error;

/// crate 内部统一使用的结果类型。
pub type Result<T> = std::result::Result<T, AgentError>;

/// Agent 对外暴露的结构化错误。
#[derive(Debug, Error)]
pub enum AgentError {
    /// 构建阶段未注册任何模型提供方。
    #[error("[NO_MODEL_PROVIDER] at least one ModelProvider is required")]
    NoModelProvider,
    /// 必须唯一的名称被重复注册。
    #[error("[NAME_CONFLICT] {kind} name '{name}' is duplicated")]
    NameConflict {
        /// 发生冲突的资源类型。
        kind: &'static str,
        /// 重复的名称。
        name: String,
    },
    /// 某个 Skill 依赖了未注册的 Tool。
    #[error("[SKILL_DEPENDENCY_NOT_MET] skill '{skill}' requires tool '{tool}'")]
    SkillDependencyNotMet {
        /// Skill 名称。
        skill: String,
        /// 缺失的 Tool 名称。
        tool: String,
    },
    /// Plugin 初始化失败。
    #[error("[PLUGIN_INIT_FAILED] plugin '{id}': {source}")]
    PluginInitFailed {
        /// Plugin 标识。
        id: String,
        /// Plugin 初始化错误。
        #[source]
        source: AnyError,
    },
    /// Plugin 的 hook 注册或分发违反了契约。
    #[error("[PLUGIN_HOOK_CONTRACT_VIOLATION] plugin '{plugin_id}': {message}")]
    PluginHookContractViolation {
        /// Plugin 标识。
        plugin_id: String,
        /// 违反契约的说明信息。
        message: String,
    },
    /// 某类存储依赖被重复注册。
    #[error("[STORAGE_DUPLICATE] {kind} storage registered more than once")]
    StorageDuplicate {
        /// 重复的存储类型。
        kind: &'static str,
    },
    /// 配置的默认模型不存在于注册表中。
    #[error("[INVALID_DEFAULT_MODEL] default model '{0}' is not registered")]
    InvalidDefaultModel(String),
    /// 次级配置字段引用了无效模型。
    #[error("[INVALID_MODEL_CONFIG] {kind} model '{model_id}' is not registered")]
    InvalidModelConfig {
        /// 引用模型的配置字段。
        kind: &'static str,
        /// 缺失的模型标识。
        model_id: String,
    },
    /// 输入校验失败。
    #[error("[INPUT_VALIDATION] {message}")]
    InputValidation {
        /// 人类可读的校验错误信息。
        message: String,
    },
    /// 当前已有活跃会话。
    #[error("[SESSION_BUSY] a session is already running")]
    SessionBusy,
    /// 当前已有活跃 turn。
    #[error("[TURN_BUSY] a turn is already running in this session")]
    TurnBusy,
    /// 请求的模型不受支持。
    #[error("[MODEL_NOT_SUPPORTED] model '{0}' is not supported")]
    ModelNotSupported(String),
    /// Provider 操作失败。
    #[error("[PROVIDER_ERROR] {message}")]
    ProviderError {
        /// 面向用户的错误信息。
        message: String,
        /// 底层 provider 错误。
        #[source]
        source: AnyError,
        /// 是否可以安全重试。
        retryable: bool,
    },
    /// Tool 执行失败。
    #[error("[TOOL_EXECUTION_ERROR] tool '{name}': {source}")]
    ToolExecutionError {
        /// Tool 名称。
        name: String,
        /// 底层 Tool 错误。
        #[source]
        source: AnyError,
    },
    /// Tool 执行超时。
    #[error("[TOOL_TIMEOUT] tool '{name}' timed out after {timeout_ms}ms")]
    ToolTimeout {
        /// Tool 名称。
        name: String,
        /// 超时时间。
        timeout_ms: u64,
    },
    /// Tool 名称无法解析。
    #[error("[TOOL_NOT_FOUND] tool '{0}'")]
    ToolNotFound(String),
    /// Skill 名称无法解析。
    #[error("[SKILL_NOT_FOUND] skill '{0}'")]
    SkillNotFound(String),
    /// 会话标识无法解析。
    #[error("[SESSION_NOT_FOUND] session '{0}'")]
    SessionNotFound(String),
    /// 底层存储操作失败。
    #[error("[STORAGE_ERROR] {0}")]
    StorageError(#[source] AnyError),
    /// 单轮请求了过多 Tool 调用。
    #[error("[MAX_TOOL_CALLS_EXCEEDED] exceeded {limit} tool calls in one turn")]
    MaxToolCallsExceeded {
        /// 配置的 Tool 调用上限。
        limit: usize,
    },
    /// 上下文压缩失败。
    #[error("[COMPACT_ERROR] context compaction failed: {message}")]
    CompactError {
        /// 人类可读的压缩失败说明。
        message: String,
    },
    /// 某个 plugin 中止了当前操作。
    #[error("[PLUGIN_ABORTED] hook '{hook}' aborted: {reason}")]
    PluginAborted {
        /// 触发中止的 hook 名称。
        hook: &'static str,
        /// 中止原因。
        reason: String,
    },
    /// 请求被取消。
    #[error("[REQUEST_CANCELLED]")]
    RequestCancelled,
    /// 后台任务发生 panic。
    #[error("[INTERNAL_PANIC] background task panicked: {message}")]
    InternalPanic {
        /// Panic 摘要信息。
        message: String,
    },
    /// Agent 已关闭。
    #[error("[AGENT_SHUTDOWN] agent has been shut down")]
    AgentShutdown,
}

impl AgentError {
    /// 返回稳定的机器可读错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoModelProvider => "NO_MODEL_PROVIDER",
            Self::NameConflict { .. } => "NAME_CONFLICT",
            Self::SkillDependencyNotMet { .. } => "SKILL_DEPENDENCY_NOT_MET",
            Self::PluginInitFailed { .. } => "PLUGIN_INIT_FAILED",
            Self::PluginHookContractViolation { .. } => "PLUGIN_HOOK_CONTRACT_VIOLATION",
            Self::StorageDuplicate { .. } => "STORAGE_DUPLICATE",
            Self::InvalidDefaultModel(_) => "INVALID_DEFAULT_MODEL",
            Self::InvalidModelConfig { .. } => "INVALID_MODEL_CONFIG",
            Self::InputValidation { .. } => "INPUT_VALIDATION",
            Self::SessionBusy => "SESSION_BUSY",
            Self::TurnBusy => "TURN_BUSY",
            Self::ModelNotSupported(_) => "MODEL_NOT_SUPPORTED",
            Self::ProviderError { .. } => "PROVIDER_ERROR",
            Self::ToolExecutionError { .. } => "TOOL_EXECUTION_ERROR",
            Self::ToolTimeout { .. } => "TOOL_TIMEOUT",
            Self::ToolNotFound(_) => "TOOL_NOT_FOUND",
            Self::SkillNotFound(_) => "SKILL_NOT_FOUND",
            Self::SessionNotFound(_) => "SESSION_NOT_FOUND",
            Self::StorageError(_) => "STORAGE_ERROR",
            Self::MaxToolCallsExceeded { .. } => "MAX_TOOL_CALLS_EXCEEDED",
            Self::CompactError { .. } => "COMPACT_ERROR",
            Self::PluginAborted { .. } => "PLUGIN_ABORTED",
            Self::RequestCancelled => "REQUEST_CANCELLED",
            Self::InternalPanic { .. } => "INTERNAL_PANIC",
            Self::AgentShutdown => "AGENT_SHUTDOWN",
        }
    }

    /// 返回重试该失败操作是否可能成功。
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ProviderError {
                retryable: true,
                ..
            }
        )
    }

    /// 返回错误来源组件。
    #[must_use]
    pub fn source_component(&self) -> &'static str {
        match self {
            Self::ProviderError { .. } | Self::CompactError { .. } => "provider",
            Self::ToolExecutionError { .. } | Self::ToolTimeout { .. } | Self::ToolNotFound(_) => {
                "tool"
            }
            Self::PluginInitFailed { .. }
            | Self::PluginHookContractViolation { .. }
            | Self::PluginAborted { .. } => "plugin",
            Self::StorageError(_) => "storage",
            _ => "agent",
        }
    }
}
