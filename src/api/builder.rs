//! 用于构建 [`crate::Agent`] 的对外 Builder。

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::api::agent::{
    Agent, AgentKernel, ModelRegistry, RegisteredModel, SkillRegistry, ToolRegistry,
};
use crate::application::plugin_manager::{ConfiguredPlugin, PluginManager};
use crate::domain::{AgentConfig, AgentError, Result, SkillDefinition};
use crate::ports::{
    MemoryStorage, ModelProvider, Plugin, SessionStorage, ToolDefinition, ToolHandler,
};

#[derive(Debug, Clone, Copy, Default)]
struct StorageRegistration {
    count: usize,
}

/// 在尚未注册任何模型提供方之前使用的标记类型。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProvider;

/// 在至少注册了一个模型提供方之后使用的标记类型。
#[derive(Debug, Clone, Copy, Default)]
pub struct HasProvider;

/// [`crate::Agent`] 的构建器。
pub struct AgentBuilder<S = NoProvider> {
    config: AgentConfig,
    providers: Vec<Arc<dyn ModelProvider>>,
    tools: Vec<Arc<dyn ToolHandler>>,
    skills: Vec<SkillDefinition>,
    plugins: Vec<ConfiguredPlugin>,
    session_storage: Option<Arc<dyn SessionStorage>>,
    memory_storage: Option<Arc<dyn MemoryStorage>>,
    session_storage_registration: StorageRegistration,
    memory_storage_registration: StorageRegistration,
    _state: PhantomData<S>,
}

impl<S> fmt::Debug for AgentBuilder<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentBuilder")
            .field("config", &self.config)
            .field("providers", &self.providers.len())
            .field("tools", &self.tools.len())
            .field("skills", &self.skills.len())
            .field("plugins", &self.plugins.len())
            .field(
                "session_storage_registered",
                &self.session_storage_registration.count,
            )
            .field(
                "memory_storage_registered",
                &self.memory_storage_registration.count,
            )
            .field("has_session_storage", &self.session_storage.is_some())
            .field("has_memory_storage", &self.memory_storage.is_some())
            .finish()
    }
}

impl AgentBuilder<NoProvider> {
    /// 创建一个新的构建器。
    #[must_use]
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            providers: Vec::new(),
            tools: Vec::new(),
            skills: Vec::new(),
            plugins: Vec::new(),
            session_storage: None,
            memory_storage: None,
            session_storage_registration: StorageRegistration::default(),
            memory_storage_registration: StorageRegistration::default(),
            _state: PhantomData,
        }
    }
}

impl<S> AgentBuilder<S> {
    /// 注册一个模型提供方。
    #[must_use]
    pub fn register_model_provider(
        self,
        provider: impl ModelProvider + 'static,
    ) -> AgentBuilder<HasProvider> {
        let mut builder = self.into_state::<HasProvider>();
        builder.providers.push(Arc::new(provider));
        builder
    }

    /// 注册一个 Tool 实现。
    #[must_use]
    pub fn register_tool(mut self, tool: impl ToolHandler + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// 注册一个 Skill 定义。
    #[must_use]
    pub fn register_skill(mut self, skill: SkillDefinition) -> Self {
        self.skills.push(skill);
        self
    }

    /// 注册一个 Plugin 及其配置负载。
    #[must_use]
    pub fn register_plugin(
        mut self,
        plugin: impl Plugin + 'static,
        config: serde_json::Value,
    ) -> Self {
        self.plugins.push(ConfiguredPlugin {
            instance: Arc::new(plugin),
            config,
        });
        self
    }

    /// 注册一个会话存储适配器。
    #[must_use]
    pub fn register_session_storage(mut self, storage: impl SessionStorage + 'static) -> Self {
        self.session_storage = Some(Arc::new(storage));
        self.session_storage_registration.count =
            self.session_storage_registration.count.saturating_add(1);
        self
    }

    /// 注册一个记忆存储适配器。
    #[must_use]
    pub fn register_memory_storage(mut self, storage: impl MemoryStorage + 'static) -> Self {
        self.memory_storage = Some(Arc::new(storage));
        self.memory_storage_registration.count =
            self.memory_storage_registration.count.saturating_add(1);
        self
    }

    /// 构建不可变的 Agent 外壳。
    ///
    /// # Errors
    ///
    /// 当校验失败、plugin 初始化失败或 plugin hook 注册失败时，
    /// 返回结构化的 [`AgentError`]。
    pub async fn build(self) -> Result<Agent> {
        self.validate_storage_registration()?;
        self.validate_config_values()?;

        let models = build_model_registry(&self.providers)?;
        validate_model_config(&self.config, &models)?;
        let tools = build_tool_registry(&self.tools)?;
        let skills = build_skill_registry(&self.skills, &tools)?;
        let (plugin_instances, plugins, hook_registry) =
            PluginManager::new().build(self.plugins).await?;

        Ok(Agent::new(AgentKernel {
            config: self.config,
            models,
            tools,
            skills,
            plugin_instances,
            plugins,
            hook_registry,
            session_storage: self.session_storage,
            memory_storage: self.memory_storage,
        }))
    }

    fn into_state<T>(self) -> AgentBuilder<T> {
        AgentBuilder {
            config: self.config,
            providers: self.providers,
            tools: self.tools,
            skills: self.skills,
            plugins: self.plugins,
            session_storage: self.session_storage,
            memory_storage: self.memory_storage,
            session_storage_registration: self.session_storage_registration,
            memory_storage_registration: self.memory_storage_registration,
            _state: PhantomData,
        }
    }

    fn validate_storage_registration(&self) -> Result<()> {
        validate_storage_duplicate("session", self.session_storage_registration.count)?;
        validate_storage_duplicate("memory", self.memory_storage_registration.count)
    }

    fn validate_config_values(&self) -> Result<()> {
        if self.providers.is_empty() {
            return Err(AgentError::NoModelProvider);
        }

        validate_positive_f64("compact_threshold", self.config.compact_threshold, 0.0, 1.0)?;
        validate_positive_u64("tool_timeout_ms", self.config.tool_timeout_ms)?;
        validate_positive_usize(
            "max_tool_calls_per_turn",
            self.config.max_tool_calls_per_turn,
        )?;
        validate_positive_u64(
            "memory_checkpoint_interval",
            self.config.memory_checkpoint_interval,
        )?;
        validate_positive_usize("memory_max_items", self.config.memory_max_items)?;

        if self.config.tool_output_max_bytes <= self.config.tool_output_metadata_max_bytes {
            return Err(AgentError::InputValidation {
                message:
                    "tool_output_max_bytes must be greater than tool_output_metadata_max_bytes"
                        .to_string(),
            });
        }

        if self.config.memory_namespace.trim().is_empty() {
            return Err(AgentError::InputValidation {
                message: "memory_namespace must not be empty".to_string(),
            });
        }

        if self.config.default_model.trim().is_empty() {
            return Err(AgentError::InputValidation {
                message: "default_model must not be empty".to_string(),
            });
        }

        Ok(())
    }
}

fn validate_storage_duplicate(kind: &'static str, count: usize) -> Result<()> {
    if count > 1 {
        return Err(AgentError::StorageDuplicate { kind });
    }

    Ok(())
}

fn validate_positive_f64(name: &'static str, value: f64, lower: f64, upper: f64) -> Result<()> {
    if value <= lower || value >= upper {
        return Err(AgentError::InputValidation {
            message: format!("{name} must be in ({lower}, {upper})"),
        });
    }

    Ok(())
}

fn validate_positive_u64(name: &'static str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(AgentError::InputValidation {
            message: format!("{name} must be greater than 0"),
        });
    }

    Ok(())
}

fn validate_positive_usize(name: &'static str, value: usize) -> Result<()> {
    if value == 0 {
        return Err(AgentError::InputValidation {
            message: format!("{name} must be greater than 0"),
        });
    }

    Ok(())
}

fn build_model_registry(providers: &[Arc<dyn ModelProvider>]) -> Result<ModelRegistry> {
    let mut registry = ModelRegistry::default();

    for provider in providers {
        let provider_id = provider.id().to_string();
        if registry.providers.contains_key(&provider_id) {
            return Err(AgentError::NameConflict {
                kind: "model provider",
                name: provider_id,
            });
        }

        for model in provider.models() {
            if registry.models.contains_key(&model.id) {
                return Err(AgentError::NameConflict {
                    kind: "model",
                    name: model.id.clone(),
                });
            }

            registry.models.insert(
                model.id.clone(),
                RegisteredModel {
                    provider_id: provider_id.clone(),
                    info: model.clone(),
                },
            );
        }

        registry.providers.insert(provider_id, Arc::clone(provider));
    }

    Ok(registry)
}

fn validate_model_config(config: &AgentConfig, registry: &ModelRegistry) -> Result<()> {
    if !registry.contains_model(&config.default_model) {
        return Err(AgentError::InvalidDefaultModel(
            config.default_model.clone(),
        ));
    }

    validate_optional_model("compact", config.compact_model.as_deref(), registry)?;
    validate_optional_model("memory", config.memory_model.as_deref(), registry)
}

fn validate_optional_model(
    kind: &'static str,
    model_id: Option<&str>,
    registry: &ModelRegistry,
) -> Result<()> {
    if let Some(model_id) = model_id
        && !registry.contains_model(model_id)
    {
        return Err(AgentError::InvalidModelConfig {
            kind,
            model_id: model_id.to_string(),
        });
    }

    Ok(())
}

fn build_tool_registry(tools: &[Arc<dyn ToolHandler>]) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::default();

    for tool in tools {
        let name = tool.name().to_string();
        if registry.handlers.contains_key(&name) {
            return Err(AgentError::NameConflict { kind: "tool", name });
        }

        registry.definitions.insert(
            name.clone(),
            ToolDefinition {
                name: name.clone(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            },
        );
        registry.handlers.insert(name, Arc::clone(tool));
    }

    Ok(registry)
}

fn build_skill_registry(skills: &[SkillDefinition], tools: &ToolRegistry) -> Result<SkillRegistry> {
    let mut registry = SkillRegistry::default();

    for skill in skills {
        if registry.skills.contains_key(&skill.name) {
            return Err(AgentError::NameConflict {
                kind: "skill",
                name: skill.name.clone(),
            });
        }

        for required_tool in &skill.required_tools {
            if !tools.handlers.contains_key(required_tool) {
                return Err(AgentError::SkillDependencyNotMet {
                    skill: skill.name.clone(),
                    tool: required_tool.clone(),
                });
            }
        }

        registry.skills.insert(skill.name.clone(), skill.clone());
    }

    Ok(registry)
}
