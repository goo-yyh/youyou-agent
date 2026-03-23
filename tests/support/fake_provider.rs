//! 集成测试使用的伪造模型提供方。
#![allow(
    dead_code,
    reason = "测试支撑会被多个集成测试按需复用，单个测试目标不一定覆盖全部辅助步骤。"
)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use youyou_agent::{ChatEvent, ChatEventStream, ChatRequest, ModelInfo, ModelProvider, TokenUsage};

/// 伪造 provider 的单步脚本。
#[derive(Debug, Clone)]
pub enum FakeProviderStep {
    /// 发出一条固定的流式事件。
    Emit(ChatEvent),
    /// 等待指定毫秒数后继续。
    DelayMs(u64),
    /// 挂起到收到取消信号为止。
    WaitForCancel,
}

/// 伪造 provider 的共享状态。
#[derive(Debug, Default)]
struct FakeProviderState {
    scripted_turns: VecDeque<Vec<FakeProviderStep>>,
    requests: Vec<ChatRequest>,
}

/// 带静态模型元数据的简单伪造 provider。
#[derive(Debug, Clone)]
pub struct FakeProvider {
    id: String,
    models: Vec<ModelInfo>,
    state: Arc<Mutex<FakeProviderState>>,
}

impl FakeProvider {
    /// 使用给定模型列表创建伪造 provider。
    #[must_use]
    pub fn new(id: impl Into<String>, models: Vec<ModelInfo>) -> Self {
        Self {
            id: id.into(),
            models,
            state: Arc::new(Mutex::new(FakeProviderState::default())),
        }
    }

    /// 追加一轮脚本化的 provider 输出。
    pub fn enqueue_script(&self, steps: Vec<FakeProviderStep>) {
        if let Ok(mut state) = self.state.lock() {
            state.scripted_turns.push_back(steps);
        }
    }

    /// 返回当前累计收到的请求数量。
    #[must_use]
    pub fn chat_calls(&self) -> usize {
        self.state.lock().map_or(0, |state| state.requests.len())
    }

    /// 返回 provider 已收到的请求副本。
    #[must_use]
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |state| state.requests.clone())
    }

    /// 返回一轮默认的成功脚本。
    fn default_script() -> Vec<FakeProviderStep> {
        vec![FakeProviderStep::Emit(ChatEvent::Done {
            usage: TokenUsage::default(),
        })]
    }
}

#[async_trait]
impl ModelProvider for FakeProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    async fn chat(
        &self,
        request: ChatRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> AnyResult<ChatEventStream> {
        let script = {
            let mut state = self
                .state
                .lock()
                .map_err(|error| anyhow::anyhow!("fake provider poisoned: {error}"))?;
            state.requests.push(request);
            state
                .scripted_turns
                .pop_front()
                .unwrap_or_else(Self::default_script)
        };

        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            for step in script {
                if execute_step(&tx, &cancel, step).await.is_err() {
                    return;
                }
            }
        });

        let stream: Pin<Box<dyn Stream<Item = AnyResult<ChatEvent>> + Send>> =
            Box::pin(ReceiverStream::new(rx));
        Ok(stream)
    }
}

/// 执行一条脚本步骤。
async fn execute_step(
    tx: &mpsc::Sender<AnyResult<ChatEvent>>,
    cancel: &tokio_util::sync::CancellationToken,
    step: FakeProviderStep,
) -> Result<(), ()> {
    match step {
        FakeProviderStep::Emit(event) => tx.send(Ok(event)).await.map_err(|_| ()),
        FakeProviderStep::DelayMs(delay_ms) => wait_or_cancel(delay_ms, cancel).await,
        FakeProviderStep::WaitForCancel => {
            cancel.cancelled().await;
            Ok(())
        }
    }
}

/// 在等待期间同时监听取消信号。
async fn wait_or_cancel(
    delay_ms: u64,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        () = tokio::time::sleep(Duration::from_millis(delay_ms)) => Ok(()),
        () = cancel.cancelled() => Ok(()),
    }
}
