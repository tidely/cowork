#![doc = include_str!("../README.md")]

mod agent;
mod compaction;
mod configuration;
pub mod context;
mod conversation;
mod definition;
mod executor;
mod host;
mod model;
pub mod persistence;
mod request;
mod session;
mod tools;

pub use agent::{Agent, AgentBuilder, AgentError, CompactionOutcome};
pub use compaction::{Compaction, CompactionError, CompactionPlan, CompactionPolicy};
pub use configuration::{Configuration, ConfigurationChange, Sampling};
pub use context::{ContextSection, SessionView};
pub use conversation::{Conversation, ConversationError, Entry, Origin};
pub use definition::{AgentDefinition, DefinitionRevision};
pub use host::{
    AgentHost, BasicHost, CallAuthorization, Decision, Grant, RenderInput, ToolPermissions,
};
pub use request::{
    ApprovalDecision, PromptError, Reply, Request, RequestError, RequestKind, Response,
    SessionHandle, Snapshot,
};
pub(crate) use request::{Command, CompletionResponder, Responder};
pub use session::{
    AgentEvent, AgentSession, EventKind, Phase, RunError, SessionError, SessionResult,
};
pub use tools::{CALL_TOOL, DynamicToolChange, SEARCH_TOOLS};

// Re-exported so hosts need not depend on these crates for common types.
pub use agent_input::{
    Delivery, Fallback, InputId, Lifetime, Policy, QueueAccess, RunId, RunOutcome,
};

#[cfg(test)]
mod tests;
