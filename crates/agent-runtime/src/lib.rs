#![doc = include_str!("../README.md")]

mod agent;
mod compaction;
mod conversation;
mod definition;
mod executor;
mod model;
pub mod persistence;
mod session;

pub use agent::{Agent, AgentBuilder, AgentError, CompactionOutcome};
pub use compaction::{Compaction, CompactionError, CompactionPlan, CompactionPolicy};
pub use conversation::{Conversation, ConversationError};
pub use definition::{AgentDefinition, ToolPermissions};
pub use executor::{
    AllowAll, AllowTools, Authorization, CallControl, ToolExecutor, ToolPolicy, ToolRequest,
};
pub use session::{
    AgentEvent, AgentSession, ControlError, EventKind, InputOrigin, Phase, PromptError, Reply,
    RunError, SessionHandle, SessionResult, Snapshot, SubmitError,
};

#[cfg(test)]
mod tests;
