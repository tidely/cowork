use async_trait::async_trait;
use futures::StreamExt;
use llm::{
    ChatMessage, ChatRequest, FinishReason, LlmError, LlmStream, Provider, StreamEvent, TokenUsage,
    ToolArgumentParseError, ToolCall, ToolDefinition,
};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn from_env() -> Self {
        let base_url =
            std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
        Self::new(base_url)
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.base_url)
    }
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::from_env()
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn stream_chat(&self, request: ChatRequest) -> Result<LlmStream, LlmError> {
        let response = self
            .client
            .post(self.chat_url())
            .json(&ollama_request(request))
            .send()
            .await
            .map_err(|error| LlmError::Transport(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read error body: {error}"));
            return Err(LlmError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        Ok(Box::pin(stream_response(response)) as LlmStream)
    }
}

fn ollama_request(request: ChatRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": request.messages.into_iter().map(ollama_message).collect::<Vec<_>>(),
        "stream": true,
        "think": request.options.think,
    });

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.into_iter().map(ollama_tool).collect());
    }

    let mut options = request.options.extra;
    if let Some(temperature) = request.options.temperature {
        options.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(max_tokens) = request.options.max_tokens {
        options.insert("num_predict".to_string(), json!(max_tokens));
    }
    if !options.is_empty() {
        body["options"] = Value::Object(options);
    }

    body
}

fn ollama_message(message: ChatMessage) -> Value {
    match message {
        ChatMessage::System { content } => json!({ "role": "system", "content": content }),
        ChatMessage::User { content } => json!({ "role": "user", "content": content }),
        ChatMessage::Assistant {
            content,
            reasoning,
            tool_calls,
        } => {
            let mut message = json!({ "role": "assistant", "content": content });
            if let Some(reasoning) = reasoning {
                message["thinking"] = json!(reasoning);
            }
            if !tool_calls.is_empty() {
                message["tool_calls"] = Value::Array(
                    tool_calls
                        .into_iter()
                        .map(|call| {
                            json!({
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                }
                            })
                        })
                        .collect(),
                );
            }
            message
        }
        ChatMessage::Tool {
            call_id,
            name,
            content,
            is_error,
        } => json!({
            "role": "tool",
            "content": content,
            "tool_call_id": call_id,
            "tool_name": name,
            "is_error": is_error,
        }),
    }
}

fn ollama_tool(tool: ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

fn stream_response(
    response: reqwest::Response,
) -> impl futures::Stream<Item = Result<StreamEvent, LlmError>> + Send + 'static {
    async_stream::try_stream! {
        yield StreamEvent::Started;

        let mut chunks = response.bytes_stream();
        let mut buffer = String::new();
        let mut next_tool_index = 0_u64;

        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| LlmError::Transport(error.to_string()))?;
            let chunk = std::str::from_utf8(&chunk)
                .map_err(|error| LlmError::Decode(error.to_string()))?;
            buffer.push_str(chunk);

            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer.drain(..=newline);
                if line.is_empty() {
                    continue;
                }

                for event in response_events(&line, &mut next_tool_index)? {
                    yield event;
                }
            }
        }

        let line = buffer.trim();
        if !line.is_empty() {
            for event in response_events(line, &mut next_tool_index)? {
                yield event;
            }
        }
    }
}

fn response_events(line: &str, next_tool_index: &mut u64) -> Result<Vec<StreamEvent>, LlmError> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| LlmError::Decode(error.to_string()))?;

    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(LlmError::Provider(error.to_string()));
    }

    let mut events = Vec::new();
    let mut emitted_tool_call = false;

    if let Some(message) = value.get("message") {
        if let Some(thinking) = message
            .get("thinking")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            events.push(StreamEvent::ReasoningDelta(thinking.to_string()));
        }

        if let Some(content) = message
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            events.push(StreamEvent::TextDelta(content.to_string()));
        }

        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                if let Some(event_group) = parse_tool_call(tool_call, next_tool_index) {
                    emitted_tool_call = true;
                    events.extend(event_group);
                }
            }
        }
    }

    if let Some(usage) = usage_from_response(&value) {
        events.push(StreamEvent::Usage(usage));
    }

    if value.get("done").and_then(Value::as_bool).unwrap_or(false) {
        let reason = if emitted_tool_call {
            FinishReason::ToolCalls
        } else {
            finish_reason(value.get("done_reason").and_then(Value::as_str))
        };
        events.push(StreamEvent::Finished { reason });
    }

    Ok(events)
}

fn parse_tool_call(tool_call: &Value, next_tool_index: &mut u64) -> Option<Vec<StreamEvent>> {
    let function = tool_call.get("function").unwrap_or(tool_call);
    let name = function.get("name")?.as_str()?.to_string();
    let arguments = function
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let raw_arguments = raw_arguments(&arguments);
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
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

    Some(events)
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

fn usage_from_response(value: &Value) -> Option<TokenUsage> {
    let input_tokens = value.get("prompt_eval_count").and_then(Value::as_u64)?;
    let output_tokens = value.get("eval_count").and_then(Value::as_u64).unwrap_or(0);
    Some(TokenUsage::from_input_output(input_tokens, output_tokens))
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
