//! Anthropic Messages API (`/v1/messages`) wire types.

mod conversion;
pub use conversion::{REPLAY_FORMAT, project_content};

use serde::{Deserialize, Serialize};

/// POST /v1/messages request body
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolParam>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum OutputFormat {
    JsonSchema { schema: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemParam {
    Text(String),
    Blocks(Vec<TextBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    #[serde(rename = "type")]
    pub r#type: String, // always "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub r#type: String, // "ephemeral"
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            r#type: "ephemeral".to_owned(),
        }
    }
}

/// Content blocks used in both requests and responses
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Encrypted reasoning the model chose to redact: an opaque `data` blob, never plaintext.
    /// Retained in canonical reasoning and native replay without interpreting it.
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Tool definition (Anthropic Messages API format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

/// Tool choice (Anthropic Messages API format)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ToolChoiceParam {
    Auto,
    None,
    Any,
    Tool { name: String },
}

/// Three modes per the Anthropic Messages API: Adaptive: 4.6+ models, API decides budget; Enabled: 4.0-4.5 models,
/// explicit budget_tokens; Disabled: pre-thinking models or thinking_budget=0.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Omitted,
    Summarized,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ThinkingConfig {
    Enabled {
        budget_tokens: u32,
    },
    Adaptive {
        // Omission preserves the model's default thinking-display policy.
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Non-streaming response from POST /v1/messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String, // "message"
    pub role: String, // "assistant"
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: Option<StopReason>,
    pub usage: MessagesUsage,
    #[serde(default, flatten)]
    pub extra: language_model::JsonObject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Refusal,
    PauseTurn,
    ModelContextWindowExceeded,
    /// Catch-all so a new server-side stop reason never fails the final `message_delta` parse and discards an already-streamed response.
    /// Preserves the wire string for logging and faithful re-serialization.
    /// Must stay the LAST variant: serde tries the tagged variants above first.
    #[serde(untagged)]
    Unknown(String),
}

impl StopReason {
    /// The verbatim wire string, derived from the serde `snake_case` renames so it cannot drift from the wire contract.
    /// `Unknown` yields its inner string unchanged.
    pub fn wire_str(&self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(s)) => s,
            other => {
                debug_assert!(
                    false,
                    "StopReason must serialize to a string, got {other:?}"
                );
                "end_turn".to_string()
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagesUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, flatten)]
    pub extra: language_model::JsonObject,
}

/// Top-level streaming event (SSE `type` field determines variant)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageStreamEvent {
    MessageStart {
        message: MessagesResponse,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        usage: MessageDeltaUsage,
    },
    MessageStop,
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: StreamDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    Ping,
    Error {
        error: StreamError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    pub stop_reason: Option<StopReason>,
    /// The stop sequence that was matched, present only when `stop_reason == "stop_sequence"`; `None` otherwise.
    /// Consumers echo it on the Messages API `message.stop_sequence`.
    /// Optional so its absence never fails the final event parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Provider detail for the stop; on `refusal`, `explanation` carries the
    /// reason the request was blocked (e.g. an Anthropic ToS auto-refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    #[serde(default, flatten)]
    pub extra: language_model::JsonObject,
}

/// Detail for a final `message_delta`, e.g. `{"type":"refusal","category":"frontier_llm","explanation":"..."}`.
/// All fields optional so an unknown shape never fails the final event parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StopDetails {
    #[serde(rename = "type", default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageDeltaUsage {
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, flatten)]
    pub extra: language_model::JsonObject,
}

/// Content delta within a content_block_delta event
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum StreamDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamError {
    #[serde(rename = "type")]
    pub r#type: String,
    pub message: String,
}

/// One page of GET /v1/models. Unknown metadata stays with the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelInfo>,
    pub has_more: bool,
    pub last_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub max_input_tokens: Option<u64>,
    #[serde(default, flatten)]
    pub extra: language_model::JsonObject,
}

#[cfg(test)]
mod tests;
