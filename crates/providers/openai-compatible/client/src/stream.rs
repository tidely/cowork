use crate::{Client, response, transport};
use futures::{StreamExt as _, stream};
use language_model::{
    Completion, CompletionStream, Content, ContentDelta, Error, GenerationMetadata, Message,
    Reasoning, Role, StreamEvent, ToolArguments, ToolCall, ToolCallId,
};
use openai_compatible_types as wire;
use provider_sse::Decoder;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, VecDeque};

struct PendingCall {
    id: ToolCallId,
    provider_id: Option<String>,
    name: String,
    arguments: String,
    block: usize,
}
enum Block {
    Text(String),
    Reasoning(String),
    Call(u32),
}

#[derive(Default)]
struct Assembly {
    blocks: Vec<Block>,
    calls: BTreeMap<u32, PendingCall>,
    text: Option<usize>,
    reasoning: Option<usize>,
    tool_start: Option<usize>,
    finish: Option<String>,
    model: Option<String>,
    usage: Option<wire::Usage>,
    extra: Map<String, Value>,
}

impl Assembly {
    fn append(&mut self, text: String, reasoning: bool, events: &mut VecDeque<StreamEvent>) {
        if text.is_empty() {
            return;
        }
        let slot = if reasoning {
            &mut self.reasoning
        } else {
            &mut self.text
        };
        let index = *slot.get_or_insert_with(|| {
            self.blocks.push(if reasoning {
                Block::Reasoning(String::new())
            } else {
                Block::Text(String::new())
            });
            self.blocks.len() - 1
        });
        match &mut self.blocks[index] {
            Block::Text(value) | Block::Reasoning(value) => value.push_str(&text),
            _ => unreachable!(),
        }
        events.push_back(StreamEvent::Delta {
            index,
            delta: if reasoning {
                ContentDelta::Reasoning(text)
            } else {
                ContentDelta::Text(text)
            },
        });
    }

    fn chunk(
        &mut self,
        chunk: wire::ChatCompletionChunk,
        events: &mut VecDeque<StreamEvent>,
    ) -> Result<(), Error> {
        if let Some(model) = chunk.model {
            if self.model.as_ref().is_some_and(|old| old != &model) {
                return Err(Error::InvalidResponse("model changed during stream".into()));
            }
            self.model = Some(model);
        }
        if let Some(value) = chunk.id {
            self.extra.insert("id".into(), value.into());
        }
        if let Some(value) = chunk.created {
            self.extra.insert("created".into(), value.into());
        }
        if let Some(value) = chunk.object {
            self.extra.insert("object".into(), value.into());
        }
        self.extra.extend(chunk.extra);
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }
        if chunk.choices.len() > 1 {
            return Err(Error::Unsupported("multiple completion choices".into()));
        }
        for choice in chunk.choices {
            if choice.index != 0 {
                return Err(Error::Unsupported(
                    "only choice index 0 is supported".into(),
                ));
            }
            if !choice.extra.is_empty() {
                self.extra.insert("choice".into(), choice.extra.into());
            }
            if let Some(delta) = choice.delta {
                response::reject_unsupported(&delta.extra)?;
                if delta.refusal.is_some() {
                    return Err(Error::Unsupported(
                        "refusal content is not represented by the generic schema".into(),
                    ));
                }
                if delta
                    .role
                    .as_deref()
                    .is_some_and(|role| role != "assistant")
                {
                    return Err(Error::InvalidResponse("expected assistant delta".into()));
                }
                let reasoning = response::reasoning(delta.reasoning, delta.reasoning_content)?;
                if self.reasoning.is_none()
                    && self.text.is_some()
                    && reasoning.as_ref().is_some_and(|text| !text.is_empty())
                {
                    return Err(Error::Unsupported(
                        "first reasoning block after text cannot preserve indexed order".into(),
                    ));
                }
                let has_content = delta.content.as_ref().is_some_and(|s| !s.is_empty())
                    || reasoning.as_ref().is_some_and(|s| !s.is_empty())
                    || delta
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty());
                if self.finish.is_some() && has_content {
                    return Err(Error::InvalidResponse("content after finish reason".into()));
                }
                if !delta.extra.is_empty() {
                    self.extra.insert("message".into(), delta.extra.into());
                }
                if self.tool_start.is_some()
                    && ((self.reasoning.is_none()
                        && reasoning.as_ref().is_some_and(|s| !s.is_empty()))
                        || (self.text.is_none()
                            && delta.content.as_ref().is_some_and(|s| !s.is_empty())))
                {
                    return Err(Error::Unsupported(
                        "new content block after tool calls cannot preserve indexed order".into(),
                    ));
                }
                if let Some(text) = reasoning {
                    self.append(text, true, events);
                }
                if let Some(text) = delta.content {
                    self.append(text, false, events);
                }
                for call in delta.tool_calls.unwrap_or_default() {
                    if call.kind.as_deref().is_some_and(|kind| kind != "function") {
                        return Err(Error::Unsupported("non-function tool call".into()));
                    }
                    // Reserve by wire index, including out-of-order arrivals.
                    // Preview indexes then match the authoritative message.

                    let start = *self.tool_start.get_or_insert(self.blocks.len());
                    let block = start + call.index as usize;
                    while self.blocks.len() <= block {
                        self.blocks
                            .push(Block::Call((self.blocks.len() - start) as u32));
                    }
                    let pending = self.calls.entry(call.index).or_insert_with(|| PendingCall {
                        id: ToolCallId::new(),
                        provider_id: None,
                        name: String::new(),
                        arguments: String::new(),
                        block,
                    });
                    if let Some(id) = call.id {
                        if id.is_empty()
                            || pending.provider_id.as_ref().is_some_and(|old| old != &id)
                        {
                            return Err(Error::InvalidResponse(
                                "empty or changed tool call ID".into(),
                            ));
                        }
                        pending.provider_id = Some(id);
                    }
                    let function = call.function.unwrap_or_default();
                    if let Some(name) = &function.name {
                        pending.name.push_str(name);
                    }
                    let arguments = function.arguments.unwrap_or_default();
                    pending.arguments.push_str(&arguments);
                    events.push_back(StreamEvent::Delta {
                        index: pending.block,
                        delta: ContentDelta::ToolCall {
                            id: pending.id.clone(),
                            name: function.name,
                            arguments,
                        },
                    });
                }
            }
            if let Some(finish) = choice.finish_reason {
                if finish.is_empty() || self.finish.is_some() {
                    return Err(Error::InvalidResponse(
                        "empty or repeated finish reason".into(),
                    ));
                }
                self.finish = Some(finish);
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Completion, Error> {
        let finish_reason = self.finish.ok_or(Error::IncompleteStream)?;
        let mut content = Vec::new();
        let mut provider_ids = std::collections::HashSet::new();
        for block in self.blocks {
            content.push(match block {
                Block::Text(text) => Content::text(text),
                Block::Reasoning(text) => Content::Reasoning(Reasoning::Text { text }),
                Block::Call(index) => {
                    let call = self
                        .calls
                        .remove(&index)
                        .ok_or_else(|| Error::InvalidResponse("missing tool call index".into()))?;
                    if call
                        .provider_id
                        .as_ref()
                        .is_some_and(|id| !provider_ids.insert(id.clone()))
                    {
                        return Err(Error::InvalidResponse(
                            "duplicate tool call provider ID".into(),
                        ));
                    }
                    if call.name.is_empty() {
                        return Err(Error::InvalidResponse(
                            "tool call lacks function name".into(),
                        ));
                    }
                    Content::ToolCall(ToolCall {
                        id: call.id,
                        provider_id: call.provider_id,
                        name: call.name,
                        arguments: ToolArguments::from_raw(call.arguments),
                    })
                }
            });
        }
        if let Some(usage) = &self.usage {
            self.extra.insert(
                "usage".into(),
                serde_json::to_value(usage).expect("wire usage serializes"),
            );
        }
        let mut message = Message::new(Role::Assistant, content);
        message.generation = Some(GenerationMetadata {
            provider: "openai-compatible".into(),
            model: self.model,
            finish_reason: Some(openai_compatible_types::finish_reason(&finish_reason)),
            usage: self.usage.map(response::usage),
            extra: self.extra,
        });
        message
            .validate()
            .map_err(|_| Error::InvalidResponse("invalid assistant content".into()))?;
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
                if frame == b"[DONE]" {
                    self.ended = true;
                    return Ok(StreamEvent::Finished(Box::new(
                        std::mem::take(&mut self.assembly).finish()?,
                    )));
                }
                let response: wire::ApiResponse<wire::ChatCompletionChunk> =
                    serde_json::from_slice(&frame)
                        .map_err(|_| Error::InvalidResponse("invalid completion chunk".into()))?;
                let chunk = match response {
                    wire::ApiResponse::Success(chunk) => chunk,
                    wire::ApiResponse::Error { error } => {
                        return Err(self.client.provider_error(&error));
                    }
                };
                self.assembly.chunk(chunk, &mut self.events)?;
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

pub(crate) fn completion_stream(client: Client, response: reqwest::Response) -> CompletionStream {
    let state = State {
        client,
        response,
        decoder: Decoder::default(),
        assembly: Assembly::default(),
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
