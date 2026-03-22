//! 领域层类型与不变量定义。

mod config;
mod error;
mod event;
mod hook;
mod ledger;
mod state;
mod types;

pub use config::{AgentConfig, EnvironmentContext, NetworkContext, SessionConfig};
pub use error::{AgentError, Result};
pub use event::{AgentEvent, AgentEventPayload};
pub use hook::{HookData, HookEvent, HookPatch, HookPayload, HookResult};
pub use ledger::{CompactionMarker, LedgerEvent, LedgerEventPayload, MetadataKey, SessionLedger};
pub use state::TurnOutcome;
pub use types::{
    ContentBlock, Memory, Message, MessageStatus, SkillDefinition, ToolOutput, UserInput,
};

pub(crate) use state::{LifecycleState, SessionSlotState};
