//! Deciding which tool calls run: the thread agent's host policy, and the
//! decisions of whoever may approve a call that asks.
//!
//! Calls within the project's level and the run's grants run at once; every
//! other call asks, and runs with exactly what it asked for once allowed.
//! Explicit narrower access is respected, and reads are global except
//! denied credentials. Only the host may approve commands, file writes, and
//! run permissions; Admin peers may approve other tools. Scheduled calls
//! needing approval fail once, then an identical repeat asks. See
//! `docs/permissions.md`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64},
};

use agent::{
    AgentHost, CallAuthorization, Decision, Input, RenderInput,
    model::{Content, ToolCallId},
    tool::Provides,
};
use gpui::Context;
use parking_lot::Mutex;
use sandbox::{
    EditFile, ListDirectory, Needs, ReadFile, RequestPermissions, RunCommand, ThreadSandbox,
    ViewImage, WriteFile,
};
use tools::{Calculate, CommentId, RespondToComment, RunComments};
use uuid::Uuid;

use crate::{
    Cowork,
    participant::ParticipantId,
    project_mode::ProjectMode,
    thread::{ApproveTools, Thread, ThreadSharing},
    transcript::RunSource,
};

/// Tools whose calls always run, as they only compute, act within the thread,
/// or read what the host shared with it. Every other tool's calls are decided
/// by what they need, and a tool added without a thought about it is asked
/// about rather than trusted.
pub(crate) const ALWAYS_ALLOWED: &[&str] = &[
    Calculate::NAME,
    RespondToComment::NAME,
    ListDirectory::NAME,
    ReadFile::NAME,
    ViewImage::NAME,
];

/// What the model is told about a call someone denied.
pub(crate) const DENIED: &str = "The user denied this tool call.";

/// What a scheduled run's model is told about a call it makes the first
/// time, rather than waiting for someone who may not be there.
pub(crate) const UNATTENDED: &str = "This tool call was not run: it needs the user's permission, and this is a scheduled run that nobody may be watching. Carry on without it if you can. If the call is necessary, make it again with exactly the same arguments: the user will then be notified and asked, and the run waits for their answer.";

/// Tools whose calls only the host may allow: they act on the host's computer
/// (its shell and tools, for `run_command`, its files, for `write_file` and
/// `edit_file`, and
/// what later commands may do there, for `request_permissions`), so an
/// `Admin` peer may not decide for it.
pub(crate) const HOST_ONLY: &[&str] = &[
    RunCommand::NAME,
    WriteFile::NAME,
    EditFile::NAME,
    RequestPermissions::NAME,
];

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

/// A prompt as the session holds it until committed: its content, and the
/// comment aliases it introduces. A steering prompt's aliases join its run's
/// comments only once it commits, so a withdrawn steer names nothing the
/// model could answer.
#[derive(Clone, Debug)]
pub(crate) struct CoworkInput {
    pub(crate) content: Vec<Content>,
    pub(crate) comments: Vec<CommentId>,
}

impl From<Vec<Content>> for CoworkInput {
    fn from(content: Vec<Content>) -> Self {
        Self {
            content,
            comments: Vec::new(),
        }
    }
}

impl RenderInput for CoworkInput {
    fn render(&self) -> Vec<Content> {
        self.content.clone()
    }
}

/// The application side of a thread's agent session: its tools' context
/// (the thread's sandbox, and the run's comments), its policy for each call,
/// and when to compact.
pub(crate) struct CoworkHost {
    sandbox: Arc<ThreadSandbox>,
    comments: Arc<RunComments>,
    /// The calls a scheduled run already made once without being asked
    /// about, by tool and arguments (whose keys may come in any order).
    unattended: Mutex<Vec<(String, serde_json::Value)>>,
    pub(crate) compaction: CompactionTrigger,
}

/// When the session summarizes its context, from the token counts its
/// requests report; see `generation.rs`.
#[derive(Default)]
pub(crate) struct CompactionTrigger {
    /// Tokens at which to compact; `u64::MAX` for never.
    pub(crate) threshold: AtomicU64,
    /// Whether the next run compacts as soon as it starts, as the context
    /// was already full.
    pub(crate) at_start: AtomicBool,
    /// Whether a compaction was asked for and has not settled yet.
    pub(crate) requested: AtomicBool,
}

impl CoworkHost {
    pub(crate) fn new(sandbox: Arc<ThreadSandbox>) -> Self {
        Self {
            sandbox,
            comments: Arc::default(),
            unattended: Mutex::default(),
            compaction: CompactionTrigger {
                threshold: AtomicU64::new(u64::MAX),
                ..CompactionTrigger::default()
            },
        }
    }

    pub(crate) fn sandbox(&self) -> &Arc<ThreadSandbox> {
        &self.sandbox
    }

    /// The comments the next run answers.
    pub(crate) fn comments(&self) -> &RunComments {
        &self.comments
    }

    /// Forgets the last run's grants and unattended calls, as a run starts.
    pub(crate) fn run_started(&self) {
        self.sandbox.end_run();
        self.unattended.lock().clear();
    }

    /// Whether a scheduled run makes this call for the first time, which is
    /// then remembered so that the same call again is asked about.
    fn refuses_unattended(&self, tool: &str, arguments: &serde_json::Value) -> bool {
        let mut refused = self.unattended.lock();
        let key = (tool.to_owned(), arguments.clone());
        if refused.contains(&key) {
            return false;
        }
        refused.push(key);
        true
    }
}

impl Provides<ThreadSandbox> for CoworkHost {
    fn provide(self: &Arc<Self>) -> Arc<ThreadSandbox> {
        self.sandbox.clone()
    }
}

impl Provides<RunComments> for CoworkHost {
    fn provide(self: &Arc<Self>) -> Arc<RunComments> {
        self.comments.clone()
    }
}

impl std::fmt::Debug for CoworkHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoworkHost")
            .field("thread", &self.sandbox.thread())
            .finish_non_exhaustive()
    }
}

impl AgentHost for CoworkHost {
    type Source = RunSource;
    type Input = CoworkInput;
    type Access = Needs;
    /// The project's mode and folders are the sandbox's, which the tools
    /// and authorization read as they are when a call starts.
    type Permissions = ();

    fn authorize(&self, call: &CallAuthorization<'_, Self>) -> Decision {
        let tool = call.tool.name();
        if !needs_approval(tool) {
            return Decision::Allow;
        }
        match call.access {
            Needs::Refused(reason) => return Decision::Deny(reason.clone()),
            Needs::Command { .. } | Needs::Write { .. } | Needs::Permissions(_)
                if self.sandbox.covers(call.access) =>
            {
                return Decision::Allow;
            }
            _ => {}
        }
        if *call.origin == RunSource::Schedule && self.refuses_unattended(tool, call.arguments) {
            return Decision::Deny(UNATTENDED.into());
        }
        Decision::Ask
    }

    /// A run's own prompt names its comments when the run starts; a steer's
    /// are added here, before the model's next reply could answer them.
    fn inputs_committed(&self, inputs: &[Input<RunSource, CoworkInput>]) {
        let comments = self.comments.current();
        for input in inputs {
            comments.extend(&input.payload.comments);
        }
    }
}

impl Cowork {
    /// Allows or denies, as the local user, the call that agent message
    /// `message_id` of the active thread is waiting on. A mirrored thread
    /// asks its host.
    pub(crate) fn decide_tool_call(
        &mut self,
        message_id: Uuid,
        call: ToolCallId,
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
        call: &ToolCallId,
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
                if !generation.decide(call, allow) {
                    return false;
                }
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
    use agent::tool::{ToolDescriptor, ToolDyn};
    use sandbox::{Access, Folders, Grant};

    use super::*;

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
        assert!(host.may_decide(Calculate::NAME));
        assert!(!admin.may_decide(RunCommand::NAME));
        assert!(!admin.may_decide(WriteFile::NAME));
        assert!(!admin.may_decide(RequestPermissions::NAME));
        assert!(admin.may_decide(Calculate::NAME));
        assert!(!writer.may_decide(Calculate::NAME));
    }

    /// A host for thread `thread` in a project in `access` mode.
    fn host_in(access: Access) -> Arc<CoworkHost> {
        let sandboxes = crate::test_support::unused_sandboxes();
        let host = Arc::new(CoworkHost::new(Arc::new(ThreadSandbox::new(
            sandboxes, "thread",
        ))));
        set_mode(&host, access);
        host
    }

    fn set_mode(host: &CoworkHost, access: Access) {
        host.sandbox().sandboxes().set_project(
            "thread",
            sandbox::Project {
                folders: Vec::new(),
                access,
            },
        );
    }

    /// What `host` decides about a call of `tool` with `arguments` in a run
    /// `origin` started, as a session would ask it.
    fn decide<T>(
        host: &Arc<CoworkHost>,
        tool: T,
        arguments: serde_json::Value,
        origin: RunSource,
    ) -> Decision
    where
        T: agent::tool::Tool,
        CoworkHost: Provides<T::Context>,
        Needs: From<T::Access>,
    {
        let tool = Arc::new(tool);
        let descriptor = ToolDescriptor {
            definition: ToolDyn::<CoworkHost, Needs>::definition(&*tool),
            capabilities: Default::default(),
        };
        let prepared = ToolDyn::<CoworkHost, Needs>::prepare(tool, arguments.clone(), host)
            .expect("valid arguments");
        host.authorize(&CallAuthorization {
            tool: &descriptor,
            arguments: &arguments,
            access: prepared.access(),
            origin: &origin,
            permissions: &(),
            grants: &[],
        })
    }

    fn by_someone() -> RunSource {
        RunSource::Participant([1; 16])
    }

    /// A `run_command` call, asking for `access` of its own if given.
    fn command(access: Option<&str>) -> serde_json::Value {
        let mut arguments = serde_json::json!({ "command": "ls" });
        if let Some(access) = access {
            arguments["access"] = access.into();
        }
        arguments
    }

    /// A folder outside any project, removed on drop, and its resolved path
    /// as grants name it.
    struct Outside(std::path::PathBuf);

    impl Outside {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cowork-outside-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            let named = path.to_string_lossy().into_owned();
            Self(Folders::resolve(&[], &[named]).unwrap().write.remove(0))
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

    /// The project mode is prior approval, while explicit narrower access
    /// still restricts the command. Escalations ask, and a scheduled run
    /// refuses them the first time.
    #[test]
    fn commands_ask_only_beyond_the_auto_approval_level() {
        let cases = [
            (Access::Read, None, false),
            (Access::Read, Some("write"), true),
            (Access::Read, Some("network"), true),
            (Access::Write, None, false),
            (Access::Write, Some("read"), false),
            (Access::Write, Some("write"), false),
            (Access::Write, Some("network"), true),
            (Access::Network, None, false),
            (Access::Network, Some("read"), false),
            (Access::Network, Some("network"), false),
        ];
        for (mode, requested, asks) in cases {
            let host = host_in(mode);
            let expected = if asks { Decision::Ask } else { Decision::Allow };
            assert_eq!(
                decide(&host, RunCommand, command(requested), by_someone()),
                expected,
                "{mode:?} mode, asking for {requested:?}"
            );
            let host = host_in(mode);
            let decided = decide(&host, RunCommand, command(requested), RunSource::Schedule);
            if asks {
                assert_eq!(decided, Decision::Deny(UNATTENDED.into()));
            } else {
                assert_eq!(decided, Decision::Allow);
            }
        }
    }

    /// A command naming a folder outside the project to write asks even in
    /// network mode; one naming a folder that can't be granted is refused
    /// without asking. Reading anywhere needs nothing.
    #[test]
    fn commands_naming_folders_are_asked_about() {
        let cache = Outside::new();
        let writes =
            serde_json::json!({ "command": "cargo fetch", "write_folders": [cache.text()] });
        for mode in [Access::Read, Access::Write, Access::Network] {
            assert_eq!(
                decide(&host_in(mode), RunCommand, writes.clone(), by_someone()),
                Decision::Ask,
                "{mode:?}"
            );
        }
        let reads = serde_json::json!({ "command": "ls", "read_folders": [cache.text()] });
        assert_eq!(
            decide(&host_in(Access::Read), RunCommand, reads, by_someone()),
            Decision::Allow
        );
        let missing = cache.0.join("no-such-folder");
        let missing = serde_json::json!({ "command": "ls", "read_folders": [missing] });
        assert!(matches!(
            decide(&host_in(Access::Read), RunCommand, missing, by_someone()),
            Decision::Deny(reason) if reason.contains("does not exist")
        ));
    }

    /// Once the run is granted more, its later commands and requests within
    /// the grant run without asking, also after the mode is lowered; a new
    /// run starts without them.
    #[test]
    fn run_grants_apply_to_later_calls_until_the_run_ends() {
        let cache = Outside::new();
        let host = host_in(Access::Read);
        let request = serde_json::json!({
            "reason": "Fetch dependencies.",
            "access": "network",
            "write_folders": [cache.text()],
        });
        assert_eq!(
            decide(&host, RequestPermissions, request.clone(), by_someone()),
            Decision::Ask
        );
        // What the tool does once allowed.
        host.sandbox().sandboxes().grant_for_run(
            "thread",
            &Grant {
                access: Access::Network,
                folders: Folders {
                    read: Vec::new(),
                    write: vec![cache.0.clone()],
                },
            },
        );
        let writes = serde_json::json!({
            "command": "cargo fetch",
            "access": "network",
            "write_folders": [cache.text()],
        });
        assert_eq!(
            decide(&host, RunCommand, writes.clone(), by_someone()),
            Decision::Allow
        );
        assert_eq!(
            decide(&host, RequestPermissions, request, by_someone()),
            Decision::Allow,
            "asking again for what the run has needs nobody"
        );
        let other = Outside::new();
        let beyond = serde_json::json!({"command": "touch", "write_folders": [other.text()]});
        assert_eq!(
            decide(&host, RunCommand, beyond, by_someone()),
            Decision::Ask
        );

        host.run_started();
        assert_eq!(
            decide(&host, RunCommand, writes, by_someone()),
            Decision::Ask
        );
    }

    /// Writing or editing a file runs at once in write and network mode, even
    /// in a scheduled run, and is asked about in read mode.
    #[test]
    fn writing_a_file_is_asked_about_only_in_read_mode() {
        let write = serde_json::json!({"path": "scratch://approved.txt", "content": "hello"});
        for (mode, asks) in [
            (Access::Read, true),
            (Access::Write, false),
            (Access::Network, false),
        ] {
            let expected = if asks { Decision::Ask } else { Decision::Allow };
            assert_eq!(
                decide(&host_in(mode), WriteFile, write.clone(), by_someone()),
                expected,
                "{mode:?}"
            );
            let scheduled = decide(
                &host_in(mode),
                WriteFile,
                write.clone(),
                RunSource::Schedule,
            );
            if asks {
                assert_eq!(scheduled, Decision::Deny(UNATTENDED.into()), "{mode:?}");
            } else {
                assert_eq!(scheduled, Decision::Allow, "{mode:?}");
            }
            // An edit needs what a write does.
            let edit = serde_json::json!({
                "path": "scratch://approved.txt", "old_string": "a", "new_string": "b",
            });
            assert_eq!(
                decide(&host_in(mode), EditFile, edit, by_someone()),
                expected,
                "{mode:?}"
            );
        }
    }

    /// A request for nothing is refused without asking.
    #[test]
    fn empty_permission_requests_are_refused() {
        let nothing = serde_json::json!({ "reason": "Nothing." });
        assert!(matches!(
            decide(
                &host_in(Access::Read),
                RequestPermissions,
                nothing,
                by_someone()
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn a_scheduled_run_asks_only_when_a_call_is_repeated() {
        let host = host_in(Access::Write);
        let network = |order: bool| {
            if order {
                serde_json::json!({"command": "curl a", "access": "network"})
            } else {
                serde_json::json!({"access": "network", "command": "curl a"})
            }
        };
        assert_eq!(
            decide(&host, RunCommand, network(true), RunSource::Schedule),
            Decision::Deny(UNATTENDED.into())
        );
        // Other arguments are another call, refused once too.
        let other = serde_json::json!({"command": "curl b", "access": "network"});
        assert_eq!(
            decide(&host, RunCommand, other, RunSource::Schedule),
            Decision::Deny(UNATTENDED.into())
        );
        // The same arguments, in any order, ask.
        assert_eq!(
            decide(&host, RunCommand, network(false), RunSource::Schedule),
            Decision::Ask
        );
        // Calls that need no approval still run at once.
        let sum = serde_json::json!({"operation": "add", "a": 1, "b": 2});
        assert_eq!(
            decide(&host, Calculate, sum, RunSource::Schedule),
            Decision::Allow
        );
    }

    /// A mode reduction changes later automatic approvals; it does not
    /// erase permissions granted for the rest of the run.
    #[test]
    fn lowering_auto_approval_preserves_only_run_grants() {
        let host = host_in(Access::Network);
        assert_eq!(
            decide(&host, RunCommand, command(None), by_someone()),
            Decision::Allow
        );
        set_mode(&host, Access::Read);
        assert_eq!(
            decide(&host, RunCommand, command(Some("write")), by_someone()),
            Decision::Ask
        );
        host.sandbox().sandboxes().grant_for_run(
            "thread",
            &Grant {
                access: Access::Write,
                ..Grant::default()
            },
        );
        assert_eq!(
            decide(&host, RunCommand, command(Some("write")), by_someone()),
            Decision::Allow
        );
        assert_eq!(
            decide(&host, RunCommand, command(Some("network")), by_someone()),
            Decision::Ask
        );
    }
}
