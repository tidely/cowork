//! Shared input is projected only at the request boundary. Native signed blocks
//! live in replay data so persistence does not need an Anthropic-specific schema.
use crate as wire;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use language_model::{
    CachePoint, CompletionRequest, Content, Error, FinishReason, Image, ImageSource, LanguageModel,
    Message, Reasoning, Role, Thinking, ToolArguments, ToolCall, ToolCallId, ToolResultContent,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub const REPLAY_FORMAT: &str = "anthropic.messages.content";

/// Anthropic accepts at most this many `cache_control` breakpoints.
const MAX_CACHE_BREAKPOINTS: usize = 4;

impl wire::StopReason {
    pub fn finish_reason(&self) -> FinishReason {
        match self {
            Self::EndTurn | Self::StopSequence => FinishReason::Stop,
            Self::ToolUse => FinishReason::ToolCalls,
            Self::MaxTokens | Self::ModelContextWindowExceeded => FinishReason::Length,
            Self::Refusal => FinishReason::Refusal,
            Self::PauseTurn | Self::Unknown(_) => FinishReason::Other(self.wire_str()),
        }
    }
}

/// Marks the last block that can carry a breakpoint. Thinking blocks can't.
fn mark_cache(blocks: &mut [wire::ContentBlock]) {
    for block in blocks.iter_mut().rev() {
        match block {
            wire::ContentBlock::Text { cache_control, .. }
            | wire::ContentBlock::Image { cache_control, .. }
            | wire::ContentBlock::ToolUse { cache_control, .. }
            | wire::ContentBlock::ToolResult { cache_control, .. } => {
                *cache_control = Some(wire::CacheControl::ephemeral());
                return;
            }
            wire::ContentBlock::Thinking { .. } | wire::ContentBlock::RedactedThinking { .. } => {}
        }
    }
}

fn text(text: String) -> wire::ContentBlock {
    wire::ContentBlock::Text {
        text,
        citations: None,
        cache_control: None,
    }
}

fn invalid(message: &str) -> Error {
    Error::InvalidRequest(message.into())
}

fn option<T: serde::de::DeserializeOwned>(value: Value, key: &str) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|_| invalid(&format!("invalid {key} option")))
}

impl wire::MessagesRequest {
    /// Builds a native request without changing the canonical input. Output
    /// tokens must be explicit; a context-window size is not an output budget.
    pub fn from_completion(
        model: &LanguageModel,
        mut request: CompletionRequest,
    ) -> Result<Self, Error> {
        model.validate()?;
        request.validate()?;
        if request.seed.is_some() {
            return Err(Error::Unsupported("Anthropic does not support seed".into()));
        }
        for (key, set) in [
            ("max_tokens", request.max_output_tokens.is_some()),
            ("temperature", request.temperature.is_some()),
            ("top_p", request.top_p.is_some()),
            ("stop_sequences", !request.stop.is_empty()),
            ("thinking", request.thinking.is_some()),
        ] {
            if set && request.options.contains_key(key) {
                return Err(invalid(&format!("shared control conflicts with {key}")));
            }
        }
        let mut output = Self {
            model: model.id.clone(),
            max_tokens: request
                .max_output_tokens
                .map(u32::try_from)
                .transpose()
                .map_err(|_| invalid("max_output_tokens exceeds u32"))?
                .unwrap_or(0),
            temperature: request.temperature,
            top_p: request.top_p,
            stop_sequences: (!request.stop.is_empty()).then_some(request.stop),
            ..Default::default()
        };
        for (key, value) in request.options {
            match key.as_str() {
                "max_tokens" => output.max_tokens = option(value, &key)?,
                "temperature" => output.temperature = Some(option(value, &key)?),
                "top_p" => output.top_p = Some(option(value, &key)?),
                "top_k" => output.top_k = Some(option(value, &key)?),
                "stop_sequences" => output.stop_sequences = Some(option(value, &key)?),
                "thinking" => output.thinking = Some(option(value, &key)?),
                "output_config" => output.output_config = Some(option(value, &key)?),
                "tool_choice" => output.tool_choice = Some(option(value, &key)?),
                "metadata" => output.metadata = Some(option(value, &key)?),
                _ => return Err(invalid(&format!("unsupported Anthropic option {key}"))),
            }
        }
        if let Some(thinking) = request.thinking {
            output.thinking = Some(match thinking {
                Thinking::Disabled => wire::ThinkingConfig::Disabled,
                Thinking::Enabled | Thinking::Effort(_) => {
                    wire::ThinkingConfig::Adaptive { display: None }
                }
            });
            let effort = match thinking {
                Thinking::Effort(effort) => Some(effort),
                Thinking::Enabled => model.thinking.default_effort.clone(),
                Thinking::Disabled => None,
            };
            if let Some(effort) = effort {
                model.thinking.validate_effort(&effort)?;
                let config = output.output_config.get_or_insert(wire::OutputConfig {
                    effort: None,
                    format: None,
                });
                if config.effort.is_some() {
                    return Err(invalid(
                        "shared thinking conflicts with output_config.effort",
                    ));
                }
                config.effort = Some(effort);
            }
        }
        if output.max_tokens == 0 {
            return Err(invalid(
                "Anthropic requires a positive max_output_tokens or max_tokens option",
            ));
        }
        if let Some(wire::ThinkingConfig::Enabled { budget_tokens }) = output.thinking
            && (budget_tokens < 1024 || budget_tokens >= output.max_tokens)
        {
            return Err(invalid(
                "thinking budget must be at least 1024 and below max_tokens",
            ));
        }
        resolve_history(&mut request.messages)?;
        let points = &request.cache[request.cache.len().saturating_sub(MAX_CACHE_BREAKPOINTS)..];
        for message in request.messages {
            message.validate()?;
            if message.role == Role::Developer {
                return Err(Error::Unsupported("Anthropic has no developer role".into()));
            }
            let role = if message.role == Role::Assistant {
                wire::MessageRole::Assistant
            } else {
                wire::MessageRole::User
            };
            let cached = points.contains(&CachePoint::AfterMessage(message.id.clone()));
            let mut blocks = message_blocks(message, model)?;
            if blocks.is_empty() {
                return Err(invalid("message content must not be empty"));
            }
            if cached {
                mark_cache(&mut blocks);
            }
            // Consecutive tool messages must share the user turn immediately
            // after the assistant's calls. Merging also follows API role rules.
            if let Some(last) = output.messages.last_mut()
                && last.role == role
                && let wire::MessageContent::Blocks(previous) = &mut last.content
            {
                previous.extend(blocks);
                continue;
            }
            output.messages.push(wire::Message {
                role,
                content: wire::MessageContent::Blocks(blocks),
            });
        }
        if output.messages.is_empty() {
            return Err(invalid("request needs a conversation message"));
        }
        if let Some(text) = request.instructions {
            let cache = points.contains(&CachePoint::AfterInstructions);
            output.system = Some(wire::SystemParam::Blocks(vec![wire::TextBlock {
                r#type: "text".into(),
                text,
                cache_control: cache.then(wire::CacheControl::ephemeral),
            }]));
        }
        let mut tools: Vec<_> = request
            .tools
            .into_iter()
            .map(|tool| wire::ToolParam {
                name: tool.name,
                description: tool.description,
                input_schema: Value::Object(tool.parameters),
                cache_control: None,
            })
            .collect();
        if points.contains(&CachePoint::AfterTools)
            && let Some(last) = tools.last_mut()
        {
            last.cache_control = Some(wire::CacheControl::ephemeral());
        }
        output.tools = (!tools.is_empty()).then_some(tools);
        Ok(output)
    }
}

fn image(image: Image) -> Result<wire::ContentBlock, Error> {
    let source = match image.source {
        ImageSource::Base64 { data } => {
            if !matches!(
                image.media_type.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ) || STANDARD.decode(&data).is_err()
            {
                return Err(invalid("unsupported image media type or invalid base64"));
            }
            wire::ImageSource::Base64 {
                media_type: image.media_type,
                data,
            }
        }
        ImageSource::Url { url } => {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(invalid("image URL must use HTTP(S)"));
            }
            wire::ImageSource::Url { url }
        }
    };
    Ok(wire::ContentBlock::Image {
        source,
        cache_control: None,
    })
}

fn message_blocks(
    message: Message,
    model: &LanguageModel,
) -> Result<Vec<wire::ContentBlock>, Error> {
    if !message.replay.is_empty() {
        if message.content.iter().any(|content| {
            matches!(
                content,
                Content::ToolCall(ToolCall {
                    arguments: ToolArguments::Invalid { .. },
                    ..
                })
            )
        }) {
            return Err(Error::Unsupported(
                "cannot replay malformed Anthropic tool arguments".into(),
            ));
        }
        if message.role != Role::Assistant || message.replay.len() != 1 {
            return Err(Error::Unsupported(
                "unsupported Anthropic replay data".into(),
            ));
        }
        let replay = &message.replay[0];
        if replay.provider != "anthropic"
            || replay.format != REPLAY_FORMAT
            || replay.version != 1
            || replay.model.as_deref() != Some(&model.id)
        {
            return Err(Error::Unsupported(
                "incompatible Anthropic replay format or model".into(),
            ));
        }
        let blocks: Vec<wire::ContentBlock> = serde_json::from_value(replay.payload.clone())
            .map_err(|_| invalid("invalid Anthropic replay payload"))?;
        let mut projected =
            project_content(&blocks).map_err(|_| invalid("invalid Anthropic replay content"))?;
        // Local IDs belong to the canonical message, not the native payload.
        for (projected, original) in projected.iter_mut().zip(&message.content) {
            if let (Content::ToolCall(call), Content::ToolCall(original)) = (projected, original) {
                call.id = original.id.clone();
            }
        }
        if projected != message.content {
            return Err(invalid(
                "message was edited without clearing its Anthropic replay data",
            ));
        }
        return Ok(blocks);
    }
    let mut blocks = Vec::new();
    for content in message.content {
        blocks.push(match content {
            Content::Text { text: value } => text(value),
            Content::Image(value) if message.role == Role::User => image(value)?,
            Content::Image(_) => {
                return Err(Error::Unsupported("Anthropic assistant images".into()));
            }
            Content::Reasoning(_) => {
                return Err(Error::Unsupported(
                    "Anthropic reasoning requires its signed native replay data".into(),
                ));
            }
            Content::ToolCall(call) => wire::ContentBlock::ToolUse {
                id: call
                    .provider_id
                    .ok_or_else(|| invalid("unresolved tool call ID"))?,
                name: call.name,
                input: match call.arguments {
                    ToolArguments::Object { value } => Value::Object(value),
                    ToolArguments::Invalid { .. } => {
                        return Err(Error::Unsupported(
                            "cannot replay malformed Anthropic tool arguments".into(),
                        ));
                    }
                },
                cache_control: None,
            },
            Content::ToolResult(result) => {
                let mut content = Vec::new();
                for part in result.content {
                    content.push(match part {
                        ToolResultContent::Text { text: value } => text(value),
                        ToolResultContent::Json { value } => text(value.to_string()),
                        ToolResultContent::Image(value) => image(value)?,
                    });
                }
                wire::ContentBlock::ToolResult {
                    tool_use_id: result
                        .provider_id
                        .ok_or_else(|| invalid("unresolved tool result ID"))?,
                    content: wire::ToolResultContent::Blocks(content),
                    is_error: Some(result.is_error),
                    cache_control: None,
                }
            }
        });
    }
    Ok(blocks)
}

/// Projects accepted native assistant blocks without losing their order.
/// Signatures stay in replay data; malformed arguments remain observations.
pub fn project_content(blocks: &[wire::ContentBlock]) -> Result<Vec<Content>, Error> {
    let mut ids = HashSet::new();
    blocks
        .iter()
        .map(|block| {
            Ok(match block {
                wire::ContentBlock::Text {
                    text, citations, ..
                } => {
                    if citations.as_ref().is_some_and(|value| {
                        value.as_array().is_none_or(|values| !values.is_empty())
                    }) {
                        return Err(Error::Unsupported("Anthropic citation content".into()));
                    }
                    Content::text(text.clone())
                }
                wire::ContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    if signature.is_empty() {
                        return Err(Error::InvalidResponse(
                            "thinking block lacks signature".into(),
                        ));
                    }
                    Content::Reasoning(Reasoning::Text {
                        text: thinking.clone(),
                    })
                }
                wire::ContentBlock::RedactedThinking { data } => {
                    Content::Reasoning(Reasoning::Redacted { data: data.clone() })
                }
                wire::ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    if id.is_empty() || name.is_empty() || !ids.insert(id) {
                        return Err(Error::InvalidResponse(
                            "empty or duplicate tool identity".into(),
                        ));
                    }
                    Content::ToolCall(ToolCall {
                        id: ToolCallId::new(),
                        provider_id: Some(id.clone()),
                        name: name.clone(),
                        arguments: ToolArguments::from_raw(input.to_string()),
                    })
                }
                _ => {
                    return Err(Error::Unsupported(
                        "unsupported Anthropic assistant block".into(),
                    ));
                }
            })
        })
        .collect()
}

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
