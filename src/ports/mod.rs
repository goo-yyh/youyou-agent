//! 对外部能力进行抽象的端口层 trait。

mod model;
mod plugin;
mod storage;
mod tool;

pub use model::{
    ChatError, ChatEvent, ChatEventStream, ChatRequest, ModelCapabilities, ModelInfo,
    ModelProvider, TokenUsage, ToolDefinition,
};
pub use plugin::{Plugin, PluginContext, PluginDescriptor};
pub use storage::{MemoryStorage, SessionPage, SessionSearchQuery, SessionStorage, SessionSummary};
pub use tool::{ToolHandler, ToolInput};

pub(crate) use plugin::HookRegistration;
