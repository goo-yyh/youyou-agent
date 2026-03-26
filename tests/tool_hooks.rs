mod support;

use std::time::Duration;

use serde_json::json;
use tokio_stream::StreamExt;
use youyou_agent::{
    AgentBuilder, AgentConfig, AgentError, ChatEvent, HookEvent, HookPatch, HookResult,
    LedgerEventPayload, ModelCapabilities, ModelInfo, PluginDescriptor, SessionConfig, TokenUsage,
    ToolOutput, TurnOutcome, UserInput,
};

use crate::support::fake_plugin::{FakePlugin, FakePluginApplyBehavior};
use crate::support::fake_provider::{FakeProvider, FakeProviderStep};
use crate::support::fake_session_storage::FakeSessionStorage;
use crate::support::fake_tool::FakeTool;

/// 构造 phase 5 测试使用的基础配置。
fn base_config() -> AgentConfig {
    AgentConfig::new("model-a", "memory/test")
}

/// 构造一个支持 tool use 的模型描述。
fn model_info(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        display_name: id.to_string(),
        context_window: 128_000,
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
        content: vec![youyou_agent::ContentBlock::Text(text.to_string())],
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

/// 构造一条 `ToolCall` 事件。
fn tool_call(call_id: &str, tool_name: &str, arguments: serde_json::Value) -> FakeProviderStep {
    FakeProviderStep::Emit(ChatEvent::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        arguments,
    })
}

/// 构造一轮结束事件。
fn done() -> FakeProviderStep {
    FakeProviderStep::Emit(ChatEvent::Done {
        usage: TokenUsage::default(),
    })
}

/// 将 turn 事件消费到结束并返回最终结果。
async fn drain_turn(mut turn: youyou_agent::RunningTurn) -> TurnOutcome {
    while turn.events.next().await.is_some() {}
    turn.join().await.expect("join should succeed")
}

#[tokio::test]
async fn turn_start_patch_appends_dynamic_sections_in_order() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![done()]);
    let (plugin_a, _handle_a) = FakePlugin::new(
        plugin_descriptor("plugin-a", vec![HookEvent::TurnStart]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredResult(
            HookEvent::TurnStart,
            HookResult::ContinueWith(HookPatch::TurnStart {
                append_dynamic_sections: vec!["section-a".to_string()],
            }),
        ),
    );
    let (plugin_b, _handle_b) = FakePlugin::new(
        plugin_descriptor("plugin-b", vec![HookEvent::TurnStart]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredResult(
            HookEvent::TurnStart,
            HookResult::ContinueWith(HookPatch::TurnStart {
                append_dynamic_sections: vec!["section-b".to_string()],
            }),
        ),
    );

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider.clone())
        .register_plugin(plugin_a, json!({}))
        .register_plugin(plugin_b, json!({}))
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("hello"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    let requests = provider.requests();
    let system_prompt = match &requests[0].messages[0] {
        youyou_agent::Message::System { content } => content.clone(),
        other => panic!("expected system message, got {other:?}"),
    };

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert!(system_prompt.contains("section-a"));
    assert!(system_prompt.contains("section-b"));
    assert!(system_prompt.find("section-a") < system_prompt.find("section-b"));
}

#[tokio::test]
async fn before_tool_use_patch_updates_effective_arguments() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![
        tool_call("call-1", "echo", json!({"value": "raw"})),
        done(),
    ]);
    provider.enqueue_script(vec![done()]);
    let tool = FakeTool::new("echo", "echo tool", false);
    let tool_handle = tool.handle();
    let session_storage = FakeSessionStorage::default();
    let (plugin, _handle) = FakePlugin::new(
        plugin_descriptor("plugin", vec![HookEvent::BeforeToolUse]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredResult(
            HookEvent::BeforeToolUse,
            HookResult::ContinueWith(HookPatch::BeforeToolUse {
                arguments: json!({"value": "patched"}),
            }),
        ),
    );

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .register_tool(tool)
        .register_plugin(plugin, json!({}))
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("run tool"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    let inputs = tool_handle.execute_inputs();
    let saved_events = session_storage.saved_events(session.session_id());

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].arguments, json!({"value": "patched"}));
    assert!(saved_events.iter().any(|event| {
        matches!(
            &event.payload,
            LedgerEventPayload::ToolCall { arguments, .. }
                if arguments == &json!({"value": "patched"})
        )
    }));
}

#[tokio::test]
async fn read_only_tools_execute_in_parallel() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![
        tool_call("call-a", "read-a", json!({})),
        tool_call("call-b", "read-b", json!({})),
        done(),
    ]);
    provider.enqueue_script(vec![done()]);
    let tool_a = FakeTool::new("read-a", "read only tool a", false).with_delay_ms(120);
    let tool_b = FakeTool::new("read-b", "read only tool b", false).with_delay_ms(120);
    let handle_a = tool_a.handle();
    let handle_b = tool_b.handle();

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .register_tool(tool_a)
        .register_tool(tool_b)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("parallel"), None)
            .await
            .expect("turn should start"),
    )
    .await;

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(handle_a.execute_calls(), 1);
    assert_eq!(handle_b.execute_calls(), 1);
    assert!(handle_a.first_started_at() < handle_b.first_finished_at());
    assert!(handle_b.first_started_at() < handle_a.first_finished_at());
}

#[tokio::test]
async fn mutating_tools_execute_in_model_order() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![
        tool_call("call-a", "write-a", json!({})),
        tool_call("call-b", "write-b", json!({})),
        done(),
    ]);
    provider.enqueue_script(vec![done()]);
    let tool_a = FakeTool::new("write-a", "mutating tool a", true).with_delay_ms(120);
    let tool_b = FakeTool::new("write-b", "mutating tool b", true).with_delay_ms(10);
    let handle_a = tool_a.handle();
    let handle_b = tool_b.handle();

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .register_tool(tool_a)
        .register_tool(tool_b)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("serial"), None)
            .await
            .expect("turn should start"),
    )
    .await;

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(handle_a.execute_calls(), 1);
    assert_eq!(handle_b.execute_calls(), 1);
    assert!(handle_b.first_started_at() >= handle_a.first_finished_at());
}

#[tokio::test]
async fn mutating_batch_short_circuits_after_failure() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![
        tool_call("call-a", "write-a", json!({})),
        tool_call("call-b", "write-b", json!({})),
        done(),
    ]);
    provider.enqueue_script(vec![done()]);
    let tool_a = FakeTool::new("write-a", "mutating tool a", true).with_failure("boom");
    let tool_b = FakeTool::new("write-b", "mutating tool b", true);
    let handle_b = tool_b.handle();
    let session_storage = FakeSessionStorage::default();

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .register_tool(tool_a)
        .register_tool(tool_b)
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("short circuit"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    let saved_events = session_storage.saved_events(session.session_id());

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(handle_b.execute_calls(), 0);
    assert!(saved_events.iter().any(|event| {
        matches!(
            &event.payload,
            LedgerEventPayload::ToolResult { call_id, output }
                if call_id == "call-b"
                    && output.content.contains("previous tool 'write-a' failed")
        )
    }));
}

#[tokio::test]
async fn tool_timeout_cancels_only_timeout_token() {
    let mut config = base_config();
    config.tool_timeout_ms = 50;

    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![
        tool_call("call-slow", "slow", json!({})),
        tool_call("call-fast", "fast", json!({})),
        done(),
    ]);
    provider.enqueue_script(vec![done()]);
    let slow_tool = FakeTool::new("slow", "slow tool", false).wait_for_timeout_cancel();
    let fast_tool = FakeTool::new("fast", "fast tool", false).with_delay_ms(10);
    let slow_handle = slow_tool.handle();
    let fast_handle = fast_tool.handle();

    let agent = AgentBuilder::new(config)
        .register_model_provider(provider)
        .register_tool(slow_tool)
        .register_tool(fast_tool)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("timeout"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(slow_handle.timeout_cancel_observed(), 1);
    assert_eq!(fast_handle.execute_calls(), 1);
}

#[tokio::test]
async fn after_tool_use_abort_stops_follow_up_loop() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![tool_call("call-1", "echo", json!({})), done()]);
    provider.enqueue_script(vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta("final".to_string())),
        done(),
    ]);
    let tool = FakeTool::new("echo", "echo tool", false);
    let (plugin, _handle) = FakePlugin::new(
        plugin_descriptor("plugin", vec![HookEvent::AfterToolUse]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredAbort(
            HookEvent::AfterToolUse,
            "stop tool loop".to_string(),
        ),
    );

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider.clone())
        .register_tool(tool)
        .register_plugin(plugin, json!({}))
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("after tool abort"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    let requests = provider.requests();

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(requests.len(), 2);
    assert!(requests[1].tools.is_empty());
}

#[tokio::test]
async fn tool_output_respects_total_and_metadata_budgets() {
    let mut config = base_config();
    config.tool_output_max_bytes = 120;
    config.tool_output_metadata_max_bytes = 32;

    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![tool_call("call-1", "dump", json!({})), done()]);
    provider.enqueue_script(vec![done()]);
    let session_storage = FakeSessionStorage::default();
    let tool = FakeTool::new("dump", "dump tool", false).with_output(ToolOutput {
        content: "x".repeat(256),
        is_error: false,
        metadata: json!({
            "payload": "y".repeat(256),
        }),
    });

    let agent = AgentBuilder::new(config.clone())
        .register_model_provider(provider)
        .register_tool(tool)
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("budgets"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    let saved_events = session_storage.saved_events(session.session_id());
    let tool_result = saved_events
        .iter()
        .find_map(|event| match &event.payload {
            LedgerEventPayload::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("tool result should be persisted");
    let total_bytes =
        tool_result.content.len() + serde_json::to_vec(&tool_result.metadata).unwrap().len();

    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(tool_result.metadata["_truncated"], json!(true));
    assert!(tool_result.content.ends_with("[output truncated]"));
    assert!(total_bytes <= config.tool_output_max_bytes);
}

#[tokio::test]
async fn max_tool_calls_exceeded_injects_limit_message_and_returns_failed_outcome() {
    let mut config = base_config();
    config.max_tool_calls_per_turn = 1;

    let provider = FakeProvider::new("provider-a", vec![model_info("model-a")]);
    provider.enqueue_script(vec![tool_call("call-1", "echo", json!({})), done()]);
    provider.enqueue_script(vec![tool_call("call-2", "echo", json!({})), done()]);
    provider.enqueue_script(vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta("final summary".to_string())),
        done(),
    ]);
    let session_storage = FakeSessionStorage::default();
    let tool = FakeTool::new("echo", "echo tool", false);

    let agent = AgentBuilder::new(config)
        .register_model_provider(provider.clone())
        .register_tool(tool)
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = drain_turn(
        session
            .send_message(text_input("limit"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    let requests = provider.requests();
    let saved_events = session_storage.saved_events(session.session_id());

    assert!(matches!(
        outcome,
        TurnOutcome::Failed(AgentError::MaxToolCallsExceeded { limit }) if limit == 1
    ));
    assert_eq!(requests.len(), 3);
    assert!(requests[2].tools.is_empty());
    assert!(saved_events.iter().any(|event| {
        matches!(
            &event.payload,
            LedgerEventPayload::SystemMessage { content }
                if content.contains("[TOOL_LIMIT_REACHED]")
        )
    }));
}
