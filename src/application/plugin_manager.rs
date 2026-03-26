//! Plugin 生命周期管理器。

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;
use tracing::warn;

use crate::application::hook_registry::HookRegistry;
use crate::domain::{AgentError, Result};
use crate::ports::{Plugin, PluginContext, PluginDescriptor};

/// 构建阶段收集到的一条 Plugin 注册记录。
#[derive(Clone)]
pub(crate) struct ConfiguredPlugin {
    /// Plugin 实例。
    pub(crate) instance: Arc<dyn Plugin>,
    /// 传给 Plugin 的初始化配置。
    pub(crate) config: serde_json::Value,
}

impl fmt::Debug for ConfiguredPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredPlugin")
            .field("descriptor", &self.instance.descriptor())
            .field("config", &self.config)
            .finish()
    }
}

/// 负责统一执行 Plugin 的初始化、apply 和回滚协议。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PluginManager;

impl PluginManager {
    /// 创建新的 `PluginManager`。
    #[must_use]
    pub(crate) fn new() -> Self {
        Self
    }

    /// 构建运行期使用的 Plugin 注册表与 Hook 注册表。
    ///
    /// # Errors
    ///
    /// 当初始化、hook 合同校验或 apply 过程失败时返回错误。
    pub(crate) async fn build(
        self,
        plugins: Vec<ConfiguredPlugin>,
    ) -> Result<(
        Vec<Arc<dyn Plugin>>,
        IndexMap<String, PluginDescriptor>,
        HookRegistry,
    )> {
        let descriptors = collect_plugin_descriptors(&plugins)?;
        let mut initialized_plugins = Vec::new();

        for plugin in &plugins {
            let descriptor = plugin.instance.descriptor();
            if let Err(source) = plugin.instance.initialize(plugin.config.clone()).await {
                rollback_plugins(&initialized_plugins).await;
                return Err(AgentError::PluginInitFailed {
                    id: descriptor.id,
                    source,
                });
            }
            initialized_plugins.push(Arc::clone(&plugin.instance));
        }

        let hook_registry = match apply_plugins(&plugins, &descriptors) {
            Ok(hook_registry) => hook_registry,
            Err(error) => {
                rollback_plugins(&initialized_plugins).await;
                return Err(error);
            }
        };

        Ok((initialized_plugins, descriptors, hook_registry))
    }
}

/// 收集并校验 Plugin 的静态 descriptor。
fn collect_plugin_descriptors(
    plugins: &[ConfiguredPlugin],
) -> Result<IndexMap<String, PluginDescriptor>> {
    let mut descriptors = IndexMap::new();

    for plugin in plugins {
        let descriptor = plugin.instance.descriptor();
        if descriptors.contains_key(&descriptor.id) {
            return Err(AgentError::NameConflict {
                kind: "plugin",
                name: descriptor.id,
            });
        }

        descriptors.insert(descriptor.id.clone(), descriptor);
    }

    Ok(descriptors)
}

/// 执行 `apply()` 并构造最终的 Hook 注册表。
fn apply_plugins(
    plugins: &[ConfiguredPlugin],
    descriptors: &IndexMap<String, PluginDescriptor>,
) -> Result<HookRegistry> {
    let mut hook_registry = HookRegistry::default();

    for plugin in plugins {
        let descriptor = plugin.instance.descriptor();
        let mut context = PluginContext::new(descriptor.clone(), plugin.config.clone());
        Arc::clone(&plugin.instance).apply(&mut context);
        let registrations = context.finish()?;
        hook_registry.extend(registrations);

        let registered_events = hook_registry.registered_events_for_plugin(&descriptor.id);
        if let Some(expected_descriptor) = descriptors.get(&descriptor.id) {
            for hook in &expected_descriptor.tapped_hooks {
                if !registered_events.contains(hook) {
                    warn!(
                        plugin_id = %expected_descriptor.id,
                        hook = %hook.as_str(),
                        "plugin declared a hook but did not register a handler",
                    );
                }
            }
        }
    }

    Ok(hook_registry)
}

/// 在失败时逆序关闭已经初始化过的 Plugin。
async fn rollback_plugins(plugins: &[Arc<dyn Plugin>]) {
    for plugin in plugins.iter().rev() {
        if let Err(error) = plugin.shutdown().await {
            let descriptor = plugin.descriptor();
            warn!(
                plugin_id = %descriptor.id,
                error = %error,
                "plugin shutdown failed during rollback",
            );
        }
    }
}
