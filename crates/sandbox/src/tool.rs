//! The tool the model runs shell commands with.

use std::sync::Arc;

use rig::tool::{Tool, ToolContext, ToolExecutionError};
use serde::Deserialize;
use serde_json::json;

use crate::{CommandOutput, SandboxError, Sandboxes, settings};

#[derive(Debug, Deserialize)]
pub struct RunCommandArgs {
    pub command: String,
}

/// Runs a shell command in the sandbox of the thread the run belongs to.
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
}

impl Tool for RunCommand {
    const NAME: &'static str = "run_command";
    type Error = SandboxError;
    type Args = RunCommandArgs;
    type Output = CommandOutput;

    fn description(&self) -> String {
        format!(
            "Run a shell command with /bin/sh -c in an isolated Alpine Linux virtual machine, \
             and get its exit code, stdout, and stderr. Every call waits for a person to allow it. \
             The machine has no network access and none of the user's files; it runs as an \
             unprivileged user starting in {workdir}, and only {workdir} and similar scratch \
             locations are writable. Files persist between calls in this conversation until the \
             machine has been idle for {idle} minutes. A command is killed after {timeout} \
             seconds, or once it writes more than {limit} KiB to stdout or stderr. Nothing can \
             be installed, as there is no network.",
            workdir = settings::WORKDIR,
            idle = settings::IDLE_TIMEOUT.as_secs() / 60,
            timeout = settings::COMMAND_TIMEOUT.as_secs(),
            limit = settings::OUTPUT_LIMIT / 1024,
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command, for example `ls -la` or `echo $((6 * 7))`"
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            SandboxError::InvalidCommand(_) => ToolExecutionError::invalid_args(error.to_string()),
            SandboxError::Unsupported => ToolExecutionError::not_found(error.to_string()),
            SandboxError::Setup(_) | SandboxError::Failed(_) | SandboxError::NotStarted(_) => {
                ToolExecutionError::other(error.to_string())
            }
        }
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.sandboxes.run(&self.thread, &args.command).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn schema_and_argument_names_match() {
        let tool = RunCommand::new(
            Arc::new(Sandboxes::new(PathBuf::from("/nonexistent"), "test")),
            "thread",
        );
        let args: RunCommandArgs = serde_json::from_value(json!({"command": "ls"})).unwrap();
        assert_eq!(args.command, "ls");
        assert_eq!(tool.parameters()["required"], json!(["command"]));
        assert!(
            serde_json::from_value::<RunCommandArgs>(json!({"cmd": "ls"})).is_err(),
            "a misnamed argument is refused"
        );
    }
}
