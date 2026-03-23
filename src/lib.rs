#![warn(rust_2024_compatibility, missing_debug_implementations, missing_docs)]
#![allow(
    clippy::module_name_repetitions,
    reason = "Public API names intentionally mirror the architecture documents."
)]
#![allow(
    clippy::struct_excessive_bools,
    reason = "Capability flags and configuration switches map directly to the product contract."
)]

//! `youyou-agent` 是一个单会话 Agent 内核，按
//! domain、port、application、API 四层清晰分离。

pub mod api;
pub mod application;
pub mod domain;
pub mod ports;
pub mod prompt;

pub use api::{Agent, AgentBuilder, HasProvider, NoProvider, RunningTurn, SessionHandle};
pub use domain::{
    AgentConfig, AgentError, AgentEvent, AgentEventPayload, CompactionMarker, ContentBlock,
    EnvironmentContext, HookData, HookEvent, HookPatch, HookPayload, HookResult, LedgerEvent,
    LedgerEventPayload, Memory, Message, MessageStatus, MetadataKey, NetworkContext, Result,
    SessionConfig, SessionLedger, SkillDefinition, ToolOutput, TurnOutcome, UserInput,
};
pub use ports::{
    ChatError, ChatEvent, ChatEventStream, ChatRequest, MemoryStorage, ModelCapabilities,
    ModelInfo, ModelProvider, Plugin, PluginContext, PluginDescriptor, SessionPage,
    SessionSearchQuery, SessionStorage, SessionSummary, TokenUsage, ToolDefinition, ToolHandler,
    ToolInput,
};
