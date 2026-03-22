//! Turn 与控制面相关的状态类型。

use crate::domain::AgentError;

/// Agent 外壳内部使用的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleState {
    /// Agent 可以接收新请求。
    Running,
}

/// 内部单会话槽位状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionSlotState {
    /// 当前没有活跃会话。
    Empty,
}

/// 运行中 turn 的最终结果。
#[derive(Debug)]
pub enum TurnOutcome {
    /// Turn 成功完成。
    Completed,
    /// Turn 被取消。
    Cancelled,
    /// Turn 以结构化错误失败。
    Failed(AgentError),
    /// 后台任务发生 panic。
    Panicked,
}
