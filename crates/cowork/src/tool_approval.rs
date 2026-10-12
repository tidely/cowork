//! Auto-approve calls within the project's level and the run's grants;
//! ask for more, granting each approved command its exact sandbox scope.
//! Explicit narrower access is respected, and reads are global except
//! denied credentials. File writes get a target-specific approval context.
//!
//! Only the host may approve commands, file writes, and run permissions;
//! Admin peers may approve other tools. Scheduled calls needing approval
//! fail once, then an identical repeat asks. See `docs/permissions.md`.

use std::sync::Arc;

use agent::{AgentEvent, ToolDecision, ToolHook};
use futures::future::BoxFuture;
use gpui::Context;
use parking_lot::Mutex;
use rig::{
    message::{CallId, ToolCall},
    tool::Tool as _,
};
use sandbox::{
    Access, Folders, Grant, ListDirectory, ReadFile, RequestPermissions, RunCommand, Sandboxes,
    WriteFile,
};
use tokio::sync::{mpsc, oneshot};
use tools::RespondToComment;
use uuid::Uuid;

use crate::{
    Cowork,
    participant::ParticipantId,
    project_mode::ProjectMode,
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
/// (its shell and tools, for `run_command`, its files, for `write_file`, and
/// what later commands may do there, for `request_permissions`), so an
/// `Admin` peer may not decide for it.
pub(crate) const HOST_ONLY: &[&str] =
    &[RunCommand::NAME, WriteFile::NAME, RequestPermissions::NAME];

pub(crate) fn needs_approval(tool: &str) -> bool {
    !ALWAYS_ALLOWED.contains(&tool)
}

pub(crate) fn host_only(tool: &str) -> bool {
    HOST_ONLY.contains(&tool)
}

/// Which waiting calls the local user may decide in a thread, and what
/// their cards show.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ApprovalRights {
    /// May approve tool calls at all: the host, or an `Admin` peer.
    approves_tools: bool,
    /// Is the host, who alone decides [`HOST_ONLY`] tools.
    is_host: bool,
    /// The project's mode, whose access a waiting command is allowed with
    /// unless it asks for its own.
    pub(crate) project_mode: ProjectMode,
}

impl ApprovalRights {
    pub(crate) fn of(thread: &Thread) -> Self {
        Self {
            approves_tools: thread.can_approve_tools(),
            is_host: thread.is_host(),
            project_mode: thread.project_mode(),
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
    SteeringConsumed {
        group: crate::timeline::UserMessageGroup,
        aliases: Vec<String>,
    },
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
    /// What the host allowed through `request_permissions` in this run,
    /// which its later commands get; read access and no folders until then.
    granted: Mutex<Grant>,
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
            granted: Mutex::default(),
        })
    }

    /// What the run has been granted so far.
    pub(crate) fn granted(&self) -> Grant {
        self.granted.lock().clone()
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
        let mut refused = refused.lock();
        if refused.contains(&key) {
            return false;
        }
        refused.push(key);
        true
    }
}

/// What a call of a tool that grants permissions asks for, resolved before
/// anyone is asked.
enum Asked {
    /// A file write, approved for this target only.
    WriteFile(String),
    /// A command: the access it asks for of its own, if any, and the folders
    /// outside the project it names.
    Command {
        access: Option<Access>,
        folders: Folders,
    },
    /// More for the rest of the run.
    Permissions(Grant),
}

impl ToolHook for ApprovalGate {
    fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolDecision> {
        Box::pin(async move {
            let tool = call.function.name.as_str();
            if !needs_approval(tool) {
                return ToolDecision::Allow;
            }
            let asked = match Self::asked(call) {
                Ok(asked) => asked,
                Err(reason) => return ToolDecision::Deny { reason },
            };
            if let Some(asked) = &asked
                && self.already_granted(asked)
            {
                if let Asked::Permissions(requested) = asked {
                    self.granted.lock().extend(requested);
                }
                return self.allow(asked);
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
            match (decision.await.unwrap_or_else(|_| undecided()), &asked) {
                (ToolDecision::Allow, Some(asked)) => {
                    if let Asked::Permissions(requested) = asked {
                        self.granted.lock().extend(requested);
                    }
                    self.allow(asked)
                }
                (decision, _) => decision,
            }
        })
    }
}

impl ApprovalGate {
    /// Whether `tool` is `write_file` in a project whose mode lets it
    /// change the project's folders; see the module documentation.
    fn writes_writable_project(&self, tool: &str) -> bool {
        tool == WriteFile::NAME && self.sandboxes.project(&self.thread).writable()
    }

    /// What `call` asks for, if it is a call of a tool that grants
    /// permissions, or why it can't be granted.
    fn asked(call: &ToolCall) -> Result<Option<Asked>, String> {
        let arguments = call.function.arguments_value();
        Ok(match call.function.name.as_str() {
            WriteFile::NAME => Some(Asked::WriteFile(
                arguments["path"]
                    .as_str()
                    .ok_or("A file path is required.")?
                    .to_owned(),
            )),
            RunCommand::NAME => Some(Asked::Command {
                access: RunCommand::requested_access(&arguments),
                folders: RunCommand::requested_folders(&arguments)?,
            }),
            RequestPermissions::NAME => Some(Asked::Permissions(RequestPermissions::requested(
                &arguments,
            )?)),
            _ => None,
        })
    }

    /// Whether what `asked` asks for needs nobody's approval: for a command,
    /// that it needs nothing beyond the project level and the run's grants, and
    /// for more permissions, that the run has them already.
    fn already_granted(&self, asked: &Asked) -> bool {
        let mut granted = self.granted();
        let project = self.sandboxes.project(&self.thread);
        granted.access = granted.access.max(project.access);
        // Naming a folder already in the project does not ask for more.
        // Only resolve existing roots; the sandbox and file tools still
        // enforce their credential and symlink restrictions.
        for folder in project.folders {
            if let Ok(path) = std::fs::canonicalize(folder.path) {
                if granted.access.writable() {
                    granted.folders.write.push(path);
                } else {
                    granted.folders.read.push(path);
                }
            }
        }
        match asked {
            Asked::WriteFile(_) => self.writes_writable_project(WriteFile::NAME),
            Asked::Command { access, folders } => granted.covers(&Grant {
                access: self.command_access(*access, &granted),
                // Reads are already allowed across the host filesystem.
                // Resolution above still rejects denied credential paths.
                folders: Folders {
                    read: Vec::new(),
                    write: folders.write.clone(),
                },
            }),
            Asked::Permissions(requested) => granted.covers(&Grant {
                access: requested.access,
                folders: Folders {
                    read: Vec::new(),
                    write: requested.folders.write.clone(),
                },
            }),
        }
    }

    /// Lets `asked` through with what it is granted now.
    fn allow(&self, asked: &Asked) -> ToolDecision {
        let granted = self.granted();
        ToolDecision::AllowWith {
            context: match asked {
                Asked::WriteFile(path) => WriteFile::context(path.clone()),
                Asked::Command { access, folders } => {
                    let mut grant = Grant {
                        access: self.command_access(*access, &granted),
                        folders: granted.folders,
                    };
                    grant.folders.extend(folders);
                    RunCommand::context(grant)
                }
                Asked::Permissions(_) => RequestPermissions::context(granted),
            },
        }
    }

    /// The access a command asking for `access` of its own runs with now:
    /// that, or the project's mode or the run's grant, whichever is more.
    fn command_access(&self, access: Option<Access>, granted: &Grant) -> Access {
        access.unwrap_or_else(|| {
            self.sandboxes
                .project(&self.thread)
                .access
                .max(granted.access)
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
        assert!(needs_approval(RequestPermissions::NAME));
        assert!(needs_approval("a_tool_nobody_listed"));
    }

    #[test]
    fn admins_decide_everything_but_host_only_tools() {
        let host = ApprovalRights {
            approves_tools: true,
            is_host: true,
            ..ApprovalRights::default()
        };
        let admin = ApprovalRights {
            approves_tools: true,
            is_host: false,
            ..ApprovalRights::default()
        };
        let writer = ApprovalRights::default();
        assert!(host.may_decide(RunCommand::NAME));
        assert!(host.may_decide(tools::Calculate::NAME));
        assert!(!admin.may_decide(RunCommand::NAME));
        assert!(!admin.may_decide(WriteFile::NAME));
        assert!(!admin.may_decide(RequestPermissions::NAME));
        assert!(admin.may_decide(tools::Calculate::NAME));
        assert!(!writer.may_decide(tools::Calculate::NAME));
    }

    fn call(name: &str) -> ToolCall {
        call_with(
            name,
            if name == WriteFile::NAME {
                serde_json::json!({"path": "scratch://approved.txt", "content": "hello"})
            } else {
                serde_json::json!({})
            },
        )
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

    /// A gate for a run someone started, or a scheduled one, in a project in
    /// write mode unless [`gate_in`] says otherwise.
    fn gate(scheduled: bool) -> (Arc<ApprovalGate>, mpsc::UnboundedReceiver<RunUpdate>) {
        let (gate, asked, _) = gate_in(scheduled, Access::Write);
        (gate, asked)
    }

    fn gate_in(
        scheduled: bool,
        access: Access,
    ) -> (
        Arc<ApprovalGate>,
        mpsc::UnboundedReceiver<RunUpdate>,
        Arc<Sandboxes>,
    ) {
        let (updates, asked) = mpsc::unbounded_channel();
        let sandboxes = crate::test_support::unused_sandboxes();
        set_mode(&sandboxes, access);
        let gate = ApprovalGate::new(updates, sandboxes.clone(), "thread", scheduled);
        (gate, asked, sandboxes)
    }

    fn set_mode(sandboxes: &Sandboxes, access: Access) {
        sandboxes.set_project(
            "thread",
            sandbox::Project {
                folders: Vec::new(),
                access,
            },
        );
    }

    /// A `run_command` call, asking for `access` of its own if given.
    fn command(access: Option<&str>) -> ToolCall {
        let mut arguments = serde_json::json!({ "command": "ls" });
        if let Some(access) = access {
            arguments["access"] = access.into();
        }
        call_with(RunCommand::NAME, arguments)
    }

    fn granted(access: Access) -> ToolDecision {
        granted_with(access, Folders::default())
    }

    fn granted_with(access: Access, folders: Folders) -> ToolDecision {
        ToolDecision::AllowWith {
            context: RunCommand::context(Grant { access, folders }),
        }
    }

    /// A folder outside any project, removed on drop, and its resolved path
    /// as grants name it.
    struct Outside(std::path::PathBuf);

    impl Outside {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cowork-outside-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }

        fn text(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for Outside {
        fn drop(&mut self) {
            _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn writing(folder: &Outside) -> Folders {
        Folders {
            read: Vec::new(),
            write: vec![folder.0.clone()],
        }
    }

    /// A command naming a folder outside the project is asked about even
    /// in read mode, and allowed with exactly that folder; one naming a
    /// folder that can't be granted is refused without asking.
    #[tokio::test]
    async fn commands_naming_folders_are_asked_about() {
        let cache = Outside::new();
        let (gate, mut asked, _) = gate_in(false, Access::Read);
        let call = call_with(
            RunCommand::NAME,
            serde_json::json!({ "command": "cargo fetch", "write_folders": [cache.text()] }),
        );
        assert_eq!(
            decide_allowing(&gate, &call, &mut asked).await,
            (
                granted_with(Access::Read, writing(&cache)),
                Some(RunCommand::NAME.to_owned())
            )
        );
        assert_eq!(gate.granted(), Grant::default(), "for that command only");

        for mode in [Access::Write, Access::Network] {
            let (gate, mut asked, _) = gate_in(false, mode);
            assert_eq!(
                decide_allowing(&gate, &call, &mut asked).await,
                (
                    granted_with(mode, writing(&cache)),
                    Some(RunCommand::NAME.to_owned())
                ),
                "even {mode:?} needs approval for extra folders"
            );
        }

        let missing = call_with(
            RunCommand::NAME,
            serde_json::json!({ "command": "ls", "read_folders": ["/no/such/folder"] }),
        );
        let decided = decide_allowing(&gate, &missing, &mut asked).await;
        assert!(
            matches!(&decided, (ToolDecision::Deny { reason }, None) if reason.contains("does not exist")),
            "{decided:?}"
        );
    }

    /// Once the host allows `request_permissions`, the run's later commands
    /// get what it granted, and run without asking unless they need more.
    #[tokio::test]
    async fn granted_permissions_apply_to_later_commands() {
        let cache = Outside::new();
        let (gate, mut asked, _) = gate_in(false, Access::Read);
        let request = call_with(
            RequestPermissions::NAME,
            serde_json::json!({
                "reason": "Fetch dependencies.",
                "access": "network",
                "write_folders": [cache.text()],
            }),
        );
        let expected = Grant {
            access: Access::Network,
            folders: writing(&cache),
        };
        assert_eq!(
            decide_allowing(&gate, &request, &mut asked).await,
            (
                ToolDecision::AllowWith {
                    context: RequestPermissions::context(expected.clone()),
                },
                Some(RequestPermissions::NAME.to_owned())
            )
        );
        assert_eq!(gate.granted(), expected);

        // Commands get the grant without asking, the mode's or their own
        // access as long as the grant covers it.
        assert_eq!(
            decide_allowing(&gate, &command(None), &mut asked).await,
            (granted_with(Access::Network, writing(&cache)), None)
        );
        assert_eq!(
            decide_allowing(&gate, &command(Some("read")), &mut asked).await,
            (granted_with(Access::Read, writing(&cache)), None)
        );
        let inside = call_with(
            RunCommand::NAME,
            serde_json::json!({ "command": "ls", "read_folders": [cache.text()] }),
        );
        assert_eq!(
            decide_allowing(&gate, &inside, &mut asked).await,
            (granted_with(Access::Network, writing(&cache)), None)
        );

        // Asking again for what the run has needs nobody either.
        assert!(matches!(
            decide_allowing(&gate, &request, &mut asked).await,
            (ToolDecision::AllowWith { .. }, None)
        ));

        // Global reads need no new grant; an extra write still asks.
        let other = Outside::new();
        let beyond = call_with(
            RunCommand::NAME,
            serde_json::json!({ "command": "ls", "read_folders": [other.text()] }),
        );
        let mut both = writing(&cache);
        both.read.push(other.0.clone());
        assert_eq!(
            decide_allowing(&gate, &beyond, &mut asked).await,
            (granted_with(Access::Network, both), None)
        );
        let beyond = call_with(
            RunCommand::NAME,
            serde_json::json!({"command": "touch", "write_folders": [other.text()]}),
        );
        assert_eq!(
            decide_allowing(&gate, &beyond, &mut asked)
                .await
                .1
                .as_deref(),
            Some(RunCommand::NAME)
        );
    }

    /// A denied request grants nothing, and one asking for nothing is
    /// refused without asking.
    #[tokio::test]
    async fn denied_or_empty_requests_grant_nothing() {
        let (gate, mut asked, _) = gate_in(false, Access::Read);
        let request = call_with(
            RequestPermissions::NAME,
            serde_json::json!({ "reason": "Write.", "access": "write" }),
        );
        let decision = gate.before_tool_call(&request);
        let answer = async {
            let Some(RunUpdate::Approval { decide, .. }) = asked.recv().await else {
                panic!("the request asks");
            };
            decide
                .send(ToolDecision::Deny {
                    reason: DENIED.into(),
                })
                .expect("the gate waits");
        };
        let (decision, ()) = tokio::join!(decision, answer);
        assert!(matches!(decision, ToolDecision::Deny { .. }));
        assert_eq!(gate.granted(), Grant::default());

        let nothing = call_with(
            RequestPermissions::NAME,
            serde_json::json!({ "reason": "Nothing." }),
        );
        assert!(matches!(
            decide_allowing(&gate, &nothing, &mut asked).await,
            (ToolDecision::Deny { .. }, None)
        ));
    }

    /// The project mode is prior approval, while explicit narrower access
    /// still restricts the command. Escalations ask even in scheduled runs.
    #[tokio::test]
    async fn commands_ask_only_beyond_the_auto_approval_level() {
        let asked_for = Some(RunCommand::NAME.to_owned());
        let cases = [
            (Access::Read, None, Access::Read, false),
            (Access::Read, Some("write"), Access::Write, true),
            (Access::Read, Some("network"), Access::Network, true),
            (Access::Write, None, Access::Write, false),
            (Access::Write, Some("read"), Access::Read, false),
            (Access::Write, Some("write"), Access::Write, false),
            (Access::Write, Some("network"), Access::Network, true),
            (Access::Network, None, Access::Network, false),
            (Access::Network, Some("read"), Access::Read, false),
            (Access::Network, Some("write"), Access::Write, false),
            (Access::Network, Some("network"), Access::Network, false),
        ];
        for (mode, requested, access, asks) in cases {
            let call = command(requested);
            let (gate, mut asked, _) = gate_in(false, mode);
            let expected = (granted(access), asks.then(|| asked_for.clone()).flatten());
            assert_eq!(
                decide_allowing(&gate, &call, &mut asked).await,
                expected,
                "{mode:?} mode, asking for {requested:?}"
            );

            // A scheduled run refuses a call that asks the first time.
            let (gate, mut asked, _) = gate_in(true, mode);
            let decided = decide_allowing(&gate, &call, &mut asked).await;
            if asks {
                assert!(matches!(decided, (ToolDecision::Deny { .. }, None)));
            } else {
                assert_eq!(decided, (granted(access), None));
            }
        }
    }

    /// Writing a file runs at once in write and network mode, even in a
    /// scheduled run, and is asked about in read mode.
    #[tokio::test]
    async fn writing_a_file_is_asked_about_only_in_read_mode() {
        for (mode, asks) in [
            (Access::Read, true),
            (Access::Write, false),
            (Access::Network, false),
        ] {
            let expected = (
                ToolDecision::AllowWith {
                    context: WriteFile::context("scratch://approved.txt".into()),
                },
                asks.then(|| WriteFile::NAME.to_owned()),
            );
            let (gate, mut asked, _) = gate_in(false, mode);
            assert_eq!(
                decide_allowing(&gate, &call(WriteFile::NAME), &mut asked).await,
                expected,
                "{mode:?}"
            );
            let (gate, mut asked, _) = gate_in(true, mode);
            let decided = decide_allowing(&gate, &call(WriteFile::NAME), &mut asked).await;
            if asks {
                assert!(
                    matches!(decided, (ToolDecision::Deny { .. }, None)),
                    "{mode:?}"
                );
            } else {
                assert_eq!(decided, expected, "{mode:?}");
            }
        }
    }

    /// Allowing a Read-mode write grants that call, without changing the
    /// project or allowing the following write automatically.
    #[tokio::test]
    async fn an_approved_file_write_succeeds_in_read_mode() {
        let project = Outside::new();
        let (gate, mut asked, sandboxes) = gate_in(false, Access::Read);
        sandboxes.set_project(
            "thread",
            sandbox::Project {
                folders: vec![sandbox::ProjectFolder {
                    name: "project".into(),
                    path: project.0.clone(),
                }],
                access: Access::Read,
            },
        );
        let args = serde_json::json!({"path": "project/new.txt", "content": "approved"});
        let call = call_with(WriteFile::NAME, args.clone());
        let (decision, asked_tool) = decide_allowing(&gate, &call, &mut asked).await;
        assert_eq!(asked_tool.as_deref(), Some(WriteFile::NAME));
        let ToolDecision::AllowWith { mut context } = decision else {
            panic!("the write has a grant")
        };
        WriteFile::new(sandboxes.clone(), "thread")
            .call(&mut context, serde_json::from_value(args.clone()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(project.0.join("new.txt")).unwrap(),
            "approved"
        );
        assert_eq!(sandboxes.project("thread").access, Access::Read);
        assert_eq!(
            decide_allowing(&gate, &call, &mut asked).await.1.as_deref(),
            Some(WriteFile::NAME)
        );
        // Approval cannot be reused for another target, nor to escape the project.
        for path in ["project/other.txt", "../outside.txt"] {
            let result = WriteFile::new(sandboxes.clone(), "thread")
                .call(
                    &mut context,
                    serde_json::from_value(serde_json::json!({"path": path, "content": "no"}))
                        .unwrap(),
                )
                .await;
            assert!(result.is_err(), "{path}");
        }
    }

    /// A mode reduction changes subsequent automatic approvals; it does
    /// not erase permissions explicitly requested for the rest of the run.
    #[tokio::test]
    async fn lowering_auto_approval_preserves_only_run_grants() {
        let (gate, mut asked, sandboxes) = gate_in(false, Access::Network);
        assert_eq!(
            decide_allowing(&gate, &command(None), &mut asked).await,
            (granted(Access::Network), None)
        );
        set_mode(&sandboxes, Access::Read);
        assert_eq!(
            decide_allowing(&gate, &command(Some("write")), &mut asked)
                .await
                .1
                .as_deref(),
            Some(RunCommand::NAME)
        );
        set_mode(&sandboxes, Access::Write);
        let request = call_with(
            RequestPermissions::NAME,
            serde_json::json!({"reason": "Edit the project.", "access": "write"}),
        );
        assert_eq!(decide_allowing(&gate, &request, &mut asked).await.1, None);
        set_mode(&sandboxes, Access::Read);
        assert_eq!(
            decide_allowing(&gate, &command(None), &mut asked).await,
            (granted(Access::Write), None)
        );
        assert_eq!(
            decide_allowing(&gate, &command(Some("read")), &mut asked).await,
            (granted(Access::Read), None)
        );
    }

    /// A waiting command is allowed with the access its card shows when
    /// it is decided: the mode's at that moment, unless it asked for its own.
    #[tokio::test]
    async fn an_allowed_command_gets_the_access_shown_when_deciding() {
        for (requested, access) in [(None, Access::Read), (Some("write"), Access::Write)] {
            let (gate, mut asked, sandboxes) = gate_in(false, Access::Write);
            // Extra folders make this wait even though Write is automatic.
            let outside = Outside::new();
            let mut call = command(requested);
            call.function
                .arguments
                .insert("write_folders".into(), serde_json::json!([outside.text()]));
            let decision = gate.before_tool_call(&call);
            let answer = async {
                let Some(RunUpdate::Approval { decide, .. }) = asked.recv().await else {
                    panic!("the command asks");
                };
                set_mode(&sandboxes, Access::Read);
                decide.send(ToolDecision::Allow).expect("the gate waits");
            };
            let (decision, ()) = tokio::join!(decision, answer);
            assert_eq!(
                decision,
                granted_with(
                    access,
                    Folders {
                        read: vec![],
                        write: vec![outside.0.clone()]
                    }
                ),
                "asking for {requested:?}"
            );
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
        // beyond the project level is refused the first time.
        let comment = call(RespondToComment::NAME);
        assert_eq!(gate.before_tool_call(&comment).await, ToolDecision::Allow);
        assert!(matches!(
            gate.before_tool_call(&command(Some("network"))).await,
            ToolDecision::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn a_command_beyond_the_auto_approval_level_can_be_denied() {
        let command = command(Some("network"));
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

    /// A reply making several calls, run by the agent loop with the gate:
    /// each call is asked about on its own, so allowing the first decides
    /// nothing for the rest. Also when the provider sends no call ids, as
    /// Ollama may, since Rig gives each call its own.
    #[tokio::test]
    async fn each_call_of_a_reply_is_asked_about_on_its_own() {
        for ids in [["c1", "c2", "c3", "c4"], ["", "", "", ""]] {
            use rig::test_utils::{
                MockCompletionModel, MockStreamEvent, mock_final_with_total_tokens,
            };
            let mut first = ids
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    MockStreamEvent::tool_call(
                        *id,
                        tools::Calculate::NAME,
                        serde_json::json!({"operation": "add", "a": index, "b": 1}),
                    )
                })
                .collect::<Vec<_>>();
            first.push(MockStreamEvent::FinalResponse(
                mock_final_with_total_tokens(10),
            ));
            let model = MockCompletionModel::from_stream_turns([
                first,
                vec![
                    MockStreamEvent::Text("Done.".into()),
                    MockStreamEvent::FinalResponse(mock_final_with_total_tokens(20)),
                ],
            ]);
            let mut tool_set = rig::tool::ToolSet::default();
            tool_set.add_tool(tools::Calculate);
            let (gate, mut asked) = gate(false);
            let run = tokio::spawn(async move {
                let mut history = Vec::new();
                let mut results = Vec::new();
                agent::Agent::new(model.erase(), tool_set)
                    .tool_hook(gate)
                    .run(rig::message::Message::user("go"), &mut history, |event| {
                        if let AgentEvent::ToolResult { call, result } = event {
                            results.push((call, result.status_name()));
                        }
                    })
                    .await
                    .expect("the run");
                results
            });
            let mut approvals = Vec::new();
            while let Some(update) = asked.recv().await {
                let RunUpdate::Approval { call, decide, .. } = update else {
                    continue;
                };
                approvals.push(call);
                // Allow only the first; deny the rest.
                _ = decide.send(if approvals.len() == 1 {
                    ToolDecision::Allow
                } else {
                    ToolDecision::Deny {
                        reason: DENIED.into(),
                    }
                });
            }
            let results = run.await.expect("the run");
            assert_eq!(approvals.len(), 4, "every call asks: {ids:?}");
            assert_eq!(
                results
                    .iter()
                    .map(|(call, _)| call.clone())
                    .collect::<Vec<_>>(),
                approvals,
                "asked one at a time, in order, each a call of its own"
            );
            assert_eq!(
                results
                    .iter()
                    .map(|(_, status)| *status)
                    .collect::<Vec<_>>(),
                ["success", "skipped", "skipped", "skipped"],
                "only the allowed call ran"
            );
        }
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
