//! What the model is told about a command.

use serde::Serialize;

/// What a command did, as the model receives it.
#[derive(Debug, Clone, Serialize)]
pub struct CommandOutput {
    /// The exit code. A command ended by a signal reports `-1`.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    /// Output that is not UTF-8 becomes replacement characters.
    pub(crate) fn new(exit_code: i32, stdout: &[u8], stderr: &[u8]) -> Self {
        Self {
            exit_code,
            stdout: String::from_utf8_lossy(stdout).into_owned(),
            stderr: String::from_utf8_lossy(stderr).into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_sees_the_exit_code_and_both_streams() {
        let output = CommandOutput::new(2, b"out", b"bad \xff");
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            serde_json::json!({"exit_code": 2, "stdout": "out", "stderr": "bad \u{FFFD}"})
        );
    }
}
