//! 活跃会话句柄。

use std::fmt;
use std::sync::{Arc, Mutex};

use crate::application::session_service::{AgentControl, SessionDescriptor, SessionService};
use crate::domain::Result;

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
