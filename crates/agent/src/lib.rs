//! A small agent loop built directly on Rig's low-level completion stream.
//!
//! Unlike Rig's multi-turn agent stream, this crate surfaces everything as it
//! streams, including a completed tool call as soon as the provider closes
//! that call's block. Tool execution still waits for the complete model turn,
//! keeping the assistant message and its tool results in a valid history
//! order.
//!
//! A run is fully described by the [`AgentEvent`]s it emits: folding them
//! with a [`TurnFold`] rebuilds exactly the history the loop records, because
//! the loop records it by folding them itself. Events are serializable, so
//! they can be sent elsewhere and folded there with the same result.

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use rig::{
    completion::{AssistantContent, CompletionModel, Message, Usage},
    message::{ToolCall, ToolCallId, UserContent},
    streaming::{BlockAccumulator, StreamEvent},
    tool::{ToolContext, ToolResult, ToolSet},
};
use serde::{Deserialize, Serialize};

/// An observation produced while an agent run is in progress.
// Nearly every event is a `Model` event, so boxing it would only add an
// allocation per streamed fragment.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentEvent {
    /// A normalized provider event, as Rig's stream yields it: text and
    /// reasoning deltas, block boundaries (the end of a tool call's block
    /// completes the call), and the terminal record.
    Model(StreamEvent),
    /// A model turn ended cleanly, so its reply joins the history. Carries
    /// what the stream reported besides its content: the reply's id, and
    /// the tokens the request used, which sum to the run's usage even when
    /// a later request fails or is cancelled.
    TurnEnded {
        message_id: Option<String>,
        usage: Usage,
    },
    /// A tool the last reply called returned. The call itself is in the
    /// reply.
    ToolResult {
        call: ToolCallId,
        result: ToolResult,
    },
}

/// Folds a run's events into the messages they add to the history.
///
/// Each completed model turn adds its reply, and once every tool that reply
/// called has returned, their results add the prompt of the next request.
/// The initial prompt is the caller's, and never comes out of a fold.
#[derive(Default)]
pub struct TurnFold {
    /// The reply being streamed.
    reply: BlockAccumulator,
    /// The calls of the last reply that have not returned yet.
    calls: Vec<ToolCall>,
    /// The results of those that have.
    results: Vec<UserContent>,
}

/// What folding one event produced.
#[derive(Default)]
pub struct Folded {
    /// A message it completed, for the history.
    pub message: Option<Message>,
    /// A block of the reply it completed, such as a tool call.
    pub block: Option<AssistantContent>,
}

impl TurnFold {
    /// A fold resuming after `reply`, the last message of the history,
    /// waiting for the results of the tools it called.
    pub fn after(reply: &Message) -> Self {
        let calls = match reply {
            Message::Assistant { content, .. } => tool_calls(content),
            Message::User { .. } | Message::System { .. } => Vec::new(),
        };
        Self {
            calls,
            ..Self::default()
        }
    }

    /// The calls of the last reply still waiting for their results.
    pub fn pending_calls(&self) -> &[ToolCall] {
        &self.calls
    }

    /// The message being folded, as far as it has come: the reply streaming,
    /// as Rig accumulates it, or once it has ended, the results of the tools
    /// that have returned. `None` while there is neither.
    ///
    /// Folding the rest of its events completes exactly this message, with
    /// more content, so the history followed by this is everything the run
    /// has produced so far.
    pub fn partial(&self) -> Option<Message> {
        if !self.results.is_empty() {
            return Some(Message::User {
                content: self.results.clone(),
            });
        }
        let content = self.reply.snapshot();
        (!content.is_empty()).then_some(Message::Assistant { id: None, content })
    }

    pub fn apply(&mut self, event: &AgentEvent) -> Folded {
        match event {
            AgentEvent::Model(event) => match self.reply.apply(event) {
                Ok(block) => Folded {
                    message: None,
                    block: block.map(|(_, block)| block),
                },
                // Rig's stream already reported it, which ended the run.
                Err(_) => Folded::default(),
            },
            AgentEvent::TurnEnded { message_id, .. } => {
                let content = self.reply.finish();
                self.calls = tool_calls(&content);
                self.results.clear();
                Folded {
                    message: Some(Message::Assistant {
                        id: message_id.clone(),
                        content,
                    }),
                    block: None,
                }
            }
            AgentEvent::ToolResult { call, result } => {
                let Some(call) = self.calls.iter().find(|pending| pending.id == *call) else {
                    return Folded::default();
                };
                self.results.push(UserContent::tool_result_for(
                    call.id.clone(),
                    call.provider.clone(),
                    call.function.name.clone(),
                    result.output().clone().into_content(),
                ));
                if self.results.len() < self.calls.len() {
                    return Folded::default();
                }
                self.calls.clear();
                Folded {
                    message: Some(Message::User {
                        content: std::mem::take(&mut self.results),
                    }),
                    block: None,
                }
            }
        }
    }
}

fn tool_calls(content: &[AssistantContent]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// A provider-independent, streaming tool loop.
pub struct Agent<M> {
    model: M,
    tools: ToolSet,
    preamble: Option<String>,
    additional_params: Option<serde_json::Value>,
}

impl<M> Agent<M> {
    pub fn new(model: M, tools: ToolSet) -> Self {
        Self {
            model,
            tools,
            preamble: None,
            additional_params: None,
        }
    }

    pub fn preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = Some(preamble.into());
        self
    }

    pub fn additional_params(mut self, params: serde_json::Value) -> Self {
        self.additional_params = Some(params);
        self
    }
}

impl<M> Agent<M>
where
    M: CompletionModel + Clone,
{
    /// Runs until the model produces a turn with no tool calls.
    ///
    /// `emit` is called synchronously as stream events arrive. It should do
    /// little work itself; forwarding events to a channel is a good default.
    ///
    /// `history` gains `prompt` and then exactly the messages a [`TurnFold`]
    /// folds out of the emitted events. On failure, it ends with the prompt
    /// of the request that failed.
    pub async fn run(
        &self,
        mut prompt: Message,
        history: &mut Vec<Message>,
        mut emit: impl FnMut(AgentEvent),
    ) -> Result<()> {
        let mut fold = TurnFold::default();
        loop {
            let mut request = self
                .model
                .completion_request(prompt.clone())
                .messages(history.clone())
                .tools(self.tools.tool_definitions());

            if let Some(preamble) = &self.preamble {
                request = request.preamble(preamble.clone());
            }
            if let Some(params) = &self.additional_params {
                request = request.additional_params(params.clone());
            }
            history.push(prompt.clone());

            let mut stream = request
                .stream()
                .await
                .context("failed to start model stream")?;
            while let Some(event) = stream.next().await {
                let event = AgentEvent::Model(event.context("model stream failed")?);
                fold.apply(&event);
                emit(event);
            }
            let ended = AgentEvent::TurnEnded {
                message_id: stream.message_id.clone(),
                usage: stream.usage(),
            };
            let reply = fold.apply(&ended).message;
            emit(ended);
            history.extend(reply);

            let calls = fold.pending_calls().to_vec();
            if calls.is_empty() {
                return Ok(());
            }
            for call in calls {
                let arguments = serde_json::to_string(&call.function.arguments)
                    .context("failed to serialize tool arguments")?;
                let result = self
                    .tools
                    .execute(&call.function.name, arguments, &mut ToolContext::new())
                    .await;
                let event = AgentEvent::ToolResult {
                    call: call.id,
                    result,
                };
                if let Some(results) = fold.apply(&event).message {
                    prompt = results;
                }
                emit(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rig::{
        streaming::{BlockClose, BlockId, BlockKind, Delta, ToolCallEnd, UnparseableToolInput},
        tool::ToolOutput,
    };

    fn model(event: StreamEvent) -> AgentEvent {
        AgentEvent::Model(event)
    }

    /// A turn that thinks, answers and calls a tool, then the tool's result.
    fn turn() -> Vec<AgentEvent> {
        let (thinking, text, tool) = (BlockId::from("r"), BlockId::from("t"), BlockId::from("c"));
        vec![
            model(StreamEvent::BlockStart {
                id: thinking.clone(),
                kind: BlockKind::Reasoning { provider_id: None },
            }),
            model(StreamEvent::BlockDelta {
                id: thinking.clone(),
                delta: Delta::Reasoning {
                    text: "Hmm.".into(),
                },
            }),
            model(StreamEvent::BlockEnd {
                id: thinking,
                end: BlockClose::Reasoning {
                    reasoning: None,
                    signature: None,
                    wire_sent: true,
                },
                block: None,
            }),
            model(StreamEvent::BlockDelta {
                id: text.clone(),
                delta: Delta::Text {
                    text: "Checking.".into(),
                },
            }),
            model(StreamEvent::BlockEnd {
                id: text,
                end: BlockClose::Text,
                block: None,
            }),
            model(StreamEvent::BlockStart {
                id: tool.clone(),
                kind: BlockKind::ToolCall,
            }),
            model(StreamEvent::BlockDelta {
                id: tool.clone(),
                delta: Delta::ToolName {
                    name: "lookup".into(),
                },
            }),
            model(StreamEvent::BlockDelta {
                id: tool.clone(),
                delta: Delta::ToolArguments {
                    arguments: r#"{"q":1}"#.into(),
                },
            }),
            model(StreamEvent::BlockEnd {
                id: tool,
                end: BlockClose::ToolCall(ToolCallEnd::new(UnparseableToolInput::Error)),
                block: None,
            }),
            AgentEvent::TurnEnded {
                message_id: Some("m1".into()),
                usage: Usage::default(),
            },
        ]
    }

    #[test]
    fn folding_rebuilds_replies_and_tool_results() {
        let mut fold = TurnFold::default();
        let mut blocks = Vec::new();
        let mut messages = Vec::new();
        for event in turn() {
            let folded = fold.apply(&event);
            blocks.extend(folded.block);
            messages.extend(folded.message);
        }
        // The tool call is complete as soon as its block ends.
        let [.., AssistantContent::ToolCall(call)] = blocks.as_slice() else {
            panic!("expected the completed tool call, got {blocks:?}");
        };
        assert_eq!(call.function.name, "lookup");
        let [Message::Assistant { id, content }] = messages.as_slice() else {
            panic!("expected the reply");
        };
        assert_eq!(id.as_deref(), Some("m1"));
        assert!(matches!(
            content.as_slice(),
            [
                AssistantContent::Reasoning(_),
                AssistantContent::Text(_),
                AssistantContent::ToolCall(_)
            ]
        ));

        // Resuming after the reply, as someone joining now would.
        assert_eq!(fold.partial(), None);
        let mut resumed = TurnFold::after(&messages[0]);
        let result = AgentEvent::ToolResult {
            call: call.id.clone(),
            result: ToolResult::success(ToolOutput::text("found")),
        };
        let expected = fold.apply(&result).message.expect("the results prompt");
        assert_eq!(resumed.apply(&result).message, Some(expected.clone()));
        assert!(matches!(
            expected,
            Message::User { content } if matches!(content.as_slice(), [UserContent::ToolResult(_)])
        ));
    }

    /// Mid-turn, the partial message is the reply so far, and just before
    /// the turn ends it is the reply the turn adds.
    #[test]
    fn partial_messages_grow_into_the_messages_folded() {
        let events = turn();
        let (turn_ended, streamed) = events.split_last().expect("a turn");
        let mut fold = TurnFold::default();
        assert_eq!(fold.partial(), None);
        fold.apply(&streamed[0]);
        fold.apply(&streamed[1]);
        let Some(Message::Assistant { content, .. }) = fold.partial() else {
            panic!("expected the reply so far");
        };
        assert!(matches!(
            content.as_slice(),
            [AssistantContent::Reasoning(_)]
        ));

        for event in &streamed[2..] {
            fold.apply(event);
        }
        let Some(Message::Assistant {
            content: partial, ..
        }) = fold.partial()
        else {
            panic!("expected the whole reply");
        };
        let Some(Message::Assistant { content, .. }) = fold.apply(turn_ended).message else {
            panic!("expected the reply");
        };
        assert_eq!(partial, content);
    }

    /// What a run emits folds the same after crossing a JSON boundary.
    #[test]
    fn events_fold_the_same_after_serialization() {
        let fold = |events: &[AgentEvent]| {
            let mut fold = TurnFold::default();
            events
                .iter()
                .filter_map(|event| fold.apply(event).message)
                .collect::<Vec<_>>()
        };
        let events = turn();
        let received = events
            .iter()
            .map(|event| {
                serde_json::from_str(&serde_json::to_string(event).expect("encode"))
                    .expect("decode")
            })
            .collect::<Vec<AgentEvent>>();
        assert_eq!(fold(&received), fold(&events));
    }
}
