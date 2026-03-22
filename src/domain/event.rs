//! turn 运行期间流式返回给调用方的事件类型。

use chrono::{DateTime, Utc};

use crate::domain::{AgentError, ToolOutput};

/// turn 处理过程中发出的一条事件。
#[derive(Debug)]
pub struct AgentEvent {
    /// 会话标识。
    pub session_id: String,
    /// Turn 标识。
    pub turn_id: String,
    /// 事件时间戳。
    pub timestamp: DateTime<Utc>,
    /// 会话内单调递增的事件序号。
    pub sequence: u64,
    /// 事件负载。
    pub payload: AgentEventPayload,
}

/// 具体的事件负载类型。
#[derive(Debug)]
pub enum AgentEventPayload {
    /// 流式文本增量。
    TextDelta(String),
    /// 流式推理增量。
    ReasoningDelta(String),
    /// Tool 调用开始。
    ToolCallStart {
        /// Provider 生成的 Tool 调用 id。
        call_id: String,
        /// Tool 名称。
        tool_name: String,
        /// 传给 Tool 的 JSON 参数。
        arguments: serde_json::Value,
    },
    /// Tool 调用结束。
    ToolCallEnd {
        /// Provider 生成的 Tool 调用 id。
        call_id: String,
        /// Tool 名称。
        tool_name: String,
        /// Tool 输出负载。
        output: ToolOutput,
        /// 耗时，单位毫秒。
        duration_ms: u64,
        /// Tool 调用是否成功。
        success: bool,
    },
    /// 已执行上下文压缩。
    ContextCompacted,
    /// Turn 成功完成。
    TurnComplete,
    /// Turn 被取消。
    TurnCancelled,
    /// 处理过程中发生错误。
    Error(AgentError),
}
