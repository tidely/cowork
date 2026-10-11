//! The tool the model runs shell commands with.
//!
//! Its definition never changes: it is part of the prompt prefix, so a
//! change would invalidate the model's prompt cache. What changes, the
//! project's folders and mode, reaches the model as an environment note in
//! the conversation instead; see [`Sandboxes::environment_note`].

use std::sync::Arc;

use rig::tool::{ContextValue, Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};

use crate::{Access, CommandOutput, SandboxError, Sandboxes};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RunCommandArgs {
    /// A command line for the platform's shell, for example `ls -la`.
    pub command: String,
    /// The access this one command needs, when it differs from the project's
    /// mode: `read` to read the project, `write` to also change it,
    /// `network` to also reach the network. Leave it out to use the mode.
    /// Asking for `read` lets a command that only reads run at once in any
    /// mode; asking for more than the mode waits for the user to allow it.
    #[serde(default)]
    pub access: Option<Access>,
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

    /// The access a call with `arguments` asks for of its own, if it names
    /// one it may have.
    pub fn requested_access(arguments: &serde_json::Value) -> Option<Access> {
        serde_json::from_value(arguments.get("access")?.clone()).ok()
    }

    /// The access a call with `arguments` runs with in `project`: its own,
    /// or the project's mode.
    pub fn access(arguments: &serde_json::Value, project: Access) -> Access {
        Self::requested_access(arguments).unwrap_or(project)
    }

    /// The context for a call allowed with `access`: the command runs with
    /// exactly that, whatever the project's mode by the time it starts.
    pub fn context(access: Access) -> ToolContext {
        let mut context = ToolContext::new();
        context
            .insert(Granted(access))
            .expect("an access always encodes");
        context
    }
}

/// The access a call was allowed with; see [`RunCommand::context`].
#[derive(Serialize, Deserialize)]
struct Granted(Access);

impl ContextValue for Granted {
    const KEY: &'static str = "sandbox.run_command.access";
}

const DESCRIPTION: &str = concat!(
    "Run a shell command on the user's computer, inside a sandbox, and get its exit code, \
     stdout, and stderr. The shell is the platform's own (the system prompt names the platform), \
     and the user's installed tools and toolchains are available. The project's mode (read, \
     write, or network), its folders, and the folder commands start in are given in the latest \
     <environment> note in the conversation. A command runs with the mode's access unless its \
     `access` argument asks for other access, for this command only. Commands with read access \
     run at once; every other command waits for the user to allow it, except in a scheduled \
     run, where the first such call is refused with an explanation. Ask for `read` when a \
     command only reads, and for more than the mode only when the command needs it, such as \
     `network` to download dependencies; if the user denies it, carry on without it. ",
    // Fixed per platform, so the definition stays the same within a session.
    // See `settings` for why Windows differs.
    reads!(),
    " Without network access, nothing can be downloaded or installed. Even with it, commands \
     cannot accept connections from outside or reach services listening on the user's own \
     computer. Each command \
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
        // A call that reaches the tool without being granted anything, as
        // from an agent without a hook, runs with what it asked for, or the
        // project's mode.
        let access = context
            .get::<Granted>()
            .ok()
            .flatten()
            .map(|granted| granted.0)
            .or(args.access);
        self.sandboxes
            .run_with(&self.thread, &args.command, access)
            .await
    }
}

#[cfg(test)]
mod tests {
    use rig::tool::ToolSet;
    use serde_json::json;

    use super::*;
    use crate::{Project, ProjectFolder, TestRoot, test_sandboxes};

    /// A call runs with the access it was granted, whatever the project's
    /// mode when it starts: read-only in a writable project, and writing in a
    /// read-only one.
    #[tokio::test(flavor = "multi_thread")]
    async fn calls_run_with_the_access_they_were_granted() {
        if !mxc_sdk::v1::platform_support().is_supported {
            return;
        }
        let (sandboxes, _scratch) = test_sandboxes();
        // Not under the scratch root, where it would be taken for a leftover.
        let project = TestRoot::new();
        let folder = project.0.clone();
        let set_mode = |access| {
            sandboxes.set_project(
                "thread",
                Project {
                    folders: vec![ProjectFolder {
                        name: "project".into(),
                        path: folder.clone(),
                    }],
                    access,
                },
            )
        };
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

        set_mode(Access::Write);
        tools
            .execute(
                RunCommand::NAME,
                write("held.txt"),
                &mut RunCommand::context(Access::Read),
            )
            .await;
        assert!(!folder.join("held.txt").exists(), "held read-only");
        tools
            .execute(RunCommand::NAME, write("mode.txt"), &mut ToolContext::new())
            .await;
        assert!(folder.join("mode.txt").exists(), "the mode's access");

        set_mode(Access::Read);
        tools
            .execute(
                RunCommand::NAME,
                write("granted.txt"),
                &mut RunCommand::context(Access::Write),
            )
            .await;
        assert!(folder.join("granted.txt").exists(), "granted write access");
        sandboxes.shutdown();
    }

    #[test]
    fn calls_name_the_access_they_ask_for() {
        let ask = |arguments| RunCommand::access(&arguments, Access::Write);
        assert_eq!(ask(json!({"command": "ls"})), Access::Write);
        assert_eq!(
            ask(json!({"command": "ls", "access": "read"})),
            Access::Read
        );
        assert_eq!(
            ask(json!({"command": "ls", "access": "network"})),
            Access::Network
        );
        assert_eq!(ask(json!({"command": "ls", "access": null})), Access::Write);
        assert_eq!(
            RunCommand::requested_access(&json!({"command": "ls", "access": "root"})),
            None,
            "an access that doesn't exist is no request"
        );
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
            "`access` argument",
            "Without network access",
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
        assert!(
            parameters["properties"]["access"].is_object(),
            "{parameters}"
        );
        let args: RunCommandArgs =
            serde_json::from_value(json!({"command": "ls", "access": "network"})).unwrap();
        assert_eq!(args.access, Some(Access::Network));
        assert_eq!(parameters["additionalProperties"], false);
        assert!(
            serde_json::from_value::<RunCommandArgs>(json!({"cmd": "ls"})).is_err(),
            "a misnamed argument is refused"
        );
    }
}
