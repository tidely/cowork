use std::{fmt, ops::AddAssign};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type ToolCallId = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub options: ChatOptions,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            options: ChatOptions::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatOptions {
    pub think: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub extra: Map<String, Value>,
}

impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            think: true,
            temperature: None,
            max_tokens: None,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: ToolCallId,
        name: String,
        content: String,
        is_error: bool,
    },
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant_with_tools(
        content: impl Into<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self::Assistant {
            content: content.into(),
            reasoning,
            tool_calls,
        }
    }

    pub fn tool(
        call_id: impl Into<ToolCallId>,
        name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::Tool {
            call_id: call_id.into(),
            name: name.into(),
            content: content.into(),
            is_error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub supports_argument_streaming: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub raw_arguments: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArgumentParseError {
    pub id: ToolCallId,
    pub name: String,
    pub raw_arguments: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    pub fn from_input_output(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        }
    }
}

impl AddAssign<TokenUsage> for TokenUsage {
    fn add_assign(&mut self, rhs: TokenUsage) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.total_tokens += rhs.total_tokens;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Cancelled,
    Error,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    Queued { position: usize },
    Started,
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStarted { id: ToolCallId, name: String },
    ToolCallArgumentDelta { id: ToolCallId, delta: String },
    ToolCallFinished(ToolCall),
    ToolCallArgumentParseError(ToolArgumentParseError),
    Usage(TokenUsage),
    Finished { reason: FinishReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    Transport(String),
    HttpStatus { status: u16, body: String },
    Decode(String),
    Provider(String),
    Tool(String),
    MaxTurns { max_turns: usize },
    Cancelled,
}

impl LlmError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::HttpStatus { status, .. } => matches!(*status, 408 | 409 | 425 | 429 | 500..=599),
            Self::Provider(message) => {
                let message = message.to_ascii_lowercase();
                message.contains("connection")
                    || message.contains("timeout")
                    || message.contains("timed out")
                    || message.contains("network")
                    || message.contains("broken pipe")
                    || message.contains("connection reset")
                    || message.contains("connection refused")
                    || message.contains("temporarily unavailable")
                    || message.contains("dns")
            }
            Self::Decode(_) | Self::Tool(_) | Self::MaxTurns { .. } | Self::Cancelled => false,
        }
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "transport error: {message}"),
            Self::HttpStatus { status, body } => {
                write!(f, "provider returned HTTP {status}: {body}")
            }
            Self::Decode(message) => write!(f, "failed to decode provider response: {message}"),
            Self::Provider(message) => write!(f, "provider error: {message}"),
            Self::Tool(message) => write!(f, "tool error: {message}"),
            Self::MaxTurns { max_turns } => {
                write!(f, "agent reached the maximum turn limit ({max_turns})")
            }
            Self::Cancelled => write!(f, "request cancelled"),
        }
    }
}

impl std::error::Error for LlmError {}
