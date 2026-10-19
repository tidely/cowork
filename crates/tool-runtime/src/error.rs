use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Execution and dispatch failures, separate from a tool's successful domain
/// output (for example, a command that ran and returned a nonzero exit code).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    NotImplemented,
    InvalidArguments,
    NotFound,
    PermissionDenied,
    Cancelled,
    Timeout,
    Execution,
    InvalidOutput,
    InvalidStream,
}

/// An actionable explanation for the caller and model, with optional structured
/// detail. Tools and hosts must keep credentials out of both fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolError {
    pub fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}
