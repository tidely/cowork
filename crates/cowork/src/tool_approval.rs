//! Which of the agent's tool calls wait for someone to allow them, asking
//! while the run waits, and deciding.
//!
//! The host's run asks through [`ApprovalGate`], its agent hook. The request
//! reaches the thread on the run's own channel, after the reply that made the
//! call, and the host tells everyone the call is waiting. Anyone who may
//! approve tool calls (the host, and `Admin` peers) then allows or denies it
//! through the same checked path; the first decision wins and the run goes on.
//! Calls of [`HOST_ONLY`] tools, which act on the host's computer, only the
//! host may decide. `run_command` runs at once while the project is
//! read-only, as Codex's read-only mode does: the sandbox lets such a command
//! change only the thread's scratch folder, and it stays read-only even if
//! the project becomes writable before it starts. While the project is
//! writable, every command is asked about. `write_file` is asked about too,
//! even when it writes the scratch folder. Listing and reading need no
//! approval: they reach only the folders the host shared with the thread, and
//! its scratch folder.
//!
//! A scheduled run may have nobody around to answer, so it doesn't wait the
//! first time: a call that needs approval fails at once, telling the model
//! that making it again with the same arguments asks the user. The repeat
//! waits as usual, and the user is notified; see `docs/schedule.md`.

use std::sync::{Arc, Mutex};

use agent::{AgentEvent, ToolDecision, ToolHook};
use futures::future::BoxFuture;
use gpui::Context;
use rig::{
    message::{CallId, ToolCall},
    tool::Tool as _,
};
use sandbox::{ListDirectory, ReadFile, RunCommand, Sandboxes, WriteFile};
use tokio::sync::{mpsc, oneshot};
use tools::RespondToComment;
use uuid::Uuid;

use crate::{
    Cowork,
    participant::ParticipantId,
    thread::{ApproveTools, Thread, ThreadSharing},
};

/// Tools whose calls always run, as they only act within the thread or
/// read what the host shared with it. Every other tool's calls wait for
/// approval, so a tool added without a thought about it is asked about
/// rather than trusted.
pub(crate) const ALWAYS_ALLOWED: &[&str] =
    &[RespondToComment::NAME, ListDirectory::NAME, ReadFile::NAME];

/// What the model is told about a call someone denied.
const DENIED: &str = "The user denied this tool call.";

/// What the model is told about a call nobody could decide, as when the
/// thread is gone.
const UNDECIDED: &str = "The tool call was not approved.";

/// What a scheduled run's model is told about a call it makes the first
/// time, rather than waiting for someone who may not be there.
pub(crate) const UNATTENDED: &str = "This tool call was not run: it needs the user's permission, and this is a scheduled run that nobody may be watching. Carry on without it if you can. If the call is necessary, make it again with exactly the same arguments: the user will then be notified and asked, and the run waits for their answer.";

/// Tools whose calls only the host may allow: they act on the host's computer
/// (its shell and tools, for `run_command`, and its files, for `write_file`),
/// so an `Admin` peer may not decide for it.
pub(crate) const HOST_ONLY: &[&str] = &[RunCommand::NAME, WriteFile::NAME];

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
    /// Where the run's commands run, whose project decides whether they
    /// are asked about.
    sandboxes: Arc<Sandboxes>,
    thread: String,
    /// For a scheduled run, the calls already refused once, by tool and
    /// arguments; `None` for a run someone started.
    unattended: Option<Mutex<Vec<CallKey>>>,
}

/// What makes two calls the same: the tool, and its arguments however their
/// keys are ordered.
type CallKey = (
    String,
    serde_json::Map<String, serde_json::Value>,
    Option<String>,
);

impl ApprovalGate {
    /// The gate of a run of `thread`, someone started or scheduled.
    pub(crate) fn new(
        updates: mpsc::UnboundedSender<RunUpdate>,
        sandboxes: Arc<Sandboxes>,
        thread: impl Into<String>,
        scheduled: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            updates,
            sandboxes,
            thread: thread.into(),
            unattended: scheduled.then(Mutex::default),
        })
    }

    /// Whether a scheduled run makes `call` for the first time, which is then
    /// remembered so that the same call again is asked about.
    fn refuses_unattended(&self, call: &ToolCall) -> bool {
        let Some(refused) = &self.unattended else {
            return false;
        };
        let key = (
            call.function.name.to_string(),
            call.function.arguments.clone(),
            call.function.invalid_arguments.clone(),
        );
        let mut refused = refused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if refused.contains(&key) {
            return false;
        }
        refused.push(key);
        true
    }
}

impl ToolHook for ApprovalGate {
    fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolDecision> {
        Box::pin(async move {
            let tool = call.function.name.as_str();
            if !needs_approval(tool) {
                return ToolDecision::Allow;
            }
            if tool == RunCommand::NAME && !self.sandboxes.project(&self.thread).writable {
                return ToolDecision::AllowWith {
                    context: RunCommand::read_only_context(),
                };
            }
            if self.refuses_unattended(call) {
                return ToolDecision::Deny {
                    reason: UNATTENDED.into(),
                };
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
        assert!(!needs_approval(ListDirectory::NAME));
        assert!(!needs_approval(ReadFile::NAME));
        assert!(needs_approval(WriteFile::NAME));
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
        assert!(!admin.may_decide(WriteFile::NAME));
        assert!(admin.may_decide(tools::Calculate::NAME));
        assert!(!writer.may_decide(tools::Calculate::NAME));
    }

    fn call(name: &str) -> ToolCall {
        call_with(name, serde_json::json!({}))
    }

    fn call_with(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(
            CallId::from_wire("call_1"),
            rig::message::ToolFunction::new(
                rig::message::ToolName::new(name).expect("a valid name"),
                arguments,
            ),
        )
    }

    /// A gate for a run someone started, or a scheduled one, in a writable
    /// project unless [`gate_in`] says otherwise.
    fn gate(scheduled: bool) -> (Arc<ApprovalGate>, mpsc::UnboundedReceiver<RunUpdate>) {
        let (gate, asked, _) = gate_in(scheduled, true);
        (gate, asked)
    }

    fn gate_in(
        scheduled: bool,
        writable: bool,
    ) -> (
        Arc<ApprovalGate>,
        mpsc::UnboundedReceiver<RunUpdate>,
        Arc<Sandboxes>,
    ) {
        let (updates, asked) = mpsc::unbounded_channel();
        let sandboxes = crate::test_support::unused_sandboxes();
        sandboxes.set_project(
            "thread",
            sandbox::Project {
                folders: Vec::new(),
                writable,
            },
        );
        let gate = ApprovalGate::new(updates, sandboxes.clone(), "thread", scheduled);
        (gate, asked, sandboxes)
    }

    /// In a read-only project, commands run at once, held read-only, even in
    /// a scheduled run; once the project is writable, they are asked about.
    #[tokio::test]
    async fn commands_in_a_read_only_project_run_at_once() {
        let held = ToolDecision::AllowWith {
            context: RunCommand::read_only_context(),
        };
        for scheduled in [false, true] {
            let (gate, mut asked, sandboxes) = gate_in(scheduled, false);
            let command = call_with(RunCommand::NAME, serde_json::json!({ "command": "ls" }));
            assert_eq!(
                decide_allowing(&gate, &command, &mut asked).await,
                (held.clone(), None)
            );

            // Writing a file is still asked about, or refused the first time
            // in a scheduled run.
            let write = call(WriteFile::NAME);
            let decided = decide_allowing(&gate, &write, &mut asked).await;
            if scheduled {
                assert!(matches!(decided, (ToolDecision::Deny { .. }, None)));
            } else {
                assert_eq!(
                    decided,
                    (ToolDecision::Allow, Some(WriteFile::NAME.to_owned()))
                );
            }

            sandboxes.set_project(
                "thread",
                sandbox::Project {
                    folders: Vec::new(),
                    writable: true,
                },
            );
            let decided = decide_allowing(&gate, &command, &mut asked).await;
            if scheduled {
                assert!(matches!(decided, (ToolDecision::Deny { .. }, None)));
            } else {
                assert_eq!(
                    decided,
                    (ToolDecision::Allow, Some(RunCommand::NAME.to_owned()))
                );
            }
        }
    }

    /// What `gate` decides about `call`, allowing it if it asks, with the
    /// tool it asked about.
    async fn decide_allowing(
        gate: &ApprovalGate,
        call: &ToolCall,
        asked: &mut mpsc::UnboundedReceiver<RunUpdate>,
    ) -> (ToolDecision, Option<String>) {
        let decision = gate.before_tool_call(call);
        tokio::pin!(decision);
        tokio::select! {
            decision = &mut decision => (decision, None),
            Some(RunUpdate::Approval { tool, decide, .. }) = asked.recv() => {
                decide.send(ToolDecision::Allow).expect("the gate waits");
                (decision.await, Some(tool))
            }
        }
    }

    #[tokio::test]
    async fn a_scheduled_run_asks_only_when_a_call_is_repeated() {
        let (gate, mut asked) = gate(true);
        let first = call_with(
            tools::Calculate::NAME,
            serde_json::json!({ "a": 1, "b": 2 }),
        );
        assert_eq!(
            gate.before_tool_call(&first).await,
            ToolDecision::Deny {
                reason: UNATTENDED.into()
            }
        );
        assert!(asked.try_recv().is_err(), "nobody was asked");

        // Other arguments are another call, refused once too.
        let other = call_with(tools::Calculate::NAME, serde_json::json!({ "a": 3 }));
        assert!(matches!(
            gate.before_tool_call(&other).await,
            ToolDecision::Deny { .. }
        ));
        assert!(asked.try_recv().is_err());

        // The same arguments, in any order, ask the user and wait.
        let repeat = call_with(
            tools::Calculate::NAME,
            serde_json::json!({ "b": 2, "a": 1 }),
        );
        let decision = gate.before_tool_call(&repeat);
        let answer = async {
            let Some(RunUpdate::Approval { decide, .. }) = asked.recv().await else {
                panic!("a repeated call asks");
            };
            decide.send(ToolDecision::Allow).expect("the gate waits");
        };
        let (decision, ()) = tokio::join!(decision, answer);
        assert_eq!(decision, ToolDecision::Allow);

        // Calls that need no approval still run at once, while a command is
        // refused the first time like any other call.
        let comment = call(RespondToComment::NAME);
        assert_eq!(gate.before_tool_call(&comment).await, ToolDecision::Allow);
        assert!(matches!(
            gate.before_tool_call(&call(RunCommand::NAME)).await,
            ToolDecision::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn every_command_in_a_writable_project_is_asked_about() {
        let command = call(RunCommand::NAME);
        let (gate, mut asked) = gate(false);
        let decision = gate.before_tool_call(&command);
        let answer = async {
            let Some(RunUpdate::Approval { tool, decide, .. }) = asked.recv().await else {
                panic!("a command asks");
            };
            assert_eq!(tool, RunCommand::NAME);
            decide
                .send(ToolDecision::Deny {
                    reason: DENIED.into(),
                })
                .expect("the gate waits");
        };
        let (decision, ()) = tokio::join!(decision, answer);
        assert!(matches!(decision, ToolDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn the_gate_asks_only_about_calls_that_need_approval() {
        let (gate, mut asked) = gate(false);

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
        let (gate, mut asked) = gate(false);
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
