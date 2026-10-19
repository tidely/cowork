use std::collections::VecDeque;

use futures::{StreamExt as _, TryStreamExt as _, stream::BoxStream};
use language_model::{
    Completion, CompletionStream, Content, ContentDelta, Error, GenerationMetadata, JsonObject,
    Message, Reasoning, Role, StreamEvent, ToolCall, ToolCallId, Usage,
};
use ollama_types as native;
use serde_json::Value;
use tokio_util::{
    codec::{FramedRead, LinesCodec, LinesCodecError},
    io::StreamReader,
};

use crate::convert::{argument_text, arguments, response_error};

pub(crate) fn response_stream(
    response: reqwest::Response,
    context: crate::ErrorContext,
) -> CompletionStream {
    let reader = StreamReader::new(
        response
            .bytes_stream()
            .map_err(|error| std::io::Error::other(error.without_url())),
    );
    let state = State {
        lines: Some(
            FramedRead::new(reader, LinesCodec::new_with_max_length(8 * 1024 * 1024))
                .map_err(framing_error)
                .boxed(),
        ),
        stopped: false,
        queue: VecDeque::new(),
        fold: Fold::default(),
        context,
    };
    futures::stream::unfold(state, |mut state| async move {
        let item = state.next().await?;
        Some((item, state))
    })
    .boxed()
}

struct State {
    lines: Option<BoxStream<'static, Result<String, Error>>>,
    stopped: bool,
    queue: VecDeque<StreamEvent>,
    fold: Fold,
    context: crate::ErrorContext,
}

impl State {
    async fn next(&mut self) -> Option<Result<StreamEvent, Error>> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(Ok(event));
            }
            if self.stopped {
                return None;
            }
            let result = self.read().await;
            if let Err(error) = result {
                self.stopped = true;
                self.lines = None;
                return Some(Err(error));
            }
        }
    }

    async fn read(&mut self) -> Result<(), Error> {
        let Some(line) = self.line().await? else {
            return Err(Error::IncompleteStream);
        };
        let item = serde_json::from_str::<native::ChatStreamItem>(&line).map_err(|error| {
            response_error(&format!(
                "invalid Ollama NDJSON at line {}, column {}",
                error.line(),
                error.column()
            ))
        })?;
        match item {
            native::ChatStreamItem::Error(error) => Err(self.context.provider(&error.error)),
            native::ChatStreamItem::Response(response) => {
                let done = response.done;
                self.queue = self.fold.apply(*response)?;
                if done {
                    self.stopped = true;
                    self.lines = None;
                }
                Ok(())
            }
        }
    }

    async fn line(&mut self) -> Result<Option<String>, Error> {
        let lines = self.lines.as_mut().expect("response initialized");
        while let Some(line) = lines.next().await {
            let line = line?;
            if !line.as_bytes().iter().all(u8::is_ascii_whitespace) {
                return Ok(Some(line));
            }
        }
        Ok(None)
    }
}

fn framing_error(error: LinesCodecError) -> Error {
    match error {
        LinesCodecError::MaxLineLengthExceeded => {
            response_error("Ollama NDJSON line exceeds 8 MiB")
        }
        LinesCodecError::Io(error) => {
            if error
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<reqwest::Error>())
                .is_some_and(reqwest::Error::is_timeout)
            {
                Error::Timeout
            } else if error.kind() == std::io::ErrorKind::InvalidData {
                response_error("invalid UTF-8 in Ollama NDJSON")
            } else {
                Error::Transport(crate::error_chain(&error))
            }
        }
    }
}

pub(crate) struct Fold {
    message: Message,
    calls: Vec<native::ToolCall>,
    seen_message: bool,
    metadata: JsonObject,
    native_messages: Vec<Value>,
}

impl Default for Fold {
    fn default() -> Self {
        Self {
            message: Message::new(Role::Assistant, Vec::new()),
            calls: Vec::new(),
            seen_message: false,
            metadata: JsonObject::new(),
            native_messages: Vec::new(),
        }
    }
}

impl Fold {
    pub(crate) fn apply(
        &mut self,
        mut response: native::ChatResponse,
    ) -> Result<VecDeque<StreamEvent>, Error> {
        let mut events = VecDeque::new();
        // Preserve all native response diagnostics, including future counters,
        // without making them fields of the provider-neutral message schema.
        let message = response.message.take();
        let Value::Object(metadata) = serde_json::to_value(&response)
            .map_err(|_| response_error("invalid Ollama response metadata"))?
        else {
            unreachable!("native response is an object");
        };
        self.metadata.extend(metadata);
        if let Some(message) = message {
            if message.role != "assistant" {
                return Err(response_error("Ollama returned a non-assistant message"));
            }
            if message
                .images
                .as_ref()
                .is_some_and(|images| !images.is_empty())
            {
                return Err(Error::Unsupported(
                    "native Ollama assistant images have no declared media type".into(),
                ));
            }
            if message.tool_name.is_some() || message.tool_call_id.is_some() {
                return Err(response_error("unexpected assistant tool result fields"));
            }
            self.seen_message = true;
            if !message.extra.is_empty() {
                self.native_messages.push(Value::Object(message.extra));
            }
            if let Some(thinking) = message.thinking.filter(|text| !text.is_empty()) {
                let index = match self.message.content.last_mut() {
                    Some(Content::Reasoning(Reasoning::Text { text })) => {
                        text.push_str(&thinking);
                        self.message.content.len() - 1
                    }
                    _ => {
                        let index = self.message.content.len();
                        self.message
                            .content
                            .push(Content::Reasoning(Reasoning::Text {
                                text: thinking.clone(),
                            }));
                        index
                    }
                };
                events.push_back(StreamEvent::Delta {
                    index,
                    delta: ContentDelta::Reasoning(thinking),
                });
            }
            if !message.content.is_empty() {
                let index = match self.message.content.last_mut() {
                    Some(Content::Text { text }) => {
                        text.push_str(&message.content);
                        self.message.content.len() - 1
                    }
                    _ => {
                        let index = self.message.content.len();
                        self.message
                            .content
                            .push(Content::text(message.content.clone()));
                        index
                    }
                };
                events.push_back(StreamEvent::Delta {
                    index,
                    delta: ContentDelta::Text(message.content),
                });
            }
            for call in message.tool_calls.unwrap_or_default() {
                self.call(call, &mut events)?;
            }
        }
        if response.done {
            if !self.seen_message {
                return Err(response_error(
                    "Ollama completed without an assistant message",
                ));
            }
            for content in &self.message.content {
                if let Content::ToolCall(call) = content
                    && call.name.trim().is_empty()
                {
                    return Err(response_error("Ollama completed an unnamed tool call"));
                }
            }
            let usage = Usage {
                input_tokens: self
                    .metadata
                    .get("prompt_eval_count")
                    .and_then(Value::as_u64),
                output_tokens: self.metadata.get("eval_count").and_then(Value::as_u64),
                total_tokens: self.metadata.get("total_tokens").and_then(Value::as_u64),
                cached_input_tokens: self
                    .metadata
                    .get("prompt_eval_cached_count")
                    .and_then(Value::as_u64),
                reasoning_tokens: self
                    .metadata
                    .get("reasoning_tokens")
                    .and_then(Value::as_u64),
            };
            let mut extra = self.metadata.clone();
            if !self.native_messages.is_empty() {
                extra.insert(
                    "message_extras".into(),
                    Value::Array(self.native_messages.clone()),
                );
            }
            if !self.calls.is_empty() {
                extra.insert(
                    "native_tool_calls".into(),
                    serde_json::to_value(&self.calls)
                        .map_err(|_| response_error("invalid tool metadata"))?,
                );
            }
            self.message.generation = Some(GenerationMetadata {
                provider: "ollama".into(),
                model: self
                    .metadata
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                finish_reason: self
                    .metadata
                    .get("done_reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                usage: (usage != Usage::default()).then_some(usage),
                extra,
            });
            self.message
                .validate()
                .map_err(|_| response_error("invalid finalized Ollama message"))?;
            events.push_back(StreamEvent::Finished(Box::new(Completion {
                message: self.message.clone(),
            })));
        }
        Ok(events)
    }

    fn call(
        &mut self,
        mut call: native::ToolCall,
        events: &mut VecDeque<StreamEvent>,
    ) -> Result<(), Error> {
        if call.id.as_ref().is_some_and(|id| id.is_empty()) {
            call.id = None;
        }
        if call
            .id
            .as_ref()
            .is_some_and(|id| self.calls.iter().any(|old| old.id.as_ref() == Some(id)))
        {
            return Err(response_error("duplicate Ollama provider tool ID"));
        }
        // Native tools are complete call events, not OpenAI argument fragments.
        // Append each occurrence; identical id-less calls can be distinct, and
        // older servers can reuse function.index=0. Preserve native indexes as
        // diagnostics rather than treating them as guaranteed identities.
        let finalized = ToolCall {
            id: ToolCallId::new(),
            provider_id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: arguments(call.function.arguments.clone()),
        };
        events.push_back(StreamEvent::Delta {
            index: self.message.content.len(),
            delta: ContentDelta::ToolCall {
                id: finalized.id.clone(),
                name: (!finalized.name.is_empty()).then(|| finalized.name.clone()),
                arguments: argument_text(&finalized.arguments),
            },
        });
        self.message.content.push(Content::ToolCall(finalized));
        self.calls.push(call);
        Ok(())
    }
}
