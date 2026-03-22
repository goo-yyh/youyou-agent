//! 集成测试使用的伪造 Tool 实现。

use anyhow::Result as AnyResult;
use async_trait::async_trait;
use serde_json::json;
use youyou_agent::{ToolHandler, ToolInput, ToolOutput};

/// 输出确定的简单伪造 Tool。
#[derive(Debug, Clone)]
pub struct FakeTool {
    name: String,
    description: String,
    mutating: bool,
}

impl FakeTool {
    /// 创建一个伪造 Tool。
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, mutating: bool) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            mutating,
        }
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
            "properties": {},
            "required": [],
        })
    }

    fn is_mutating(&self) -> bool {
        self.mutating
    }

    async fn execute(
        &self,
        _input: ToolInput,
        _timeout_cancel: tokio_util::sync::CancellationToken,
    ) -> AnyResult<ToolOutput> {
        Ok(ToolOutput {
            content: "ok".to_string(),
            is_error: false,
            metadata: json!({}),
        })
    }
}
