//! The tool the model runs shell commands with.
//!
//! Its definition never changes: it is part of the prompt prefix, so a
//! change would invalidate the model's prompt cache. What changes, the
//! project's folders and mode, reaches the model as an environment note in
//! the conversation instead; see [`Sandboxes::environment_note`].

use std::sync::Arc;

use rig::tool::{ContextValue, Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};

use crate::{CommandOutput, SandboxError, Sandboxes};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RunCommandArgs {
    /// A command line for the platform's shell, for example `ls -la`.
    pub command: String,
}

/// Runs a shell command, contained, on the host's computer for the thread
/// the run belongs to.
pub struct RunCommand {
    sandboxes: Arc<Sandboxes>,
    thread: String,
}

impl RunCommand {
    pub fn new(sandboxes: Arc<Sandboxes>, thread: impl Into<String>) -> Self {
        Self {
            sandboxes,
            thread: thread.into(),
        }
    }

    /// The context for a call allowed only on the condition that it changes
    /// nothing in the project: the command then runs with the project
    /// read-only, even if the project became writable after the call was
    /// allowed.
    pub fn read_only_context() -> ToolContext {
        let mut context = ToolContext::new();
        context
            .insert(ReadOnly)
            .expect("a unit struct always encodes");
        context
    }
}

/// Marks a call's context as [`RunCommand::read_only_context`].
#[derive(Serialize, Deserialize)]
struct ReadOnly;

impl ContextValue for ReadOnly {
    const KEY: &'static str = "sandbox.run_command.read_only";
}

const DESCRIPTION: &str = concat!(
    "Run a shell command on the user's computer, inside a sandbox, and get its exit code, \
     stdout, and stderr. The shell is the platform's own (the system prompt names the platform), \
     and the user's installed tools and toolchains are available. While the project is read-only, \
     calls run at once. While it is writable, every call waits for the user to allow it, except \
     in a scheduled run, where the first such call is refused with an explanation. Which project \
     folders commands can use, whether they can change them, and the \
     folder commands start in are given in the latest <environment> note in the conversation. ",
    // Fixed per platform, so the definition stays the same within a session.
    // See `settings` for why Windows differs.
    reads!(),
    " Commands have no network access, so nothing can be downloaded or installed. Each command \
     can write a scratch folder, its HOME and TMPDIR, kept for this conversation. Anything a \
     command leaves running in the background is ended when it exits."
);

#[cfg(windows)]
macro_rules! reads {
    () => {
        "Commands can read the user's other files too, except credentials, but change nothing \
         outside the project and the scratch folder; don't read outside the project unless the \
         user asks."
    };
}

#[cfg(not(windows))]
macro_rules! reads {
    () => {
        "Commands cannot read the user's other files."
    };
}
use reads;

impl Tool for RunCommand {
    const NAME: &'static str = "run_command";
    type Error = SandboxError;
    type Args = RunCommandArgs;
    type Output = CommandOutput;

    fn description(&self) -> String {
        DESCRIPTION.to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        schemars::schema_for!(RunCommandArgs).into()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            SandboxError::InvalidCommand(_) => ToolExecutionError::invalid_args(error.to_string()),
            SandboxError::Unsupported(_) => ToolExecutionError::not_found(error.to_string()),
            SandboxError::Setup(_) | SandboxError::Failed(_) => {
                ToolExecutionError::other(error.to_string())
            }
        }
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        if context.contains::<ReadOnly>() {
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
    use rig::tool::ToolSet;
    use serde_json::json;

    use super::*;
    use crate::{Project, ProjectFolder, TestRoot, test_sandboxes};

    /// A call allowed on a read-only project changes nothing even when the
    /// project became writable before the command started.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_only_calls_stay_read_only() {
        if !mxc_sdk::v1::platform_support().is_supported {
            return;
        }
        let (sandboxes, _scratch) = test_sandboxes();
        // Not under the scratch root, where it would be taken for a leftover.
        let project = TestRoot::new();
        let folder = project.0.clone();
        sandboxes.set_project(
            "thread",
            Project {
                folders: vec![ProjectFolder {
                    name: "project".into(),
                    path: folder.clone(),
                }],
                writable: true,
            },
        );
        let mut tools = ToolSet::default();
        tools.add_tool(RunCommand::new(sandboxes.clone(), "thread"));
        let write = |file: &str| {
            let command = if cfg!(windows) {
                format!("cmd /c echo agent> {file}")
            } else {
                format!("echo agent > {file}")
            };
            json!({ "command": command }).to_string()
        };

        tools
            .execute(
                RunCommand::NAME,
                write("held.txt"),
                &mut RunCommand::read_only_context(),
            )
            .await;
        assert!(!folder.join("held.txt").exists(), "held read-only");

        tools
            .execute(
                RunCommand::NAME,
                write("allowed.txt"),
                &mut ToolContext::new(),
            )
            .await;
        assert!(
            folder.join("allowed.txt").exists(),
            "the project is writable"
        );
        sandboxes.shutdown();
    }

    #[test]
    fn the_definition_leaves_what_changes_to_the_environment_note() {
        let (sandboxes, _home) = test_sandboxes();
        let tool = RunCommand::new(sandboxes, "thread");
        let description = tool.description();
        for guidance in [
            "platform's own",
            "installed tools",
            "<environment> note",
            "no network access",
            "HOME and TMPDIR",
        ] {
            assert!(
                description.contains(guidance),
                "missing guidance: {guidance}"
            );
        }
        assert!(!description.contains('/'), "names no host paths");
    }

    #[test]
    fn schema_and_argument_names_match() {
        let (sandboxes, _home) = test_sandboxes();
        let tool = RunCommand::new(sandboxes, "thread");
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
