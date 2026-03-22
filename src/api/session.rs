//! 后续 phase 会补齐的 Session API 占位模块。

/// 活跃会话句柄。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHandle {
    session_id: String,
}
