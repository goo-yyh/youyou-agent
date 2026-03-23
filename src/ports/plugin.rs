//! Plugin 生命周期抽象。

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::{AgentError, HookEvent, HookPayload, HookResult};

type HookFuture = Pin<Box<dyn Future<Output = HookResult> + Send>>;
type HookHandler = Arc<dyn Fn(HookPayload) -> HookFuture + Send + Sync>;

#[derive(Debug, Clone)]
struct PluginContractViolation {
    plugin_id: String,
    message: String,
}

/// 构建阶段声明的静态 Plugin 元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    /// 唯一 Plugin 标识。
    pub id: String,
    /// 面向用户的显示名称。
    pub display_name: String,
    /// 渲染进 prompt 的描述文本。
    pub description: String,
    /// Plugin 声明会注册的 hooks。
    #[serde(default)]
    pub tapped_hooks: Vec<HookEvent>,
}

/// `Plugin::apply()` 期间捕获的内部注册记录。
#[derive(Clone)]
pub(crate) struct HookRegistration {
    /// 当前注册的 Hook 事件。
    pub(crate) event: HookEvent,
    /// Plugin 标识。
    pub(crate) plugin_id: String,
    /// Plugin 配置快照。
    pub(crate) plugin_config: serde_json::Value,
    /// 擦除具体类型后的 Hook handler。
    pub(crate) handler: HookHandler,
}

impl fmt::Debug for HookRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookRegistration")
            .field("event", &self.event)
            .field("plugin_id", &self.plugin_id)
            .field("plugin_config", &self.plugin_config)
            .finish_non_exhaustive()
    }
}

/// 传给 `Plugin::apply()` 用于注册 hook 的上下文。
pub struct PluginContext {
    descriptor: PluginDescriptor,
    plugin_config: serde_json::Value,
    registrations: Vec<HookRegistration>,
    first_error: Option<PluginContractViolation>,
}

impl fmt::Debug for PluginContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginContext")
            .field("descriptor", &self.descriptor)
            .field("plugin_config", &self.plugin_config)
            .field("registrations", &self.registrations.len())
            .field("has_error", &self.first_error.is_some())
            .finish()
    }
}

impl PluginContext {
    /// 创建新的 Plugin 上下文。
    #[must_use]
    pub fn new(descriptor: PluginDescriptor, plugin_config: serde_json::Value) -> Self {
        Self {
            descriptor,
            plugin_config,
            registrations: Vec::new(),
            first_error: None,
        }
    }

    /// 为 Plugin 注册一个 Hook handler。
    ///
    /// # Errors
    ///
    /// 当 Plugin 试图注册未在 descriptor 中声明的 hook 时返回错误。
    pub fn tap<F>(&mut self, event: HookEvent, handler: F) -> crate::Result<()>
    where
        F: Fn(HookPayload) -> HookFuture + Send + Sync + 'static,
    {
        self.validate_tap(&event)?;
        self.registrations.push(HookRegistration {
            event,
            plugin_id: self.descriptor.id.clone(),
            plugin_config: self.plugin_config.clone(),
            handler: Arc::new(handler),
        });
        Ok(())
    }

    /// 返回与该上下文关联的静态 descriptor。
    #[must_use]
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// 在没有契约违规时返回已捕获的注册记录。
    ///
    /// # 错误
    ///
    /// 当 `apply()` 期间记录到契约违规时，返回第一条违规错误。
    pub(crate) fn finish(self) -> crate::Result<Vec<HookRegistration>> {
        if let Some(error) = self.first_error {
            return Err(AgentError::PluginHookContractViolation {
                plugin_id: error.plugin_id,
                message: error.message,
            });
        }

        Ok(self.registrations)
    }

    fn validate_tap(&mut self, event: &HookEvent) -> crate::Result<()> {
        if self.descriptor.tapped_hooks.contains(event) {
            return Ok(());
        }

        let error = PluginContractViolation {
            plugin_id: self.descriptor.id.clone(),
            message: format!("attempted to tap undeclared hook {}", event.as_str()),
        };
        if self.first_error.is_none() {
            self.first_error = Some(error.clone());
        }

        Err(AgentError::PluginHookContractViolation {
            plugin_id: error.plugin_id,
            message: error.message,
        })
    }
}

/// Plugin 生命周期契约。
#[async_trait]
pub trait Plugin: Send + Sync {
    /// 返回静态 Plugin descriptor。
    fn descriptor(&self) -> PluginDescriptor;

    /// 使用调用方提供的配置负载初始化 Plugin。
    ///
    /// # 错误
    ///
    /// 当 Plugin 初始化失败时返回错误。
    async fn initialize(&self, config: serde_json::Value) -> AnyResult<()>;

    /// 将 Plugin 的 hooks 注册到给定上下文。
    fn apply(self: Arc<Self>, ctx: &mut PluginContext);

    /// 关闭 Plugin。
    ///
    /// # 错误
    ///
    /// 当 Plugin 关闭失败时返回错误。
    async fn shutdown(&self) -> AnyResult<()>;
}
