//! [`crate::AgentBuilder`] 返回的对外 Agent 外壳。

use std::fmt;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::application::hook_registry::HookRegistry;
use crate::application::session_service::{AgentControl, ModelCatalog, SessionService};
use crate::domain::{AgentConfig, AgentError, Result, SkillDefinition};
use crate::ports::{
    MemoryStorage, ModelInfo, ModelProvider, Plugin, PluginDescriptor, SessionPage,
    SessionSearchQuery, SessionStorage, SessionSummary, ToolDefinition, ToolHandler,
};

use super::SessionHandle;

#[derive(Debug)]
pub(crate) struct RegisteredModel {
    pub(crate) provider_id: String,
    pub(crate) info: ModelInfo,
}

#[derive(Default)]
pub(crate) struct ModelRegistry {
    pub(crate) providers: IndexMap<String, Arc<dyn ModelProvider>>,
    pub(crate) models: IndexMap<String, RegisteredModel>,
}

impl fmt::Debug for ModelRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let models: Vec<_> = self
            .models
            .iter()
            .map(|(model_id, registered)| {
                (
                    model_id,
                    registered.provider_id.as_str(),
                    registered.info.context_window,
                )
            })
            .collect();

        formatter
            .debug_struct("ModelRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("models", &models)
            .finish()
    }
}

impl ModelRegistry {
    pub(crate) fn contains_model(&self, model_id: &str) -> bool {
        self.models.contains_key(model_id)
    }

    /// 解析给定模型的静态信息。
    pub(crate) fn resolve_model(&self, model_id: &str) -> Result<&RegisteredModel> {
        self.models
            .get(model_id)
            .ok_or_else(|| AgentError::ModelNotSupported(model_id.to_string()))
    }
}

impl ModelCatalog for ModelRegistry {
    fn resolve_context_window(&self, model_id: &str) -> Result<usize> {
        self.resolve_model(model_id)
            .map(|registered| registered.info.context_window)
    }
}

#[derive(Default)]
pub(crate) struct ToolRegistry {
    pub(crate) handlers: IndexMap<String, Arc<dyn ToolHandler>>,
    pub(crate) definitions: IndexMap<String, ToolDefinition>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .field("definitions", &self.definitions.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Default)]
pub(crate) struct SkillRegistry {
    pub(crate) skills: IndexMap<String, SkillDefinition>,
}

pub(crate) struct AgentKernel {
    pub(crate) config: AgentConfig,
    pub(crate) models: ModelRegistry,
    pub(crate) tools: ToolRegistry,
    pub(crate) skills: SkillRegistry,
    pub(crate) plugin_instances: Vec<Arc<dyn Plugin>>,
    pub(crate) plugins: IndexMap<String, PluginDescriptor>,
    pub(crate) hook_registry: HookRegistry,
    pub(crate) session_storage: Option<Arc<dyn SessionStorage>>,
    pub(crate) memory_storage: Option<Arc<dyn MemoryStorage>>,
}

impl fmt::Debug for AgentKernel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentKernel")
            .field("config", &self.config)
            .field("models", &self.models)
            .field("tools", &self.tools)
            .field("skills", &self.skills)
            .field("plugin_instances", &self.plugin_instances.len())
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .field("hook_registry", &self.hook_registry)
            .field("has_session_storage", &self.session_storage.is_some())
            .field("has_memory_storage", &self.memory_storage.is_some())
            .finish()
    }
}

/// `AgentBuilder::build()` 产出的不可变 Agent 外壳。
pub struct Agent {
    pub(crate) kernel: Arc<AgentKernel>,
    pub(crate) control: Arc<Mutex<AgentControl>>,
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let control = self.control.lock().ok();
        let lifecycle = control.as_ref().map(|control| control.lifecycle);
        let slot = control.as_ref().map(|control| &control.slot);

        formatter
            .debug_struct("Agent")
            .field(
                "models",
                &self.kernel.models.models.keys().collect::<Vec<_>>(),
            )
            .field(
                "tools",
                &self.kernel.tools.definitions.keys().collect::<Vec<_>>(),
            )
            .field(
                "skills",
                &self.kernel.skills.skills.keys().collect::<Vec<_>>(),
            )
            .field("plugins", &self.kernel.plugins.keys().collect::<Vec<_>>())
            .field("lifecycle", &lifecycle)
            .field("slot", &slot)
            .finish_non_exhaustive()
    }
}

impl Agent {
    pub(crate) fn new(kernel: AgentKernel) -> Self {
        Self {
            kernel: Arc::new(kernel),
            control: Arc::new(Mutex::new(AgentControl::default())),
        }
    }

    /// 新建一个会话。
    ///
    /// # Errors
    ///
    /// 当 session 槽位已被占用、模型非法或关键持久化失败时返回错误。
    pub async fn new_session(&self, config: crate::domain::SessionConfig) -> Result<SessionHandle> {
        let descriptor = self.session_service().new_session(config).await?;
        Ok(SessionHandle::new(
            Arc::clone(&self.kernel),
            Arc::clone(&self.control),
            descriptor,
        ))
    }

    /// 恢复一个已持久化的会话。
    ///
    /// # Errors
    ///
    /// 当未注册 `SessionStorage`、会话不存在或账本损坏时返回错误。
    pub async fn resume_session(&self, session_id: &str) -> Result<SessionHandle> {
        let descriptor = self.session_service().resume_session(session_id).await?;
        Ok(SessionHandle::new(
            Arc::clone(&self.kernel),
            Arc::clone(&self.control),
            descriptor,
        ))
    }

    /// 关闭当前 agent。
    ///
    /// # Errors
    ///
    /// 当活跃会话关闭被 hook 中止时返回错误。
    pub async fn shutdown(&self) -> Result<()> {
        self.session_service()
            .shutdown(&self.kernel.plugin_instances)
            .await
    }

    /// 分页列出已持久化会话。
    ///
    /// # Errors
    ///
    /// 当未注册 `SessionStorage` 或存储访问失败时返回错误。
    pub async fn list_sessions(&self, cursor: Option<&str>, limit: usize) -> Result<SessionPage> {
        self.session_service().list_sessions(cursor, limit).await
    }

    /// 搜索已持久化会话。
    ///
    /// # Errors
    ///
    /// 当未注册 `SessionStorage` 或存储访问失败时返回错误。
    pub async fn find_sessions(&self, query: &SessionSearchQuery) -> Result<Vec<SessionSummary>> {
        self.session_service().find_sessions(query).await
    }

    /// 删除一个已持久化会话。
    ///
    /// # Errors
    ///
    /// 当目标会话仍是当前活跃会话，或未注册 `SessionStorage` 时返回错误。
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        self.session_service().delete_session(session_id).await
    }

    /// 构造会话生命周期服务。
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
}
