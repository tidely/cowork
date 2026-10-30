//! The tool the model runs shell commands with.
//!
//! Its definition never changes: it is part of the prompt prefix, so a
//! change would invalidate the model's prompt cache. What changes, the
//! project's folders and mode, reaches the model as an environment note in
//! the conversation instead; see [`Sandboxes::environment_note`].

use std::{num::NonZeroU64, time::Duration};

use agent::tool::{Tool, ToolCall, ToolError, ToolErrorKind};
use serde::Deserialize;

use crate::{Access, CommandOutput, Folders, Needs, SandboxError, ThreadSandbox};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunCommandArgs {
    /// A command line for the platform's shell, for example `ls -la`.
    pub command: String,
    /// Maximum command runtime in milliseconds, as a positive integer.
    /// Starts when the command launches, after approval. Omit for no time
    /// limit. On expiry the command and its children are stopped, and the
    /// timeout error includes stdout and stderr captured so far.
    #[serde(default)]
    pub timeout_ms: Option<NonZeroU64>,
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
pub struct RunCommand;

impl RunCommand {
    pub const NAME: &'static str = "run_command";

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
}

impl From<SandboxError> for ToolError {
    fn from(error: SandboxError) -> Self {
        let kind = match error {
            SandboxError::InvalidCommand(_) => ToolErrorKind::InvalidArguments,
            SandboxError::Unsupported(_) => ToolErrorKind::NotFound,
            SandboxError::Timeout { .. } => ToolErrorKind::Timeout,
            SandboxError::Setup(_) | SandboxError::Failed(_) => ToolErrorKind::Execution,
        };
        ToolError::new(kind, error.to_string())
    }
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
     running in the background is ended when it exits. Use `timeout_ms` to limit a command's \
     runtime in milliseconds; omit it for no time limit. A timeout stops the command and its \
     children and returns a timeout error with the output captured so far."
);

impl Tool for RunCommand {
    type Args = RunCommandArgs;
    type Output = CommandOutput;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_owned()
    }

    /// The access the command asks for of its own, and the folders outside
    /// the project it names, resolved on this computer.
    fn access(&self, args: &RunCommandArgs, _: &ThreadSandbox) -> Needs {
        match Folders::resolve(&args.read_folders, &args.write_folders) {
            Ok(folders) => Needs::Command {
                access: args.access,
                folders,
            },
            Err(reason) => Needs::Refused(reason),
        }
    }

    /// Runs with what the call was allowed, and the project's mode or the
    /// run's grant as they are when it starts; see
    /// [`ThreadSandbox::command_grant`].
    async fn run(
        &self,
        call: ToolCall<Needs>,
        sandbox: &Self::Context,
        args: RunCommandArgs,
    ) -> Result<CommandOutput, ToolError> {
        let Needs::Command { access, folders } = call.access else {
            return Err(ToolError::new(
                ToolErrorKind::PermissionDenied,
                "the command was not allowed",
            ));
        };
        let grant = sandbox.command_grant(access, &folders);
        Ok(sandbox
            .sandboxes()
            .run_with(
                sandbox.thread(),
                &args.command,
                Some(grant),
                args.timeout_ms.map(|ms| Duration::from_millis(ms.get())),
            )
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent::tool::ToolErrorKind;
    use serde_json::json;

    use super::*;
    use crate::{Grant, Project, ProjectFolder, TestRoot, call, prepare, test_sandboxes};

    /// Invented arguments reach the model as actionable errors, before any
    /// command, file operation, or permission request can run.
    #[tokio::test]
    async fn unknown_arguments_return_model_feedback() {
        let (sandboxes, _scratch) = test_sandboxes();
        let sandbox = Arc::new(ThreadSandbox::new(sandboxes.clone(), "thread"));
        for (name, mut arguments) in [
            (RunCommand::NAME, json!({ "command": "echo hello" })),
            (crate::ListDirectory::NAME, json!({ "path": "." })),
            (
                crate::ReadFile::NAME,
                json!({ "path": "scratch://test.txt" }),
            ),
            (
                crate::ViewImage::NAME,
                json!({ "path": "scratch://test.png" }),
            ),
            (
                crate::WriteFile::NAME,
                json!({ "path": "scratch://test.txt", "content": "hello" }),
            ),
            (crate::RequestPermissions::NAME, json!({ "reason": "Test" })),
        ] {
            arguments["timeout"] = json!("60000");
            let prepared = match name {
                RunCommand::NAME => prepare(RunCommand, &sandbox, arguments),
                crate::ListDirectory::NAME => prepare(crate::ListDirectory, &sandbox, arguments),
                crate::ReadFile::NAME => prepare(crate::ReadFile, &sandbox, arguments),
                crate::ViewImage::NAME => prepare(crate::ViewImage, &sandbox, arguments),
                crate::WriteFile::NAME => prepare(crate::WriteFile, &sandbox, arguments),
                _ => prepare(crate::RequestPermissions, &sandbox, arguments),
            };
            let Err(error) = prepared else {
                panic!("{name} accepted an unknown argument");
            };
            assert_eq!(error.kind, ToolErrorKind::InvalidArguments, "{name}");
            assert!(error.message.contains("unknown field `timeout`"), "{error}");
            assert!(error.message.contains("expected"), "{error}");
        }
    }

    #[tokio::test]
    async fn invalid_timeouts_return_model_feedback_before_running() {
        let (sandboxes, _scratch) = test_sandboxes();
        let sandbox = Arc::new(ThreadSandbox::new(sandboxes.clone(), "thread"));
        for timeout in [json!(0), json!(-1), json!(1.5), json!("60000")] {
            let Err(error) = prepare(
                RunCommand,
                &sandbox,
                json!({ "command": "echo hello", "timeout_ms": timeout }),
            ) else {
                panic!("{timeout} was accepted");
            };
            assert_eq!(error.kind, ToolErrorKind::InvalidArguments, "{timeout}");
            assert!(
                error.message.contains("invalid value") || error.message.contains("invalid type")
            );
        }
    }

    /// A call needs what it asks for of its own and the folders it names;
    /// one naming a folder that can't be granted is refused before anyone
    /// is asked.
    #[test]
    fn calls_need_the_access_and_folders_they_ask_for() {
        let (sandboxes, _scratch) = test_sandboxes();
        let sandbox = Arc::new(ThreadSandbox::new(sandboxes, "thread"));
        let cache = TestRoot::new();
        let needs = |arguments| {
            prepare(RunCommand, &sandbox, arguments)
                .unwrap()
                .access()
                .clone()
        };
        assert_eq!(
            needs(json!({"command": "ls"})),
            Needs::Command {
                access: None,
                folders: Folders::default()
            }
        );
        assert_eq!(
            needs(json!({
                "command": "cargo fetch",
                "access": "network",
                "write_folders": [cache.0.to_string_lossy()],
            })),
            Needs::Command {
                access: Some(Access::Network),
                folders: Folders {
                    read: Vec::new(),
                    write: vec![crate::resolved(&cache.0).unwrap()],
                },
            }
        );
        let missing = cache.0.join("no-such-folder");
        assert!(matches!(
            needs(json!({"command": "ls", "read_folders": [missing]})),
            Needs::Refused(reason) if reason.contains("does not exist")
        ));
    }

    /// A call runs with the access it asked for, whatever the project's
    /// mode: read-only in a writable project, and writing in a read-only
    /// one once allowed; without asking, it runs with the mode.
    #[tokio::test(flavor = "multi_thread")]
    async fn calls_run_with_the_access_they_ask_for() {
        if !mxc_sdk::v1::platform_support().is_supported {
            return;
        }
        let (sandboxes, _scratch) = test_sandboxes();
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
        let sandbox = Arc::new(ThreadSandbox::new(sandboxes.clone(), "thread"));
        let write = |file: &str, access: Option<&str>| {
            let command = if cfg!(windows) {
                format!("cmd /c echo agent> {file}")
            } else {
                format!("echo agent > {file}")
            };
            let mut arguments = json!({ "command": command });
            if let Some(access) = access {
                arguments["access"] = access.into();
            }
            arguments
        };

        set_mode(Access::Write);
        _ = call(RunCommand, &sandbox, write("held.txt", Some("read"))).await;
        assert!(!folder.join("held.txt").exists(), "held read-only");
        _ = call(RunCommand, &sandbox, write("mode.txt", None)).await;
        assert!(folder.join("mode.txt").exists(), "the mode's access");

        set_mode(Access::Read);
        _ = call(RunCommand, &sandbox, write("granted.txt", Some("write"))).await;
        assert!(folder.join("granted.txt").exists(), "granted write access");
    }

    /// A folder outside the project is writable only for a command naming
    /// it to write, or once the run was granted it.
    #[tokio::test(flavor = "multi_thread")]
    async fn granted_folders_outside_the_project_are_writable() {
        if !mxc_sdk::v1::platform_support().is_supported {
            return;
        }
        let (sandboxes, _scratch) = test_sandboxes();
        let sandbox = Arc::new(ThreadSandbox::new(sandboxes.clone(), "thread"));
        let outside = TestRoot::new();
        let cache = crate::resolved(&outside.0).unwrap();
        let file = cache.join("cached.txt");
        let command = if cfg!(windows) {
            format!("cmd /c echo agent> \"{}\"", file.display())
        } else {
            format!("echo agent > '{}'", file.display())
        };
        let cache_text = cache.to_string_lossy().into_owned();

        _ = call(RunCommand, &sandbox, json!({ "command": command })).await;
        assert!(!file.exists(), "not granted");

        _ = call(
            RunCommand,
            &sandbox,
            json!({ "command": command, "read_folders": [cache_text] }),
        )
        .await;
        assert!(!file.exists(), "granted only reading");

        _ = call(
            RunCommand,
            &sandbox,
            json!({ "command": command, "write_folders": [cache_text] }),
        )
        .await;
        assert!(file.exists(), "granted writing");
        std::fs::remove_file(&file).unwrap();

        sandboxes.grant_for_run(
            "thread",
            &Grant {
                folders: Folders {
                    read: Vec::new(),
                    write: vec![cache.clone()],
                },
                ..Grant::default()
            },
        );
        _ = call(RunCommand, &sandbox, json!({ "command": command })).await;
        assert!(file.exists(), "granted for the run");
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
    }
}
