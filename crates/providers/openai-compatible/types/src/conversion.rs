use crate as wire;
use base64::Engine as _;
use language_model::{
    CompletionRequest, Content, Error, ImageSource, Message, Reasoning, Role, Thinking,
    ToolArguments, ToolResultContent,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Normalizes a chat-completions finish reason; servers may add their own.
pub fn finish_reason(reason: &str) -> language_model::FinishReason {
    use language_model::FinishReason;
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.into()),
    }
}

impl wire::ChatCompletionRequest {
    /// Builds the wire request for a model, validating the whole history and
    /// refusing anything chat completions can't represent. Streaming mode is
    /// left to the client.
    pub fn from_completion(
        model: &language_model::LanguageModel,
        mut request: CompletionRequest,
    ) -> Result<Self, Error> {
        model.validate()?;
        request.validate()?;
        if request.thinking.is_some() && request.options.contains_key("reasoning_effort") {
            return Err(Error::InvalidRequest(
                "shared thinking conflicts with reasoning_effort option".into(),
            ));
        }
        let controls: [(&[&str], bool); 5] = [
            (
                &["max_tokens", "max_completion_tokens"],
                request.max_output_tokens.is_some(),
            ),
            (&["temperature"], request.temperature.is_some()),
            (&["top_p"], request.top_p.is_some()),
            (&["stop"], !request.stop.is_empty()),
            (&["seed"], request.seed.is_some()),
        ];
        for (keys, typed) in controls {
            if typed && let Some(key) = keys.iter().find(|key| request.options.contains_key(**key))
            {
                return Err(Error::InvalidRequest(format!(
                    "a typed control and the {key} option cannot both be supplied"
                )));
            }
        }
        let reasoning_effort = match &request.thinking {
            None => None,
            Some(Thinking::Effort(effort)) => {
                model.thinking.validate_effort(effort)?;
                Some(effort.clone())
            }
            Some(Thinking::Enabled) => {
                let effort = model.thinking.default_effort.as_ref().ok_or_else(|| {
                    Error::Unsupported("model has no configured default thinking effort".into())
                })?;
                model.thinking.validate_effort(effort)?;
                Some(effort.clone())
            }
            Some(Thinking::Disabled) => {
                let effort = model
                    .thinking
                    .disabled_effort
                    .as_ref()
                    .filter(|effort| !effort.trim().is_empty())
                    .ok_or_else(|| {
                        Error::Unsupported(
                            "model has no configured disabled thinking effort".into(),
                        )
                    })?;
                Some(effort.clone())
            }
        };
        resolve_history(&mut request.messages)?;
        let mut messages = Vec::new();
        if let Some(text) = request.instructions {
            messages.push(wire::Message {
                role: "system".into(),
                content: Some(wire::Content::Text(text)),
                ..Default::default()
            });
        }
        for message in request.messages {
            messages.extend(self::messages(message)?);
        }
        let tools: Vec<_> = request
            .tools
            .into_iter()
            .map(wire::ToolDefinition::from)
            .collect();
        let mut output = wire::ChatCompletionRequest {
            model: model.id.clone(),
            messages,
            tools: (!tools.is_empty()).then_some(tools),
            reasoning_effort,
            // `max_tokens` rather than `max_completion_tokens`: Ollama and
            // vLLM both accept it.
            max_tokens: request.max_output_tokens,
            temperature: request.temperature,
            top_p: request.top_p,
            stop: (!request.stop.is_empty()).then_some(wire::Stop::Many(request.stop)),
            seed: request.seed,
            ..Default::default()
        };
        apply_options(&mut output, request.options)?;
        Ok(output)
    }
}

/// Validate correlation across the sequence and resolve optional wire identities
/// before mapping each message independently.
fn resolve_history(messages: &mut [Message]) -> Result<(), Error> {
    let mut calls = HashMap::new();
    let mut resolved = HashSet::new();
    for message in messages {
        if message.role != Role::Tool && calls.len() != resolved.len() {
            return Err(Error::InvalidRequest(
                "tool calls must be resolved before the next message".into(),
            ));
        }
        let mut provider_ids = HashSet::new();
        for content in &mut message.content {
            match content {
                Content::ToolCall(call) => {
                    let provider_id = call
                        .provider_id
                        .get_or_insert_with(|| format!("call_{}", call.id.0));
                    if provider_id.is_empty()
                        || !provider_ids.insert(provider_id.clone())
                        || calls
                            .insert(call.id.clone(), (provider_id.clone(), call.name.clone()))
                            .is_some()
                    {
                        return Err(Error::InvalidRequest(
                            "empty provider ID or duplicate local tool call ID".into(),
                        ));
                    }
                }
                Content::ToolResult(result) => {
                    let Some((id, name)) = calls.get(&result.call) else {
                        return Err(Error::InvalidRequest(
                            "tool result has no preceding call".into(),
                        ));
                    };
                    if !resolved.insert(result.call.clone())
                        || result.provider_id.as_ref().is_some_and(|value| value != id)
                        || result.name.as_ref().is_some_and(|value| value != name)
                    {
                        return Err(Error::InvalidRequest(
                            "duplicate or mismatched tool result".into(),
                        ));
                    }
                    result.provider_id = Some(id.clone());
                    result.name = Some(name.clone());
                }
                _ => {}
            }
        }
    }
    if calls.len() != resolved.len() {
        return Err(Error::InvalidRequest(
            "request contains unanswered tool calls".into(),
        ));
    }
    Ok(())
}

/// One generic tool message expands into one wire message per result.
pub(crate) fn messages(message: Message) -> Result<Vec<wire::Message>, Error> {
    message.validate()?;
    let mut messages = Vec::new();
    if message.role == Role::Tool && message.content.is_empty() {
        return Err(Error::InvalidRequest(
            "tool message must contain a result".into(),
        ));
    }
    if !message.replay.is_empty() {
        return Err(Error::Unsupported(
            "native replay data is not supported by chat completions".into(),
        ));
    }
    let mut output = wire::Message {
        role: match message.role {
            Role::System => "system",
            Role::Developer => "developer",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
        .into(),
        ..Default::default()
    };
    let mut parts = Vec::new();
    let mut tools = Vec::new();
    let mut reasoning = String::new();
    let mut provider_ids = HashSet::new();
    let mut stage = 0;
    for content in message.content {
        if message.role == Role::Assistant {
            let next = match &content {
                Content::Reasoning(_) => 0,
                Content::Text { .. } | Content::Image(_) => 1,
                _ => 2,
            };
            if next < stage {
                return Err(Error::Unsupported(
                    "interleaved assistant content cannot be represented by chat completions"
                        .into(),
                ));
            }
            stage = next;
        }
        match content {
            Content::Text { text } => parts.push(wire::ContentPart {
                kind: "text".into(),
                text: Some(text),
                ..Default::default()
            }),
            Content::Image(image) => {
                if message.role != Role::User {
                    return Err(Error::Unsupported(
                        "assistant image history is not in the shared subset".into(),
                    ));
                }
                let url = match image.source {
                    ImageSource::Url { url } => url,
                    ImageSource::Base64 { data } => {
                        if !image
                            .media_type
                            .parse::<mime::Mime>()
                            .is_ok_and(|media_type| {
                                media_type.type_() == mime::IMAGE
                                    && !media_type.subtype().as_str().is_empty()
                            })
                            || image.media_type.contains([';', ',', '\r', '\n'])
                            || base64::engine::general_purpose::STANDARD
                                .decode(&data)
                                .is_err()
                        {
                            return Err(Error::InvalidRequest(
                                "invalid base64 image or MIME type".into(),
                            ));
                        }
                        format!("data:{};base64,{data}", image.media_type)
                    }
                };
                parts.push(wire::ContentPart {
                    kind: "image_url".into(),
                    image_url: Some(wire::ImageUrl::Object(wire::ImageUrlObject {
                        url,
                        ..Default::default()
                    })),
                    ..Default::default()
                });
            }
            Content::Reasoning(Reasoning::Text { text }) => reasoning.push_str(&text),
            Content::Reasoning(Reasoning::Redacted { .. }) => {
                return Err(Error::Unsupported(
                    "redacted reasoning is not supported".into(),
                ));
            }
            Content::ToolCall(call) => {
                let provider_id = call
                    .provider_id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}", call.id.0));
                if provider_id.is_empty() || !provider_ids.insert(provider_id.clone()) {
                    return Err(Error::InvalidRequest(
                        "empty or duplicate provider tool call ID".into(),
                    ));
                }
                tools.push(wire::ToolCall {
                    id: Some(provider_id),
                    kind: Some("function".into()),
                    function: wire::FunctionCall {
                        name: call.name,
                        arguments: match call.arguments {
                            ToolArguments::Object { value } => Value::Object(value).to_string(),
                            ToolArguments::Invalid { raw } => raw,
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                });
            }
            Content::ToolResult(result) => {
                let id = result
                    .provider_id
                    .as_ref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        Error::InvalidRequest("tool result needs a resolved provider ID".into())
                    })?;
                let mut text = Vec::new();
                for content in result.content {
                    text.push(match content {
                        ToolResultContent::Text { text } => text,
                        ToolResultContent::Json { value } => value.to_string(),
                        ToolResultContent::Image(_) => {
                            return Err(Error::Unsupported(
                                "tool result images are not in the shared subset".into(),
                            ));
                        }
                    });
                }
                let text = text.join("\n");
                // Standard tool messages have no error flag; describe it.
                let text = if result.is_error {
                    format!("Tool error:\n{text}")
                } else {
                    text
                };
                messages.push(wire::Message {
                    role: "tool".into(),
                    tool_call_id: Some(id.clone()),
                    content: Some(wire::Content::Text(text)),
                    ..Default::default()
                });
            }
        }
    }
    if message.role == Role::Tool {
        return Ok(messages);
    }
    output.content = if parts.len() == 1 && parts[0].kind == "text" {
        Some(wire::Content::Text(parts.remove(0).text.unwrap()))
    } else if parts.is_empty() {
        None
    } else {
        Some(wire::Content::Parts(parts))
    };
    if !tools.is_empty() {
        output.tool_calls = Some(tools);
    }
    if !reasoning.is_empty() {
        output.reasoning = Some(reasoning);
    }
    messages.push(output);
    Ok(messages)
}

impl From<language_model::ToolDefinition> for wire::ToolDefinition {
    fn from(tool: language_model::ToolDefinition) -> Self {
        Self {
            kind: "function".into(),
            function: wire::FunctionDefinition {
                name: tool.name,
                description: tool.description,
                parameters: Some(Value::Object(tool.parameters)),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}
fn invalid_option(key: &str) -> Error {
    Error::InvalidRequest(format!("invalid {key} option"))
}

fn option<T: serde::de::DeserializeOwned>(value: Value, key: &str) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|_| invalid_option(key))
}

fn apply_options(
    output: &mut wire::ChatCompletionRequest,
    options: wire::Extensions,
) -> Result<(), Error> {
    for (key, value) in options {
        match key.as_str() {
            "model" | "messages" | "tools" | "stream" => {
                return Err(Error::InvalidRequest(format!(
                    "option {key} overrides structural request data"
                )));
            }
            "functions" | "function_call" => {
                return Err(Error::Unsupported(
                    "legacy function calling is not supported".into(),
                ));
            }
            "n" if value.as_u64() != Some(1) => {
                return Err(Error::Unsupported("only n=1 is supported".into()));
            }
            "temperature" => output.temperature = option(value, &key)?,
            "top_p" => output.top_p = option(value, &key)?,
            "max_tokens" => output.max_tokens = option(value, &key)?,
            "max_completion_tokens" => output.max_completion_tokens = option(value, &key)?,
            "frequency_penalty" => output.frequency_penalty = option(value, &key)?,
            "presence_penalty" => output.presence_penalty = option(value, &key)?,
            "parallel_tool_calls" => output.parallel_tool_calls = option(value, &key)?,
            "reasoning_effort" => output.reasoning_effort = option(value, &key)?,
            "seed" => output.seed = option(value, &key)?,
            "response_format" => output.response_format = Some(value),
            "stop" => output.stop = option(value, &key)?,
            "stream_options" => {
                if !value.is_object() {
                    return Err(invalid_option(&key));
                }
                if value
                    .get("include_usage")
                    .is_some_and(|value| !value.is_boolean())
                {
                    return Err(Error::InvalidRequest(
                        "include_usage must be boolean".into(),
                    ));
                }
                output.stream_options = Some(option(value, &key)?);
            }
            "tool_choice" => {
                if !matches!(value, Value::Null | Value::String(_) | Value::Object(_)) {
                    return Err(invalid_option(&key));
                }
                if let Some(function) = value.get("function")
                    && !function.is_object()
                {
                    return Err(invalid_option(&key));
                }
                output.tool_choice = option(value, &key)?;
            }
            _ => {
                output.extra.insert(key, value);
            }
        }
    }
    Ok(())
}
