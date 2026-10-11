//! The tool the model runs shell commands with.
//!
//! Its definition never changes: it is part of the prompt prefix, so a
//! change would invalidate the model's prompt cache. What changes, the
//! project's folders and mode, reaches the model as an environment note in
//! the conversation instead; see [`Sandboxes::environment_note`].

use std::sync::Arc;

use rig::tool::{ContextValue, Tool, ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};

use crate::{Access, CommandOutput, Folders, Grant, SandboxError, Sandboxes};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RunCommandArgs {
    /// A command line for the platform's shell, for example `ls -la`.
    pub command: String,
    /// The access this one command needs, when it differs from the project's
    /// mode: `read` to read host files, `write` to also change the project,
    /// `network` to also reach the network. Leave it out to use the mode.
    /// Asking for `read` lets a command that only reads run at once in any
    /// mode; asking for more than the mode waits for the user to allow it.
    #[serde(default)]
    pub access: Option<Access>,
    /// Folders outside the project this one command also needs to read, as
    /// absolute paths on the user's computer, or starting with `~/` for the
    /// user's home folder. Optional: host files are already readable except credentials.
    #[serde(default)]
    pub read_folders: Vec<String>,
    /// Folders outside the project this one command also needs to change,
    /// such as `~/.cargo` for `cargo fetch`, written like `read_folders`.
    /// Waits unless already covered by the project or run.
    #[serde(default)]
    pub write_folders: Vec<String>,
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

    /// The folders outside the project a call with `arguments` names, as the
    /// model wrote them: to read, and to write.
    pub fn named_folders(arguments: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        crate::grant::named_folders(arguments)
    }

    /// The folders outside the project a call with `arguments` asks for,
    /// resolved on this computer, or why they can't be granted.
    pub fn requested_folders(arguments: &serde_json::Value) -> Result<Folders, String> {
        let (read, write) = Self::named_folders(arguments);
        Folders::resolve(&read, &write)
    }

    /// The context for a call allowed with `grant`: the command runs with
    /// exactly that, whatever the project's mode by the time it starts.
    pub fn context(grant: Grant) -> ToolContext {
        let mut context = ToolContext::new();
        context
            .insert(Granted(grant))
            .expect("a grant always encodes");
        context
    }
}

/// What a call was allowed with; see [`RunCommand::context`].
#[derive(Serialize, Deserialize)]
struct Granted(Grant);

impl ContextValue for Granted {
    const KEY: &'static str = "sandbox.run_command.access";
}

const DESCRIPTION: &str = concat!(
    "Run a shell command on the user's computer, inside a sandbox, and get its exit code, \
     stdout, and stderr. The shell is the platform's own (the system prompt names the platform), \
     and the user's installed tools and toolchains are available. The project's mode (read, \
     write, or network), its folders, and the folder commands start in are given in the latest \
     <environment> note in the conversation. A command runs with the mode's access unless its \
     `access` argument asks for other access, for this command only. Commands covered by the \
     project's auto-approval level and this run's grants run at once; extra access or writable folders \
     wait for the user to allow them, except in a scheduled \
     run, where the first such call is refused with an explanation. Ask for `read` when a \
     command only reads, and for more than the mode only when the command needs it, such as \
     `network` to download dependencies. A command that needs folders outside the project, \
     such as a package manager's cache, names them in `write_folders`; \
     to keep extra access for later commands, ask for it once with `request_permissions`. \
     Permissions granted that way apply to later commands automatically. If the user denies \
     something, carry on without it. ",
    "Commands can read the host filesystem except denied credentials. Writes stay within the \
     project and scratch folders unless extra writable folders are granted. ",
    " Without network access, nothing can be downloaded or installed. Even with it, commands \
     cannot accept connections from outside or reach services listening on the user's own \
     computer. Each command \
     can write a scratch folder, its TMPDIR, kept for this conversation; commands start there \
     when the project has no folders. HOME is the user's own home folder, so `~` means the \
     same in a command as in `read_folders` and `write_folders`. Anything a command leaves \
     running in the background is ended when it exits."
);

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
        let grant = match context.get::<Granted>().ok().flatten() {
            Some(Granted(grant)) => Some(grant),
            None if args.access.is_none()
                && args.read_folders.is_empty()
                && args.write_folders.is_empty() =>
            {
                None
            }
            None => Some(Grant {
                access: args
                    .access
                    .unwrap_or_else(|| self.sandboxes.project(&self.thread).access),
                folders: Folders::resolve(&args.read_folders, &args.write_folders)
                    .map_err(SandboxError::InvalidCommand)?,
            }),
        };
        self.sandboxes
            .run_with(&self.thread, &args.command, grant)
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

        let access = |access| {
            RunCommand::context(Grant {
                access,
                ..Grant::default()
            })
        };
        set_mode(Access::Write);
        tools
            .execute(
                RunCommand::NAME,
                write("held.txt"),
                &mut access(Access::Read),
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
                &mut access(Access::Write),
            )
            .await;
        assert!(folder.join("granted.txt").exists(), "granted write access");
        sandboxes.shutdown();
    }

    /// A folder outside the project is writable only for a command granted
    /// it, and only while the rest stays as the grant says.
    #[tokio::test(flavor = "multi_thread")]
    async fn granted_folders_outside_the_project_are_writable() {
        if !mxc_sdk::v1::platform_support().is_supported {
            return;
        }
        let (sandboxes, _scratch) = test_sandboxes();
        let outside = TestRoot::new();
        let cache = crate::resolved(&outside.0).unwrap();
        let mut tools = ToolSet::default();
        tools.add_tool(RunCommand::new(sandboxes.clone(), "thread"));
        let file = cache.join("cached.txt");
        let command = if cfg!(windows) {
            format!("cmd /c echo agent> \"{}\"", file.display())
        } else {
            format!("echo agent > '{}'", file.display())
        };
        let arguments = json!({ "command": command }).to_string();

        tools
            .execute(RunCommand::NAME, arguments.clone(), &mut ToolContext::new())
            .await;
        assert!(!file.exists(), "not granted");

        let read_only = Grant {
            folders: Folders {
                read: vec![cache.clone()],
                write: Vec::new(),
            },
            ..Grant::default()
        };
        tools
            .execute(
                RunCommand::NAME,
                arguments.clone(),
                &mut RunCommand::context(read_only),
            )
            .await;
        assert!(!file.exists(), "granted only reading");

        let writable = Grant {
            folders: Folders {
                read: Vec::new(),
                write: vec![cache.clone()],
            },
            ..Grant::default()
        };
        tools
            .execute(
                RunCommand::NAME,
                arguments,
                &mut RunCommand::context(writable),
            )
            .await;
        assert!(file.exists(), "granted writing");
        sandboxes.shutdown();
    }

    #[test]
    fn calls_name_the_access_and_folders_they_ask_for() {
        let ask = |arguments| RunCommand::requested_access(&arguments);
        assert_eq!(ask(json!({"command": "ls"})), None);
        assert_eq!(
            ask(json!({"command": "ls", "access": "read"})),
            Some(Access::Read)
        );
        assert_eq!(
            ask(json!({"command": "ls", "access": "network"})),
            Some(Access::Network)
        );
        assert_eq!(ask(json!({"command": "ls", "access": null})), None);
        assert_eq!(
            ask(json!({"command": "ls", "access": "root"})),
            None,
            "an access that doesn't exist is no request"
        );

        assert_eq!(
            RunCommand::named_folders(&json!({
                "command": "cargo fetch",
                "read_folders": ["/srv/docs"],
                "write_folders": ["~/.cargo"],
            })),
            (vec!["/srv/docs".to_owned()], vec!["~/.cargo".to_owned()])
        );
        assert_eq!(
            RunCommand::named_folders(&json!({"command": "ls", "write_folders": "~"})),
            (Vec::new(), Vec::new()),
            "malformed folders name none"
        );
        assert!(
            RunCommand::requested_folders(&json!({"command": "ls", "write_folders": ["src"]}))
                .is_err()
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
            "its TMPDIR",
            "HOME is the user's own home folder",
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
        for folders in ["read_folders", "write_folders"] {
            assert_eq!(
                parameters["properties"][folders]["type"], "array",
                "{parameters}"
            );
        }
        assert_eq!(parameters["additionalProperties"], false);
        assert!(
            serde_json::from_value::<RunCommandArgs>(json!({"cmd": "ls"})).is_err(),
            "a misnamed argument is refused"
        );
    }
}
