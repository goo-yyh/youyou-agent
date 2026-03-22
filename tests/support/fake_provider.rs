//! 集成测试使用的伪造模型提供方。

use std::pin::Pin;

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use futures::{Stream, stream};
use youyou_agent::{ChatEvent, ChatEventStream, ChatRequest, ModelInfo, ModelProvider};

/// 带静态模型元数据的简单伪造 provider。
#[derive(Debug, Clone)]
pub struct FakeProvider {
    id: String,
    models: Vec<ModelInfo>,
}

impl FakeProvider {
    /// 使用给定模型列表创建伪造 provider。
    #[must_use]
    pub fn new(id: impl Into<String>, models: Vec<ModelInfo>) -> Self {
        Self {
            id: id.into(),
            models,
        }
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
        _request: ChatRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> AnyResult<ChatEventStream> {
        let stream: Pin<Box<dyn Stream<Item = AnyResult<ChatEvent>> + Send>> =
            Box::pin(stream::empty());
        Ok(stream)
    }
}
