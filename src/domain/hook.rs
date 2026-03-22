//! Plugin 与应用层共享的 Hook 契约。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{ToolOutput, UserInput};

/// Plugin 系统支持的 Hook 事件。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    /// 在会话变为活跃前触发。
    SessionStart,
    /// 会话关闭时触发。
    SessionEnd,
    /// Turn 开始前触发。
    TurnStart,
    /// Turn 结束后触发。
    TurnEnd,
    /// Tool 调用前触发。
    BeforeToolUse,
    /// Tool 调用后触发。
    AfterToolUse,
    /// 上下文压缩前触发。
    BeforeCompact,
}

impl HookEvent {
    /// 返回该事件是否支持 `ContinueWith` patch。
    #[must_use]
    pub fn supports_patch(&self) -> bool {
        matches!(self, Self::TurnStart | Self::BeforeToolUse)
    }

    /// 返回用于错误报告的稳定 hook 名称。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::TurnStart => "TurnStart",
            Self::TurnEnd => "TurnEnd",
            Self::BeforeToolUse => "BeforeToolUse",
            Self::AfterToolUse => "AfterToolUse",
            Self::BeforeCompact => "BeforeCompact",
        }
    }
}

/// 传递给单个 hook handler 的负载。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookPayload {
    /// 当前正在分发的 hook 事件。
    pub event: HookEvent,
    /// 会话标识。
    pub session_id: String,
    /// 可选的 turn 标识。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Plugin 级配置快照。
    pub plugin_config: serde_json::Value,
    /// 事件相关负载。
    pub data: HookData,
    /// Hook 分发时间戳。
    pub timestamp: DateTime<Utc>,
}

/// 事件特定的 hook 数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HookData {
    /// 会话启动负载。
    SessionStart {
        /// 解析后的会话模型标识。
        model_id: String,
    },
    /// 会话结束负载。
    SessionEnd {
        /// 会话中的消息数量。
        message_count: usize,
    },
    /// Turn 开始负载。
    TurnStart {
        /// 本轮用户输入。
        user_input: UserInput,
        /// 从 plugins 收集到的可变动态段落。
        dynamic_sections: Vec<String>,
    },
    /// Turn 结束负载。
    TurnEnd {
        /// 最终 assistant 输出。
        assistant_output: String,
        /// 本轮执行的 Tool 调用次数。
        tool_calls_count: usize,
        /// 本轮是否被取消。
        cancelled: bool,
    },
    /// Tool 调用前负载。
    BeforeToolUse {
        /// Tool 名称。
        tool_name: String,
        /// Tool 参数。
        arguments: serde_json::Value,
    },
    /// Tool 调用后负载。
    AfterToolUse {
        /// Tool 名称。
        tool_name: String,
        /// Tool 输出。
        output: ToolOutput,
        /// 耗时，单位毫秒。
        duration_ms: u64,
        /// Tool 是否执行成功。
        success: bool,
    },
    /// 压缩前负载。
    BeforeCompact {
        /// 当前可见消息数。
        message_count: usize,
        /// 预估 token 数量。
        estimated_tokens: usize,
    },
}

/// Hook handler 返回的统一结果契约。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HookResult {
    /// 继续处理且不做修改。
    Continue,
    /// 继续处理，并对工作负载应用 patch。
    ContinueWith(HookPatch),
    /// 中止当前操作。
    Abort(String),
}

/// `ContinueWith` 使用的事件特定 patch 负载。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HookPatch {
    /// 为 `TurnStart` 追加动态 prompt 段落。
    TurnStart {
        /// 需要追加的 prompt 段落。
        append_dynamic_sections: Vec<String>,
    },
    /// 为 `BeforeToolUse` 替换 Tool 参数。
    BeforeToolUse {
        /// 替换后的 JSON 参数。
        arguments: serde_json::Value,
    },
}

impl HookPatch {
    /// 返回 patch 是否与给定事件匹配。
    #[must_use]
    pub fn matches(&self, event: HookEvent) -> bool {
        matches!(
            (self, event),
            (Self::TurnStart { .. }, HookEvent::TurnStart)
                | (Self::BeforeToolUse { .. }, HookEvent::BeforeToolUse)
        )
    }
}
