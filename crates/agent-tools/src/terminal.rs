use std::borrow::Cow;

use async_trait::async_trait;
use llm::{Tool, ToolError, ToolOutput, parse_args, schema_for};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TerminalInput {
    /// The shell command to run. It is executed through the platform shell
    /// (`cmd /C` on Windows, `sh -c` elsewhere), so pipes, redirection, globbing,
    /// and `&&`/`||` work as in a normal terminal.
    pub command: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Terminal;

#[async_trait]
impl Tool for Terminal {
    fn name(&self) -> Cow<'static, str> {
        "terminal".into()
    }

    fn description(&self) -> Cow<'static, str> {
        "Run a shell command on the user's machine and capture its combined \
         stdout, stderr, and exit status. Cross-platform: the command runs through \
         `cmd /C` on Windows and `sh -c` on other systems. Every call requires \
         explicit user approval, so prefer a single self-contained command over \
         many small ones."
            .into()
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<TerminalInput>()
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let input: TerminalInput = parse_args(arguments)?;
        run_terminal(&input.command)
            .await
            .map(TerminalResult::into_tool_output)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

/// The captured result of a finished command: the decoded streams plus the exit
/// code (`None` when the process was terminated by a signal without one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

impl TerminalResult {
    /// Whether the command failed — a non-zero exit code, or no code at all
    /// (killed by a signal). Drives whether the tool output is flagged an error.
    pub fn is_failure(&self) -> bool {
        self.exit_code != Some(0)
    }

    /// Human/model-readable rendering of the streams and the exit status. stderr
    /// is labelled so the two streams can be told apart, and the exit status is
    /// appended only when the command did not succeed cleanly.
    pub fn display(&self) -> String {
        let mut sections = Vec::new();
        let stdout = self.stdout.trim_end();
        let stderr = self.stderr.trim_end();
        if !stdout.is_empty() {
            sections.push(stdout.to_string());
        }
        if !stderr.is_empty() {
            sections.push(format!("stderr:\n{stderr}"));
        }
        let body = sections.join("\n");

        let status = match self.exit_code {
            Some(0) => None,
            Some(code) => Some(format!("exited with code {code}")),
            None => Some("terminated by signal".to_string()),
        };

        match (body.is_empty(), status) {
            (true, None) => "(no output)".to_string(),
            (true, Some(status)) => format!("(no output, {status})"),
            (false, None) => body,
            (false, Some(status)) => format!("{body}\n({status})"),
        }
    }

    fn into_tool_output(self) -> ToolOutput {
        let content = self.display();
        if self.is_failure() {
            ToolOutput::error(content)
        } else {
            ToolOutput::text(content)
        }
    }
}

/// Run `command` through the platform shell, capturing its output. Errors only
/// when the shell itself cannot be spawned; a command that runs and exits
/// non-zero returns `Ok` with the failing status recorded in [`TerminalResult`].
pub async fn run_terminal(command: &str) -> std::io::Result<TerminalResult> {
    let output = shell_command(command).output().await?;
    Ok(TerminalResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
    })
}

/// Build the platform shell invocation. `kill_on_drop` is set so that dropping
/// the call future (e.g. when the agent run is cancelled) tears down the child
/// process instead of leaving it running detached.
fn shell_command(command: &str) -> Command {
    let mut shell = if cfg!(windows) {
        let mut shell = Command::new("cmd");
        shell.arg("/C");
        shell
    } else {
        let mut shell = Command::new("sh");
        shell.arg("-c");
        shell
    };
    shell.arg(command);
    shell.kill_on_drop(true);
    shell
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn runs_command_and_captures_stdout() {
        let result = run_terminal("echo hello").await.expect("command runs");

        assert!(
            result.stdout.contains("hello"),
            "stdout: {:?}",
            result.stdout
        );
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.is_failure());
    }

    #[tokio::test]
    async fn captures_nonzero_exit_code() {
        let result = run_terminal("exit 3").await.expect("command runs");

        assert_eq!(result.exit_code, Some(3));
        assert!(result.is_failure());
    }

    #[tokio::test]
    async fn captures_stderr() {
        let result = run_terminal("echo oops 1>&2").await.expect("command runs");

        assert!(
            result.stderr.contains("oops"),
            "stderr: {:?}",
            result.stderr
        );
    }

    #[tokio::test]
    async fn call_returns_output_for_successful_command() {
        let output = Terminal
            .call(json!({ "command": "echo hi" }))
            .await
            .expect("tool call succeeds");

        assert!(
            output.content.contains("hi"),
            "content: {:?}",
            output.content
        );
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn call_flags_failing_command_as_error() {
        let output = Terminal
            .call(json!({ "command": "exit 1" }))
            .await
            .expect("tool call succeeds");

        assert!(output.is_error);
        assert!(output.content.contains("exited with code 1"));
    }

    #[tokio::test]
    async fn call_rejects_missing_command_argument() {
        let error = Terminal
            .call(json!({}))
            .await
            .expect_err("missing command is rejected");

        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    // Formatting is exercised without spawning a shell so the rendering rules are
    // pinned independent of platform `echo` quirks.

    #[test]
    fn display_shows_plain_output_on_success() {
        let result = TerminalResult {
            stdout: "done\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
        };

        assert_eq!(result.display(), "done");
    }

    #[test]
    fn display_appends_exit_code_on_failure() {
        let result = TerminalResult {
            stdout: "partial\n".into(),
            stderr: "boom\n".into(),
            exit_code: Some(2),
        };

        assert_eq!(
            result.display(),
            "partial\nstderr:\nboom\n(exited with code 2)"
        );
    }

    #[test]
    fn display_reports_empty_output() {
        let result = TerminalResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        };

        assert_eq!(result.display(), "(no output)");
    }

    #[test]
    fn display_reports_signal_termination() {
        let result = TerminalResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        };

        assert_eq!(result.display(), "(no output, terminated by signal)");
        assert!(result.is_failure());
    }
}
