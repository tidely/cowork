//! The tool the model runs shell commands with.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use rig::tool::{Tool, ToolContext, ToolExecutionError};
use serde::Deserialize;

use crate::{CommandOutput, SandboxError, Sandboxes, settings};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RunCommandArgs {
    /// A POSIX sh command (not Bash), for example `ls -la` or `printf '%s\n' hello`.
    pub command: String,
}

/// Runs a shell command in the sandbox of the thread the run belongs to.
pub struct RunCommand {
    sandboxes: Arc<Sandboxes>,
    thread: String,
    pass: ReadOnlyPass,
}

impl RunCommand {
    pub fn new(sandboxes: Arc<Sandboxes>, thread: impl Into<String>) -> Self {
        let thread = thread.into();
        Self {
            pass: ReadOnlyPass {
                sandboxes: sandboxes.clone(),
                thread: thread.clone(),
                granted: Arc::new(AtomicBool::new(false)),
            },
            sandboxes,
            thread,
        }
    }

    /// What lets this tool's next call run without asking while the project
    /// is read-only. For the run's approval hook.
    pub fn read_only_pass(&self) -> ReadOnlyPass {
        self.pass.clone()
    }
}

/// Lets a call of [`RunCommand`] run without anyone allowing it, as long as
/// the thread's project is read-only: its commands cannot change anything
/// outside the sandbox then.
///
/// The approval hook grants the pass instead of asking; the tool's next call
/// takes it and runs with [`Sandboxes::run_read_only`], so a project made
/// writable between the two refuses the command rather than running it
/// unasked. This relies on the agent deciding about a call and running it
/// before deciding about the next, as it does.
#[derive(Clone)]
pub struct ReadOnlyPass {
    sandboxes: Arc<Sandboxes>,
    thread: String,
    granted: Arc<AtomicBool>,
}

impl ReadOnlyPass {
    /// Grants the pass if the project is read-only now, returning whether
    /// it did.
    pub fn grant_if_read_only(&self) -> bool {
        let read_only = !self.sandboxes.project(&self.thread).writable;
        if read_only {
            self.granted.store(true, Ordering::SeqCst);
        }
        read_only
    }

    fn take(&self) -> bool {
        self.granted.swap(false, Ordering::SeqCst)
    }
}

impl Tool for RunCommand {
    const NAME: &'static str = "run_command";
    type Error = SandboxError;
    type Args = RunCommandArgs;
    type Output = CommandOutput;

    fn description(&self) -> String {
        format!(
            "Run a shell command with /bin/sh -c in an isolated Alpine Linux virtual machine, \
             and get its exit code, stdout, and stderr. While the project is read-only calls run \
                          at once; when the user allowed writing, every call waits for a person to allow it. \
             Use POSIX sh (BusyBox ash), not Bash. Basic BusyBox utilities are available, \
             including ls, cat, printf, grep, sed, awk, find, sort, and tar. Bash, git, Python, \
             Node.js, and Rust toolchains are not installed. Check other commands with \
             `command -v NAME` or list utilities with `busybox --list`. \
             The machine has no network access. The folders the user added to the project are \
             in {projects}, one directory each (list them with `ls {projects}`); there are no \
             other user files. They are the user's real files: they are read-only unless the \
             user allowed writing, and then changes apply to the user's computer at once. \
             Commands run as an unprivileged user starting in {workdir}; besides a writable \
             project, only {scratch} and similar scratch locations are writable, so put other \
             files there. Files persist between calls \
             in this conversation until the machine has been idle for {idle} minutes, or until \
             the user changes the project, which starts a fresh machine (the output then says \
             `sandbox_restarted`). A command is killed after {timeout} seconds, or once it \
             writes more than {limit} KiB to stdout or stderr. Nothing can be installed, as \
             there is no network.",
            projects = settings::PROJECTS_DIR,
            workdir = settings::WORKDIR,
            scratch = settings::SCRATCH_DIR,
            idle = settings::IDLE_TIMEOUT.as_secs() / 60,
            timeout = settings::COMMAND_TIMEOUT.as_secs(),
            limit = settings::OUTPUT_LIMIT / 1024,
        )
    }

    fn parameters(&self) -> serde_json::Value {
        schemars::schema_for!(RunCommandArgs).into()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            SandboxError::InvalidCommand(_) => ToolExecutionError::invalid_args(error.to_string()),
            SandboxError::Unsupported => ToolExecutionError::not_found(error.to_string()),
            SandboxError::Setup(_)
            | SandboxError::Failed(_)
            | SandboxError::NotStarted(_)
            | SandboxError::NeedsApproval => ToolExecutionError::other(error.to_string()),
        }
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        if self.pass.take() {
            self.sandboxes
                .run_read_only(&self.thread, &args.command)
                .await
        } else {
            self.sandboxes.run(&self.thread, &args.command).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    #[test]
    fn description_names_the_guest_shell_and_tools() {
        let tool = RunCommand::new(
            Arc::new(Sandboxes::new(PathBuf::from("/nonexistent"), "test")),
            "thread",
        );
        let description = tool.description();
        for guidance in [
            "Use POSIX sh (BusyBox ash), not Bash",
            "Basic BusyBox utilities are available",
            "Rust toolchains are not installed",
            "busybox --list",
            "command -v NAME",
            "/projects",
            "read-only unless the user allowed writing",
            "starting in /projects",
            "only /tmp and similar scratch locations are writable",
        ] {
            assert!(
                description.contains(guidance),
                "missing guidance: {guidance}"
            );
        }
        let parameter = tool.parameters()["properties"]["command"]["description"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(parameter.contains("not Bash"));
    }

    fn writable(writable: bool) -> crate::Project {
        crate::Project {
            folders: vec![crate::ProjectFolder {
                name: "zed".into(),
                path: "/host/zed".into(),
            }],
            writable,
        }
    }

    #[tokio::test]
    async fn the_pass_is_granted_only_while_read_only_and_used_once() {
        let sandboxes = Arc::new(Sandboxes::new(PathBuf::from("/nonexistent"), "test"));
        let tool = RunCommand::new(sandboxes.clone(), "thread");
        let pass = tool.read_only_pass();

        sandboxes.set_project("thread", writable(true));
        assert!(!pass.grant_if_read_only());
        assert!(!pass.take());

        sandboxes.set_project("thread", writable(false));
        assert!(pass.grant_if_read_only());
        assert!(tool.pass.take(), "the tool sees the grant");
        assert!(!tool.pass.take(), "and uses it once");
    }

    /// A project made writable after the pass was granted refuses the
    /// command before anything starts.
    #[tokio::test]
    async fn a_pass_does_not_run_commands_in_a_writable_project() {
        let sandboxes = Arc::new(Sandboxes::new(PathBuf::from("/nonexistent"), "test"));
        sandboxes.set_project("thread", writable(true));
        assert!(matches!(
            sandboxes.sandbox("thread", true).await,
            Err(SandboxError::NeedsApproval)
        ));
        assert!(!sandboxes.has_sandbox("thread"), "nothing was started");
    }

    #[test]
    fn schema_and_argument_names_match() {
        let tool = RunCommand::new(
            Arc::new(Sandboxes::new(PathBuf::from("/nonexistent"), "test")),
            "thread",
        );
        let args: RunCommandArgs = serde_json::from_value(json!({"command": "ls"})).unwrap();
        assert_eq!(args.command, "ls");
        let parameters = tool.parameters();
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["properties"]["command"]["type"], "string");
        assert_eq!(parameters["required"], json!(["command"]));
        assert_eq!(parameters["additionalProperties"], false);
        assert!(
            serde_json::from_value::<RunCommandArgs>(json!({"cmd": "ls"})).is_err(),
            "a misnamed argument is refused"
        );
    }
}
