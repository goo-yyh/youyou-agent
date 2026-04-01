//! Skill 注册表与显式触发解析。

use indexmap::IndexMap;

use crate::domain::{ContentBlock, Message, Result, SkillDefinition, UserInput};

/// Skill 管理器。
///
/// 该组件只负责两件事：
/// 1. 从用户输入中解析显式 `/skill_name` 调用。
/// 2. 渲染被触发 Skill 的注入消息，以及返回隐式 Skill 列表。
#[derive(Debug, Clone, Default)]
pub struct SkillManager {
    /// 按注册顺序保存的 Skill 定义。
    skills: IndexMap<String, SkillDefinition>,
}

impl SkillManager {
    /// 使用给定 Skill 列表创建管理器。
    #[must_use]
    pub fn new(skills: impl IntoIterator<Item = SkillDefinition>) -> Self {
        let skills = skills
            .into_iter()
            .map(|skill| (skill.name.clone(), skill))
            .collect();

        Self { skills }
    }

    /// 从用户输入中解析显式 Skill 调用。
    ///
    /// 仅扫描 [`ContentBlock::Text`]，不会扫描图片或文件内容。
    ///
    /// 返回值分为两部分：
    /// 1. 已识别到的 Skill 定义，按出现顺序返回，允许重复。
    /// 2. 未识别的 Skill 名称，按出现顺序返回，允许重复。
    #[must_use]
    pub fn parse_invocations(&self, input: &UserInput) -> (Vec<&SkillDefinition>, Vec<String>) {
        let mut matched_skills = Vec::new();
        let mut unknown_skills = Vec::new();

        for block in &input.content {
            let ContentBlock::Text(text) = block else {
                continue;
            };

            for skill_name in parse_skill_names(text) {
                if let Some(skill) = self.skills.get(skill_name.as_str()) {
                    matched_skills.push(skill);
                } else {
                    unknown_skills.push(skill_name);
                }
            }
        }

        (matched_skills, unknown_skills)
    }

    /// 解析用户输入中的 Skill 调用。
    ///
    /// 未识别的 `/name` 片段会被忽略，以便将原始文本继续传给模型，
    /// 避免路径、接口路由等 slash token 阻断整个 turn。
    ///
    /// # Errors
    ///
    /// 当前不会因为未识别的 Skill 失败。保留 `Result` 仅用于维持现有 API 兼容。
    pub fn resolve_invocations(&self, input: &UserInput) -> Result<Vec<&SkillDefinition>> {
        let (matched_skills, _unknown_skills) = self.parse_invocations(input);

        Ok(matched_skills)
    }

    /// 将一个 Skill 渲染为模型可见的系统消息。
    #[must_use]
    pub fn render_injection(&self, skill: &SkillDefinition) -> Message {
        Message::System {
            content: format!(
                "<skill>\n<name>{}</name>\n{}\n</skill>",
                skill.name, skill.prompt_template
            ),
        }
    }

    /// 返回允许隐式展示给模型参考的 Skill 列表。
    #[must_use]
    pub fn implicit_skills(&self) -> Vec<&SkillDefinition> {
        self.skills
            .values()
            .filter(|skill| skill.allow_implicit_invocation)
            .collect()
    }
}

/// 从文本块中提取所有 `/skill_name` 形式的调用。
#[must_use]
fn parse_skill_names(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut skill_names = Vec::new();
    let mut index = 0;

    while index < characters.len() {
        let current = characters[index];
        let previous = index
            .checked_sub(1)
            .map(|previous_index| characters[previous_index]);

        if current == '/' && is_invocation_prefix(previous) {
            let mut cursor = index + 1;
            let mut skill_name = String::new();

            while cursor < characters.len() && is_skill_name_char(characters[cursor]) {
                skill_name.push(characters[cursor]);
                cursor += 1;
            }

            if !skill_name.is_empty() && is_invocation_suffix(&characters, cursor) {
                skill_names.push(skill_name);
                index = cursor;
                continue;
            }
        }

        index += 1;
    }

    skill_names
}

/// 判断 `/` 前一个字符是否允许开始一个 Skill 调用。
#[must_use]
fn is_invocation_prefix(previous: Option<char>) -> bool {
    previous.is_none_or(|character| {
        character.is_whitespace() || matches!(character, '(' | '[' | '{' | '"' | '\'')
    })
}

/// 判断 Skill 名称后的字符是否仍然符合显式调用语义。
///
/// 绝对路径、API 路由、文件名等更像“slash token”而不是显式 Skill 调用，
/// 这里通过后缀约束将它们排除。
#[must_use]
fn is_invocation_suffix(characters: &[char], cursor: usize) -> bool {
    let next = characters.get(cursor).copied();
    let following = characters.get(cursor + 1).copied();

    match next {
        None => true,
        Some(character)
            if character.is_whitespace()
                || matches!(
                    character,
                    ')' | ']' | '}' | '"' | '\'' | ',' | ':' | ';' | '!' | '?'
                ) =>
        {
            true
        }
        Some('.') => following.is_none_or(is_non_path_terminator),
        Some(character) => is_non_path_terminator(character),
    }
}

#[must_use]
fn is_non_path_terminator(character: char) -> bool {
    matches!(
        character,
        ')' | ']'
            | '}'
            | '"'
            | '\''
            | ','
            | ':'
            | ';'
            | '!'
            | '?'
            | '，'
            | '。'
            | '！'
            | '？'
            | '；'
            | '：'
            | '、'
    )
}

/// 判断字符是否可作为 Skill 名称的一部分。
#[must_use]
fn is_skill_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::SkillManager;
    use crate::domain::{ContentBlock, Message, SkillDefinition, UserInput};

    /// 构造测试使用的 Skill。
    fn sample_skill(name: &str, allow_implicit_invocation: bool) -> SkillDefinition {
        SkillDefinition {
            name: name.to_string(),
            display_name: name.to_string(),
            description: format!("skill {name}"),
            prompt_template: format!("use {name}"),
            required_tools: Vec::new(),
            allow_implicit_invocation,
        }
    }

    #[test]
    fn test_should_parse_known_and_unknown_skill_invocations() {
        let manager = SkillManager::new(vec![sample_skill("commit", true)]);
        let input = UserInput {
            content: vec![ContentBlock::Text(
                "/commit\nunknown: /missing\nplain text".to_string(),
            )],
        };

        let (matched, unknown) = manager.parse_invocations(&input);

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "commit");
        assert_eq!(unknown, vec!["missing".to_string()]);
    }

    #[test]
    fn test_should_render_skill_injection_as_system_message() {
        let manager = SkillManager::new(vec![sample_skill("review", true)]);
        let skill = manager.skills.get("review").expect("expected skill");

        let message = manager.render_injection(skill);

        assert!(matches!(message, Message::System { .. }));
    }

    #[test]
    fn test_should_not_parse_paths_as_skill_invocations() {
        let manager = SkillManager::new(vec![
            sample_skill("tmp", true),
            sample_skill("api", true),
            sample_skill("commit", true),
        ]);
        let input = UserInput {
            content: vec![ContentBlock::Text(
                "inspect /tmp/project and /api/v1/users, then run /commit.".to_string(),
            )],
        };

        let (matched, unknown) = manager.parse_invocations(&input);
        let matched_names = matched
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(matched_names, vec!["commit"]);
        assert!(unknown.is_empty());
    }
}
