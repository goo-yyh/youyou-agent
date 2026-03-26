mod support;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_stream::StreamExt;
use youyou_agent::{
    AgentBuilder, AgentConfig, AgentError, AgentEventPayload, ChatEvent, ChatRequest, ContentBlock,
    HookEvent, LedgerEventPayload, Message, ModelCapabilities, ModelInfo, PluginDescriptor,
    RunningTurn, SessionConfig, TokenUsage, TurnOutcome, UserInput,
};

use crate::support::fake_plugin::{FakePlugin, FakePluginApplyBehavior};
use crate::support::fake_provider::{FakeProvider, FakeProviderStep};
use crate::support::fake_session_storage::{FailingPayload, FakeSessionStorage};
use crate::support::fake_tool::FakeTool;

/// 构造 acceptance 测试使用的基础配置。
fn base_config(default_model: &str) -> AgentConfig {
    AgentConfig::new(default_model, "memory/test")
}

/// 构造一个支持流式与 tool use 的模型描述。
fn model_info(id: &str, context_window: usize) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        display_name: id.to_string(),
        context_window,
        capabilities: ModelCapabilities {
            tool_use: true,
            vision: true,
            streaming: true,
        },
    }
}

/// 构造最简单的文本输入。
fn text_input(text: &str) -> UserInput {
    UserInput {
        content: vec![ContentBlock::Text(text.to_string())],
    }
}

/// 构造一个最小 plugin 描述。
fn plugin_descriptor(id: &str, tapped_hooks: Vec<HookEvent>) -> PluginDescriptor {
    PluginDescriptor {
        id: id.to_string(),
        display_name: id.to_string(),
        description: format!("plugin {id}"),
        tapped_hooks,
    }
}

/// 构造一条 provider `Done` 事件。
fn done() -> FakeProviderStep {
    FakeProviderStep::Emit(ChatEvent::Done {
        usage: TokenUsage::default(),
    })
}

/// 构造一轮纯文本完成脚本。
fn assistant_script(text: &str) -> Vec<FakeProviderStep> {
    vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta(text.to_string())),
        done(),
    ]
}

/// 构造一条 `ToolCall` 事件。
fn tool_call(call_id: &str, tool_name: &str, arguments: serde_json::Value) -> FakeProviderStep {
    FakeProviderStep::Emit(ChatEvent::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        arguments,
    })
}

/// 生成一个指定长度的长文本。
fn repeated_text(ch: char, len: usize) -> String {
    std::iter::repeat_n(ch, len).collect()
}

/// 等待一个 turn 完成并返回终态。
async fn finish_turn(mut turn: RunningTurn) -> TurnOutcome {
    while turn.events.next().await.is_some() {}
    turn.join().await.expect("turn should join successfully")
}

/// 在收到首个文本增量后取消 turn，并等待终态。
async fn cancel_after_text_delta(mut turn: RunningTurn) -> TurnOutcome {
    while let Some(event) = turn.events.next().await {
        if matches!(event.payload, AgentEventPayload::TextDelta(_)) {
            turn.cancel();
            break;
        }
    }

    while turn.events.next().await.is_some() {}
    turn.join().await.expect("turn should join successfully")
}

/// 在收到首个 `ToolCallStart` 事件后取消 turn，并等待终态。
async fn cancel_after_tool_start(mut turn: RunningTurn) -> TurnOutcome {
    while let Some(event) = turn.events.next().await {
        if matches!(event.payload, AgentEventPayload::ToolCallStart { .. }) {
            turn.cancel();
            break;
        }
    }

    while turn.events.next().await.is_some() {}
    turn.join().await.expect("turn should join successfully")
}

/// 读取 provider 可见历史，跳过最前面的 system prompt。
fn history_messages(request: &ChatRequest) -> &[Message] {
    request
        .messages
        .split_first()
        .map(|(_, history)| history)
        .expect("chat request should contain a leading system prompt")
}

/// 提取请求中的用户文本。
fn user_texts(request: &ChatRequest) -> Vec<String> {
    history_messages(request)
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => Some(content_text(content)),
            Message::Assistant { .. }
            | Message::ToolCall { .. }
            | Message::ToolResult { .. }
            | Message::System { .. } => None,
        })
        .collect()
}

/// 提取请求中的 assistant 文本。
fn assistant_texts(request: &ChatRequest) -> Vec<String> {
    history_messages(request)
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(content_text(content)),
            Message::User { .. }
            | Message::ToolCall { .. }
            | Message::ToolResult { .. }
            | Message::System { .. } => None,
        })
        .collect()
}

/// 提取请求历史中的 system 文本。
fn system_history_texts(request: &ChatRequest) -> Vec<String> {
    history_messages(request)
        .iter()
        .filter_map(|message| match message {
            Message::System { content } => Some(content.clone()),
            Message::User { .. }
            | Message::Assistant { .. }
            | Message::ToolCall { .. }
            | Message::ToolResult { .. } => None,
        })
        .collect()
}

/// 提取请求历史中的 tool trace。
fn tool_trace(request: &ChatRequest) -> Vec<String> {
    history_messages(request)
        .iter()
        .filter_map(|message| match message {
            Message::ToolCall {
                call_id,
                tool_name,
                arguments,
            } => Some(format!("call:{call_id}:{tool_name}:{arguments}")),
            Message::ToolResult { call_id, output } => Some(format!(
                "result:{call_id}:{}:{}",
                output.is_error, output.content
            )),
            Message::User { .. } | Message::Assistant { .. } | Message::System { .. } => None,
        })
        .collect()
}

/// 将纯文本内容块拼接成一个字符串。
fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.clone(),
            ContentBlock::Image { .. } | ContentBlock::File { .. } => {
                panic!("acceptance tests only use text content")
            }
        })
        .collect::<String>()
}

/// 运行一组 `AfterToolUse` abort 场景，并返回两个 tool 的执行次数与终态。
async fn run_after_tool_abort_case(mutating: bool) -> (usize, usize, usize, TurnOutcome) {
    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(vec![
        tool_call("call-a", "tool-a", json!({"value": "a"})),
        tool_call("call-b", "tool-b", json!({"value": "b"})),
        done(),
    ]);
    provider.enqueue_script(assistant_script("after tool abort"));

    let tool_a = FakeTool::new("tool-a", "tool a", mutating).with_delay_ms(40);
    let tool_b = FakeTool::new("tool-b", "tool b", mutating).with_delay_ms(40);
    let handle_a = tool_a.handle();
    let handle_b = tool_b.handle();
    let (plugin, _plugin_handle) = FakePlugin::new(
        plugin_descriptor("abort-plugin", vec![HookEvent::AfterToolUse]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredAbort(
            HookEvent::AfterToolUse,
            "stop after tool".to_string(),
        ),
    );

    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(provider.clone())
        .register_tool(tool_a)
        .register_tool(tool_b)
        .register_plugin(plugin, json!({}))
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let outcome = finish_turn(
        session
            .send_message(text_input("run tool"), None)
            .await
            .expect("turn should start"),
    )
    .await;

    (
        handle_a.execute_calls(),
        handle_b.execute_calls(),
        provider.chat_calls(),
        outcome,
    )
}

#[tokio::test]
async fn shutdown_rejects_new_session_while_agent_is_shutting_down() {
    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta("partial".to_string())),
        FakeProviderStep::WaitForCancel,
    ]);

    let agent = Arc::new(
        AgentBuilder::new(base_config("chat-model"))
            .register_model_provider(provider)
            .build()
            .await
            .expect("agent should build"),
    );
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let turn = session
        .send_message(text_input("hello"), None)
        .await
        .expect("turn should start");

    let shutdown_agent = Arc::clone(&agent);
    let shutdown_task = tokio::spawn(async move { shutdown_agent.shutdown().await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(matches!(
        agent.new_session(SessionConfig::default()).await,
        Err(AgentError::AgentShutdown)
    ));
    assert!(
        shutdown_task
            .await
            .expect("shutdown task should join")
            .is_ok()
    );
    assert!(matches!(finish_turn(turn).await, TurnOutcome::Cancelled));
}

#[tokio::test]
async fn session_start_abort_does_not_leak_reserved_slot() {
    let (plugin, _handle) = FakePlugin::new(
        plugin_descriptor("abort-on-start", vec![HookEvent::SessionStart]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredAbort(
            HookEvent::SessionStart,
            "blocked by plugin".to_string(),
        ),
    );
    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(FakeProvider::new(
            "provider-a",
            vec![model_info("chat-model", 8_192)],
        ))
        .register_plugin(plugin, json!({"enabled": true}))
        .build()
        .await
        .expect("agent should build");

    let first = agent.new_session(SessionConfig::default()).await;
    let second = agent.new_session(SessionConfig::default()).await;

    assert!(matches!(
        first,
        Err(AgentError::PluginAborted { hook, reason })
            if hook == "SessionStart" && reason == "blocked by plugin"
    ));
    assert!(matches!(
        second,
        Err(AgentError::PluginAborted { hook, reason })
            if hook == "SessionStart" && reason == "blocked by plugin"
    ));
}

#[tokio::test]
async fn session_end_abort_does_not_poison_active_session() {
    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(assistant_script("still alive after failed close"));
    let (plugin, _handle) = FakePlugin::new(
        plugin_descriptor("abort-on-end", vec![HookEvent::SessionEnd]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredAbort(
            HookEvent::SessionEnd,
            "blocked by plugin".to_string(),
        ),
    );

    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(provider)
        .register_plugin(plugin, json!({"enabled": true}))
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let close = session.close().await;
    assert!(matches!(
        close,
        Err(AgentError::PluginAborted { hook, reason })
            if hook == "SessionEnd" && reason == "blocked by plugin"
    ));

    let outcome = finish_turn(
        session
            .send_message(text_input("session should still be usable"), None)
            .await
            .expect("turn should still start after failed close"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));
}

#[tokio::test]
async fn summary_compaction_resume_is_consistent_end_to_end() {
    let mut config = base_config("chat-model");
    config.compact_model = Some("compact-model".to_string());
    config.compact_threshold = 0.8;

    let session_storage = FakeSessionStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 4_096)]);
    let first_turn_user = repeated_text('a', 900);
    let first_turn_assistant = repeated_text('b', 900);

    chat_provider.enqueue_script(assistant_script(&first_turn_assistant));
    compact_provider.enqueue_script(assistant_script("summary checkpoint"));
    chat_provider.enqueue_script(assistant_script("ack"));
    chat_provider.enqueue_script(assistant_script("resume"));

    let agent = AgentBuilder::new(config)
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider)
        .register_session_storage(session_storage)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let session_id = session.session_id().to_string();

    let outcome = finish_turn(
        session
            .send_message(text_input(&first_turn_user), None)
            .await
            .expect("first turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let outcome = finish_turn(
        session
            .send_message(text_input("keep current turn"), None)
            .await
            .expect("second turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let live_request = chat_provider
        .requests()
        .last()
        .cloned()
        .expect("live request should exist");
    assert!(system_history_texts(&live_request).iter().any(|text| {
        text.contains("Another language model started to solve this problem")
            && text.contains("summary checkpoint")
    }));
    assert_eq!(
        user_texts(&live_request),
        vec!["keep current turn".to_string()]
    );
    assert!(
        !assistant_texts(&live_request)
            .iter()
            .any(|text| text.contains(first_turn_assistant.as_str()))
    );

    session.close().await.expect("session should close");

    let resumed = agent
        .resume_session(&session_id)
        .await
        .expect("session should resume");
    let outcome = finish_turn(
        resumed
            .send_message(text_input("after resume"), None)
            .await
            .expect("resumed turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let resumed_request = chat_provider
        .requests()
        .last()
        .cloned()
        .expect("resumed request should exist");
    assert!(system_history_texts(&resumed_request).iter().any(|text| {
        text.contains("Another language model started to solve this problem")
            && text.contains("summary checkpoint")
    }));
    assert_eq!(
        user_texts(&resumed_request),
        vec!["keep current turn".to_string(), "after resume".to_string()]
    );
    assert_eq!(assistant_texts(&resumed_request), vec!["ack".to_string()]);
}

#[tokio::test]
async fn cancellation_boundaries_cover_stream_and_tool_execution() {
    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta("partial".to_string())),
        FakeProviderStep::WaitForCancel,
    ]);

    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(provider)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let outcome = cancel_after_text_delta(
        session
            .send_message(text_input("cancel stream"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Cancelled));

    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(vec![tool_call("call-1", "read-tool", json!({})), done()]);
    let tool = FakeTool::new("read-tool", "read tool", false).with_delay_ms(120);
    let handle = tool.handle();

    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(provider.clone())
        .register_tool(tool)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let outcome = cancel_after_tool_start(
        session
            .send_message(text_input("cancel read tool"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Cancelled));
    assert_eq!(handle.execute_calls(), 1);
    assert_eq!(provider.chat_calls(), 1);

    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(vec![tool_call("call-1", "write-tool", json!({})), done()]);
    let tool = FakeTool::new("write-tool", "write tool", true).with_delay_ms(120);
    let handle = tool.handle();

    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(provider.clone())
        .register_tool(tool)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let outcome = cancel_after_tool_start(
        session
            .send_message(text_input("cancel write tool"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Cancelled));
    assert_eq!(handle.execute_calls(), 1);
    assert_eq!(provider.chat_calls(), 1);
}

#[tokio::test]
async fn critical_turn_persistence_failure_stops_turn_immediately() {
    let session_storage = FakeSessionStorage::default();
    session_storage.fail_on_payload(FailingPayload::UserMessage);
    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);

    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(provider.clone())
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = finish_turn(
        session
            .send_message(text_input("should fail"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(
        outcome,
        TurnOutcome::Failed(AgentError::StorageError(_))
    ));
    assert!(
        !session_storage
            .saved_events(session.session_id())
            .iter()
            .any(|event| matches!(event.payload, LedgerEventPayload::UserMessage { .. }))
    );

    session_storage.clear_failures();
    provider.enqueue_script(assistant_script("recovered"));

    let outcome = finish_turn(
        session
            .send_message(text_input("after recover"), None)
            .await
            .expect("recovery turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));
}

#[tokio::test]
async fn after_tool_use_abort_behaves_differently_for_parallel_and_serial_batches() {
    let (parallel_a, parallel_b, parallel_calls, parallel_outcome) =
        run_after_tool_abort_case(false).await;
    let (serial_a, serial_b, serial_calls, serial_outcome) = run_after_tool_abort_case(true).await;

    assert!(matches!(parallel_outcome, TurnOutcome::Completed));
    assert_eq!(parallel_a, 1);
    assert_eq!(parallel_b, 1);
    assert_eq!(parallel_calls, 2);

    assert!(matches!(serial_outcome, TurnOutcome::Completed));
    assert_eq!(serial_a, 1);
    assert_eq!(serial_b, 0);
    assert_eq!(serial_calls, 2);
}

#[tokio::test]
async fn max_tool_calls_exceeded_runs_final_no_tool_request() {
    let mut config = base_config("chat-model");
    config.max_tool_calls_per_turn = 1;

    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(vec![
        tool_call("call-1", "echo", json!({"value": "one"})),
        tool_call("call-2", "echo", json!({"value": "two"})),
        done(),
    ]);
    provider.enqueue_script(assistant_script("final after limit"));
    let session_storage = FakeSessionStorage::default();

    let agent = AgentBuilder::new(config)
        .register_model_provider(provider.clone())
        .register_tool(FakeTool::new("echo", "echo tool", false))
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = finish_turn(
        session
            .send_message(text_input("too many tools"), None)
            .await
            .expect("turn should start"),
    )
    .await;

    assert!(matches!(
        outcome,
        TurnOutcome::Failed(AgentError::MaxToolCallsExceeded { limit }) if limit == 1
    ));
    assert_eq!(provider.chat_calls(), 2);

    let final_request = provider
        .requests()
        .get(1)
        .cloned()
        .expect("final no-tool request should exist");
    assert!(final_request.tools.is_empty());
    assert!(
        system_history_texts(&final_request)
            .iter()
            .any(|text| text.contains("[TOOL_LIMIT_REACHED]"))
    );
    assert!(
        session_storage
            .saved_events(session.session_id())
            .iter()
            .any(|event| {
                matches!(
                    &event.payload,
                    LedgerEventPayload::SystemMessage { content }
                        if content.contains("[TOOL_LIMIT_REACHED]")
                )
            })
    );
}

#[tokio::test]
async fn plugin_initialize_failure_rolls_back_prior_plugins() {
    let (plugin_a, handle_a) = FakePlugin::new(
        plugin_descriptor("plugin-a", vec![]),
        None,
        None,
        FakePluginApplyBehavior::Noop,
    );
    let (plugin_b, handle_b) = FakePlugin::new(
        plugin_descriptor("plugin-b", vec![]),
        Some("boom".to_string()),
        None,
        FakePluginApplyBehavior::Noop,
    );

    let build_result = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(FakeProvider::new(
            "provider-a",
            vec![model_info("chat-model", 8_192)],
        ))
        .register_plugin(plugin_a, json!({}))
        .register_plugin(plugin_b, json!({}))
        .build()
        .await;

    assert!(matches!(
        build_result,
        Err(AgentError::PluginInitFailed { .. })
    ));
    assert_eq!(handle_a.initialize_calls(), 1);
    assert_eq!(handle_a.shutdown_calls(), 1);
    assert_eq!(handle_b.initialize_calls(), 1);
}

#[tokio::test]
async fn synthetic_tool_messages_survive_resume_projection() {
    let session_storage = FakeSessionStorage::default();
    let provider = FakeProvider::new("provider-a", vec![model_info("chat-model", 8_192)]);
    provider.enqueue_script(vec![tool_call("call-1", "missing-tool", json!({})), done()]);
    provider.enqueue_script(assistant_script("after synthetic"));
    provider.enqueue_script(assistant_script("live compare"));
    provider.enqueue_script(assistant_script("after resume"));

    let agent = AgentBuilder::new(base_config("chat-model"))
        .register_model_provider(provider.clone())
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let session_id = session.session_id().to_string();

    let outcome = finish_turn(
        session
            .send_message(text_input("first turn"), None)
            .await
            .expect("first turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let outcome = finish_turn(
        session
            .send_message(text_input("live comparison"), None)
            .await
            .expect("live comparison turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let live_request = provider
        .requests()
        .get(2)
        .cloned()
        .expect("live comparison request should exist");
    let live_tool_trace = tool_trace(&live_request);
    assert!(
        live_tool_trace
            .iter()
            .any(|entry| entry.contains("missing-tool"))
    );
    assert!(
        live_tool_trace
            .iter()
            .any(|entry| entry.contains("[Tool error] tool 'missing-tool' not found"))
    );

    session.close().await.expect("session should close");

    let resumed = agent
        .resume_session(&session_id)
        .await
        .expect("session should resume");
    let outcome = finish_turn(
        resumed
            .send_message(text_input("after resume"), None)
            .await
            .expect("resumed turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let resumed_request = provider
        .requests()
        .get(3)
        .cloned()
        .expect("resumed request should exist");
    assert_eq!(tool_trace(&resumed_request), live_tool_trace);
    assert!(
        session_storage
            .saved_events(&session_id)
            .iter()
            .any(|event| {
                matches!(
                    &event.payload,
                    LedgerEventPayload::ToolResult { output, .. }
                        if output.content.contains("[Tool error] tool 'missing-tool' not found")
                )
            })
    );
}
