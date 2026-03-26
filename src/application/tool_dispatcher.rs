//! Tool 批次解析与执行器。

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::application::hook_registry::HookRegistry;
use crate::domain::{
    AgentError, AgentEvent, AgentEventPayload, HookData, HookPatch, Result, ToolOutput,
};
use crate::ports::{ToolHandler, ToolInput};

/// 模型在单次 provider 响应中请求的一次 Tool 调用。
#[derive(Debug, Clone)]
pub(crate) struct RequestedToolCall {
    /// Provider 生成的调用 id。
    pub(crate) call_id: String,
    /// Tool 名称。
    pub(crate) tool_name: String,
    /// 模型提供的原始参数。
    pub(crate) arguments: Value,
}

/// 已完成 handler 解析和 `BeforeToolUse` patch 的 Tool 调用。
#[derive(Clone)]
pub(crate) struct ResolvedToolCall {
    /// Provider 生成的调用 id。
    pub(crate) call_id: String,
    /// Tool 名称。
    pub(crate) tool_name: String,
    /// 模型原始参数。
    pub(crate) requested_arguments: Value,
    /// 真实执行参数。
    pub(crate) effective_arguments: Value,
    /// 当前 Tool 是否会修改外部状态。
    pub(crate) is_mutating: bool,
    /// 真正执行时使用的 handler。
    handler: Arc<dyn ToolHandler>,
}

impl fmt::Debug for ResolvedToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedToolCall")
            .field("call_id", &self.call_id)
            .field("tool_name", &self.tool_name)
            .field("requested_arguments", &self.requested_arguments)
            .field("effective_arguments", &self.effective_arguments)
            .field("is_mutating", &self.is_mutating)
            .finish_non_exhaustive()
    }
}

/// 已准备好的单条 Tool 调用。
#[derive(Debug, Clone)]
pub(crate) struct PreparedToolCall {
    /// Provider 生成的调用 id。
    pub(crate) call_id: String,
    /// Tool 名称。
    pub(crate) tool_name: String,
    /// 记账与执行统一使用的最终参数。
    pub(crate) effective_arguments: Value,
    execution: PreparedExecution,
}

impl PreparedToolCall {
    /// 创建待执行的 Tool 调用。
    fn executable(call: ResolvedToolCall) -> Self {
        Self {
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            effective_arguments: call.effective_arguments.clone(),
            execution: PreparedExecution::Execute(call),
        }
    }

    /// 创建 synthetic `ToolResult` 调用。
    fn synthetic(
        call_id: String,
        tool_name: String,
        effective_arguments: Value,
        record: ToolExecutionRecord,
        error: Option<SyntheticError>,
    ) -> Self {
        Self {
            call_id,
            tool_name,
            effective_arguments,
            execution: PreparedExecution::Synthetic { record, error },
        }
    }

    /// 生成取消结果。
    fn into_cancelled_record(self, dispatcher: &ToolDispatcher) -> ToolExecutionRecord {
        build_record(
            self.call_id,
            self.tool_name,
            self.effective_arguments,
            ToolOutput {
                content: "[Tool cancelled]".to_string(),
                is_error: true,
                metadata: Value::Null,
            },
            0,
            false,
            dispatcher,
        )
    }

    /// 生成跳过结果。
    fn into_skipped_record(
        self,
        message: String,
        dispatcher: &ToolDispatcher,
    ) -> ToolExecutionRecord {
        build_record(
            self.call_id,
            self.tool_name,
            self.effective_arguments,
            ToolOutput {
                content: message,
                is_error: true,
                metadata: Value::Null,
            },
            0,
            false,
            dispatcher,
        )
    }
}

/// 一个已经准备好的 Tool 批次。
#[derive(Debug, Clone)]
pub(crate) struct PreparedToolBatch {
    /// 按模型返回顺序排列的 Tool 调用。
    pub(crate) calls: Vec<PreparedToolCall>,
    /// 当前批次是否包含 mutating Tool。
    pub(crate) has_mutating: bool,
}

impl PreparedToolBatch {
    /// 返回批次内的调用数量。
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.calls.len()
    }
}

/// 单个 Tool 调用在 prepare 阶段的处理计划。
#[derive(Debug, Clone)]
enum PreparedExecution {
    /// 需要真实执行。
    Execute(ResolvedToolCall),
    /// 直接生成 synthetic 结果。
    Synthetic {
        /// synthetic 结果本体。
        record: ToolExecutionRecord,
        /// 需要同步发出的结构化错误事件。
        error: Option<SyntheticError>,
    },
}

/// synthetic 结果对应的错误事件类型。
#[derive(Debug, Clone)]
enum SyntheticError {
    /// Tool 名称无法解析。
    ToolNotFound { tool_name: String },
}

/// 单条 Tool 调用的标准执行结果。
#[allow(
    dead_code,
    reason = "Phase 5 的测试和后续 phase 会继续消费这些执行细节字段。"
)]
#[derive(Debug, Clone)]
pub(crate) struct ToolExecutionRecord {
    /// Provider 生成的调用 id。
    pub(crate) call_id: String,
    /// Tool 名称。
    pub(crate) tool_name: String,
    /// 真实执行参数。
    pub(crate) effective_arguments: Value,
    /// Tool 输出。
    pub(crate) output: ToolOutput,
    /// 耗时，单位毫秒。
    pub(crate) duration_ms: u64,
    /// Tool 是否成功。
    pub(crate) success: bool,
}

/// 一个 Tool 批次执行完成后的整体结果。
#[derive(Debug, Clone)]
pub(crate) struct ToolBatchOutcome {
    /// 与输入顺序一致的结果列表。
    pub(crate) results: Vec<ToolExecutionRecord>,
    /// 当前批次结束后是否应将整个 turn 视为取消。
    pub(crate) cancelled: bool,
    /// 当前批次结束后是否应停止继续发起带 Tool 的模型请求。
    pub(crate) stop_after_batch: bool,
}

/// 单次真实 Tool 执行后的中间状态。
#[derive(Debug, Clone)]
struct CompletedToolCall {
    /// 标准化后的执行记录。
    record: ToolExecutionRecord,
    /// 若 mutating 批次需要短路，给出剩余 Tool 的跳过提示。
    skip_remaining_message: Option<String>,
    /// `AfterToolUse` 是否要求停止继续发起带 Tool 的后续请求。
    after_tool_abort: bool,
}

/// 不带事件发射的原始 Tool 执行结果。
#[derive(Debug)]
struct RawToolExecution {
    /// 已解析的 Tool 调用。
    call: ResolvedToolCall,
    /// Tool 输出。
    output: ToolOutput,
    /// 耗时，单位毫秒。
    duration_ms: u64,
    /// Tool 是否成功。
    success: bool,
    /// 若 mutating 批次需要短路，给出剩余 Tool 的跳过提示。
    skip_remaining_message: Option<String>,
    /// 若需要补发错误事件，则在 finalize 阶段发送。
    error_event: Option<AgentError>,
}

/// 无状态的 Tool 调度器。
#[derive(Clone)]
pub(crate) struct ToolDispatcher {
    handlers: IndexMap<String, Arc<dyn ToolHandler>>,
    timeout: Duration,
    output_max_bytes: usize,
    metadata_max_bytes: usize,
}

impl fmt::Debug for ToolDispatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolDispatcher")
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .field("timeout", &self.timeout)
            .field("output_max_bytes", &self.output_max_bytes)
            .field("metadata_max_bytes", &self.metadata_max_bytes)
            .finish()
    }
}

impl ToolDispatcher {
    /// 创建新的 `ToolDispatcher`。
    #[must_use]
    pub(crate) fn new(
        handlers: IndexMap<String, Arc<dyn ToolHandler>>,
        timeout_ms: u64,
        output_max_bytes: usize,
        metadata_max_bytes: usize,
    ) -> Self {
        Self {
            handlers,
            timeout: Duration::from_millis(timeout_ms),
            output_max_bytes,
            metadata_max_bytes,
        }
    }

    /// 将模型请求的 `ToolCall` 批次解析成稳定计划。
    ///
    /// # Errors
    ///
    /// 当 hook 合同被破坏时返回错误。
    #[allow(
        clippy::too_many_lines,
        reason = "prepare 阶段需要把 not-found、hook patch 和 mutating 短路规则集中收口。"
    )]
    pub(crate) async fn prepare_batch(
        &self,
        requested_calls: Vec<RequestedToolCall>,
        hook_registry: &HookRegistry,
        session_id: &str,
        turn_id: &str,
    ) -> Result<PreparedToolBatch> {
        let has_mutating = requested_calls.iter().any(|call| {
            self.handlers
                .get(&call.tool_name)
                .is_some_and(|handler| handler.is_mutating())
        });
        let mut calls = Vec::with_capacity(requested_calls.len());
        let mut skip_remaining_message = None;

        for requested in requested_calls {
            if let Some(message) = skip_remaining_message.clone() {
                calls.push(PreparedToolCall::synthetic(
                    requested.call_id.clone(),
                    requested.tool_name.clone(),
                    requested.arguments.clone(),
                    build_record(
                        requested.call_id,
                        requested.tool_name,
                        requested.arguments,
                        ToolOutput {
                            content: message,
                            is_error: true,
                            metadata: Value::Null,
                        },
                        0,
                        false,
                        self,
                    ),
                    None,
                ));
                continue;
            }

            let Some(handler) = self.handlers.get(&requested.tool_name) else {
                if has_mutating {
                    skip_remaining_message = Some(skipped_not_found_output(&requested.tool_name));
                }
                calls.push(PreparedToolCall::synthetic(
                    requested.call_id.clone(),
                    requested.tool_name.clone(),
                    requested.arguments.clone(),
                    build_record(
                        requested.call_id,
                        requested.tool_name.clone(),
                        requested.arguments,
                        tool_not_found_output(&requested.tool_name),
                        0,
                        false,
                        self,
                    ),
                    Some(SyntheticError::ToolNotFound {
                        tool_name: requested.tool_name,
                    }),
                ));
                continue;
            };

            let effective_arguments = match self
                .dispatch_before_tool_use(
                    hook_registry,
                    session_id,
                    turn_id,
                    &requested.tool_name,
                    &requested.arguments,
                )
                .await
            {
                Ok(arguments) => arguments,
                Err(AgentError::PluginAborted { .. }) => {
                    if has_mutating {
                        skip_remaining_message =
                            Some("[Tool skipped: previous tool aborted by plugin]".to_string());
                    }
                    calls.push(PreparedToolCall::synthetic(
                        requested.call_id.clone(),
                        requested.tool_name.clone(),
                        requested.arguments.clone(),
                        build_record(
                            requested.call_id,
                            requested.tool_name,
                            requested.arguments,
                            ToolOutput {
                                content: "[Tool aborted by plugin]".to_string(),
                                is_error: true,
                                metadata: Value::Null,
                            },
                            0,
                            false,
                            self,
                        ),
                        None,
                    ));
                    continue;
                }
                Err(error) => return Err(error),
            };

            calls.push(PreparedToolCall::executable(ResolvedToolCall {
                call_id: requested.call_id,
                tool_name: requested.tool_name,
                requested_arguments: requested.arguments,
                effective_arguments,
                is_mutating: handler.is_mutating(),
                handler: Arc::clone(handler),
            }));
        }

        Ok(PreparedToolBatch {
            calls,
            has_mutating,
        })
    }

    /// 执行一个已准备好的 Tool 批次。
    ///
    /// # Errors
    ///
    /// 当 hook 合同被破坏时返回错误。
    #[allow(
        clippy::too_many_arguments,
        reason = "Tool 批次执行需要显式接收 turn 上下文、事件通道和序号游标。"
    )]
    pub(crate) async fn execute_batch(
        &self,
        batch: PreparedToolBatch,
        hook_registry: &HookRegistry,
        turn_cancel: &CancellationToken,
        session_id: &str,
        turn_id: &str,
        event_sequence: &mut u64,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<ToolBatchOutcome> {
        if batch.has_mutating {
            return self
                .execute_mutating_batch(
                    batch,
                    hook_registry,
                    turn_cancel,
                    session_id,
                    turn_id,
                    event_sequence,
                    event_tx,
                )
                .await;
        }

        self.execute_read_only_batch(
            batch,
            hook_registry,
            turn_cancel,
            session_id,
            turn_id,
            event_sequence,
            event_tx,
        )
        .await
    }

    /// 分发 `BeforeToolUse` hook 并返回最终参数。
    async fn dispatch_before_tool_use(
        &self,
        hook_registry: &HookRegistry,
        session_id: &str,
        turn_id: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<Value> {
        let patch = hook_registry
            .dispatch(
                crate::domain::HookEvent::BeforeToolUse,
                session_id,
                Some(turn_id),
                HookData::BeforeToolUse {
                    tool_name: tool_name.to_string(),
                    arguments: arguments.clone(),
                },
            )
            .await?;

        Ok(match patch {
            Some(HookPatch::BeforeToolUse { arguments }) => arguments,
            Some(HookPatch::TurnStart { .. }) | None => arguments.clone(),
        })
    }

    /// 执行包含 mutating Tool 的串行批次。
    #[allow(
        clippy::too_many_arguments,
        reason = "串行批次需要完整的 turn 上下文才能实现短路、取消和事件发射。"
    )]
    async fn execute_mutating_batch(
        &self,
        batch: PreparedToolBatch,
        hook_registry: &HookRegistry,
        turn_cancel: &CancellationToken,
        session_id: &str,
        turn_id: &str,
        event_sequence: &mut u64,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<ToolBatchOutcome> {
        let mut results = Vec::with_capacity(batch.len());
        let mut remaining_calls = batch.calls.into_iter();
        let mut cancelled = false;
        let mut stop_after_batch = false;

        while let Some(call) = remaining_calls.next() {
            if turn_cancel.is_cancelled() {
                cancelled = true;
                results.push(call.into_cancelled_record(self));
                results
                    .extend(remaining_calls.map(|remaining| remaining.into_cancelled_record(self)));
                break;
            }

            match call.execution {
                PreparedExecution::Synthetic { record, error } => {
                    if let Some(error) = error {
                        emit_agent_event(
                            session_id,
                            turn_id,
                            event_sequence,
                            event_tx,
                            AgentEventPayload::Error(error.to_agent_error()),
                        )
                        .await;
                    }
                    results.push(record);
                }
                PreparedExecution::Execute(resolved_call) => {
                    emit_agent_event(
                        session_id,
                        turn_id,
                        event_sequence,
                        event_tx,
                        AgentEventPayload::ToolCallStart {
                            call_id: resolved_call.call_id.clone(),
                            tool_name: resolved_call.tool_name.clone(),
                            arguments: resolved_call.effective_arguments.clone(),
                        },
                    )
                    .await;

                    let raw = self.perform_tool_execution(resolved_call).await;
                    let completed = self
                        .finalize_tool_execution(
                            raw,
                            hook_registry,
                            session_id,
                            turn_id,
                            event_sequence,
                            event_tx,
                        )
                        .await?;
                    let skip_message = completed.skip_remaining_message.clone();

                    if completed.after_tool_abort {
                        stop_after_batch = true;
                    }
                    results.push(completed.record);

                    if completed.after_tool_abort || skip_message.is_some() {
                        let message = skip_message.unwrap_or_else(|| {
                            "[Tool skipped: previous tool aborted by plugin]".to_string()
                        });
                        results.extend(
                            remaining_calls.map(|remaining| {
                                remaining.into_skipped_record(message.clone(), self)
                            }),
                        );
                        break;
                    }
                }
            }
        }

        Ok(ToolBatchOutcome {
            results,
            cancelled,
            stop_after_batch,
        })
    }

    /// 执行只读并行批次。
    #[allow(
        clippy::too_many_arguments,
        reason = "并行批次也需要 turn 上下文来发事件并在批次末统一处理取消语义。"
    )]
    async fn execute_read_only_batch(
        &self,
        batch: PreparedToolBatch,
        hook_registry: &HookRegistry,
        turn_cancel: &CancellationToken,
        session_id: &str,
        turn_id: &str,
        event_sequence: &mut u64,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<ToolBatchOutcome> {
        if turn_cancel.is_cancelled() {
            return Ok(ToolBatchOutcome {
                results: batch
                    .calls
                    .into_iter()
                    .map(|call| call.into_cancelled_record(self))
                    .collect(),
                cancelled: true,
                stop_after_batch: false,
            });
        }

        let mut results = vec![None; batch.len()];
        let mut in_flight = FuturesUnordered::new();
        let mut stop_after_batch = false;

        for (index, call) in batch.calls.into_iter().enumerate() {
            match call.execution {
                PreparedExecution::Synthetic { record, error } => {
                    if let Some(error) = error {
                        emit_agent_event(
                            session_id,
                            turn_id,
                            event_sequence,
                            event_tx,
                            AgentEventPayload::Error(error.to_agent_error()),
                        )
                        .await;
                    }
                    results[index] = Some(record);
                }
                PreparedExecution::Execute(resolved_call) => {
                    emit_agent_event(
                        session_id,
                        turn_id,
                        event_sequence,
                        event_tx,
                        AgentEventPayload::ToolCallStart {
                            call_id: resolved_call.call_id.clone(),
                            tool_name: resolved_call.tool_name.clone(),
                            arguments: resolved_call.effective_arguments.clone(),
                        },
                    )
                    .await;

                    let dispatcher = self.clone();
                    in_flight.push(async move {
                        (
                            index,
                            dispatcher.perform_tool_execution(resolved_call).await,
                        )
                    });
                }
            }
        }

        while let Some((index, raw)) = in_flight.next().await {
            let completed = self
                .finalize_tool_execution(
                    raw,
                    hook_registry,
                    session_id,
                    turn_id,
                    event_sequence,
                    event_tx,
                )
                .await?;
            if completed.after_tool_abort {
                stop_after_batch = true;
            }
            results[index] = Some(completed.record);
        }

        Ok(ToolBatchOutcome {
            results: collect_ordered_results(results)?,
            cancelled: turn_cancel.is_cancelled(),
            stop_after_batch,
        })
    }

    /// 真正执行一次 Tool 调用，但不直接发出事件。
    #[allow(
        clippy::too_many_lines,
        reason = "Tool 执行需要同时处理超时、JoinError、错误事件和输出预算。"
    )]
    async fn perform_tool_execution(&self, call: ResolvedToolCall) -> RawToolExecution {
        let start = Instant::now();
        let handler = Arc::clone(&call.handler);
        let input = ToolInput {
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            arguments: call.effective_arguments.clone(),
        };
        let timeout_cancel = CancellationToken::new();
        let task_cancel = timeout_cancel.clone();
        let result = tokio::time::timeout(
            self.timeout,
            tokio::spawn(async move { handler.execute(input, task_cancel).await }),
        )
        .await;
        let duration_ms = saturating_duration_ms(start.elapsed());

        match result {
            Ok(Ok(Ok(output))) => {
                let output = normalize_tool_output(output, self);
                let success = !output.is_error;
                RawToolExecution {
                    call,
                    output,
                    duration_ms,
                    success,
                    skip_remaining_message: None,
                    error_event: None,
                }
            }
            Ok(Ok(Err(source))) => {
                let tool_name = call.tool_name.clone();
                RawToolExecution {
                    call,
                    output: normalize_tool_output(
                        ToolOutput {
                            content: format!("[Tool error] tool '{tool_name}' failed"),
                            is_error: true,
                            metadata: Value::Null,
                        },
                        self,
                    ),
                    duration_ms,
                    success: false,
                    skip_remaining_message: Some(format!(
                        "[Tool skipped: previous tool '{tool_name}' failed]"
                    )),
                    error_event: Some(AgentError::ToolExecutionError {
                        name: tool_name,
                        source,
                    }),
                }
            }
            Ok(Err(join_error)) => {
                let tool_name = call.tool_name.clone();
                RawToolExecution {
                    call,
                    output: normalize_tool_output(
                        ToolOutput {
                            content: format!("[Tool error] tool '{tool_name}' failed"),
                            is_error: true,
                            metadata: Value::Null,
                        },
                        self,
                    ),
                    duration_ms,
                    success: false,
                    skip_remaining_message: Some(format!(
                        "[Tool skipped: previous tool '{tool_name}' failed]"
                    )),
                    error_event: Some(AgentError::ToolExecutionError {
                        name: tool_name,
                        source: anyhow::anyhow!(join_error),
                    }),
                }
            }
            Err(_) => {
                timeout_cancel.cancel();
                let tool_name = call.tool_name.clone();
                RawToolExecution {
                    call,
                    output: normalize_tool_output(
                        ToolOutput {
                            content: format!("[Tool timeout] tool '{tool_name}' timed out"),
                            is_error: true,
                            metadata: Value::Null,
                        },
                        self,
                    ),
                    duration_ms,
                    success: false,
                    skip_remaining_message: Some(format!(
                        "[Tool skipped: previous tool '{tool_name}' timed out]"
                    )),
                    error_event: Some(AgentError::ToolTimeout {
                        name: tool_name,
                        timeout_ms: timeout_ms(self.timeout),
                    }),
                }
            }
        }
    }

    /// 发送事件、分发 `AfterToolUse` hook，并产出最终记录。
    #[allow(
        clippy::too_many_arguments,
        reason = "finalize 阶段需要同时携带 hook、事件和 turn 上下文。"
    )]
    async fn finalize_tool_execution(
        &self,
        raw: RawToolExecution,
        hook_registry: &HookRegistry,
        session_id: &str,
        turn_id: &str,
        event_sequence: &mut u64,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<CompletedToolCall> {
        if let Some(error) = raw.error_event {
            emit_agent_event(
                session_id,
                turn_id,
                event_sequence,
                event_tx,
                AgentEventPayload::Error(error),
            )
            .await;
        }

        emit_agent_event(
            session_id,
            turn_id,
            event_sequence,
            event_tx,
            AgentEventPayload::ToolCallEnd {
                call_id: raw.call.call_id.clone(),
                tool_name: raw.call.tool_name.clone(),
                output: raw.output.clone(),
                duration_ms: raw.duration_ms,
                success: raw.success && !raw.output.is_error,
            },
        )
        .await;

        let after_tool_abort = match hook_registry
            .dispatch(
                crate::domain::HookEvent::AfterToolUse,
                session_id,
                Some(turn_id),
                HookData::AfterToolUse {
                    tool_name: raw.call.tool_name.clone(),
                    output: raw.output.clone(),
                    duration_ms: raw.duration_ms,
                    success: raw.success && !raw.output.is_error,
                },
            )
            .await
        {
            Ok(_) => false,
            Err(AgentError::PluginAborted { .. }) => true,
            Err(error) => return Err(error),
        };

        Ok(CompletedToolCall {
            record: build_record(
                raw.call.call_id,
                raw.call.tool_name,
                raw.call.effective_arguments,
                raw.output,
                raw.duration_ms,
                raw.success,
                self,
            ),
            skip_remaining_message: raw.skip_remaining_message,
            after_tool_abort,
        })
    }
}

impl SyntheticError {
    /// 转为事件流使用的结构化错误。
    fn to_agent_error(&self) -> AgentError {
        match self {
            Self::ToolNotFound { tool_name } => AgentError::ToolNotFound(tool_name.clone()),
        }
    }
}

/// 发出一条带序号的过程事件。
async fn emit_agent_event(
    session_id: &str,
    turn_id: &str,
    event_sequence: &mut u64,
    event_tx: &mpsc::Sender<AgentEvent>,
    payload: AgentEventPayload,
) {
    let event = AgentEvent {
        session_id: session_id.to_string(),
        turn_id: turn_id.to_string(),
        timestamp: chrono::Utc::now(),
        sequence: *event_sequence,
        payload,
    };
    *event_sequence = event_sequence.saturating_add(1);
    let _ = event_tx.send(event).await;
}

/// 构造统一的 `ToolExecutionRecord`。
fn build_record(
    call_id: String,
    tool_name: String,
    effective_arguments: Value,
    output: ToolOutput,
    duration_ms: u64,
    success: bool,
    dispatcher: &ToolDispatcher,
) -> ToolExecutionRecord {
    ToolExecutionRecord {
        call_id,
        tool_name,
        effective_arguments,
        output: normalize_tool_output(output, dispatcher),
        duration_ms,
        success,
    }
}

/// 生成未找到 Tool 时的 synthetic 输出。
fn tool_not_found_output(tool_name: &str) -> ToolOutput {
    ToolOutput {
        content: format!("[Tool error] tool '{tool_name}' not found"),
        is_error: true,
        metadata: Value::Null,
    }
}

/// 生成未找到 Tool 后的批次跳过提示。
fn skipped_not_found_output(tool_name: &str) -> String {
    format!("[Tool skipped: previous tool '{tool_name}' not found]")
}

/// 按配置限制 Tool 输出大小。
fn normalize_tool_output(output: ToolOutput, dispatcher: &ToolDispatcher) -> ToolOutput {
    let metadata_size = serde_json::to_vec(&output.metadata).map_or(0, |bytes| bytes.len());
    let metadata = if metadata_size > dispatcher.metadata_max_bytes {
        json!({
            "_truncated": true,
            "_original_bytes": metadata_size,
        })
    } else {
        output.metadata
    };
    let metadata_bytes = serde_json::to_vec(&metadata).map_or(0, |bytes| bytes.len());
    let content_budget = dispatcher.output_max_bytes.saturating_sub(metadata_bytes);

    ToolOutput {
        content: truncate_content_to_budget(&output.content, content_budget),
        is_error: output.is_error,
        metadata,
    }
}

/// 在总预算内截断 Tool 文本输出。
fn truncate_content_to_budget(content: &str, budget: usize) -> String {
    const SUFFIX: &str = "\n\n[output truncated]";

    if content.len() <= budget {
        return content.to_string();
    }

    if budget == 0 {
        return String::new();
    }

    if budget <= SUFFIX.len() {
        return SUFFIX[..budget].to_string();
    }

    let mut prefix = String::new();
    let prefix_budget = budget.saturating_sub(SUFFIX.len());

    for character in content.chars() {
        if prefix.len().saturating_add(character.len_utf8()) > prefix_budget {
            break;
        }
        prefix.push(character);
    }

    prefix.push_str(SUFFIX);
    prefix
}

/// 将耗时转换为毫秒，溢出时饱和。
fn saturating_duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

/// 将超时时长安全转换为毫秒整数。
fn timeout_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

/// 将只读并行批次的结果槽位恢复成有序结果。
fn collect_ordered_results(
    results: Vec<Option<ToolExecutionRecord>>,
) -> Result<Vec<ToolExecutionRecord>> {
    let mut ordered = Vec::with_capacity(results.len());

    for result in results {
        let Some(result) = result else {
            return Err(AgentError::InternalPanic {
                message: "tool dispatcher lost a read-only batch result".to_string(),
            });
        };
        ordered.push(result);
    }

    Ok(ordered)
}
