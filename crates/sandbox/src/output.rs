//! Collecting a command's output up to a limit, and what the model is told.

use serde::Serialize;

/// One output stream, kept up to a limit.
#[derive(Debug)]
pub(crate) struct CappedStream {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl CappedStream {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }

    /// Keeps what fits of `chunk`. Returns whether anything was dropped, now
    /// or before.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> bool {
        let room = self.limit - self.bytes.len();
        if chunk.len() > room {
            self.bytes.extend_from_slice(&chunk[..room]);
            self.truncated = true;
        } else {
            self.bytes.extend_from_slice(chunk);
        }
        self.truncated
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    /// The kept bytes as text. A character split by the limit, or bytes that
    /// are not UTF-8, become replacement characters.
    pub(crate) fn into_text(self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// Why a command did not get to finish on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Killed {
    /// It ran past the time limit.
    TimedOut,
    /// It wrote more than the output limit.
    OutputLimit,
}

/// What a command did, as the model receives it.
#[derive(Debug, Serialize)]
pub struct CommandOutput {
    /// The exit code, absent when the command never reported one.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Whether output past the limit was dropped.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed: Option<Killed>,
}

impl CommandOutput {
    pub(crate) fn new(
        exit_code: Option<i32>,
        stdout: CappedStream,
        stderr: CappedStream,
        killed: Option<Killed>,
    ) -> Self {
        Self {
            exit_code,
            truncated: stdout.truncated() || stderr.truncated(),
            stdout: stdout.into_text(),
            stderr: stderr.into_text(),
            killed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_output_up_to_the_limit() {
        let mut stream = CappedStream::new(8);
        assert!(!stream.push(b"abc"));
        assert!(!stream.push(b"defgh"));
        assert!(stream.push(b"i"), "the ninth byte is past the limit");
        assert!(stream.push(b""), "a stream stays truncated");
        assert_eq!(stream.into_text(), "abcdefgh");
    }

    #[test]
    fn a_character_split_by_the_limit_is_replaced() {
        let mut stream = CappedStream::new(4);
        stream.push("abc€".as_bytes());
        assert_eq!(stream.into_text(), "abc\u{FFFD}");
    }

    #[test]
    fn the_model_sees_flags_only_when_they_apply() {
        let finished =
            CommandOutput::new(Some(0), CappedStream::new(4), CappedStream::new(4), None);
        assert_eq!(
            serde_json::to_value(&finished).unwrap(),
            serde_json::json!({"exit_code": 0, "stdout": "", "stderr": ""})
        );

        let mut flood = CappedStream::new(1);
        flood.push(b"yy");
        let killed = CommandOutput::new(
            Some(137),
            flood,
            CappedStream::new(4),
            Some(Killed::OutputLimit),
        );
        assert_eq!(
            serde_json::to_value(&killed).unwrap(),
            serde_json::json!({
                "exit_code": 137,
                "stdout": "y",
                "stderr": "",
                "truncated": true,
                "killed": "output_limit"
            })
        );
    }
}
