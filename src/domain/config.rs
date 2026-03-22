//! Agent 共享的配置类型。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 构建 [`crate::Agent`] 时传入的静态配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    /// 新建会话时默认使用的模型标识。
    pub default_model: String,
    /// 按顺序拼接的项目级系统指令。
    #[serde(default)]
    pub system_instructions: Vec<String>,
    /// 可选的人设与行为风格提示，会注入 system prompt。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    /// 可选的运行时环境事实，会暴露给模型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_context: Option<EnvironmentContext>,
    /// Tool 执行超时时间，单位毫秒。
    pub tool_timeout_ms: u64,
    /// 单轮允许的最大 Tool 调用次数。
    pub max_tool_calls_per_turn: usize,
    /// 单次 Tool 结果的序列化总预算。
    pub tool_output_max_bytes: usize,
    /// Tool 结果中 metadata 部分的序列化预算。
    pub tool_output_metadata_max_bytes: usize,
    /// 触发主动上下文压缩的阈值比例。
    pub compact_threshold: f64,
    /// 可选的压缩专用模型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_model: Option<String>,
    /// 可选的压缩 prompt 覆盖文本。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_prompt: Option<String>,
    /// 可选的记忆提取专用模型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_model: Option<String>,
    /// 记忆 checkpoint 的轮次间隔。
    pub memory_checkpoint_interval: u64,
    /// 单次请求最多注入的记忆条目数量。
    pub memory_max_items: usize,
    /// 用于隔离该 Agent 记忆的命名空间。
    pub memory_namespace: String,
}

impl AgentConfig {
    /// 创建一个带文档默认值的配置实例。
    #[must_use]
    pub fn new(default_model: impl Into<String>, memory_namespace: impl Into<String>) -> Self {
        Self {
            default_model: default_model.into(),
            memory_namespace: memory_namespace.into(),
            ..Self::default()
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_model: String::new(),
            system_instructions: Vec::new(),
            personality: None,
            environment_context: None,
            tool_timeout_ms: 120_000,
            max_tool_calls_per_turn: 50,
            tool_output_max_bytes: 1_048_576,
            tool_output_metadata_max_bytes: 65_536,
            compact_threshold: 0.8,
            compact_model: None,
            compact_prompt: None,
            memory_model: None,
            memory_checkpoint_interval: 10,
            memory_max_items: 20,
            memory_namespace: String::new(),
        }
    }
}

/// 创建会话时传入的会话级覆盖配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    /// 可选的默认模型覆盖值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// 可选的会话级附加 system prompt 片段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,
}

/// 调用方希望模型看到的运行时环境事实。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentContext {
    /// 宿主运行时的当前工作目录。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// 表示宿主环境的 shell 名称或可执行文件路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// 暴露给模型的当前本地日期。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_date: Option<String>,
    /// 暴露给模型的当前时区。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// 可选的网络访问描述。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkContext>,
    /// 可选的子 Agent 描述块。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagents: Option<String>,
}

impl EnvironmentContext {
    /// 将环境上下文序列化为 prompt 管线需要的 XML 片段。
    #[must_use]
    pub fn serialize_to_xml(&self) -> String {
        let mut lines = Vec::new();

        if let Some(cwd) = &self.cwd {
            lines.push(format!(
                "  <cwd>{}</cwd>",
                xml_escape(cwd.to_string_lossy().as_ref())
            ));
        }

        if let Some(shell) = &self.shell {
            lines.push(format!("  <shell>{}</shell>", xml_escape(shell)));
        }

        if let Some(current_date) = &self.current_date {
            lines.push(format!(
                "  <current_date>{}</current_date>",
                xml_escape(current_date),
            ));
        }

        if let Some(timezone) = &self.timezone {
            lines.push(format!("  <timezone>{}</timezone>", xml_escape(timezone)));
        }

        if let Some(network) = &self.network {
            lines.push("  <network enabled=\"true\">".to_string());
            for allowed in &network.allowed_domains {
                lines.push(format!("    <allowed>{}</allowed>", xml_escape(allowed)));
            }
            for denied in &network.denied_domains {
                lines.push(format!("    <denied>{}</denied>", xml_escape(denied)));
            }
            lines.push("  </network>".to_string());
        }

        if let Some(subagents) = &self.subagents {
            lines.push("  <subagents>".to_string());
            for line in subagents.lines() {
                lines.push(format!("    {}", xml_escape(line)));
            }
            lines.push("  </subagents>".to_string());
        }

        if lines.is_empty() {
            "<environment_context />".to_string()
        } else {
            format!(
                "<environment_context>\n{}\n</environment_context>",
                lines.join("\n")
            )
        }
    }
}

/// 调用方希望模型看到的网络策略事实。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkContext {
    /// 显式允许访问的域名列表。
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// 显式拒绝访问的域名列表。
    #[serde(default)]
    pub denied_domains: Vec<String>,
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::{EnvironmentContext, NetworkContext};

    #[test]
    fn test_should_serialize_environment_context_to_xml() {
        let context = EnvironmentContext {
            cwd: Some("/tmp/project".into()),
            shell: Some("/bin/zsh".to_string()),
            current_date: Some("2026-03-22".to_string()),
            timezone: Some("Asia/Shanghai".to_string()),
            network: Some(NetworkContext {
                allowed_domains: vec!["example.com".to_string()],
                denied_domains: vec!["blocked.example".to_string()],
            }),
            subagents: Some("worker-1".to_string()),
        };

        let xml = context.serialize_to_xml();

        assert!(xml.contains("<environment_context>"));
        assert!(xml.contains("<cwd>/tmp/project</cwd>"));
        assert!(xml.contains("<shell>/bin/zsh</shell>"));
        assert!(xml.contains("<allowed>example.com</allowed>"));
        assert!(xml.contains("<denied>blocked.example</denied>"));
        assert!(xml.contains("<subagents>"));
    }
}
