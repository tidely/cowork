use std::collections::{HashMap, HashSet, VecDeque};

use crate as native;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use language_model::{
    CompletionRequest, Content, Error, Image, ImageSource, Message, Reasoning, Role, Thinking,
    ToolArguments, ToolCallId, ToolResultContent,
};
use serde_json::{Value, json};

impl From<language_model::ToolDefinition> for native::Tool {
    fn from(tool: language_model::ToolDefinition) -> Self {
        Self::Function {
            function: native::FunctionDefinition {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
            },
        }
    }
}

impl native::ChatRequest {
    /// Builds the native request for a model, validating the whole history and
    /// refusing anything native Ollama can't represent. Streaming mode is left
    /// to the client.
    pub fn from_completion(
        model: &language_model::LanguageModel,
        mut request: CompletionRequest,
    ) -> Result<Self, Error> {
        model.validate()?;
        request.validate()?;
        if request.thinking.is_some() && request.options.contains_key("think") {
            return Err(Error::InvalidRequest(
                "thinking and the native think option cannot both be supplied".into(),
            ));
        }
        let think = match request.thinking {
            None => None,
            Some(Thinking::Enabled) => Some(match &model.thinking.default_effort {
                Some(effort) => {
                    model.thinking.validate_effort(effort)?;
                    native::Think::Level(effort.clone())
                }
                None => native::Think::Enabled(true),
            }),
            Some(Thinking::Disabled) => Some(native::Think::Enabled(false)),
            Some(Thinking::Effort(effort)) => {
                model.thinking.validate_effort(&effort)?;
                Some(native::Think::Level(effort))
            }
        };
        normalize_tool_results(&mut request.messages)?;
        let mut messages = Vec::new();
        for message in request.messages {
            messages.extend(self::messages(message)?);
        }
        let mut body = native::ChatRequest {
            model: model.id.clone(),
            messages,
            tools: request.tools.into_iter().map(native::Tool::from).collect(),
            think,
            ..Default::default()
        };
        for (key, value) in request.options {
            match key.as_str() {
                "think" => body.think = option(value, &key)?,
                "format" => body.format = Some(option(value, &key)?),
                "keep_alive" => body.keep_alive = Some(option(value, &key)?),
                "options" => body.options = option(value, &key)?,
                "truncate" => body.truncate = Some(option(value, &key)?),
                "shift" => body.shift = Some(option(value, &key)?),
                "logprobs" => body.logprobs = Some(option(value, &key)?),
                "top_logprobs" => body.top_logprobs = Some(option(value, &key)?),
                _ => {
                    return Err(Error::Unsupported(format!(
                        "unsupported Ollama request option: {key}"
                    )));
                }
            }
        }
        let controls = [
            (
                "num_predict",
                request.max_output_tokens.map(|value| json!(value)),
            ),
            ("temperature", request.temperature.map(|value| json!(value))),
            ("top_p", request.top_p.map(|value| json!(value))),
            (
                "stop",
                (!request.stop.is_empty()).then(|| json!(request.stop)),
            ),
            ("seed", request.seed.map(|value| json!(value))),
        ];
        for (key, value) in controls {
            let Some(value) = value else { continue };
            if body.options.contains_key(key) {
                return Err(Error::InvalidRequest(format!(
                    "a typed control and the native {key} option cannot both be supplied"
                )));
            }
            body.options.insert(key.into(), value);
        }
        Ok(body)
    }
}

fn option<T: serde::de::DeserializeOwned>(value: Value, key: &str) -> Result<T, Error> {
    serde_json::from_value(value)
        .map_err(|_| Error::InvalidRequest(format!("invalid value for Ollama option {key}")))
}

/// Correlation belongs to the conversation, not to one message's projection.
/// Resolve omitted native fields here before converting messages independently.
fn normalize_tool_results(messages: &mut [Message]) -> Result<(), Error> {
    let mut calls = HashMap::<ToolCallId, (String, Option<String>)>::new();
    let mut pending = VecDeque::new();
    for message in messages {
        if message.role == Role::Tool {
            for content in &mut message.content {
                let Content::ToolResult(result) = content else {
                    unreachable!("request validation checks tool content");
                };
                let Some((name, provider_id)) = calls.get(&result.call) else {
                    return Err(Error::InvalidRequest(
                        "tool result has no preceding call".into(),
                    ));
                };
                // Same-name id-less calls can only be correlated by order.
                if pending.front() != Some(&result.call) {
                    return Err(Error::InvalidRequest(
                        "native Ollama tool results must follow their call order".into(),
                    ));
                }
                pending.pop_front();
                if result.name.as_ref().is_some_and(|value| value != name)
                    || result
                        .provider_id
                        .as_ref()
                        .is_some_and(|value| Some(value) != provider_id.as_ref())
                {
                    return Err(Error::InvalidRequest(
                        "tool result disagrees with its call".into(),
                    ));
                }
                result.name = Some(name.clone());
                result.provider_id = provider_id.clone();
            }
        } else {
            if !pending.is_empty() {
                return Err(Error::InvalidRequest(
                    "tool calls must be resolved before the next message".into(),
                ));
            }
            for content in &message.content {
                if let Content::ToolCall(call) = content {
                    if calls
                        .insert(
                            call.id.clone(),
                            (call.name.clone(), call.provider_id.clone()),
                        )
                        .is_some()
                    {
                        return Err(Error::InvalidRequest("duplicate local tool call ID".into()));
                    }
                    pending.push_back(call.id.clone());
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(Error::InvalidRequest("tool calls have no results".into()));
    }
    Ok(())
}

/// One generic message can expand into multiple native tool-result messages.
/// Standalone tool results must already carry their resolved tool names.
fn messages(message: Message) -> Result<Vec<native::Message>, Error> {
    message.validate()?;
    if !message.replay.is_empty() {
        return Err(Error::Unsupported(
            "Ollama adapter does not support opaque replay data".into(),
        ));
    }
    if message.role == Role::Tool {
        let mut output = Vec::with_capacity(message.content.len());
        for content in message.content {
            let Content::ToolResult(result) = content else {
                unreachable!("message validation checks tool content");
            };
            let name = result.name.ok_or_else(|| {
                Error::InvalidRequest("tool result needs a name from its preceding call".into())
            })?;
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for part in result.content {
                match part {
                    ToolResultContent::Text { text } => texts.push(text),
                    ToolResultContent::Json { value } => texts.push(value.to_string()),
                    ToolResultContent::Image(image) => images.push(image_data(image)?),
                }
            }
            let content = texts.join("\n");
            output.push(native::Message {
                role: "tool".into(),
                content: if result.is_error {
                    format!("Error: {content}")
                } else {
                    content
                },
                tool_name: Some(name),
                tool_call_id: result.provider_id,
                images: (!images.is_empty()).then_some(images),
                ..Default::default()
            });
        }
        return Ok(output);
    }
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Developer => {
            return Err(Error::Unsupported(
                "native Ollama has no developer role".into(),
            ));
        }
        Role::Tool => unreachable!(),
    };
    let mut native = native::Message {
        role: role.into(),
        ..Default::default()
    };
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
                    "interleaved assistant content cannot be represented by native Ollama".into(),
                ));
            }
            stage = next;
        }
        match content {
            Content::Text { text } => native.content.push_str(&text),
            Content::Reasoning(Reasoning::Text { text }) => {
                native.thinking.get_or_insert_default().push_str(&text)
            }
            Content::Reasoning(Reasoning::Redacted { .. }) => {
                return Err(Error::Unsupported(
                    "native Ollama cannot replay redacted reasoning".into(),
                ));
            }
            Content::Image(image) => native
                .images
                .get_or_insert_default()
                .push(image_data(image)?),
            Content::ToolCall(call) => {
                if let Some(id) = call.provider_id.as_ref()
                    && (id.is_empty() || !provider_ids.insert(id.clone()))
                {
                    return Err(Error::InvalidRequest(
                        "provider tool IDs must be nonempty and unique within a reply".into(),
                    ));
                }
                let arguments = match call.arguments {
                    ToolArguments::Object { value } => native::ToolArguments::Object(value),
                    ToolArguments::Invalid { .. } => {
                        return Err(Error::Unsupported(
                            "native Ollama cannot replay malformed tool arguments".into(),
                        ));
                    }
                };
                let native_calls = native.tool_calls.get_or_insert_default();
                let index = u32::try_from(native_calls.len())
                    .map_err(|_| Error::InvalidRequest("too many tool calls".into()))?;
                native_calls.push(native::ToolCall {
                    id: call.provider_id,
                    function: native::FunctionCall {
                        index: Some(index),
                        name: call.name,
                        arguments: Some(arguments),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            }
            Content::ToolResult(_) => unreachable!("validated role/content"),
        }
    }
    Ok(vec![native])
}

fn image_data(image: Image) -> Result<String, Error> {
    let ImageSource::Base64 { data } = image.source else {
        return Err(Error::Unsupported(
            "native Ollama requires base64 image data".into(),
        ));
    };
    if !image
        .media_type
        .parse::<mime::Mime>()
        .is_ok_and(|media_type| {
            media_type.type_() == mime::IMAGE && !media_type.subtype().as_str().is_empty()
        })
    {
        return Err(Error::InvalidRequest(
            "image content needs an image media type".into(),
        ));
    }
    STANDARD
        .decode(&data)
        .map_err(|_| Error::InvalidRequest("invalid base64 image".into()))?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use language_model::{ToolCall, ToolResult};
    use serde_json::json;

    fn request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message::new(Role::User, vec![Content::text("Hello")])],
            ..Default::default()
        }
    }

    #[test]
    fn shared_thinking_uses_native_switches_and_model_defined_effort() {
        let mut model = language_model::LanguageModel::new("test-model");
        for (thinking, expected) in [
            (None, None),
            (Some(Thinking::Enabled), Some(native::Think::Enabled(true))),
            (
                Some(Thinking::Disabled),
                Some(native::Think::Enabled(false)),
            ),
            (
                Some(Thinking::Effort("custom".into())),
                Some(native::Think::Level("custom".into())),
            ),
        ] {
            let mut input = request();
            input.thinking = thinking;
            let projected = native::ChatRequest::from_completion(&model, input).unwrap();
            assert_eq!(projected.think, expected);
        }

        model.thinking.effort_levels = Some(vec![language_model::ThinkingLevel {
            value: "custom".into(),
            label: "Custom effort".into(),
        }]);
        model.thinking.default_effort = Some("custom".into());
        model.thinking.disabled_effort = Some("custom".into());
        for thinking in [Thinking::Enabled, Thinking::Effort("custom".into())] {
            let mut input = request();
            input.thinking = Some(thinking);
            assert_eq!(
                native::ChatRequest::from_completion(&model, input)
                    .unwrap()
                    .think,
                Some(native::Think::Level("custom".into()))
            );
        }
        let mut input = request();
        input.thinking = Some(Thinking::Effort("unadvertised".into()));
        assert!(matches!(
            native::ChatRequest::from_completion(&model, input),
            Err(Error::Unsupported(_))
        ));
        model.thinking.default_effort = Some("unadvertised".into());
        let mut input = request();
        input.thinking = Some(Thinking::Enabled);
        assert!(matches!(
            native::ChatRequest::from_completion(&model, input),
            Err(Error::Unsupported(_))
        ));
        let mut input = request();
        input.thinking = Some(Thinking::Disabled);
        assert_eq!(
            native::ChatRequest::from_completion(&model, input)
                .unwrap()
                .think,
            Some(native::Think::Enabled(false))
        );
        assert!(
            native::ChatRequest::from_completion(&model, request())
                .unwrap()
                .think
                .is_none()
        );
        for raw in [Value::Null, json!(false), json!("custom")] {
            let mut input = request();
            input.thinking = Some(Thinking::Disabled);
            input.options.insert("think".into(), raw);
            assert!(matches!(
                native::ChatRequest::from_completion(&model, input),
                Err(Error::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn typed_adapters_expand_tool_results_after_correlating_history() {
        use crate as native;
        use crate::ChatRequest;

        let user = Message::new(Role::User, vec![Content::text("A"), Content::text("B")]);
        let projected: Vec<native::Message> = messages(user).unwrap();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].content, "AB");

        for media_type in ["image/png", "IMAGE/PNG", "image/png; charset=utf-8"] {
            let projected: Vec<native::Message> = messages(Message::new(
                Role::User,
                vec![Content::Image(Image {
                    media_type: media_type.into(),
                    source: ImageSource::Base64 {
                        data: "aGVsbG8=".into(),
                    },
                })],
            ))
            .unwrap();
            assert_eq!(
                projected[0].images.as_deref(),
                Some(["aGVsbG8=".to_owned()].as_slice())
            );
        }
        for media_type in ["image/", "text/plain"] {
            assert!(matches!(
                messages(Message::new(
                    Role::User,
                    vec![Content::Image(Image {
                        media_type: media_type.into(),
                        source: ImageSource::Base64 {
                            data: "aGVsbG8=".into()
                        },
                    })],
                )),
                Err(Error::InvalidRequest(_))
            ));
        }

        let calls: Vec<_> = [Some("provider-id".to_owned()), None]
            .into_iter()
            .map(|provider_id| ToolCall {
                id: ToolCallId::new(),
                provider_id,
                name: "same_name".into(),
                arguments: ToolArguments::Object {
                    value: Default::default(),
                },
            })
            .collect();
        let mut results = Message::new(
            Role::Tool,
            calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    Content::ToolResult(ToolResult {
                        call: call.id.clone(),
                        provider_id: None,
                        name: None,
                        content: vec![ToolResultContent::Text {
                            text: index.to_string(),
                        }],
                        is_error: index == 1,
                    })
                })
                .collect(),
        );
        assert!(matches!(
            messages(results.clone()),
            Err(Error::InvalidRequest(_))
        ));
        let mut input = request();
        input.messages.push(Message::new(
            Role::Assistant,
            calls.iter().cloned().map(Content::ToolCall).collect(),
        ));
        input.messages.push(results.clone());
        let projected: native::ChatRequest =
            ChatRequest::from_completion(&language_model::LanguageModel::new("test-model"), input)
                .unwrap();
        assert_eq!(projected.model, "test-model");
        assert_eq!(projected.messages.len(), 4);
        assert_eq!(
            projected.messages[2].tool_call_id.as_deref(),
            Some("provider-id")
        );
        assert_eq!(projected.messages[2].content, "0");
        assert!(projected.messages[3].tool_call_id.is_none());
        assert_eq!(projected.messages[3].content, "Error: 1");
        for content in &mut results.content {
            let Content::ToolResult(result) = content else {
                unreachable!()
            };
            result.name = Some("same_name".into());
        }
        let projected: Vec<native::Message> = messages(results).unwrap();
        assert_eq!(projected.len(), 2);
        assert!(
            projected
                .iter()
                .all(|message| message.tool_name.as_deref() == Some("same_name"))
        );
        assert!(matches!(
            messages(Message::new(
                Role::User,
                vec![Content::ToolResult(ToolResult {
                    call: calls[0].id.clone(),
                    provider_id: None,
                    name: Some("same_name".into()),
                    content: vec![],
                    is_error: false,
                })]
            )),
            Err(Error::InvalidRequest(_))
        ));
        for content in [
            vec![
                Content::text("answer"),
                Content::Reasoning(Reasoning::Text {
                    text: "late thinking".into(),
                }),
            ],
            vec![
                Content::ToolCall(calls[0].clone()),
                Content::text("late answer"),
            ],
        ] {
            assert!(matches!(
                messages(Message::new(Role::Assistant, content)),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn request_controls_convert_to_native_fields_without_json_roundtrip() {
        use crate as native;
        use crate::ChatRequest;

        assert!(matches!(
            ChatRequest::from_completion(&language_model::LanguageModel::new(""), request()),
            Err(Error::InvalidRequest(_))
        ));
        let mut input = request();
        input.options = json!({
            "think":false, "format":{"type":"object"}, "keep_alive":-1,
            "options":{"future":{"nested":true}}, "truncate":false, "shift":true,
            "logprobs":true, "top_logprobs":4294967295_u64
        })
        .as_object()
        .unwrap()
        .clone();
        let projected: native::ChatRequest =
            ChatRequest::from_completion(&language_model::LanguageModel::new("test-model"), input)
                .unwrap();
        assert_eq!(projected.think, Some(native::Think::Enabled(false)));
        assert!(
            matches!(projected.format, Some(native::ResponseFormat::Schema(ref value)) if value["type"] == "object")
        );
        assert!(
            matches!(projected.keep_alive, Some(native::KeepAlive::Seconds(ref value)) if value.as_i64() == Some(-1))
        );
        assert_eq!(projected.options["future"]["nested"], true);
        assert_eq!(
            (
                projected.truncate,
                projected.shift,
                projected.logprobs,
                projected.top_logprobs
            ),
            (Some(false), Some(true), Some(true), Some(u32::MAX))
        );
        for (key, value) in [
            ("think", json!([])),
            ("format", json!(null)),
            ("keep_alive", json!(true)),
            ("options", json!([])),
            ("truncate", json!(0)),
            ("shift", json!("true")),
            ("logprobs", json!(null)),
            ("top_logprobs", json!(4294967296_u64)),
        ] {
            let mut input = request();
            input.options.insert(key.into(), value);
            assert!(matches!(
                ChatRequest::from_completion(
                    &language_model::LanguageModel::new("test-model"),
                    input
                ),
                Err(Error::InvalidRequest(_))
            ));
        }
        let mut input = request();
        input.options = json!({"think":null, "format":"json", "keep_alive":"5m"})
            .as_object()
            .unwrap()
            .clone();
        let projected: native::ChatRequest =
            ChatRequest::from_completion(&language_model::LanguageModel::new("test-model"), input)
                .unwrap();
        assert!(projected.think.is_none());
        assert_eq!(
            projected.format,
            Some(native::ResponseFormat::Name("json".into()))
        );
        assert_eq!(
            projected.keep_alive,
            Some(native::KeepAlive::Duration("5m".into()))
        );

        let mut input = request();
        input.max_output_tokens = Some(64);
        input.temperature = Some(0.5);
        input.top_p = Some(0.9);
        input.stop = vec!["end".into()];
        input.seed = Some(7);
        input.options = json!({"options":{"num_ctx":8192}})
            .as_object()
            .unwrap()
            .clone();
        let model = language_model::LanguageModel::new("test-model");
        let projected = ChatRequest::from_completion(&model, input.clone()).unwrap();
        assert_eq!(
            Value::Object(projected.options),
            json!({"num_ctx":8192, "num_predict":64, "temperature":0.5,
                   "top_p":0.9, "stop":["end"], "seed":7})
        );
        input.options = json!({"options":{"num_predict":1}})
            .as_object()
            .unwrap()
            .clone();
        assert!(matches!(
            ChatRequest::from_completion(&model, input),
            Err(Error::InvalidRequest(_))
        ));
    }
}
