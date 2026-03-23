mod support;

use serde_json::json;
use youyou_agent::{
    AgentBuilder, AgentConfig, AgentError, HookEvent, MetadataKey, ModelCapabilities, ModelInfo,
    PluginDescriptor, SessionConfig, SessionSearchQuery,
};

use crate::support::fake_memory_storage::FakeMemoryStorage;
use crate::support::fake_plugin::{FakePlugin, FakePluginApplyBehavior};
use crate::support::fake_provider::FakeProvider;
use crate::support::fake_session_storage::FakeSessionStorage;

/// 返回 phase 2 测试使用的基础配置。
fn base_config(default_model: &str, memory_namespace: &str) -> AgentConfig {
    AgentConfig::new(default_model, memory_namespace)
}

/// 构造一个带通用能力的伪造模型。
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

/// 构造一个最小 plugin 描述。
fn plugin_descriptor(id: &str, tapped_hooks: Vec<HookEvent>) -> PluginDescriptor {
    PluginDescriptor {
        id: id.to_string(),
        display_name: id.to_string(),
        description: id.to_string(),
        tapped_hooks,
    }
}

#[tokio::test]
async fn new_session_claims_slot_with_reservation() {
    let agent = AgentBuilder::new(base_config("model-a", "memory/test"))
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .build()
        .await
        .expect("agent should build");

    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    assert!(!session.session_id().is_empty());
    assert!(matches!(
        agent.new_session(SessionConfig::default()).await,
        Err(AgentError::SessionBusy)
    ));
}

#[tokio::test]
async fn session_start_abort_rolls_back_reserved_slot() {
    let (plugin, _handle) = FakePlugin::new(
        plugin_descriptor("abort-on-start", vec![HookEvent::SessionStart]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclaredAbort(
            HookEvent::SessionStart,
            "blocked by plugin".to_string(),
        ),
    );
    let agent = AgentBuilder::new(base_config("model-a", "memory/test"))
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
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
async fn resume_restores_pinned_model_and_memory_namespace() {
    let session_storage = FakeSessionStorage::default();
    let memory_storage = FakeMemoryStorage::default();
    let agent = AgentBuilder::new(base_config("model-a", "memory/original"))
        .register_model_provider(FakeProvider::new(
            "provider-a",
            vec![model_info("model-a"), model_info("model-b")],
        ))
        .register_session_storage(session_storage.clone())
        .register_memory_storage(memory_storage.clone())
        .build()
        .await
        .expect("agent should build");

    let session = agent
        .new_session(SessionConfig {
            model_id: Some("model-b".to_string()),
            system_prompt_override: Some("override prompt".to_string()),
        })
        .await
        .expect("session should be created");
    let session_id = session.session_id().to_string();

    session.close().await.expect("session should close cleanly");

    let resumed_agent = AgentBuilder::new(base_config("model-a", "memory/changed"))
        .register_model_provider(FakeProvider::new(
            "provider-a",
            vec![model_info("model-a"), model_info("model-b")],
        ))
        .register_session_storage(session_storage.clone())
        .register_memory_storage(memory_storage.clone())
        .build()
        .await
        .expect("resumed agent should build");

    let resumed = resumed_agent
        .resume_session(&session_id)
        .await
        .expect("session should resume");

    assert_eq!(resumed.model_id(), "model-b");
    assert_eq!(resumed.memory_namespace(), "memory/original");
    assert_eq!(resumed.system_prompt_override(), Some("override prompt"));
    assert_eq!(
        memory_storage
            .list_recent_namespaces()
            .last()
            .map(String::as_str),
        Some("memory/original")
    );
}

#[tokio::test]
async fn critical_metadata_persist_failure_aborts_session_creation() {
    let session_storage = FakeSessionStorage::default();
    session_storage.fail_on_metadata_key(MetadataKey::SessionConfig);

    let agent = AgentBuilder::new(base_config("model-a", "memory/test"))
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");

    let creation = agent.new_session(SessionConfig::default()).await;

    assert!(matches!(creation, Err(AgentError::StorageError(_))));
    assert!(session_storage.summaries().is_empty());

    session_storage.clear_failures();
    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should succeed after clearing failure");
    assert!(!session.session_id().is_empty());
}

#[tokio::test]
async fn session_busy_when_active_session_exists() {
    let session_storage = FakeSessionStorage::default();
    let agent = AgentBuilder::new(base_config("model-a", "memory/test"))
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");

    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    assert!(matches!(
        agent.resume_session(session.session_id()).await,
        Err(AgentError::SessionBusy)
    ));
}

#[tokio::test]
async fn delete_active_session_returns_session_busy() {
    let session_storage = FakeSessionStorage::default();
    let agent = AgentBuilder::new(base_config("model-a", "memory/test"))
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");

    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    assert!(matches!(
        agent.delete_session(session.session_id()).await,
        Err(AgentError::SessionBusy)
    ));
}

#[tokio::test]
async fn discovery_apis_require_session_storage() {
    let agent = AgentBuilder::new(base_config("model-a", "memory/test"))
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .build()
        .await
        .expect("agent should build");

    assert!(matches!(
        agent.list_sessions(None, 10).await,
        Err(AgentError::StorageError(error))
            if error.to_string().contains("SessionStorage not registered")
    ));
    assert!(matches!(
        agent.find_sessions(&SessionSearchQuery::IdPrefix("abc".to_string()))
            .await,
        Err(AgentError::StorageError(error))
            if error.to_string().contains("SessionStorage not registered")
    ));
    assert!(matches!(
        agent.delete_session("abc").await,
        Err(AgentError::StorageError(error))
            if error.to_string().contains("SessionStorage not registered")
    ));
    assert!(matches!(
        agent.resume_session("abc").await,
        Err(AgentError::StorageError(error))
            if error.to_string().contains("SessionStorage not registered")
    ));
}

#[tokio::test]
async fn summary_is_eventually_consistent_with_ledger() {
    let session_storage = FakeSessionStorage::default();
    let agent = AgentBuilder::new(base_config("model-a", "memory/test"))
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_session_storage(session_storage.clone())
        .build()
        .await
        .expect("agent should build");

    let session = agent
        .new_session(SessionConfig::default())
        .await
        .expect("session should be created");

    let page = agent
        .list_sessions(None, 10)
        .await
        .expect("sessions should list");
    let summary = page
        .sessions
        .iter()
        .find(|summary| summary.session_id == session.session_id())
        .expect("summary should exist");
    let saved_events = session_storage.saved_events(session.session_id());
    let last_event = saved_events.last().expect("metadata events should exist");

    assert_eq!(saved_events.len(), 2);
    assert_eq!(summary.updated_at, last_event.timestamp);
    assert_eq!(summary.message_count, 0);
}
