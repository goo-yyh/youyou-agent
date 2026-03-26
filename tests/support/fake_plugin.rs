//! 集成测试使用的伪造 Plugin 实现。
#![allow(
    dead_code,
    reason = "测试支撑会被多个集成测试按需复用，单个测试目标不一定覆盖全部辅助变体。"
)]

use std::sync::{Arc, Mutex};

use anyhow::{Result as AnyResult, anyhow};
use async_trait::async_trait;
use youyou_agent::{HookEvent, HookPayload, HookResult, Plugin, PluginContext, PluginDescriptor};

/// 伪造 Plugin 的可配置 apply 行为。
#[derive(Debug, Clone)]
pub enum FakePluginApplyBehavior {
    /// 不注册任何内容。
    Noop,
    /// 注册一个已声明的 hook。
    RegisterDeclared(HookEvent),
    /// 注册一个已声明的 hook，并在触发时返回中止结果。
    RegisterDeclaredAbort(HookEvent, String),
    /// 注册一个已声明的 hook，并返回给定结果。
    RegisterDeclaredResult(HookEvent, HookResult),
    /// 尝试注册一个未声明的 hook。
    RegisterUndeclared(HookEvent),
}

#[derive(Debug, Clone)]
struct FakePluginState {
    initialize: usize,
    shutdown: usize,
    apply: usize,
}

/// 用于观测伪造 Plugin 行为的共享句柄。
#[derive(Debug, Clone)]
pub struct FakePluginHandle {
    state: Arc<Mutex<FakePluginState>>,
}

impl FakePluginHandle {
    /// 返回 `initialize` 调用次数。
    #[must_use]
    pub fn initialize_calls(&self) -> usize {
        self.state.lock().map_or(0, |state| state.initialize)
    }

    /// 返回 `shutdown` 调用次数。
    #[must_use]
    pub fn shutdown_calls(&self) -> usize {
        self.state.lock().map_or(0, |state| state.shutdown)
    }

    /// 返回 `apply` 调用次数。
    #[must_use]
    pub fn apply_calls(&self) -> usize {
        self.state.lock().map_or(0, |state| state.apply)
    }
}

/// 一个初始化与 apply 行为都可配置的伪造 Plugin。
#[derive(Debug, Clone)]
pub struct FakePlugin {
    descriptor: PluginDescriptor,
    initialize_error: Option<String>,
    shutdown_error: Option<String>,
    apply_behavior: FakePluginApplyBehavior,
    state: Arc<Mutex<FakePluginState>>,
}

impl FakePlugin {
    /// 创建一个伪造 Plugin，并返回 Plugin 与观测句柄。
    #[must_use]
    pub fn new(
        descriptor: PluginDescriptor,
        initialize_error: Option<String>,
        shutdown_error: Option<String>,
        apply_behavior: FakePluginApplyBehavior,
    ) -> (Self, FakePluginHandle) {
        let state = Arc::new(Mutex::new(FakePluginState {
            initialize: 0,
            shutdown: 0,
            apply: 0,
        }));
        (
            Self {
                descriptor,
                initialize_error,
                shutdown_error,
                apply_behavior,
                state: Arc::clone(&state),
            },
            FakePluginHandle { state },
        )
    }
}

#[async_trait]
impl Plugin for FakePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        self.descriptor.clone()
    }

    async fn initialize(&self, _config: serde_json::Value) -> AnyResult<()> {
        if let Ok(mut state) = self.state.lock() {
            state.initialize = state.initialize.saturating_add(1);
        }

        if let Some(message) = &self.initialize_error {
            return Err(anyhow!(message.clone()));
        }

        Ok(())
    }

    fn apply(self: Arc<Self>, ctx: &mut PluginContext) {
        if let Ok(mut state) = self.state.lock() {
            state.apply = state.apply.saturating_add(1);
        }

        let event = match &self.apply_behavior {
            FakePluginApplyBehavior::Noop => return,
            FakePluginApplyBehavior::RegisterDeclared(event)
            | FakePluginApplyBehavior::RegisterDeclaredAbort(event, _)
            | FakePluginApplyBehavior::RegisterDeclaredResult(event, _)
            | FakePluginApplyBehavior::RegisterUndeclared(event) => event.clone(),
        };

        let hook_result = match &self.apply_behavior {
            FakePluginApplyBehavior::RegisterDeclaredAbort(_, reason) => {
                HookResult::Abort(reason.clone())
            }
            FakePluginApplyBehavior::RegisterDeclaredResult(_, hook_result) => hook_result.clone(),
            FakePluginApplyBehavior::Noop
            | FakePluginApplyBehavior::RegisterDeclared(_)
            | FakePluginApplyBehavior::RegisterUndeclared(_) => HookResult::Continue,
        };

        let _ = ctx.tap(event, move |_payload: HookPayload| {
            let hook_result = hook_result.clone();
            Box::pin(async move { hook_result })
        });
    }

    async fn shutdown(&self) -> AnyResult<()> {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = state.shutdown.saturating_add(1);
        }

        if let Some(message) = &self.shutdown_error {
            return Err(anyhow!(message.clone()));
        }

        Ok(())
    }
}
