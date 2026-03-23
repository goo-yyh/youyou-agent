//! 内部 Hook 注册存储。

use std::collections::HashSet;
use std::fmt;

use chrono::Utc;
use indexmap::IndexMap;

use crate::domain::{AgentError, HookData, HookEvent, HookPatch, HookPayload, Result};
use crate::ports::HookRegistration;

/// 按事件类型分组、按注册顺序保存的 Hook 注册表。
#[derive(Clone, Default)]
pub(crate) struct HookRegistry {
    handlers: IndexMap<HookEvent, Vec<HookRegistration>>,
}

impl fmt::Debug for HookRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let counts: Vec<(HookEvent, usize)> = self
            .handlers
            .iter()
            .map(|(event, handlers)| (event.clone(), handlers.len()))
            .collect();

        formatter
            .debug_struct("HookRegistry")
            .field("handlers", &counts)
            .finish()
    }
}

impl HookRegistry {
    /// 注册一条已捕获的 Plugin hook 记录。
    pub(crate) fn register(&mut self, registration: HookRegistration) {
        self.handlers
            .entry(registration.event.clone())
            .or_default()
            .push(registration);
    }

    /// 批量注册已捕获的 Plugin hook 记录。
    pub(crate) fn extend<I>(&mut self, registrations: I)
    where
        I: IntoIterator<Item = HookRegistration>,
    {
        for registration in registrations {
            self.register(registration);
        }
    }

    /// 返回指定 Plugin 已注册的 Hook 事件集合。
    pub(crate) fn registered_events_for_plugin(&self, plugin_id: &str) -> HashSet<HookEvent> {
        let mut events = HashSet::new();

        for (event, registrations) in &self.handlers {
            let has_registration = registrations
                .iter()
                .any(|registration| registration.plugin_id == plugin_id);
            if has_registration {
                events.insert(event.clone());
            }
        }

        events
    }

    /// 按注册顺序分发指定 hook 事件。
    ///
    /// # 错误
    ///
    /// 当某个 plugin 返回 `Abort`，或返回了与当前事件不匹配的 patch 时返回错误。
    pub(crate) async fn dispatch(
        &self,
        event: HookEvent,
        session_id: &str,
        turn_id: Option<&str>,
        data: HookData,
    ) -> Result<Vec<HookPatch>> {
        let mut patches = Vec::new();
        let Some(registrations) = self.handlers.get(&event) else {
            return Ok(patches);
        };

        let timestamp = Utc::now();
        for registration in registrations {
            let payload = HookPayload {
                event: event.clone(),
                session_id: session_id.to_string(),
                turn_id: turn_id.map(str::to_string),
                plugin_config: registration.plugin_config.clone(),
                data: data.clone(),
                timestamp,
            };

            match (registration.handler)(payload).await {
                crate::domain::HookResult::Continue => {}
                crate::domain::HookResult::ContinueWith(patch) => {
                    if !event.supports_patch() || !patch.matches(event.clone()) {
                        return Err(AgentError::PluginHookContractViolation {
                            plugin_id: registration.plugin_id.clone(),
                            message: format!(
                                "hook {} returned an incompatible patch",
                                event.as_str(),
                            ),
                        });
                    }
                    patches.push(patch);
                }
                crate::domain::HookResult::Abort(reason) => {
                    return Err(AgentError::PluginAborted {
                        hook: event.as_str(),
                        reason,
                    });
                }
            }
        }

        Ok(patches)
    }
}
