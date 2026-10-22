//! The application side of a session: who sends requests, what inputs look
//! like, and how tool calls are authorized.

use std::{collections::BTreeSet, fmt::Debug};

use language_model::Content;
use tool_runtime::ToolDescriptor;

use crate::{Request, tools::BUILTIN_TOOLS};

/// Model-visible input before the session commits it. Rendered at commit,
/// always as user content, so no source can make its text privileged.
pub trait RenderInput {
    fn render(&self) -> Vec<Content>;
}

impl RenderInput for String {
    fn render(&self) -> Vec<Content> {
        vec![Content::text(self.clone())]
    }
}

impl RenderInput for Vec<Content> {
    fn render(&self) -> Vec<Content> {
        self.clone()
    }
}

/// Access the host allowed for the rest of a run, for one tool.
#[derive(Clone, Debug, PartialEq)]
pub struct Grant<A> {
    pub tool: String,
    pub access: A,
}

/// Everything authorization may consider for one call, captured when the
/// call is about to run.
pub struct CallAuthorization<'a, H: AgentHost> {
    /// The real tool, also when the model reached it through `call_tool`.
    pub tool: &'a ToolDescriptor,
    /// The arguments the real tool receives (for `call_tool`, the inner
    /// call's), for policies that recognize a call by what it says, such as
    /// asking only when the same call is repeated.
    pub arguments: &'a serde_json::Value,
    /// What these arguments need, as the tool computed it.
    pub access: &'a H::Access,
    /// Who sent the input that started this run.
    pub origin: &'a H::Source,
    pub permissions: &'a H::Permissions,
    pub grants: &'a [Grant<H::Access>],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
    /// Park the call and emit `ApprovalRequested`; a `ResolveApproval`
    /// request decides it.
    Ask,
}

/// The application's types and policy for a session.
///
/// The session never checks authority itself: [`AgentHost::admit`] sees every
/// request with its source first. Tool contexts come from the host too: a
/// tool whose `Context` is `T` can be registered when the host implements
/// `Provides<T>`.
pub trait AgentHost: Send + Sync + Sized + 'static {
    /// Who sent a request. Recorded on conversation entries and runs.
    type Source: Clone + PartialEq + Debug + Send + Sync + 'static;
    /// Model-visible input, kept typed until it is committed.
    type Input: RenderInput + Clone + Debug + Send + Sync + 'static;
    /// What one tool call needs. Each tool's own access converts into it.
    type Access: Clone + Debug + Send + Sync + 'static;
    /// The live permission state, part of the session's configuration.
    type Permissions: Clone + Debug + Send + Sync + 'static;

    /// Whether this source may make this request. Called before the session
    /// acts on anything; an error rejects the request with that reason.
    fn admit(&self, _request: &Request<Self>) -> Result<(), String> {
        Ok(())
    }

    /// Whether one call may run now.
    fn authorize(&self, call: &CallAuthorization<'_, Self>) -> Decision;

    /// The tool policy as the model should read it, so it can avoid calls it
    /// knows would ask or be refused. `None` says nothing.
    fn describe_permissions(
        &self,
        _permissions: &Self::Permissions,
        _tools: &[ToolDescriptor],
    ) -> Option<String> {
        None
    }
}

/// A host for single-user programs: string sources and inputs, tools that
/// need no context, and permissions by tool name.
#[derive(Clone, Copy, Debug, Default)]
pub struct BasicHost;

impl AgentHost for BasicHost {
    type Source = String;
    type Input = String;
    type Access = ();
    type Permissions = ToolPermissions;

    fn authorize(&self, call: &CallAuthorization<'_, Self>) -> Decision {
        let name = call.tool.name();
        if call.permissions.allows(name) || call.grants.iter().any(|grant| grant.tool == name) {
            Decision::Allow
        } else if call.permissions.ask_otherwise {
            Decision::Ask
        } else {
            Decision::Deny(format!("tool {name} is not allowed"))
        }
    }

    fn describe_permissions(
        &self,
        permissions: &ToolPermissions,
        tools: &[ToolDescriptor],
    ) -> Option<String> {
        let names: Vec<_> = tools
            .iter()
            .map(ToolDescriptor::name)
            .filter(|name| !BUILTIN_TOOLS.contains(name))
            .collect();
        if names.is_empty() {
            return None;
        }
        let allowed: Vec<_> = names
            .iter()
            .copied()
            .filter(|name| permissions.allows(name))
            .collect();
        let others = if permissions.ask_otherwise {
            "need the user's approval, which they may refuse"
        } else {
            "are refused"
        };
        Some(if allowed.len() == names.len() {
            "All tools run without approval.".into()
        } else if allowed.is_empty() {
            format!("Calls to any tool {others}.")
        } else {
            format!(
                "These tools run without approval: {}. Calls to other tools {others}.",
                allowed.join(", ")
            )
        })
    }
}

/// [`BasicHost`]'s permissions. The default refuses every call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolPermissions {
    /// Tools that run without asking.
    pub allowed: BTreeSet<String>,
    pub allow_all: bool,
    /// Whether calls to other tools ask the host (`true`) or are refused.
    pub ask_otherwise: bool,
}

impl ToolPermissions {
    pub fn allow(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed: names.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    pub fn allow_all() -> Self {
        Self {
            allow_all: true,
            ..Self::default()
        }
    }

    pub fn ask_otherwise(mut self) -> Self {
        self.ask_otherwise = true;
        self
    }

    pub fn allows(&self, name: &str) -> bool {
        self.allow_all || self.allowed.contains(name)
    }
}
