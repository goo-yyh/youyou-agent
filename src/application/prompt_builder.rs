//! System prompt 文本渲染器。

use crate::domain::{AgentConfig, Memory, SkillDefinition};
use crate::ports::PluginDescriptor;
use crate::prompt::templates;

/// `PromptBuilder` 的一次构建输入。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptBuildContext {
    /// 允许模型参考的隐式 Skill 列表。
    pub implicit_skills: Vec<SkillDefinition>,
    /// 当前会话中已激活的 Plugin 列表。
    pub plugins: Vec<PluginDescriptor>,
    /// 本轮需要注入的记忆列表。
    pub memories: Vec<Memory>,
    /// 由 `TurnStart` hook 追加的动态段落。
    pub dynamic_sections: Vec<String>,
}

/// Prompt 渲染结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedPrompt {
    /// 最终渲染后的完整 system prompt 文本。
    pub text: String,
}

/// 无状态的 system prompt 渲染器。
#[derive(Debug, Clone, Copy, Default)]
pub struct PromptBuilder;

impl PromptBuilder {
    /// 创建一个新的 `PromptBuilder`。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 按规范顺序渲染 system prompt。
    #[must_use]
    pub fn build(
        &self,
        agent_config: &AgentConfig,
        system_prompt_override: Option<&str>,
        context: &PromptBuildContext,
    ) -> RenderedPrompt {
        let mut sections = Vec::new();

        if let Some(section) = render_system_instructions(&agent_config.system_instructions) {
            sections.push(section);
        }

        if let Some(override_text) = normalize_optional_text(system_prompt_override) {
            sections.push(render_tagged_section(
                "system_prompt_override",
                override_text,
            ));
        }

        if let Some(personality) = normalize_optional_text(agent_config.personality.as_deref()) {
            sections.push(render_personality_section(personality));
        }

        if let Some(section) = render_skill_list_section(&context.implicit_skills) {
            sections.push(section);
        }

        if let Some(section) = render_plugin_list_section(&context.plugins) {
            sections.push(section);
        }

        if let Some(section) = render_memories_section(&context.memories) {
            sections.push(section);
        }

        if let Some(environment_context) = agent_config.environment_context.as_ref() {
            sections.push(environment_context.serialize_to_xml());
        }

        sections.extend(
            context
                .dynamic_sections
                .iter()
                .filter(|section| !section.trim().is_empty())
                .cloned(),
        );

        RenderedPrompt {
            text: sections.join("\n\n"),
        }
    }
}

/// 规范化可选文本。
#[must_use]
fn normalize_optional_text(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|text| !text.is_empty())
}

/// 渲染系统指令段落。
#[must_use]
fn render_system_instructions(system_instructions: &[String]) -> Option<String> {
    let instructions = system_instructions
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|instruction| !instruction.is_empty())
        .collect::<Vec<_>>();

    if instructions.is_empty() {
        return None;
    }

    Some(render_tagged_section(
        "system_instructions",
        &instructions.join("\n\n"),
    ))
}

/// 渲染通用 XML 包裹段落。
#[must_use]
fn render_tagged_section(tag: &str, body: &str) -> String {
    format!("<{tag}>\n{body}\n</{tag}>")
}

/// 渲染 personality 段落。
#[must_use]
fn render_personality_section(personality: &str) -> String {
    format!(
        "<personality_spec>\nUser has requested new communication style. Follow the instructions below:\n\n{personality}\n</personality_spec>",
    )
}

/// 渲染隐式 Skill 列表段落。
#[must_use]
fn render_skill_list_section(skills: &[SkillDefinition]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut lines = vec![
        "## Skills".to_string(),
        String::new(),
        "Below is the list of skills available in this session. Each skill is a".to_string(),
        "reusable prompt template that can be invoked by the user.".to_string(),
        String::new(),
        "### Available skills".to_string(),
    ];

    lines.extend(
        skills
            .iter()
            .map(|skill| format!("- {}: {}", skill.name, skill.description)),
    );

    lines.extend([
        String::new(),
        "### How to use skills".to_string(),
        "- Trigger rules: Skills are activated ONLY when the user explicitly".to_string(),
        "  uses /skill_name syntax. Do not activate skills on your own.".to_string(),
        "- If the user invokes a skill, follow the skill's prompt instructions".to_string(),
        "  for that turn. Multiple invocations mean use them all.".to_string(),
        "- If the user writes /something that is not in the skill list,".to_string(),
        "  treat it as regular user text and continue with the best fallback.".to_string(),
        "- Suggestion: If a task clearly matches a skill's description, you".to_string(),
        "  may suggest the user invoke it (e.g., \"you can use /commit for".to_string(),
        "  this\"), but do NOT invoke it yourself.".to_string(),
        "- Coordination: If multiple skills apply, choose the minimal set and".to_string(),
        "  state the order.".to_string(),
    ]);

    Some(lines.join("\n"))
}

/// 渲染 Plugin 列表段落。
#[must_use]
fn render_plugin_list_section(plugins: &[PluginDescriptor]) -> Option<String> {
    if plugins.is_empty() {
        return None;
    }

    let mut lines = vec![
        "## Plugins".to_string(),
        String::new(),
        "The following plugins are active in this session. Plugins extend the".to_string(),
        "agent's capabilities by hooking into lifecycle events.".to_string(),
        String::new(),
        "### Active plugins".to_string(),
    ];

    lines.extend(plugins.iter().map(|plugin| {
        format!(
            "- {} ({}): {}",
            plugin.id, plugin.display_name, plugin.description
        )
    }));

    Some(lines.join("\n"))
}

/// 渲染记忆段落。
#[must_use]
fn render_memories_section(memories: &[Memory]) -> Option<String> {
    if memories.is_empty() {
        return None;
    }

    let rendered_memories = memories
        .iter()
        .map(render_memory_item)
        .collect::<Vec<_>>()
        .join("\n");

    Some(templates::render_memory_read_path(&rendered_memories))
}

/// 渲染单条记忆。
#[must_use]
fn render_memory_item(memory: &Memory) -> String {
    let tags_suffix = if memory.tags.is_empty() {
        String::new()
    } else {
        format!("; tags: {}", memory.tags.join(", "))
    };

    format!(
        "- [{}] {} (source: {}{})",
        memory.id, memory.content, memory.source, tags_suffix,
    )
}
