use llm::{
    ChatMessage, ChatRequest, FinishReason, LlmError, StreamEvent, TokenUsage,
    ToolArgumentParseError, ToolCall, ToolDefinition,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OllamaChatRequest {
    pub model: String,
    pub messages: Vec<OllamaChatMessage>,
    pub stream: bool,
    pub think: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OllamaTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<OllamaChatOptions>,
}

impl OllamaChatRequest {
    pub fn from_chat_request(request: ChatRequest) -> Self {
        request.into()
    }
}

impl From<ChatRequest> for OllamaChatRequest {
    fn from(request: ChatRequest) -> Self {
        let mut extra = request.options.extra;
        if request.options.temperature.is_some() {
            extra.remove("temperature");
        }
        if request.options.max_tokens.is_some() {
            extra.remove("num_predict");
        }

        let options = OllamaChatOptions {
            temperature: request.options.temperature,
            num_predict: request.options.max_tokens,
            extra,
        }
        .into_option();

        Self {
            model: request.model,
            messages: request.messages.into_iter().map(Into::into).collect(),
            stream: true,
            think: request.options.think,
            tools: request.tools.into_iter().map(Into::into).collect(),
            options,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum OllamaChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, alias = "reasoning", skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<OllamaToolCall>,
    },
    Tool {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

impl OllamaChatMessage {
    fn into_stream_events(
        self,
        next_tool_index: &mut u64,
        emitted_tool_call: &mut bool,
    ) -> Vec<StreamEvent> {
        let Self::Assistant {
            content,
            thinking,
            tool_calls,
        } = self
        else {
            return Vec::new();
        };

        let mut events = Vec::new();
        if let Some(thinking) = thinking.filter(|text| !text.is_empty()) {
            events.push(StreamEvent::ReasoningDelta(thinking));
        }
        if !content.is_empty() {
            events.push(StreamEvent::TextDelta(content));
        }

        for tool_call in tool_calls {
            *emitted_tool_call = true;
            events.extend(tool_call.into_stream_events(next_tool_index));
        }

        events
    }
}

impl From<ChatMessage> for OllamaChatMessage {
    fn from(message: ChatMessage) -> Self {
        match message {
            ChatMessage::System { content } => Self::System { content },
            ChatMessage::User { content } => Self::User { content },
            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => Self::Assistant {
                content,
                thinking: reasoning,
                tool_calls: tool_calls
                    .into_iter()
                    .map(OllamaToolCall::from_generic_tool_call)
                    .collect(),
            },
            ChatMessage::Tool {
                call_id,
                name,
                content,
                is_error,
            } => Self::Tool {
                content,
                tool_call_id: Some(call_id),
                tool_name: Some(name),
                is_error: Some(is_error),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OllamaToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub function: OllamaFunctionCall,
}

impl OllamaToolCall {
    pub fn from_generic_tool_call(call: ToolCall) -> Self {
        Self {
            // Ollama tool calls in assistant messages are keyed by function; the
            // synthetic llm::ToolCall id is tracked on the corresponding tool
            // result message, not echoed back in the assistant history.
            id: None,
            function: OllamaFunctionCall {
                name: call.name,
                arguments: call.arguments,
            },
        }
    }

    fn into_stream_events(self, next_tool_index: &mut u64) -> Vec<StreamEvent> {
        let name = self.function.name;
        let raw_arguments = raw_arguments(&self.function.arguments);
        let id = self.id.unwrap_or_else(|| {
            *next_tool_index += 1;
            format!("ollama-tool-call-{next_tool_index}")
        });

        let mut events = vec![StreamEvent::ToolCallStarted {
            id: id.clone(),
            name: name.clone(),
        }];
        if !raw_arguments.is_empty() {
            events.push(StreamEvent::ToolCallArgumentDelta {
                id: id.clone(),
                delta: raw_arguments.clone(),
            });
        }

        match parse_tool_arguments(&raw_arguments) {
            Ok(arguments) => events.push(StreamEvent::ToolCallFinished(ToolCall {
                id,
                name,
                raw_arguments,
                arguments,
            })),
            Err(error) => events.push(StreamEvent::ToolCallArgumentParseError(
                ToolArgumentParseError {
                    id,
                    name,
                    raw_arguments,
                    error: error.to_string(),
                },
            )),
        }

        events
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OllamaFunctionCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaFunctionTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OllamaTool {
    Function { function: OllamaFunctionTool },
}

impl From<ToolDefinition> for OllamaTool {
    fn from(tool: ToolDefinition) -> Self {
        Self::Function {
            function: OllamaFunctionTool {
                name: tool.name,
                description: Some(tool.description),
                parameters: Some(tool.parameters),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OllamaChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl OllamaChatOptions {
    fn into_option(self) -> Option<Self> {
        if self.temperature.is_none() && self.num_predict.is_none() && self.extra.is_empty() {
            None
        } else {
            Some(self)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct OllamaChatResponse {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub message: Option<OllamaChatMessage>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub done_reason: Option<String>,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

impl OllamaChatResponse {
    pub fn into_stream_events(
        self,
        next_tool_index: &mut u64,
    ) -> Result<Vec<StreamEvent>, LlmError> {
        let Self {
            message,
            done,
            done_reason,
            prompt_eval_count,
            eval_count,
            error,
            ..
        } = self;

        if let Some(error) = error.filter(|message| !message.is_empty()) {
            return Err(LlmError::Provider(error));
        }

        let mut events = Vec::new();
        let mut emitted_tool_call = false;

        if let Some(message) = message {
            events.extend(message.into_stream_events(next_tool_index, &mut emitted_tool_call));
        }

        if let Some(input_tokens) = prompt_eval_count {
            events.push(StreamEvent::Usage(TokenUsage::from_input_output(
                input_tokens,
                eval_count.unwrap_or(0),
            )));
        }

        if done {
            let reason = if emitted_tool_call {
                FinishReason::ToolCalls
            } else {
                finish_reason(done_reason.as_deref())
            };
            events.push(StreamEvent::Finished { reason });
        }

        Ok(events)
    }
}

fn raw_arguments(arguments: &Value) -> String {
    match arguments {
        Value::String(arguments) => arguments.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn parse_tool_arguments(arguments: &str) -> Result<Value, serde_json::Error> {
    if arguments.trim().is_empty() {
        Ok(Value::Object(Default::default()))
    } else {
        serde_json::from_str(arguments)
    }
}

fn finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") | None => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("cancelled" | "canceled") => FinishReason::Cancelled,
        Some("error") => FinishReason::Error,
        Some(other) => FinishReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_request_conversion_serializes_ollama_shape() {
        let mut request = ChatRequest::new(
            "gemma4",
            vec![
                ChatMessage::system("system prompt"),
                ChatMessage::assistant_with_tools(
                    "I'll read it",
                    Some("thinking".to_string()),
                    vec![ToolCall {
                        id: "call-1".to_string(),
                        name: "read_file".to_string(),
                        raw_arguments: r#"{"path":"Cargo.toml"}"#.to_string(),
                        arguments: json!({ "path": "Cargo.toml" }),
                    }],
                ),
                ChatMessage::tool("call-1", "read_file", "contents", false),
            ],
        );
        request.options.think = false;
        request.options.temperature = Some(0.25);
        request.options.max_tokens = Some(128);
        request.tools.push(ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
            supports_argument_streaming: false,
        });

        let body = serde_json::to_value(OllamaChatRequest::from_chat_request(request)).unwrap();

        assert_eq!(
            body,
            json!({
                "model": "gemma4",
                "messages": [
                    { "role": "system", "content": "system prompt" },
                    {
                        "role": "assistant",
                        "content": "I'll read it",
                        "thinking": "thinking",
                        "tool_calls": [
                            {
                                "function": {
                                    "name": "read_file",
                                    "arguments": { "path": "Cargo.toml" }
                                }
                            }
                        ]
                    },
                    {
                        "role": "tool",
                        "content": "contents",
                        "tool_call_id": "call-1",
                        "tool_name": "read_file",
                        "is_error": false
                    }
                ],
                "stream": true,
                "think": false,
                "tools": [
                    {
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "description": "Read a file",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string" }
                                },
                                "required": ["path"]
                            }
                        }
                    }
                ],
                "options": {
                    "temperature": 0.25,
                    "num_predict": 128
                }
            })
        );
    }

    #[test]
    fn chat_response_conversion_emits_generic_stream_events() {
        let response: OllamaChatResponse = serde_json::from_value(json!({
            "model": "gemma4",
            "created_at": "2026-01-01T00:00:00Z",
            "message": {
                "role": "assistant",
                "thinking": "checking",
                "content": "I'll call a tool",
                "tool_calls": [
                    {
                        "function": {
                            "name": "read_file",
                            "arguments": { "path": "Cargo.toml" }
                        }
                    }
                ]
            },
            "done": true,
            "prompt_eval_count": 3,
            "eval_count": 5
        }))
        .unwrap();
        let mut next_tool_index = 0;

        let events = response.into_stream_events(&mut next_tool_index).unwrap();

        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningDelta("checking".to_string()),
                StreamEvent::TextDelta("I'll call a tool".to_string()),
                StreamEvent::ToolCallStarted {
                    id: "ollama-tool-call-1".to_string(),
                    name: "read_file".to_string(),
                },
                StreamEvent::ToolCallArgumentDelta {
                    id: "ollama-tool-call-1".to_string(),
                    delta: r#"{"path":"Cargo.toml"}"#.to_string(),
                },
                StreamEvent::ToolCallFinished(ToolCall {
                    id: "ollama-tool-call-1".to_string(),
                    name: "read_file".to_string(),
                    raw_arguments: r#"{"path":"Cargo.toml"}"#.to_string(),
                    arguments: json!({ "path": "Cargo.toml" }),
                }),
                StreamEvent::Usage(TokenUsage::from_input_output(3, 5)),
                StreamEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ]
        );
    }

    #[test]
    fn chat_response_error_maps_to_provider_error() {
        let response: OllamaChatResponse = serde_json::from_value(json!({
            "error": "model not found"
        }))
        .unwrap();
        let mut next_tool_index = 0;

        let error = response
            .into_stream_events(&mut next_tool_index)
            .expect_err("provider errors should fail conversion");

        assert_eq!(error, LlmError::Provider("model not found".to_string()));
    }
}
