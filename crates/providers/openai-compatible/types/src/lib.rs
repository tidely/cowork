//! Wire data for the chat-completions subset shared by Ollama and vLLM.
//!
//! These are tolerant protocol containers, not a validated conversation model.
//! String discriminators and flattened extension fields retain server additions.
//! Generic request conversion validates representability. HTTP, SSE framing,
//! and stream assembly belong to clients.

mod conversion;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Unrecognized wire fields. Callers constructing requests must not insert keys
/// already represented by explicit fields: Serde flattening permits duplicates.
pub type Extensions = Map<String, Value>;

/// A successful endpoint payload or an in-band provider error. Error comes first
/// so an error is never accepted as success when both sets of fields coexist.
/// Keep the error payload open: servers also use strings or malformed values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ApiResponse<T> {
    Error { error: Value },
    Success(T),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Kept open because structured-output dialects vary across servers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

impl ChatCompletionRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stop {
    One(String),
    Many(Vec<String>),
}

/// Shared request/response message shape; roles remain open for server variants.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Some server versions use `reasoning`, others `reasoning_content`.
    /// Both can coexist, so these must not be Serde aliases of one field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// Text and image parts have explicit fields; unknown part kinds and their
/// payloads survive decoding. The client decides which combinations it supports.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

/// Ollama documents the bare string form as well as object-shaped image URLs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageUrl {
    Url(String),
    Object(ImageUrlObject),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageUrlObject {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDefinition,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String),
    Named(NamedToolChoice),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NamedToolChoice {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: NamedFunction,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NamedFunction {
    pub name: String,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Optional on decode for implementations that omit IDs. Clients must
    /// provide an ID when sending standard assistant/tool history exchanges.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub function: FunctionCall,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Raw model text: even complete calls can contain malformed JSON.
    pub arguments: String,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub choices: Vec<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    /// Open string: servers add reasons beyond stop/length/tool_calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

/// JSON carried by an SSE `data:` event, excluding framing and `[DONE]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// An empty list is valid for a final usage-only chunk.
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    /// Usually an object (possibly empty); tolerate null/omitted final deltas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<MessageDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// Scoped to the choice, not globally across a stream or conversation.
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallDelta>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<Value>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    pub data: Vec<Model>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorObject,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub message: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<Value>,
    /// vLLM uses numeric codes; other implementations use strings or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(default, flatten)]
    pub extra: Extensions,
}

#[cfg(test)]
mod tests;
