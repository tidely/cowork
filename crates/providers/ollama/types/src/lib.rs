//! Native Ollama wire data and conversions from the shared language-model types.
//!
//! `ChatResponse` is one complete JSON value, either a streaming chunk or a
//! non-streaming response. It does not imply a completed assistant turn.

mod conversion;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Provider-defined options and metadata remain open JSON objects.
pub type JsonObject = Map<String, Value>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<Think>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<KeepAlive>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub options: JsonObject,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shift: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
}

/// Named thinking levels are model-defined, not a closed low/medium/high enum.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Think {
    Enabled(bool),
    Level(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeepAlive {
    Duration(String),
    Seconds(serde_json::Number),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseFormat {
    Name(String),
    Schema(JsonObject),
}

/// Native messages also serve as streamed deltas. Empty content is meaningful
/// for thinking-only, tool-only, and final chunks. Images are base64 strings,
/// not URLs; conversion from shared messages validates their encoded bytes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub function: FunctionCall,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(default)]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "present_arguments",
        skip_serializing_if = "Option::is_none"
    )]
    pub arguments: Option<ToolArguments>,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

/// Native arguments are objects. Unexpected valid JSON is retained rather
/// than discarding the response or coercing it to a callable object. Invalid
/// JSON framing itself remains a decoding error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolArguments {
    Object(JsonObject),
    Invalid(Value),
}

// A present JSON null is malformed arguments, not an absent streamed field.
// Keeping the distinction lets adapters retain the provider's actual output.
fn present_arguments<'de, D>(deserializer: D) -> Result<Option<ToolArguments>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ToolArguments::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tool {
    Function { function: FunctionDefinition },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Full JSON Schema, including nested schemas and extension keywords.
    pub parameters: JsonObject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    pub done: bool,
    /// Open string: server versions and models may introduce new reasons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_reason: Option<String>,
    /// All duration fields are nanoseconds, as returned by the native API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_duration: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_duration: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_eval_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_eval_cached_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_eval_duration: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_duration: Option<u64>,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

/// Errors can occur inside a successful HTTP stream, so transport status alone
/// cannot distinguish a response chunk from a provider error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatStreamItem {
    Error(ErrorResponse),
    Response(Box<ChatResponse>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListModelsResponse {
    pub models: Vec<Model>,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<ModelDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

/// Missing or newly introduced metadata must not prevent listing a model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub families: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShowModelRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbose: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShowModelResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modelfile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<ModelDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_info: Option<JsonObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// Model-defined supported thinking values and defaults; intentionally raw
    /// until the adapter needs to interpret this evolving discovery metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(default, flatten)]
    pub extra: JsonObject,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn streamed_thinking_tools_and_final_usage_are_independent() {
        let ndjson = r#"{"model":"qwen3","message":{"role":"assistant","thinking":"Consider"},"done":false}
{"message":{"role":"assistant","tool_calls":[{"function":{"index":0,"name":"weather","arguments":{"city":"Helsinki"}}}]},"done":false}
{"done":true,"done_reason":"stop","prompt_eval_count":100,"prompt_eval_cached_count":80,"eval_count":12,"eval_duration":1000000,"remote_model":"qwen3"}"#;
        let chunks: Vec<ChatStreamItem> = ndjson
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let ChatStreamItem::Response(thinking) = &chunks[0] else {
            panic!("expected response");
        };
        let message = thinking.message.as_ref().unwrap();
        assert!(message.content.is_empty());
        assert_eq!(message.thinking.as_deref(), Some("Consider"));
        let ChatStreamItem::Response(tools) = &chunks[1] else {
            panic!("expected response");
        };
        let call = &tools.message.as_ref().unwrap().tool_calls.as_ref().unwrap()[0];
        assert!(call.id.is_none());
        assert_eq!(call.function.index, Some(0));
        let Some(ToolArguments::Object(arguments)) = &call.function.arguments else {
            panic!("expected object arguments");
        };
        assert_eq!(arguments["city"], "Helsinki");
        let ChatStreamItem::Response(done) = &chunks[2] else {
            panic!("expected response");
        };
        assert!(done.done);
        assert!(done.message.is_none());
        assert_eq!(done.prompt_eval_cached_count, Some(80));
        assert_eq!(done.eval_count, Some(12));
        assert_eq!(done.extra["remote_model"], "qwen3");
    }

    #[test]
    fn malformed_arguments_survive_and_stream_errors_are_not_chunks() {
        let value = json!({"role":"assistant", "images":null, "tool_calls":[
            {"id":"call_1", "function":{"name":"weather", "arguments":"{broken"}},
            {"function":{"index":1}},
            {"function":{"name":"weather", "arguments":null}}
        ]});
        let message: Message = serde_json::from_value(value.clone()).unwrap();
        let calls = message.tool_calls.as_ref().unwrap();
        assert!(matches!(&calls[0].function.arguments,
            Some(ToolArguments::Invalid(Value::String(raw))) if raw == "{broken"));
        assert!(calls[1].function.arguments.is_none());
        assert_eq!(
            calls[2].function.arguments,
            Some(ToolArguments::Invalid(Value::Null))
        );
        assert_eq!(
            serde_json::to_value(&calls[0]).unwrap(),
            value["tool_calls"][0]
        );
        let item: ChatStreamItem = serde_json::from_value(json!({
            "error":"model stopped", "retry_after":2
        }))
        .unwrap();
        let ChatStreamItem::Error(error) = item else {
            panic!("expected error");
        };
        assert_eq!(error.extra["retry_after"], 2);
        assert!(serde_json::from_str::<ChatStreamItem>("{}").is_err());
    }

    #[test]
    fn request_keeps_schema_images_and_model_defined_controls() {
        let value = json!({
            "model":"vision", "stream":true, "think":"custom-effort",
            "keep_alive":-1, "format":{"type":"object"},
            "messages":[
                {"role":"user", "content":"Describe", "images":["YWJj"]},
                {"role":"tool", "content":"sunny", "tool_name":"weather", "tool_call_id":"c1"}
            ],
            "tools":[{"type":"function","function":{
                "name":"weather", "parameters":{"type":"object", "$defs":{
                    "city":{"type":"string"}}, "additionalProperties":false}
            }}],
            "options":{"temperature":0.3, "num_ctx":32768, "future_option":true}
        });
        let request: ChatRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), value);
        let disabled: ChatRequest = serde_json::from_value(json!({
            "model":"m", "messages":[], "think":false
        }))
        .unwrap();
        assert_eq!(disabled.think, Some(Think::Enabled(false)));
    }

    #[test]
    fn discovery_retains_unfamiliar_metadata_without_requiring_local_details() {
        let list: ListModelsResponse = serde_json::from_value(json!({"models":[
            {"name":"local", "model":"local", "size":123,
             "details":{"family":"qwen", "families":null, "runner":"new"}},
            {"name":"remote", "remote_host":"https://example.invalid"}
        ]}))
        .unwrap();
        assert_eq!(
            list.models[0].details.as_ref().unwrap().extra["runner"],
            "new"
        );
        assert!(list.models[1].details.is_none());
        let show = json!({
            "parameters":"num_ctx 8192", "capabilities":["tools","future"],
            "model_info":{"general.architecture":"qwen", "qwen.context_length":32768},
            "thinking":{"values":[false,"custom"],"default":"custom"},
            "projector_info":{"vision":true}
        });
        let parsed: ShowModelResponse = serde_json::from_value(show.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), show);
    }
}
