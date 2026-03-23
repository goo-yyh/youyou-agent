//! Turn 与控制面相关的状态类型。

use crate::domain::AgentError;

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
