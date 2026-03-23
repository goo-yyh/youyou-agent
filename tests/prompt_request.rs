use chrono::Utc;
use youyou_agent::application::prompt_builder::{PromptBuildContext, PromptBuilder};
use youyou_agent::application::request_builder::{
    ChatRequestBuilder, RequestBuildOptions, RequestContext, ResolvedSessionConfig,
};
use youyou_agent::application::skill_manager::SkillManager;
use youyou_agent::{
    AgentConfig, AgentError, ContentBlock, EnvironmentContext, Memory, Message, ModelCapabilities,
    NetworkContext, PluginDescriptor, SkillDefinition, ToolDefinition, UserInput,
};

/// 构造 phase 3 测试用的基础配置。
fn base_config() -> AgentConfig {
    let mut config = AgentConfig::new("model-a", "memory/test");
    config.system_instructions = vec!["You are the project agent.".to_string()];
    config.personality = Some("请直接、简洁、务实。".to_string());
    config.environment_context = Some(EnvironmentContext {
        cwd: Some("/tmp/project".into()),
        shell: Some("/bin/zsh".to_string()),
        current_date: Some("2026-03-24".to_string()),
        timezone: Some("Asia/Shanghai".to_string()),
        network: None,
        subagents: None,
    });
    config
}

/// 构造测试使用的 Skill。
fn sample_skill(name: &str, allow_implicit_invocation: bool) -> SkillDefinition {
    SkillDefinition {
        name: name.to_string(),
        display_name: name.to_string(),
        description: format!("skill {name}"),
        prompt_template: format!("Use {name}."),
        required_tools: Vec::new(),
        allow_implicit_invocation,
    }
}

/// 构造测试使用的 Plugin。
fn sample_plugin(id: &str) -> PluginDescriptor {
    PluginDescriptor {
        id: id.to_string(),
        display_name: format!("Plugin {id}"),
        description: format!("plugin {id}"),
        tapped_hooks: Vec::new(),
    }
}

/// 构造测试使用的记忆。
fn sample_memory(id: &str, content: &str) -> Memory {
    Memory {
        id: id.to_string(),
        namespace: "memory/test".to_string(),
        content: content.to_string(),
        source: "unit-test".to_string(),
        tags: vec!["phase3".to_string()],
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// 构造测试使用的 Tool 定义。
fn sample_tool_definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("tool {name}"),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
        }),
    }
}

#[test]
fn implicit_skill_list_only_contains_allow_implicit_invocation() {
    let manager = SkillManager::new(vec![
        sample_skill("commit", true),
        sample_skill("review", false),
    ]);

    let implicit = manager.implicit_skills();
    let prompt = PromptBuilder::new().build(
        &base_config(),
        None,
        &PromptBuildContext {
            implicit_skills: implicit.into_iter().cloned().collect(),
            ..PromptBuildContext::default()
        },
    );

    assert_eq!(prompt.text.matches("- commit:").count(), 1);
    assert!(!prompt.text.contains("- review:"));
}

#[test]
fn unknown_skill_fails_before_turn_starts() {
    let manager = SkillManager::new(vec![sample_skill("commit", true)]);
    let input = UserInput {
        content: vec![ContentBlock::Text("/missing".to_string())],
    };

    let result = manager.resolve_invocations(&input);

    assert!(matches!(
        result,
        Err(AgentError::SkillNotFound(name)) if name == "missing"
    ));
}

#[test]
fn system_prompt_sections_follow_requirement_order() {
    let rendered = PromptBuilder::new().build(
        &base_config(),
        Some("session override"),
        &PromptBuildContext {
            implicit_skills: vec![sample_skill("commit", true)],
            plugins: vec![sample_plugin("lint")],
            memories: vec![sample_memory("memory-1", "remember the build command")],
            dynamic_sections: vec!["<dynamic>extra</dynamic>".to_string()],
        },
    );

    assert_ordered(
        &rendered.text,
        &[
            "<system_instructions>",
            "<system_prompt_override>",
            "<personality_spec>",
            "## Skills",
            "## Plugins",
            "## Memory",
            "<environment_context>",
            "<dynamic>extra</dynamic>",
        ],
    );
}

#[test]
fn tool_definitions_do_not_appear_in_prompt_text() {
    let tool_name = "tool-only-in-request";
    let rendered = PromptBuilder::new().build(&base_config(), None, &PromptBuildContext::default());
    let request = ChatRequestBuilder::new()
        .build(
            &rendered,
            &RequestContext {
                messages: vec![Message::User {
                    content: vec![ContentBlock::Text("hello".to_string())],
                }],
                model_capabilities: ModelCapabilities {
                    tool_use: true,
                    vision: true,
                    streaming: true,
                },
                tool_definitions: vec![sample_tool_definition(tool_name)],
            },
            &ResolvedSessionConfig {
                model_id: "model-a".to_string(),
                system_prompt_override: None,
            },
            &RequestBuildOptions::default(),
        )
        .expect("request should build");

    assert!(!rendered.text.contains(tool_name));
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, tool_name);
}

#[test]
fn environment_context_serializes_to_expected_xml() {
    let context = EnvironmentContext {
        cwd: Some("/tmp/project".into()),
        shell: Some("/bin/zsh".to_string()),
        current_date: Some("2026-03-24".to_string()),
        timezone: Some("Asia/Shanghai".to_string()),
        network: Some(NetworkContext {
            allowed_domains: vec!["example.com".to_string()],
            denied_domains: vec!["blocked.example".to_string()],
        }),
        subagents: Some("- worker-1: Atlas".to_string()),
    };

    let xml = context.serialize_to_xml();

    assert_eq!(
        xml,
        "<environment_context>\n  <cwd>/tmp/project</cwd>\n  <shell>/bin/zsh</shell>\n  <current_date>2026-03-24</current_date>\n  <timezone>Asia/Shanghai</timezone>\n  <network enabled=\"true\">\n    <allowed>example.com</allowed>\n    <denied>blocked.example</denied>\n  </network>\n  <subagents>\n    - worker-1: Atlas\n  </subagents>\n</environment_context>"
    );
}

#[test]
fn personality_is_wrapped_with_personality_spec_tag() {
    let rendered = PromptBuilder::new().build(&base_config(), None, &PromptBuildContext::default());

    assert!(rendered.text.contains(
        "<personality_spec>\nUser has requested new communication style. Follow the instructions below:\n\n请直接、简洁、务实。\n</personality_spec>"
    ));
}

#[test]
fn allow_tools_false_sends_empty_tools() {
    let request = ChatRequestBuilder::new()
        .build(
            &PromptBuilder::new().build(&base_config(), None, &PromptBuildContext::default()),
            &RequestContext {
                messages: vec![Message::User {
                    content: vec![ContentBlock::Text("hello".to_string())],
                }],
                model_capabilities: ModelCapabilities {
                    tool_use: true,
                    vision: true,
                    streaming: true,
                },
                tool_definitions: vec![sample_tool_definition("search")],
            },
            &ResolvedSessionConfig {
                model_id: "model-a".to_string(),
                system_prompt_override: None,
            },
            &RequestBuildOptions { allow_tools: false },
        )
        .expect("request should build");

    assert!(request.tools.is_empty());
}

#[test]
fn image_requires_vision_capability() {
    let result = ChatRequestBuilder::new().build(
        &PromptBuilder::new().build(&base_config(), None, &PromptBuildContext::default()),
        &RequestContext {
            messages: vec![Message::User {
                content: vec![ContentBlock::Image {
                    data: "ZmFrZQ==".to_string(),
                    media_type: "image/png".to_string(),
                }],
            }],
            model_capabilities: ModelCapabilities {
                tool_use: true,
                vision: false,
                streaming: true,
            },
            tool_definitions: Vec::new(),
        },
        &ResolvedSessionConfig {
            model_id: "model-a".to_string(),
            system_prompt_override: None,
        },
        &RequestBuildOptions::default(),
    );

    assert!(matches!(
        result,
        Err(AgentError::InputValidation { message })
            if message.contains("does not support image input")
    ));
}

#[test]
fn file_content_uses_text_channel_without_vision() {
    let request = ChatRequestBuilder::new()
        .build(
            &PromptBuilder::new().build(&base_config(), None, &PromptBuildContext::default()),
            &RequestContext {
                messages: vec![Message::User {
                    content: vec![ContentBlock::File {
                        name: "README.md".to_string(),
                        media_type: "text/markdown".to_string(),
                        text: "# Title".to_string(),
                    }],
                }],
                model_capabilities: ModelCapabilities {
                    tool_use: true,
                    vision: false,
                    streaming: true,
                },
                tool_definitions: Vec::new(),
            },
            &ResolvedSessionConfig {
                model_id: "model-a".to_string(),
                system_prompt_override: None,
            },
            &RequestBuildOptions::default(),
        )
        .expect("file inputs should not require vision");

    let Some(Message::User { content }) = request.messages.get(1) else {
        panic!("expected the normalized user message");
    };

    assert!(
        content
            .iter()
            .all(|block| matches!(block, ContentBlock::Text(_)))
    );
    assert!(matches!(
        content.first(),
        Some(ContentBlock::Text(text))
            if text.contains("<file>") && text.contains("<name>README.md</name>")
    ));
}

/// 断言多个片段在文本中按顺序出现。
fn assert_ordered(haystack: &str, needles: &[&str]) {
    let mut previous_index = 0;

    for needle in needles {
        let index = haystack[previous_index..].find(needle).map_or_else(
            || panic!("needle '{needle}' not found in prompt:\n{haystack}"),
            |offset| previous_index + offset,
        );
        assert!(
            index >= previous_index,
            "needle '{needle}' appeared out of order in prompt:\n{haystack}"
        );
        previous_index = index + needle.len();
    }
}
