mod support;

use tokio_stream::StreamExt;
use youyou_agent::{
    AgentBuilder, AgentConfig, AgentError, AgentEventPayload, ContentBlock, MessageStatus,
    ModelCapabilities, ModelInfo, SessionConfig, UserInput,
};

use crate::support::fake_provider::{FakeProvider, FakeProviderStep};
use crate::support::fake_session_storage::FakeSessionStorage;

/// 构造 phase 4 测试使用的基础配置。
fn base_config() -> AgentConfig {
    AgentConfig::new("model-a", "memory/test")
}

/// 构造指定能力的模型元数据。
fn model_info(id: &str, vision: bool) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        display_name: id.to_string(),
        context_window: 128_000,
        capabilities: ModelCapabilities {
            tool_use: true,
            vision,
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

/// 返回一组正常完成的 provider 脚本。
fn completed_script() -> Vec<FakeProviderStep> {
    vec![
        FakeProviderStep::Emit(youyou_agent::ChatEvent::TextDelta("hello".to_string())),
        FakeProviderStep::Emit(youyou_agent::ChatEvent::ReasoningDelta(
            "thinking".to_string(),
        )),
        FakeProviderStep::Emit(youyou_agent::ChatEvent::Done {
            usage: youyou_agent::TokenUsage::default(),
        }),
    ]
}

/// 返回一组收到取消前先输出部分文本的脚本。
fn cancellable_script() -> Vec<FakeProviderStep> {
    vec![
        FakeProviderStep::Emit(youyou_agent::ChatEvent::TextDelta("partial".to_string())),
        FakeProviderStep::WaitForCancel,
    ]
}

#[tokio::test]
async fn turn_complete_is_last_event() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a", true)]);
    provider.enqueue_script(completed_script());
    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let mut turn = session
        .send_message(text_input("hello"), None)
        .await
        .expect("turn should start");

    let mut payloads = Vec::new();
    while let Some(event) = turn.events.next().await {
        payloads.push(event.payload);
    }

    let outcome = turn.join().await.expect("join should succeed");

    assert!(matches!(outcome, youyou_agent::TurnOutcome::Completed));
    assert!(matches!(
        payloads.last(),
        Some(AgentEventPayload::TurnComplete)
    ));
}

#[tokio::test]
async fn event_sequence_is_monotonic_per_turn() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a", true)]);
    provider.enqueue_script(completed_script());
    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let mut turn = session
        .send_message(text_input("hello"), None)
        .await
        .expect("turn should start");

    let mut sequences = Vec::new();
    while let Some(event) = turn.events.next().await {
        sequences.push(event.sequence);
    }

    let _ = turn.join().await.expect("join should succeed");

    assert!(!sequences.is_empty());
    assert!(sequences.windows(2).all(|window| window[0] < window[1]));
}

#[tokio::test]
async fn cancel_is_idempotent() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a", true)]);
    provider.enqueue_script(cancellable_script());
    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let mut turn = session
        .send_message(text_input("please stream"), None)
        .await
        .expect("turn should start");

    let first_event = turn
        .events
        .next()
        .await
        .expect("first text delta should arrive");
    assert!(matches!(
        first_event.payload,
        AgentEventPayload::TextDelta(_)
    ));

    turn.cancel();
    turn.cancel();

    let mut payloads = Vec::new();
    while let Some(event) = turn.events.next().await {
        payloads.push(event.payload);
    }

    let outcome = turn.join().await.expect("join should succeed");

    assert!(matches!(outcome, youyou_agent::TurnOutcome::Cancelled));
    assert!(matches!(
        payloads.last(),
        Some(AgentEventPayload::TurnCancelled)
    ));
}

#[tokio::test]
async fn cancelled_turn_persists_incomplete_assistant_message() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a", true)]);
    provider.enqueue_script(cancellable_script());
    let session_storage = FakeSessionStorage::default();
    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let mut turn = session
        .send_message(text_input("please stream"), None)
        .await
        .expect("turn should start");

    let _ = turn
        .events
        .next()
        .await
        .expect("first text delta should arrive");
    turn.cancel();
    while turn.events.next().await.is_some() {}

    let outcome = turn.join().await.expect("join should succeed");
    let saved_events = session_storage.saved_events(session.session_id());

    assert!(matches!(outcome, youyou_agent::TurnOutcome::Cancelled));
    assert!(saved_events.iter().any(|event| {
        matches!(
            &event.payload,
            youyou_agent::LedgerEventPayload::AssistantMessage { content, status }
                if *status == MessageStatus::Incomplete
                    && matches!(content.first(), Some(ContentBlock::Text(text)) if text == "partial")
        )
    }));
}

#[tokio::test]
async fn join_returns_outcome_after_events_are_consumed() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a", true)]);
    provider.enqueue_script(completed_script());
    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let mut turn = session
        .send_message(text_input("hello"), None)
        .await
        .expect("turn should start");

    while turn.events.next().await.is_some() {}

    let outcome = turn.join().await.expect("join should succeed");

    assert!(matches!(outcome, youyou_agent::TurnOutcome::Completed));
}

#[tokio::test]
async fn turn_busy_when_active_turn_exists() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a", true)]);
    provider.enqueue_script(vec![FakeProviderStep::WaitForCancel]);
    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider)
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let first_turn = session
        .send_message(text_input("hello"), None)
        .await
        .expect("first turn should start");

    let second_turn = session.send_message(text_input("again"), None).await;

    assert!(matches!(second_turn, Err(AgentError::TurnBusy)));

    first_turn.cancel();
    let outcome = first_turn.join().await.expect("join should succeed");
    assert!(matches!(outcome, youyou_agent::TurnOutcome::Cancelled));
}

#[tokio::test]
async fn invalid_multimodal_input_fails_before_turn_spawn() {
    let provider = FakeProvider::new("provider-a", vec![model_info("model-a", false)]);
    let agent = AgentBuilder::new(base_config())
        .register_model_provider(provider.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let result = session
        .send_message(
            UserInput {
                content: vec![ContentBlock::Image {
                    data: "ZmFrZQ==".to_string(),
                    media_type: "image/png".to_string(),
                }],
            },
            None,
        )
        .await;

    assert!(matches!(
        result,
        Err(AgentError::InputValidation { message })
            if message.contains("does not support image input")
    ));
    assert_eq!(provider.chat_calls(), 0);
}
