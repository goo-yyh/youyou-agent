//! [`crate::AgentBuilder`] 返回的对外 Agent 外壳。

use std::fmt;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::application::hook_registry::HookRegistry;
use crate::domain::{AgentConfig, LifecycleState, SessionSlotState, SkillDefinition};
use crate::ports::{
    MemoryStorage, ModelInfo, ModelProvider, PluginDescriptor, SessionStorage, ToolDefinition,
    ToolHandler,
};

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
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .field("hook_registry", &self.hook_registry)
            .field("has_session_storage", &self.session_storage.is_some())
            .field("has_memory_storage", &self.memory_storage.is_some())
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct AgentControl {
    pub(crate) lifecycle: LifecycleState,
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
}
