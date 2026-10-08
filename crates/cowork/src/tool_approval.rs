//! Which of the agent's tool calls wait for someone to allow them, asking
//! while the run waits, and deciding.
//!
//! The host's run asks through [`ApprovalGate`], its agent hook. The request
//! reaches the thread on the run's own channel, after the reply that made the
//! call, and the host tells everyone the call is waiting. Anyone who may
//! approve tool calls (the host, and `Admin` peers) then allows or denies it
//! through the same checked path; the first decision wins and the run goes on.
//! Calls of [`HOST_ONLY`] tools, which run something on the host's computer,
//! only the host may decide.

use std::sync::Arc;

use agent::{AgentEvent, ToolDecision, ToolHook};
use futures::future::BoxFuture;
use gpui::Context;
use rig::{
    message::{CallId, ToolCall},
    tool::Tool as _,
};
use sandbox::RunCommand;
use tokio::sync::{mpsc, oneshot};
use tools::RespondToComment;
use uuid::Uuid;

use crate::{
    Cowork,
    participant::ParticipantId,
    thread::{ApproveTools, Thread, ThreadSharing},
};

/// Tools whose calls always run, as they only act within the thread.
/// Every other tool's calls wait for approval, so a tool added without a
/// thought about it is asked about rather than trusted.
pub(crate) const ALWAYS_ALLOWED: &[&str] = &[RespondToComment::NAME];

/// What the model is told about a call someone denied.
const DENIED: &str = "The user denied this tool call.";

/// What the model is told about a call nobody could decide, as when the
/// thread is gone.
const UNDECIDED: &str = "The tool call was not approved.";

/// Tools whose calls only the host may allow: they act on the host's computer
/// (a sandbox there, for `run_command`), so an `Admin` peer may not decide
/// for it.
pub(crate) const HOST_ONLY: &[&str] = &[RunCommand::NAME];

pub(crate) fn needs_approval(tool: &str) -> bool {
    !ALWAYS_ALLOWED.contains(&tool)
}

pub(crate) fn host_only(tool: &str) -> bool {
    HOST_ONLY.contains(&tool)
}

/// Which waiting calls the local user may decide in a thread.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ApprovalRights {
    /// May approve tool calls at all: the host, or an `Admin` peer.
    approves_tools: bool,
    /// Is the host, who alone decides [`HOST_ONLY`] tools.
    is_host: bool,
}

impl ApprovalRights {
    pub(crate) fn of(thread: &Thread) -> Self {
        Self {
            approves_tools: thread.can_approve_tools(),
            is_host: thread.is_host(),
        }
    }

    pub(crate) fn may_decide(self, tool: &str) -> bool {
        self.approves_tools && (self.is_host || !host_only(tool))
    }
}

/// What a run reports to its thread, in the order it happened.
// Nearly every update is an event, so boxing it would only add an allocation
// per streamed fragment.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RunUpdate {
    Event(AgentEvent),
    /// A call of the last reply waits for a decision, given through `decide`.
    Approval {
        call: CallId,
        /// The tool called, which decides who may answer.
        tool: String,
        decide: oneshot::Sender<ToolDecision>,
    },
}

/// The run's hook: lets calls that need no approval through, and asks the
/// thread about the rest. It shares the run's channel with its events, so a
/// request always arrives after the reply whose call it is.
pub(crate) struct ApprovalGate {
    updates: mpsc::UnboundedSender<RunUpdate>,
}

impl ApprovalGate {
    pub(crate) fn new(updates: mpsc::UnboundedSender<RunUpdate>) -> Arc<Self> {
        Arc::new(Self { updates })
    }
}

impl ToolHook for ApprovalGate {
    fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolDecision> {
        Box::pin(async move {
            if !needs_approval(call.function.name.as_str()) {
                return ToolDecision::Allow;
            }
            let (decide, decision) = oneshot::channel();
            let undecided = || ToolDecision::Deny {
                reason: UNDECIDED.into(),
            };
            if self
                .updates
                .send(RunUpdate::Approval {
                    call: call.id.clone(),
                    tool: call.function.name.to_string(),
                    decide,
                })
                .is_err()
            {
                return undecided();
            }
            decision.await.unwrap_or_else(|_| undecided())
        })
    }
}

impl Cowork {
    /// Allows or denies, as the local user, the call that agent message
    /// `message_id` of the active thread is waiting on. A mirrored thread
    /// asks its host.
    pub(crate) fn decide_tool_call(
        &mut self,
        message_id: Uuid,
        call: CallId,
        allow: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.active_thread(cx) else {
            return;
        };
        let state = thread.read(cx);
        let actor = state.participant_id();
        if matches!(state.sharing, ThreadSharing::Connected { .. }) {
            _ = thread.update(cx, |thread, _| {
                thread.with_authorized::<ApproveTools, _>(actor, |auth| {
                    auth.request_decision(message_id, &call, allow)
                })
            });
            return;
        }
        let thread_id = state.instance_id;
        self.resolve_tool_call(thread_id, actor, message_id, &call, allow, cx);
    }

    /// Hands `actor`'s decision on `call` to the run producing message
    /// `message_id` of a local or hosted thread, if it is still waiting on
    /// it; a decision that raced another, or outlived its run, changes
    /// nothing.
    pub(crate) fn resolve_tool_call(
        &mut self,
        thread_id: Uuid,
        actor: ParticipantId,
        message_id: Uuid,
        call: &CallId,
        allow: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        if !thread.read(cx).is_host() {
            return;
        }
        let host = thread.read(cx).participant_id();
        let Some(generation) = self
            .active_generations
            .get_mut(&thread_id)
            .filter(|generation| generation.message_id() == message_id)
        else {
            return;
        };
        // Decided within the current-policy guard, so a revoked peer's
        // decision never reaches the run.
        let decided = thread.update(cx, |thread, cx| {
            thread.with_authorized::<ApproveTools, _>(actor, |auth| {
                if actor != host && generation.approval_is_host_only(call) {
                    return false;
                }
                let Some(decide) = generation.take_approval(call) else {
                    return false;
                };
                _ = decide.send(if allow {
                    ToolDecision::Allow
                } else {
                    ToolDecision::Deny {
                        reason: DENIED.into(),
                    }
                });
                auth.resolved(message_id, call, cx);
                true
            })
        });
        if decided.is_ok_and(|decided| decided) {
            self.thread_updated(thread_id, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_tools_run_without_approval() {
        assert!(!needs_approval(RespondToComment::NAME));
        assert!(needs_approval(tools::Calculate::NAME));
        assert!(needs_approval(RunCommand::NAME));
        assert!(needs_approval("a_tool_nobody_listed"));
    }

    #[test]
    fn admins_decide_everything_but_host_only_tools() {
        let host = ApprovalRights {
            approves_tools: true,
            is_host: true,
        };
        let admin = ApprovalRights {
            approves_tools: true,
            is_host: false,
        };
        let writer = ApprovalRights::default();
        assert!(host.may_decide(RunCommand::NAME));
        assert!(host.may_decide(tools::Calculate::NAME));
        assert!(!admin.may_decide(RunCommand::NAME));
        assert!(admin.may_decide(tools::Calculate::NAME));
        assert!(!writer.may_decide(tools::Calculate::NAME));
    }

    fn call(name: &str) -> ToolCall {
        ToolCall::new(
            CallId::from_wire("call_1"),
            rig::message::ToolFunction::new(
                rig::message::ToolName::new(name).expect("a valid name"),
                serde_json::json!({}),
            ),
        )
    }

    #[tokio::test]
    async fn the_gate_asks_only_about_calls_that_need_approval() {
        let (updates, mut asked) = mpsc::unbounded_channel();
        let gate = ApprovalGate::new(updates);

        let comment = call(RespondToComment::NAME);
        assert_eq!(gate.before_tool_call(&comment).await, ToolDecision::Allow);
        assert!(asked.try_recv().is_err(), "comments are never asked about");

        let calculation = call(tools::Calculate::NAME);
        let decision = gate.before_tool_call(&calculation);
        let answer = async {
            let Some(RunUpdate::Approval { call, tool, decide }) = asked.recv().await else {
                panic!("expected an approval request");
            };
            assert_eq!(call, CallId::from_wire("call_1"));
            assert_eq!(tool, tools::Calculate::NAME);
            decide.send(ToolDecision::Allow).expect("the gate waits");
        };
        let (decision, ()) = tokio::join!(decision, answer);
        assert_eq!(decision, ToolDecision::Allow);
    }

    /// A request nobody answers, as when the thread is gone, denies the call
    /// rather than leaving the run waiting.
    #[tokio::test]
    async fn an_abandoned_request_denies_the_call() {
        let (updates, mut asked) = mpsc::unbounded_channel();
        let gate = ApprovalGate::new(updates);
        let calculation = call(tools::Calculate::NAME);
        let decision = gate.before_tool_call(&calculation);
        let abandon = async {
            drop(asked.recv().await);
        };
        let (decision, ()) = tokio::join!(decision, abandon);
        assert_eq!(
            decision,
            ToolDecision::Deny {
                reason: UNDECIDED.into()
            }
        );
    }
}
