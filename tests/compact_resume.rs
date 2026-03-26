mod support;

use serde_json::json;
use tokio_stream::StreamExt;
use youyou_agent::{
    AgentBuilder, AgentConfig, AgentError, ChatError, ChatEvent, ChatRequest, ContentBlock,
    HookEvent, LedgerEventPayload, Message, MessageStatus, MetadataKey, ModelCapabilities,
    ModelInfo, RunningTurn, SessionConfig, TurnOutcome, UserInput,
};

use crate::support::fake_plugin::{FakePlugin, FakePluginApplyBehavior};
use crate::support::fake_provider::{FakeProvider, FakeProviderStep};
use crate::support::fake_session_storage::FakeSessionStorage;

/// 恢复提示的固定文本。
const RESUME_CANCEL_NOTICE: &str = "[此消息因用户取消而中断]";

/// 构造 phase 6 测试使用的基础配置。
fn base_config() -> AgentConfig {
    let mut config = AgentConfig::new("chat-model", "memory/test");
    config.compact_model = Some("compact-model".to_string());
    config.compact_threshold = 0.8;
    config
}

/// 构造一个模型元数据。
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

/// 生成一个指定长度的长文本。
fn repeated_text(ch: char, len: usize) -> String {
    std::iter::repeat_n(ch, len).collect()
}

/// 返回一个标准的 assistant 成功脚本。
fn assistant_script(text: &str) -> Vec<FakeProviderStep> {
    vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta(text.to_string())),
        FakeProviderStep::Emit(ChatEvent::Done {
            usage: youyou_agent::TokenUsage::default(),
        }),
    ]
}

/// 返回一个 `context_length_exceeded` 错误脚本。
fn context_length_exceeded_script(message: &str) -> Vec<FakeProviderStep> {
    vec![FakeProviderStep::Emit(ChatEvent::Error(ChatError {
        message: message.to_string(),
        retryable: false,
        is_context_length_exceeded: true,
    }))]
}

/// 返回一个普通 compact 失败脚本。
fn compact_failure_script(message: &str) -> Vec<FakeProviderStep> {
    vec![FakeProviderStep::Emit(ChatEvent::Error(ChatError {
        message: message.to_string(),
        retryable: false,
        is_context_length_exceeded: false,
    }))]
}

/// 等待一个 turn 完成并返回终态。
async fn finish_turn(mut turn: RunningTurn) -> TurnOutcome {
    while turn.events.next().await.is_some() {}
    turn.join().await.expect("turn should join successfully")
}

/// 从请求中取出 provider 可见历史，跳过最前面的 system prompt。
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

/// 提取请求中的 system 文本。
fn system_texts(request: &ChatRequest) -> Vec<String> {
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

/// 将内容块拼成单个文本。
fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.clone(),
            ContentBlock::Image { .. } | ContentBlock::File { .. } => {
                panic!("phase 6 tests only use text content")
            }
        })
        .collect::<String>()
}

/// 返回一个最小 plugin 描述。
fn plugin_descriptor(id: &str, tapped_hooks: Vec<HookEvent>) -> youyou_agent::PluginDescriptor {
    youyou_agent::PluginDescriptor {
        id: id.to_string(),
        display_name: id.to_string(),
        description: id.to_string(),
        tapped_hooks,
    }
}

#[tokio::test]
async fn summary_compaction_resume_matches_live_projection() {
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

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider.clone())
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
        .expect("second regular request should exist");
    let live_system_texts = system_texts(&live_request);
    assert!(live_system_texts.iter().any(|text| {
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
    let resumed_system_texts = system_texts(&resumed_request);
    assert!(resumed_system_texts.iter().any(|text| {
        text.contains("Another language model started to solve this problem")
            && text.contains("summary checkpoint")
    }));
    assert_eq!(
        user_texts(&resumed_request),
        vec!["keep current turn".to_string(), "after resume".to_string()]
    );
    assert_eq!(assistant_texts(&resumed_request), vec!["ack".to_string()]);

    let compaction_events = session_storage
        .saved_events(&session_id)
        .into_iter()
        .filter(|event| {
            matches!(
                event.payload,
                LedgerEventPayload::Metadata {
                    key: MetadataKey::ContextCompaction,
                    ..
                }
            )
        })
        .count();
    assert_eq!(compaction_events, 1);
}

#[tokio::test]
async fn truncation_compaction_resume_matches_live_projection() {
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 4_096)]);
    let first_turn_user = repeated_text('c', 900);
    let first_turn_assistant = repeated_text('d', 900);

    chat_provider.enqueue_script(assistant_script(&first_turn_assistant));
    compact_provider.enqueue_script(compact_failure_script("compact unavailable"));
    chat_provider.enqueue_script(assistant_script("ack"));
    chat_provider.enqueue_script(assistant_script("resume"));

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider.clone())
        .register_session_storage(FakeSessionStorage::default())
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
        .expect("second regular request should exist");
    let live_system_texts = system_texts(&live_request);
    assert!(live_system_texts.iter().any(|text| {
        text.contains("Earlier context was truncated because summary compaction was unavailable")
    }));
    assert_eq!(
        user_texts(&live_request),
        vec!["keep current turn".to_string()]
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
    let resumed_system_texts = system_texts(&resumed_request);
    assert!(resumed_system_texts.iter().any(|text| {
        text.contains("Earlier context was truncated because summary compaction was unavailable")
    }));
    assert_eq!(
        user_texts(&resumed_request),
        vec!["keep current turn".to_string(), "after resume".to_string()]
    );
    assert_eq!(assistant_texts(&resumed_request), vec!["ack".to_string()]);
}

#[tokio::test]
async fn current_turn_anchor_is_preserved_during_compact() {
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 4_096)]);
    let first_turn_user = repeated_text('e', 900);
    let first_turn_assistant = repeated_text('f', 900);

    chat_provider.enqueue_script(assistant_script(&first_turn_assistant));
    compact_provider.enqueue_script(assistant_script("summary checkpoint"));
    chat_provider.enqueue_script(assistant_script("ack"));

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider.clone())
        .register_session_storage(FakeSessionStorage::default())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

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

    let compact_request = compact_provider
        .requests()
        .last()
        .cloned()
        .expect("compact request should exist");
    let regular_request = chat_provider
        .requests()
        .last()
        .cloned()
        .expect("second regular request should exist");

    assert!(
        user_texts(&compact_request)
            .iter()
            .all(|text| text != "keep current turn")
    );
    assert!(
        user_texts(&regular_request)
            .iter()
            .any(|text| text == "keep current turn")
    );
    assert!(
        user_texts(&compact_request)
            .iter()
            .any(|text| text == first_turn_user.as_str())
    );
}

#[tokio::test]
async fn before_compact_abort_skips_only_estimate_trigger() {
    let session_storage = FakeSessionStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 4_096)]);
    let first_turn_user = repeated_text('g', 900);
    let first_turn_assistant = repeated_text('h', 900);
    let (plugin, _handle) = FakePlugin::new(
        plugin_descriptor("abort-before-compact", vec![HookEvent::BeforeCompact]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredAbort(
            HookEvent::BeforeCompact,
            "skip compact".to_string(),
        ),
    );

    chat_provider.enqueue_script(assistant_script(&first_turn_assistant));
    chat_provider.enqueue_script(context_length_exceeded_script("request is too large"));

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider.clone())
        .register_session_storage(session_storage.clone())
        .register_plugin(plugin, json!({ "enabled": true }))
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

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
    assert!(matches!(
        outcome,
        TurnOutcome::Failed(AgentError::CompactError { message })
            if message.contains("BeforeCompact hook aborted fallback compaction")
    ));
    assert_eq!(compact_provider.chat_calls(), 0);
    assert_eq!(chat_provider.chat_calls(), 2);
    assert!(
        session_storage
            .saved_events(session.session_id())
            .iter()
            .all(|event| {
                !matches!(
                    event.payload,
                    LedgerEventPayload::Metadata {
                        key: MetadataKey::ContextCompaction,
                        ..
                    }
                )
            })
    );
}

#[tokio::test]
async fn context_length_exceeded_retry_happens_once() {
    let session_storage = FakeSessionStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 4_096)]);
    let first_turn_user = repeated_text('i', 900);
    let first_turn_assistant = repeated_text('j', 900);

    let mut config = base_config();
    config.compact_threshold = 0.99;

    chat_provider.enqueue_script(assistant_script(&first_turn_assistant));
    chat_provider.enqueue_script(context_length_exceeded_script("first attempt too large"));
    compact_provider.enqueue_script(assistant_script("summary checkpoint"));
    chat_provider.enqueue_script(context_length_exceeded_script("retry still too large"));

    let agent = AgentBuilder::new(config)
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider.clone())
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
    assert!(matches!(
        outcome,
        TurnOutcome::Failed(AgentError::CompactError { message })
            if message.contains("provider still rejected the request after fallback compaction")
    ));
    assert_eq!(chat_provider.chat_calls(), 3);
    assert_eq!(compact_provider.chat_calls(), 1);

    let compaction_events = session_storage
        .saved_events(session.session_id())
        .into_iter()
        .filter(|event| {
            matches!(
                event.payload,
                LedgerEventPayload::Metadata {
                    key: MetadataKey::ContextCompaction,
                    ..
                }
            )
        })
        .count();
    assert_eq!(compaction_events, 1);
}

#[tokio::test]
async fn incomplete_message_resume_appends_cancel_notice_without_writing_ledger() {
    let session_storage = FakeSessionStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 4_096)]);

    chat_provider.enqueue_script(vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta("partial".to_string())),
        FakeProviderStep::WaitForCancel,
    ]);
    chat_provider.enqueue_script(assistant_script("resumed"));

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider)
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let session_id = session.session_id().to_string();

    let mut turn = session
        .send_message(text_input("please stream"), None)
        .await
        .expect("turn should start");
    let _ = turn
        .events
        .next()
        .await
        .expect("first delta should arrive before cancel");
    turn.cancel();

    let outcome = finish_turn(turn).await;
    assert!(matches!(outcome, TurnOutcome::Cancelled));

    let saved_events = session_storage.saved_events(&session_id);
    assert!(saved_events.iter().any(|event| {
        matches!(
            &event.payload,
            LedgerEventPayload::AssistantMessage { content, status }
                if *status == MessageStatus::Incomplete
                    && matches!(content.first(), Some(ContentBlock::Text(text)) if text == "partial")
        )
    }));
    assert!(saved_events.iter().all(|event| {
        !matches!(
            &event.payload,
            LedgerEventPayload::SystemMessage { content } if content == RESUME_CANCEL_NOTICE
        )
    }));

    session.close().await.expect("session should close");

    let resumed = agent
        .resume_session(&session_id)
        .await
        .expect("session should resume");
    let outcome = finish_turn(
        resumed
            .send_message(text_input("after cancel"), None)
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
    let history = history_messages(&resumed_request);
    assert!(matches!(
        history.first(),
        Some(Message::User { content }) if content_text(content) == "please stream"
    ));
    assert!(matches!(
        history.get(1),
        Some(Message::Assistant { content, status })
            if content_text(content) == "partial" && *status == MessageStatus::Incomplete
    ));
    assert!(matches!(
        history.get(2),
        Some(Message::System { content }) if content == RESUME_CANCEL_NOTICE
    ));
    assert!(matches!(
        history.last(),
        Some(Message::User { content }) if content_text(content) == "after cancel"
    ));

    let saved_events_after_resume = session_storage.saved_events(&session_id);
    assert!(saved_events_after_resume.iter().all(|event| {
        !matches!(
            &event.payload,
            LedgerEventPayload::SystemMessage { content } if content == RESUME_CANCEL_NOTICE
        )
    }));
}

#[tokio::test]
async fn compact_marker_is_not_applied_if_persistence_fails() {
    let session_storage = FakeSessionStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 4_096)]);
    let first_turn_user = repeated_text('k', 900);
    let first_turn_assistant = repeated_text('l', 900);

    chat_provider.enqueue_script(assistant_script(&first_turn_assistant));
    compact_provider.enqueue_script(assistant_script("summary checkpoint"));
    session_storage.fail_on_metadata_key(MetadataKey::ContextCompaction);

    let agent = AgentBuilder::new(base_config())
        .register_model_provider(chat_provider.clone())
        .register_model_provider(compact_provider.clone())
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
    assert!(matches!(
        outcome,
        TurnOutcome::Failed(AgentError::StorageError(_))
    ));
    assert_eq!(chat_provider.chat_calls(), 1);
    assert_eq!(compact_provider.chat_calls(), 1);
    assert!(
        session_storage
            .saved_events(session.session_id())
            .iter()
            .all(|event| {
                !matches!(
                    event.payload,
                    LedgerEventPayload::Metadata {
                        key: MetadataKey::ContextCompaction,
                        ..
                    }
                )
            })
    );
}
