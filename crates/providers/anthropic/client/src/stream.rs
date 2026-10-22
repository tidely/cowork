use crate::{Client, transport};
use anthropic_types as wire;
use futures::{StreamExt as _, stream};
use language_model::{
    Completion, CompletionStream, Content, ContentDelta, Error, GenerationMetadata, Message,
    ReplayData, Role, StreamEvent, ToolArguments, ToolCallId, Usage,
};
use provider_sse::Decoder;
use serde_json::{Value, json};
use std::collections::VecDeque;

struct Block {
    native: wire::ContentBlock,
    call_id: Option<ToolCallId>,
    arguments: Option<String>,
    signature_delta: bool,
    stopped: bool,
}

#[derive(Default)]
struct Assembly {
    request_model: String,
    message: Option<wire::MessagesResponse>,
    blocks: Vec<Block>,
    active: Option<usize>,
    stop_reason_received: bool,
}

fn invalid(message: &str) -> Error {
    Error::InvalidResponse(message.into())
}

impl Assembly {
    fn event(
        &mut self,
        event: wire::MessageStreamEvent,
        events: &mut VecDeque<StreamEvent>,
    ) -> Result<Option<Completion>, Error> {
        use wire::{ContentBlock as B, MessageStreamEvent as E, StreamDelta as D};
        if matches!(event, E::Ping) {
            return Ok(None);
        }
        if let E::MessageStart { message } = event {
            if self.message.is_some()
                || message.role != "assistant"
                || message.r#type != "message"
                || message.id.is_empty()
                || message.model.is_empty()
                || !message.content.is_empty()
                || message.stop_reason.is_some()
            {
                return Err(invalid("invalid or repeated message_start"));
            }
            self.message = Some(message);
            return Ok(None);
        }
        let message = self
            .message
            .as_mut()
            .ok_or_else(|| invalid("event before message_start"))?;
        match event {
            E::ContentBlockStart {
                index,
                content_block,
            } => {
                if self.stop_reason_received
                    || self.active.is_some()
                    || index as usize != self.blocks.len()
                {
                    return Err(invalid("out-of-order content_block_start"));
                }
                let index = index as usize;
                let call_id = match &content_block {
                    B::Text {
                        text, citations, ..
                    } => {
                        if citations.as_ref().is_some_and(|value| {
                            value.as_array().is_none_or(|values| !values.is_empty())
                        }) {
                            return Err(Error::Unsupported("Anthropic citations".into()));
                        }
                        if !text.is_empty() {
                            events.push_back(StreamEvent::Delta {
                                index,
                                delta: ContentDelta::Text(text.clone()),
                            });
                        }
                        None
                    }
                    B::Thinking { thinking, .. } => {
                        if !thinking.is_empty() {
                            events.push_back(StreamEvent::Delta {
                                index,
                                delta: ContentDelta::Reasoning(thinking.clone()),
                            });
                        }
                        None
                    }
                    B::RedactedThinking { .. } => None,
                    B::ToolUse { id, name, .. } => {
                        if id.is_empty() || name.is_empty() {
                            return Err(invalid("tool block needs an ID and name"));
                        }
                        let id = ToolCallId::new();
                        events.push_back(StreamEvent::Delta {
                            index,
                            delta: ContentDelta::ToolCall {
                                id: id.clone(),
                                name: Some(name.clone()),
                                arguments: String::new(),
                            },
                        });
                        Some(id)
                    }
                    _ => {
                        return Err(Error::Unsupported(
                            "unsupported Anthropic output block".into(),
                        ));
                    }
                };
                self.blocks.push(Block {
                    native: content_block,
                    call_id,
                    arguments: None,
                    signature_delta: false,
                    stopped: false,
                });
                self.active = Some(index);
            }
            E::ContentBlockDelta { index, delta } => {
                if self.stop_reason_received || self.active != Some(index as usize) {
                    return Err(invalid("delta outside its active block"));
                }
                let block = &mut self.blocks[index as usize];
                let delta = match (&mut block.native, delta) {
                    (B::Text { text, .. }, D::TextDelta { text: delta }) => {
                        text.push_str(&delta);
                        Some(ContentDelta::Text(delta))
                    }
                    (B::Thinking { thinking, .. }, D::ThinkingDelta { thinking: delta }) => {
                        thinking.push_str(&delta);
                        Some(ContentDelta::Reasoning(delta))
                    }
                    (B::Thinking { signature, .. }, D::SignatureDelta { signature: delta }) => {
                        if !block.signature_delta {
                            signature.clear();
                            block.signature_delta = true;
                        }
                        signature.push_str(&delta);
                        None
                    }
                    (B::ToolUse { input, .. }, D::InputJsonDelta { partial_json }) => {
                        // The start's empty object is a placeholder. Nonempty
                        // initial input plus deltas has no unambiguous merge.
                        if block.arguments.is_none()
                            && input.as_object().is_none_or(|value| !value.is_empty())
                        {
                            return Err(invalid(
                                "tool input has both initial data and JSON deltas",
                            ));
                        }
                        block
                            .arguments
                            .get_or_insert_default()
                            .push_str(&partial_json);
                        Some(ContentDelta::ToolCall {
                            id: block.call_id.clone().expect("tool ID"),
                            name: None,
                            arguments: partial_json,
                        })
                    }
                    _ => return Err(invalid("delta does not match its block type")),
                };
                if let Some(delta) = delta {
                    events.push_back(StreamEvent::Delta {
                        index: index as usize,
                        delta,
                    });
                }
            }
            E::ContentBlockStop { index } => {
                if self.stop_reason_received || self.active != Some(index as usize) {
                    return Err(invalid("stop outside its active block"));
                }
                let block = &mut self.blocks[index as usize];
                if let (B::ToolUse { input, .. }, Some(raw)) = (&mut block.native, &block.arguments)
                {
                    *input =
                        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()));
                }
                block.stopped = true;
                self.active = None;
            }
            E::MessageDelta { delta, usage } => {
                if self.active.is_some()
                    || (self.stop_reason_received && delta.stop_reason.is_some())
                {
                    return Err(invalid(
                        "message_delta before block stop or repeated stop reason",
                    ));
                }
                if let Some(reason) = delta.stop_reason {
                    if reason.wire_str().is_empty() {
                        return Err(invalid("empty stop reason"));
                    }
                    message.stop_reason = Some(reason);
                    self.stop_reason_received = true;
                }
                if let Some(sequence) = delta.stop_sequence {
                    message
                        .extra
                        .insert("stop_sequence".into(), sequence.into());
                }
                if let Some(details) = delta.stop_details {
                    message.extra.insert("stop_details".into(), json!(details));
                }
                message.extra.extend(delta.extra);
                if usage.input_tokens.is_some() {
                    message.usage.input_tokens = usage.input_tokens;
                }
                if usage.output_tokens.is_some() {
                    message.usage.output_tokens = usage.output_tokens;
                }
                if usage.cache_read_input_tokens.is_some() {
                    message.usage.cache_read_input_tokens = usage.cache_read_input_tokens;
                }
                if usage.cache_creation_input_tokens.is_some() {
                    message.usage.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                }
                message.usage.extra.extend(usage.extra);
            }
            E::MessageStop => {
                if !self.stop_reason_received || self.active.is_some() {
                    return Err(Error::IncompleteStream);
                }
                return self.finish().map(Some);
            }
            E::Error { .. } => return Err(invalid("unhandled stream error")), // handled with redaction by State
            E::MessageStart { .. } | E::Ping => unreachable!(),
        }
        Ok(None)
    }

    fn finish(&mut self) -> Result<Completion, Error> {
        let native = self.message.take().ok_or(Error::IncompleteStream)?;
        if self.blocks.iter().any(|block| !block.stopped) {
            return Err(Error::IncompleteStream);
        }
        let blocks: Vec<_> = self
            .blocks
            .iter()
            .map(|block| block.native.clone())
            .collect();
        let mut content = wire::project_content(&blocks)?;
        for (content, block) in content.iter_mut().zip(&self.blocks) {
            if let Content::ToolCall(call) = content {
                call.id = block.call_id.clone().expect("tool ID");
                if let Some(raw) = &block.arguments {
                    call.arguments = ToolArguments::from_raw(raw.clone());
                }
            }
        }
        let mut message = Message::new(Role::Assistant, content);
        if blocks.iter().any(|block| {
            matches!(
                block,
                wire::ContentBlock::Thinking { .. } | wire::ContentBlock::RedactedThinking { .. }
            )
        }) {
            message.replay.push(ReplayData {
                provider: "anthropic".into(),
                model: Some(self.request_model.clone()),
                format: wire::REPLAY_FORMAT.into(),
                version: 1,
                payload: json!(blocks),
            });
        }
        let mut extra = native.extra;
        extra.insert("id".into(), native.id.into());
        extra.insert("usage".into(), json!(native.usage));
        // Anthropic's base input counter excludes cache reads and creations.
        // The shared input count includes them; preserve the original counters.
        let input_tokens = native
            .usage
            .input_tokens
            .map(|base| {
                base.checked_add(native.usage.cache_read_input_tokens.unwrap_or(0))
                    .and_then(|sum| {
                        sum.checked_add(native.usage.cache_creation_input_tokens.unwrap_or(0))
                    })
                    .ok_or_else(|| invalid("input token count overflow"))
            })
            .transpose()?;
        let total_tokens = match (input_tokens, native.usage.output_tokens) {
            (Some(input), Some(output)) => Some(
                input
                    .checked_add(output)
                    .ok_or_else(|| invalid("total token count overflow"))?,
            ),
            _ => None,
        };
        message.generation = Some(GenerationMetadata {
            provider: "anthropic".into(),
            model: Some(native.model),
            finish_reason: native.stop_reason.map(|reason| reason.finish_reason()),
            usage: Some(Usage {
                input_tokens,
                output_tokens: native.usage.output_tokens,
                total_tokens,
                cached_input_tokens: native.usage.cache_read_input_tokens,
                reasoning_tokens: None,
            }),
            extra,
        });
        message
            .validate()
            .map_err(|_| invalid("invalid assistant message"))?;
        Ok(Completion { message })
    }
}

struct State {
    client: Client,
    response: reqwest::Response,
    decoder: Decoder,
    assembly: Assembly,
    events: VecDeque<StreamEvent>,
    ended: bool,
}

impl State {
    async fn next(&mut self) -> Result<StreamEvent, Error> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(event);
            }
            if let Some(frame) = self.decoder.next_frame() {
                let event: wire::MessageStreamEvent = serde_json::from_slice(&frame)
                    .map_err(|_| invalid("invalid Messages stream event"))?;
                if let wire::MessageStreamEvent::Error { error } = event {
                    return Err(self.client.provider_error(&json!(error)));
                }
                if let Some(completion) = self.assembly.event(event, &mut self.events)? {
                    self.ended = true;
                    return Ok(StreamEvent::Finished(Box::new(completion)));
                }
                continue;
            }
            let bytes = self
                .response
                .chunk()
                .await
                .map_err(transport)?
                .ok_or(Error::IncompleteStream)?;
            self.decoder.feed(&bytes);
        }
    }
}
pub(crate) fn completion_stream(
    client: Client,
    response: reqwest::Response,
    request_model: String,
) -> CompletionStream {
    let state = State {
        client,
        response,
        decoder: Decoder::default(),
        assembly: Assembly {
            request_model,
            ..Default::default()
        },
        events: VecDeque::new(),
        ended: false,
    };
    stream::unfold(state, |mut state| async move {
        if state.ended {
            return None;
        }
        let event = state.next().await;
        if event.is_err() {
            state.ended = true;
        }
        Some((event, state))
    })
    .boxed()
}
