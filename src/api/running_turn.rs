//! 运行中 turn 的对外句柄。

use std::fmt;

use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::domain::{AgentError, AgentEvent, Result, TurnOutcome};

/// 运行中 turn 的句柄。
pub struct RunningTurn {
    /// 实时事件流。
    pub events: ReceiverStream<AgentEvent>,
    /// Turn 级取消令牌。
    cancel: CancellationToken,
    /// Turn 终态结果接收端。
    outcome_rx: oneshot::Receiver<TurnOutcome>,
}

impl RunningTurn {
    /// 创建一个新的运行中 turn 句柄。
    pub(crate) fn new(
        events: ReceiverStream<AgentEvent>,
        cancel: CancellationToken,
        outcome_rx: oneshot::Receiver<TurnOutcome>,
    ) -> Self {
        Self {
            events,
            cancel,
            outcome_rx,
        }
    }

    /// 取消当前 turn。
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// 返回当前 turn 的取消令牌。
    #[must_use]
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// 等待 turn 结束并返回最终结果。
    ///
    /// # Errors
    ///
    /// 当后台 supervisor 在发送终态前异常退出时返回错误。
    pub async fn join(self) -> Result<TurnOutcome> {
        self.outcome_rx
            .await
            .map_err(|error| AgentError::InternalPanic {
                message: format!("turn outcome channel dropped unexpectedly: {error}"),
            })
    }
}

impl fmt::Debug for RunningTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunningTurn")
            .field("cancel", &self.cancel)
            .finish_non_exhaustive()
    }
}
