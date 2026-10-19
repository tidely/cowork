//! Current in-memory data. These types are intentionally not serialized: storage
//! goes through a frozen, versioned DTO, even when its shape currently matches.

use crate::{Error, JsonObject, MessageId, ToolCallId};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: Vec<Content>,
    pub generation: Option<GenerationMetadata>,
    pub replay: Vec<ReplayData>,
}

impl Message {
    pub fn new(role: Role, content: Vec<Content>) -> Self {
        Self {
            id: MessageId::new(),
            role,
            content,
            generation: None,
            replay: Vec::new(),
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::new(Role::System, vec![Content::text(text)])
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![Content::text(text)])
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, vec![Content::text(text)])
    }

    /// Checks role/content combinations without guessing provider capabilities.
    /// Malformed arguments are valid *observations*, never executable objects.
    pub fn validate(&self) -> Result<(), Error> {
        if self.id.0.is_empty() {
            return Err(Error::InvalidRequest("message ID must not be empty".into()));
        }
        for content in &self.content {
            let allowed = match content {
                Content::Text { .. } => self.role != Role::Tool,
                Content::Image(_) => matches!(self.role, Role::User | Role::Assistant),
                Content::Reasoning(_) | Content::ToolCall(_) => self.role == Role::Assistant,
                Content::ToolResult(_) => self.role == Role::Tool,
            };
            if !allowed {
                return Err(Error::InvalidRequest(format!(
                    "content is incompatible with {:?} role",
                    self.role
                )));
            }
            match content {
                Content::ToolCall(call) if call.id.0.is_empty() || call.name.is_empty() => {
                    return Err(Error::InvalidRequest(
                        "tool call needs an ID and name".into(),
                    ));
                }
                Content::ToolResult(result) if result.call.0.is_empty() => {
                    return Err(Error::InvalidRequest("tool result needs a call ID".into()));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text { text: String },
    Reasoning(Reasoning),
    Image(Image),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reasoning {
    Text {
        text: String,
    },
    /// Opaque provider redaction, never coerced into visible text.
    Redacted {
        data: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub media_type: String,
    pub source: ImageSource,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImageSource {
    Base64 {
        data: String,
    },
    /// Retaining a URL does not make the remote bytes durable. Applications
    /// needing offline restoration must materialize them before persistence.
    Url {
        url: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub provider_id: Option<String>,
    pub name: String,
    pub arguments: ToolArguments,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolArguments {
    Object { value: JsonObject },
    Invalid { raw: String },
}

impl ToolArguments {
    /// Only JSON objects are executable arguments. Valid arrays/scalars/null
    /// and broken JSON keep their original text; none is repaired or coerced.
    pub fn from_raw(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(value)) => Self::Object { value },
            _ => Self::Invalid { raw },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub call: ToolCallId,
    pub provider_id: Option<String>,
    pub name: Option<String>,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolResultContent {
    Text { text: String },
    Json { value: Value },
    Image(Image),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenerationMetadata {
    pub provider: String,
    pub model: Option<String>,
    /// Raw provider reason, retaining unknown future strings.
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub extra: JsonObject,
}

/// Optional native replay data, retained by storage even if an adapter cannot
/// use it. An adapter must reject unknown formats rather than silently omit
/// them. Editing content invalidates its replay data: callers must clear it or
/// regenerate it before submitting the edited message. No plugin registry is
/// implied by this namespaced, versioned escape hatch.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayData {
    pub provider: String,
    pub model: Option<String>,
    pub format: String,
    pub version: u32,
    pub payload: Value,
}
