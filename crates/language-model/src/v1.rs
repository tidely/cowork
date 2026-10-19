//! Frozen first durable message schema. Change its format only by adding a new
//! version module and explicit migrations. Unknown fields are rejected at every
//! object boundary so an older executable cannot silently erase newer data.

use crate::{JsonObject, MessageId, ToolCallId, VersionedType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: Vec<Content>,
    pub generation: Option<GenerationMetadata>,
    pub replay: Vec<ReplayData>,
}

impl VersionedType for Message {
    const TYPE_ID: &'static str = "cowork.llm.message.v1";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Content {
    Text { text: String },
    Reasoning { reasoning: Reasoning },
    Image { image: Image },
    ToolCall { call: ToolCall },
    ToolResult { result: ToolResult },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reasoning {
    Text { text: String },
    Redacted { data: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub media_type: String,
    pub source: ImageSource,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageSource {
    Base64 { data: String },
    Url { url: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub provider_id: Option<String>,
    pub name: String,
    pub arguments: ToolArguments,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolArguments {
    Object { value: JsonObject },
    Invalid { raw: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub call: ToolCallId,
    pub provider_id: Option<String>,
    pub name: Option<String>,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolResultContent {
    Text { text: String },
    Json { value: Value },
    Image { image: Image },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationMetadata {
    pub provider: String,
    pub model: Option<String>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub extra: JsonObject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayData {
    pub provider: String,
    pub model: Option<String>,
    pub format: String,
    pub version: u32,
    pub payload: Value,
}
