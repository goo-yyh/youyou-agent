mod support;

use serde_json::json;
use youyou_agent::{
    AgentBuilder, AgentConfig, AgentError, HookEvent, ModelCapabilities, ModelInfo,
    PluginDescriptor, SkillDefinition,
};

use crate::support::fake_memory_storage::FakeMemoryStorage;
use crate::support::fake_plugin::{FakePlugin, FakePluginApplyBehavior};
use crate::support::fake_provider::FakeProvider;
use crate::support::fake_session_storage::FakeSessionStorage;
use crate::support::fake_tool::FakeTool;

fn base_config() -> AgentConfig {
    AgentConfig::new("model-a", "memory/test")
}

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

fn sample_skill(name: &str, required_tools: Vec<&str>) -> SkillDefinition {
    SkillDefinition {
        name: name.to_string(),
        display_name: name.to_string(),
        description: format!("skill {name}"),
        prompt_template: "Use the skill.".to_string(),
        required_tools: required_tools.into_iter().map(str::to_string).collect(),
        allow_implicit_invocation: true,
    }
}

fn sample_plugin_descriptor(id: &str, tapped_hooks: Vec<HookEvent>) -> PluginDescriptor {
    PluginDescriptor {
        id: id.to_string(),
        display_name: id.to_string(),
        description: format!("plugin {id}"),
        tapped_hooks,
    }
}

#[tokio::test]
async fn rejects_missing_model_provider() {
    let result = AgentBuilder::new(base_config()).build().await;

    assert!(matches!(result, Err(AgentError::NoModelProvider)));
}

#[tokio::test]
async fn rejects_duplicate_model_id_across_providers() {
    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_model_provider(FakeProvider::new("provider-b", vec![model_info("model-a")]))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::NameConflict {
            kind: "model",
            name,
        }) if name == "model-a"
    ));
}

#[tokio::test]
async fn rejects_duplicate_tool_name() {
    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_tool(FakeTool::new("tool-a", "tool", false))
        .register_tool(FakeTool::new("tool-a", "tool", false))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::NameConflict {
            kind: "tool",
            name,
        }) if name == "tool-a"
    ));
}

#[tokio::test]
async fn rejects_duplicate_skill_name() {
    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_skill(sample_skill("skill-a", Vec::new()))
        .register_skill(sample_skill("skill-a", Vec::new()))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::NameConflict {
            kind: "skill",
            name,
        }) if name == "skill-a"
    ));
}

#[tokio::test]
async fn rejects_invalid_default_model() {
    let mut config = base_config();
    config.default_model = "missing-model".to_string();

    let result = AgentBuilder::new(config)
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::InvalidDefaultModel(model_id)) if model_id == "missing-model"
    ));
}

#[tokio::test]
async fn rejects_invalid_compact_threshold() {
    let mut config = base_config();
    config.compact_threshold = 1.2;

    let result = AgentBuilder::new(config)
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::InputValidation { message })
            if message.contains("compact_threshold")
    ));
}

#[tokio::test]
async fn rejects_invalid_memory_namespace() {
    let mut config = base_config();
    config.memory_namespace = "   ".to_string();

    let result = AgentBuilder::new(config)
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::InputValidation { message })
            if message.contains("memory_namespace")
    ));
}

#[tokio::test]
async fn rejects_skill_missing_tool_dependency() {
    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_skill(sample_skill("skill-a", vec!["missing-tool"]))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::SkillDependencyNotMet { skill, tool })
            if skill == "skill-a" && tool == "missing-tool"
    ));
}

#[tokio::test]
async fn plugin_initialize_failure_rolls_back_prior_plugins() {
    let (plugin_ok, handle_ok) = FakePlugin::new(
        sample_plugin_descriptor("plugin-ok", vec![HookEvent::TurnStart]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclared(HookEvent::TurnStart),
    );
    let (plugin_bad, handle_bad) = FakePlugin::new(
        sample_plugin_descriptor("plugin-bad", Vec::new()),
        Some("boom".to_string()),
        None,
        FakePluginApplyBehavior::Noop,
    );

    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_plugin(plugin_ok, json!({"enabled": true}))
        .register_plugin(plugin_bad, json!({"enabled": true}))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::PluginInitFailed { id, .. }) if id == "plugin-bad"
    ));
    assert_eq!(handle_ok.initialize_calls(), 1);
    assert_eq!(handle_ok.shutdown_calls(), 1);
    assert_eq!(handle_bad.initialize_calls(), 1);
    assert_eq!(handle_bad.shutdown_calls(), 0);
}

#[tokio::test]
async fn plugin_apply_failure_rolls_back_initialized_plugins() {
    let (plugin_ok, handle_ok) = FakePlugin::new(
        sample_plugin_descriptor("plugin-ok", vec![HookEvent::TurnStart]),
        None,
        None,
        FakePluginApplyBehavior::RegisterDeclared(HookEvent::TurnStart),
    );
    let (plugin_bad, handle_bad) = FakePlugin::new(
        sample_plugin_descriptor("plugin-bad", vec![HookEvent::SessionStart]),
        None,
        None,
        FakePluginApplyBehavior::RegisterUndeclared(HookEvent::TurnStart),
    );

    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_plugin(plugin_ok, json!({}))
        .register_plugin(plugin_bad, json!({}))
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::PluginHookContractViolation { plugin_id, .. })
            if plugin_id == "plugin-bad"
    ));
    assert_eq!(handle_ok.initialize_calls(), 1);
    assert_eq!(handle_ok.shutdown_calls(), 1);
    assert_eq!(handle_bad.initialize_calls(), 1);
    assert_eq!(handle_bad.shutdown_calls(), 1);
    assert_eq!(handle_bad.apply_calls(), 1);
}

#[tokio::test]
async fn rejects_duplicate_session_storage() {
    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_session_storage(FakeSessionStorage::default())
        .register_session_storage(FakeSessionStorage::default())
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::StorageDuplicate { kind }) if kind == "session"
    ));
}

#[tokio::test]
async fn rejects_duplicate_memory_storage() {
    let result = AgentBuilder::new(base_config())
        .register_model_provider(FakeProvider::new("provider-a", vec![model_info("model-a")]))
        .register_memory_storage(FakeMemoryStorage::default())
        .register_memory_storage(FakeMemoryStorage::default())
        .build()
        .await;

    assert!(matches!(
        result,
        Err(AgentError::StorageDuplicate { kind }) if kind == "memory"
    ));
}
