//! 后续 phase 会补齐的 RunningTurn API 占位模块。

/// 运行中 turn 的句柄。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningTurn {
    turn_id: String,
}
