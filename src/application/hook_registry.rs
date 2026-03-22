//! 内部 Hook 注册存储。

use std::collections::HashSet;
use std::fmt;

use indexmap::IndexMap;

use crate::domain::HookEvent;
use crate::ports::HookRegistration;

/// 按事件类型分组、按注册顺序保存的 Hook 注册表。
#[derive(Default)]
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
}
