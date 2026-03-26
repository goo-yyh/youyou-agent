//! 集成测试使用的伪造 Tool 实现。
#![allow(
    dead_code,
    reason = "测试支撑会被多个集成测试按需复用，单个测试目标不一定覆盖全部辅助行为。"
)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result as AnyResult, anyhow};
use async_trait::async_trait;
use serde_json::json;
use youyou_agent::{ToolHandler, ToolInput, ToolOutput};

/// 伪造 Tool 的共享运行状态。
#[derive(Debug, Default)]
struct FakeToolState {
    execute_inputs: Vec<ToolInput>,
    started_at: Vec<Instant>,
    finished_at: Vec<Instant>,
    active_calls: usize,
    max_active_calls: usize,
    timeout_cancel_observed: usize,
}

/// 对测试暴露的观测句柄。
#[derive(Debug, Clone)]
pub struct FakeToolHandle {
    state: Arc<Mutex<FakeToolState>>,
}

impl FakeToolHandle {
    /// 返回执行次数。
    #[must_use]
    pub fn execute_calls(&self) -> usize {
        self.state
            .lock()
            .map_or(0, |state| state.execute_inputs.len())
    }

    /// 返回收到的输入副本。
    #[must_use]
    pub fn execute_inputs(&self) -> Vec<ToolInput> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |state| state.execute_inputs.clone())
    }

    /// 返回首次开始执行的时间。
    #[must_use]
    pub fn first_started_at(&self) -> Option<Instant> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.started_at.first().copied())
    }

    /// 返回首次完成执行的时间。
    #[must_use]
    pub fn first_finished_at(&self) -> Option<Instant> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.finished_at.first().copied())
    }

    /// 返回执行期间观察到的最大并发数。
    #[must_use]
    pub fn max_active_calls(&self) -> usize {
        self.state.lock().map_or(0, |state| state.max_active_calls)
    }

    /// 返回观察到 timeout token 被取消的次数。
    #[must_use]
    pub fn timeout_cancel_observed(&self) -> usize {
        self.state
            .lock()
            .map_or(0, |state| state.timeout_cancel_observed)
    }
}

/// 伪造 Tool 的行为配置。
#[derive(Debug, Clone)]
struct FakeToolBehavior {
    delay_ms: u64,
    failure_message: Option<String>,
    wait_for_timeout_cancel: bool,
    output: ToolOutput,
}

impl Default for FakeToolBehavior {
    fn default() -> Self {
        Self {
            delay_ms: 0,
            failure_message: None,
            wait_for_timeout_cancel: false,
            output: ToolOutput {
                content: "ok".to_string(),
                is_error: false,
                metadata: json!({}),
            },
        }
    }
}

/// 可配置的伪造 Tool。
#[derive(Debug, Clone)]
pub struct FakeTool {
    name: String,
    description: String,
    mutating: bool,
    behavior: FakeToolBehavior,
    state: Arc<Mutex<FakeToolState>>,
}

impl FakeTool {
    /// 创建一个默认成功的伪造 Tool。
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, mutating: bool) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            mutating,
            behavior: FakeToolBehavior::default(),
            state: Arc::new(Mutex::new(FakeToolState::default())),
        }
    }

    /// 返回当前 Tool 的观测句柄。
    #[must_use]
    pub fn handle(&self) -> FakeToolHandle {
        FakeToolHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// 为执行增加固定延迟。
    #[must_use]
    pub fn with_delay_ms(mut self, delay_ms: u64) -> Self {
        self.behavior.delay_ms = delay_ms;
        self
    }

    /// 将 Tool 改为返回指定失败。
    #[must_use]
    pub fn with_failure(mut self, message: impl Into<String>) -> Self {
        self.behavior.failure_message = Some(message.into());
        self
    }

    /// 将 Tool 改为等待 timeout token 被取消。
    #[must_use]
    pub fn wait_for_timeout_cancel(mut self) -> Self {
        self.behavior.wait_for_timeout_cancel = true;
        self
    }

    /// 覆盖默认输出。
    #[must_use]
    pub fn with_output(mut self, output: ToolOutput) -> Self {
        self.behavior.output = output;
        self
    }
}

#[async_trait]
impl ToolHandler for FakeTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": [],
        })
    }

    fn is_mutating(&self) -> bool {
        self.mutating
    }

    async fn execute(
        &self,
        input: ToolInput,
        timeout_cancel: tokio_util::sync::CancellationToken,
    ) -> AnyResult<ToolOutput> {
        mark_tool_start(&self.state, input);

        let result = if self.behavior.wait_for_timeout_cancel {
            timeout_cancel.cancelled().await;
            mark_timeout_cancel_observed(&self.state);
            Ok(self.behavior.output.clone())
        } else {
            if self.behavior.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.behavior.delay_ms)).await;
            }

            match &self.behavior.failure_message {
                Some(message) => Err(anyhow!(message.clone())),
                None => Ok(self.behavior.output.clone()),
            }
        };

        mark_tool_finish(&self.state);
        result
    }
}

/// 记录一次 Tool 开始执行。
fn mark_tool_start(state: &Arc<Mutex<FakeToolState>>, input: ToolInput) {
    if let Ok(mut state) = state.lock() {
        state.execute_inputs.push(input);
        state.started_at.push(Instant::now());
        state.active_calls = state.active_calls.saturating_add(1);
        state.max_active_calls = state.max_active_calls.max(state.active_calls);
    }
}

/// 记录 timeout token 被观察到。
fn mark_timeout_cancel_observed(state: &Arc<Mutex<FakeToolState>>) {
    if let Ok(mut state) = state.lock() {
        state.timeout_cancel_observed = state.timeout_cancel_observed.saturating_add(1);
    }
}

/// 记录一次 Tool 完成执行。
fn mark_tool_finish(state: &Arc<Mutex<FakeToolState>>) {
    if let Ok(mut state) = state.lock() {
        state.finished_at.push(Instant::now());
        state.active_calls = state.active_calls.saturating_sub(1);
    }
}
