mod support;

use chrono::{Duration, Utc};
use tokio_stream::StreamExt;
use youyou_agent::{
    AgentBuilder, AgentConfig, ChatError, ChatEvent, ChatRequest, ContentBlock, Memory, Message,
    ModelCapabilities, ModelInfo, RunningTurn, SessionConfig, TurnOutcome, UserInput,
};

use crate::support::fake_memory_storage::{FakeMemoryStorage, RecordedSearch};
use crate::support::fake_provider::{FakeProvider, FakeProviderStep};
use crate::support::fake_session_storage::FakeSessionStorage;

/// 构造 phase 7 测试使用的基础配置。
fn base_config(memory_namespace: &str) -> AgentConfig {
    let mut config = AgentConfig::new("chat-model", memory_namespace);
    config.compact_model = Some("compact-model".to_string());
    config.memory_model = Some("memory-model".to_string());
    config.compact_threshold = 0.99;
    config.memory_checkpoint_interval = 10;
    config.memory_max_items = 8;
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

/// 构造文本输入。
fn text_input(text: &str) -> UserInput {
    UserInput {
        content: vec![ContentBlock::Text(text.to_string())],
    }
}

/// 构造测试用记忆。
fn sample_memory(
    id: &str,
    namespace: &str,
    content: &str,
    updated_at: chrono::DateTime<Utc>,
) -> Memory {
    Memory {
        id: id.to_string(),
        namespace: namespace.to_string(),
        content: content.to_string(),
        source: "manual".to_string(),
        tags: vec!["test".to_string()],
        created_at: updated_at,
        updated_at,
    }
}

/// 返回一组正常完成的 provider 脚本。
fn assistant_script(text: &str) -> Vec<FakeProviderStep> {
    vec![
        FakeProviderStep::Emit(ChatEvent::TextDelta(text.to_string())),
        FakeProviderStep::Emit(ChatEvent::Done {
            usage: youyou_agent::TokenUsage::default(),
        }),
    ]
}

/// 返回一个 empty extraction JSON。
fn empty_extraction_script() -> Vec<FakeProviderStep> {
    assistant_script(r#"{"memoryOperations":[],"rolloutSummary":"","rolloutSlug":""}"#)
}

/// 返回一个 provider 错误脚本。
fn provider_error_script(message: &str) -> Vec<FakeProviderStep> {
    vec![FakeProviderStep::Emit(ChatEvent::Error(ChatError {
        message: message.to_string(),
        retryable: false,
        is_context_length_exceeded: false,
    }))]
}

/// 生成一个指定长度的长文本。
fn repeated_text(ch: char, len: usize) -> String {
    std::iter::repeat_n(ch, len).collect()
}

/// 构造一个带 chat/compact/memory provider 的 agent。
async fn build_agent(
    config: AgentConfig,
    chat_provider: FakeProvider,
    compact_provider: FakeProvider,
    memory_provider: FakeProvider,
    memory_storage: FakeMemoryStorage,
    session_storage: Option<FakeSessionStorage>,
) -> youyou_agent::Agent {
    let mut builder = AgentBuilder::new(config)
        .register_model_provider(chat_provider)
        .register_model_provider(compact_provider)
        .register_model_provider(memory_provider)
        .register_memory_storage(memory_storage);

    if let Some(session_storage) = session_storage {
        builder = builder.register_session_storage(session_storage);
    }

    builder.build().await.expect("agent should build")
}

/// 等待一个 turn 完成。
async fn finish_turn(mut turn: RunningTurn) -> TurnOutcome {
    while turn.events.next().await.is_some() {}
    turn.join().await.expect("turn should join successfully")
}

/// 读取 provider 首条系统消息文本。
fn system_prompt(request: &ChatRequest) -> &str {
    match request.messages.first() {
        Some(Message::System { content }) => content,
        other => panic!("expected leading system prompt, got {other:?}"),
    }
}

/// 提取记忆模型请求里的用户文本。
fn extraction_user_text(request: &ChatRequest) -> String {
    match request.messages.get(1) {
        Some(Message::User { content }) => content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text.clone(),
                ContentBlock::Image { .. } | ContentBlock::File { .. } => {
                    panic!("memory extraction should only use text input")
                }
            })
            .collect::<String>(),
        other => panic!("expected memory extraction user message, got {other:?}"),
    }
}

#[tokio::test]
async fn bootstrap_memories_follow_list_recent_order() {
    let memory_storage = FakeMemoryStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 8_192)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);
    let now = Utc::now();

    memory_storage.insert(sample_memory(
        "memory-1",
        "memory/test",
        "old note",
        now - Duration::minutes(2),
    ));
    memory_storage.insert(sample_memory("memory-2", "memory/test", "newest note", now));
    memory_storage.insert(sample_memory(
        "memory-3",
        "memory/test",
        "middle note",
        now - Duration::minutes(1),
    ));
    chat_provider.enqueue_script(assistant_script("ok"));

    let agent = build_agent(
        base_config("memory/test"),
        chat_provider.clone(),
        compact_provider,
        memory_provider,
        memory_storage.clone(),
        None,
    )
    .await;
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = finish_turn(
        session
            .send_message(text_input("hello"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let request = chat_provider
        .requests()
        .first()
        .cloned()
        .expect("chat request should exist");
    let prompt = system_prompt(&request);
    let newest_pos = prompt
        .find("[memory-2] newest note")
        .expect("newest memory should be rendered");
    let middle_pos = prompt
        .find("[memory-3] middle note")
        .expect("middle memory should be rendered");
    let oldest_pos = prompt
        .find("[memory-1] old note")
        .expect("oldest memory should be rendered");

    assert!(newest_pos < middle_pos);
    assert!(middle_pos < oldest_pos);
}

#[tokio::test]
async fn search_uses_only_explicit_text_blocks() {
    let memory_storage = FakeMemoryStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 8_192)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);

    chat_provider.enqueue_script(assistant_script("ok"));

    let agent = build_agent(
        base_config("memory/test"),
        chat_provider,
        compact_provider,
        memory_provider,
        memory_storage.clone(),
        None,
    )
    .await;
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let input = UserInput {
        content: vec![
            ContentBlock::Text("alpha".to_string()),
            ContentBlock::Image {
                data: "aGVsbG8=".to_string(),
                media_type: "image/png".to_string(),
            },
            ContentBlock::File {
                name: "note.txt".to_string(),
                media_type: "text/plain".to_string(),
                text: "file body".to_string(),
            },
            ContentBlock::Text("beta".to_string()),
        ],
    };

    let outcome = finish_turn(
        session
            .send_message(input, None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(
        memory_storage.search_calls(),
        vec![RecordedSearch {
            namespace: "memory/test".to_string(),
            query: "alpha\nbeta".to_string(),
            limit: 8,
        }]
    );
}

#[tokio::test]
async fn pure_image_or_file_turn_skips_memory_search() {
    let memory_storage = FakeMemoryStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 8_192)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);

    chat_provider.enqueue_script(assistant_script("image ok"));
    chat_provider.enqueue_script(assistant_script("file ok"));

    let agent = build_agent(
        base_config("memory/test"),
        chat_provider,
        compact_provider,
        memory_provider,
        memory_storage.clone(),
        None,
    )
    .await;
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let image_turn = UserInput {
        content: vec![ContentBlock::Image {
            data: "aGVsbG8=".to_string(),
            media_type: "image/png".to_string(),
        }],
    };
    let file_turn = UserInput {
        content: vec![ContentBlock::File {
            name: "note.txt".to_string(),
            media_type: "text/plain".to_string(),
            text: "body".to_string(),
        }],
    };

    let outcome = finish_turn(
        session
            .send_message(image_turn, None)
            .await
            .expect("image turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let outcome = finish_turn(
        session
            .send_message(file_turn, None)
            .await
            .expect("file turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert!(memory_storage.search_calls().is_empty());
}

#[tokio::test]
async fn checkpoint_uses_ledger_seq_not_message_index() {
    let mut config = base_config("memory/test");
    config.compact_threshold = 0.8;
    config.memory_checkpoint_interval = 1;

    let memory_storage = FakeMemoryStorage::default();
    let session_storage = FakeSessionStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 512)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);
    let first_turn_user = repeated_text('a', 900);
    let first_turn_assistant = repeated_text('b', 900);

    chat_provider.enqueue_script(assistant_script(&first_turn_assistant));
    memory_provider.enqueue_script(empty_extraction_script());
    compact_provider.enqueue_script(assistant_script("summary checkpoint"));
    chat_provider.enqueue_script(assistant_script("second assistant"));
    memory_provider.enqueue_script(empty_extraction_script());

    let agent = build_agent(
        config,
        chat_provider.clone(),
        compact_provider,
        memory_provider.clone(),
        memory_storage,
        Some(session_storage),
    )
    .await;
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
            .send_message(text_input("second turn"), None)
            .await
            .expect("second turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let second_request = memory_provider
        .requests()
        .get(1)
        .cloned()
        .expect("second checkpoint request should exist");
    let extraction_text = extraction_user_text(&second_request);

    assert!(extraction_text.contains("second turn"));
    assert!(extraction_text.contains("second assistant"));
    assert!(!extraction_text.contains(first_turn_user.as_str()));
    assert!(!extraction_text.contains(first_turn_assistant.as_str()));
    assert!(!extraction_text.contains("summary checkpoint"));
}

#[tokio::test]
async fn update_missing_target_id_degrades_to_create() {
    let mut config = base_config("memory/test");
    config.memory_checkpoint_interval = 1;

    let memory_storage = FakeMemoryStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 8_192)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);

    chat_provider.enqueue_script(assistant_script("ok"));
    memory_provider.enqueue_script(assistant_script(
        r#"{"memoryOperations":[{"action":"update","targetId":"missing-id","content":"remember this","tags":["phase7"]}],"rolloutSummary":"","rolloutSlug":""}"#,
    ));

    let agent = build_agent(
        config,
        chat_provider,
        compact_provider,
        memory_provider,
        memory_storage.clone(),
        None,
    )
    .await;
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = finish_turn(
        session
            .send_message(text_input("hello"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let memories = memory_storage.memories();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "remember this");
    assert_eq!(memories[0].source, "checkpoint");
    assert_eq!(memories[0].namespace, "memory/test");
    assert_eq!(memories[0].tags, vec!["phase7".to_string()]);
    assert_ne!(memories[0].id, "missing-id");
}

#[tokio::test]
async fn delete_missing_target_id_is_ignored() {
    let mut config = base_config("memory/test");
    config.memory_checkpoint_interval = 1;

    let memory_storage = FakeMemoryStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 8_192)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);
    let now = Utc::now();

    memory_storage.insert(sample_memory("keep-id", "memory/test", "keep this", now));
    chat_provider.enqueue_script(assistant_script("ok"));
    memory_provider.enqueue_script(assistant_script(
        r#"{"memoryOperations":[{"action":"delete","targetId":"missing-id"}],"rolloutSummary":"","rolloutSlug":""}"#,
    ));

    let agent = build_agent(
        config,
        chat_provider,
        compact_provider,
        memory_provider,
        memory_storage.clone(),
        None,
    )
    .await;
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = finish_turn(
        session
            .send_message(text_input("hello"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    let memories = memory_storage.memories();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].id, "keep-id");
    assert_eq!(memories[0].content, "keep this");
}

#[tokio::test]
async fn close_extraction_failure_does_not_block_session_close() {
    let mut config = base_config("memory/test");
    config.memory_checkpoint_interval = 100;

    let memory_storage = FakeMemoryStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 8_192)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);

    chat_provider.enqueue_script(assistant_script("ok"));
    memory_provider.enqueue_script(provider_error_script("close extraction failed"));

    let agent = build_agent(
        config,
        chat_provider,
        compact_provider,
        memory_provider.clone(),
        memory_storage,
        None,
    )
    .await;
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let outcome = finish_turn(
        session
            .send_message(text_input("hello"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));

    session
        .close()
        .await
        .expect("close should not be blocked by extraction failure");
    assert_eq!(memory_provider.chat_calls(), 1);

    let next_session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("slot should be released after close");
    assert!(!next_session.session_id().is_empty());
}

#[tokio::test]
async fn resume_reuses_pinned_memory_namespace_from_ledger() {
    let session_storage = FakeSessionStorage::default();
    let memory_storage = FakeMemoryStorage::default();
    let chat_provider = FakeProvider::new("chat-provider", vec![model_info("chat-model", 8_192)]);
    let compact_provider =
        FakeProvider::new("compact-provider", vec![model_info("compact-model", 8_192)]);
    let memory_provider =
        FakeProvider::new("memory-provider", vec![model_info("memory-model", 8_192)]);

    let agent = build_agent(
        base_config("memory/original"),
        chat_provider.clone(),
        compact_provider.clone(),
        memory_provider.clone(),
        memory_storage.clone(),
        Some(session_storage.clone()),
    )
    .await;
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");
    let session_id = session.session_id().to_string();
    session.close().await.expect("session should close");

    chat_provider.enqueue_script(assistant_script("ok"));

    let resumed_agent = build_agent(
        base_config("memory/changed"),
        chat_provider,
        compact_provider,
        memory_provider,
        memory_storage.clone(),
        Some(session_storage),
    )
    .await;
    let resumed = resumed_agent
        .resume_session(&session_id)
        .await
        .expect("session should resume");

    let outcome = finish_turn(
        resumed
            .send_message(text_input("after resume"), None)
            .await
            .expect("turn should start"),
    )
    .await;
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(
        memory_storage
            .list_recent_namespaces()
            .last()
            .map(String::as_str),
        Some("memory/original")
    );
    assert_eq!(
        memory_storage
            .search_calls()
            .last()
            .map(|call| call.namespace.as_str()),
        Some("memory/original")
    );
}
