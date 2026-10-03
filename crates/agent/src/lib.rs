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
//!
//! The history is Rig's own: each reply is what Rig's [`CompletionFold`]
//! collects from the canonical blocks the stream's block ends carry, which
//! is the response Rig returns for the turn.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use rig::{
    DynModel,
    completion::{AssistantContent, CompletionRequest, Message, Usage},
    message::{Reasoning, ReasoningContent, Text, ToolCall, ToolCallId, UserContent},
    operation::{Completion, CompletionFold},
    streaming::{BlockClose, BlockId, BlockKind, Delta, StreamEvent, stamp_reasoning},
    tool::{ToolContext, ToolResult, ToolSet},
    wire::Fold as _,
};
use serde::{Deserialize, Serialize};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// An observation produced while an agent run is in progress.
// Nearly every event is a `Model` event, so boxing it would only add an
// allocation per streamed fragment.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentEvent {
    /// A canonical provider event, as Rig's stream yields it: text and
    /// reasoning deltas, block ends carrying the block they finalized (the
    /// end of a tool call's block completes the call), and the terminal
    /// record.
    Model(StreamEvent),
    /// A model turn ended cleanly, so its reply joins the history. Carries
    /// what the stream reported besides its content: the reply's id, the
    /// tokens the request used, which sum to the run's usage even when a
    /// later request fails or is cancelled, and the issuer Rig records on
    /// the reply's reasoning.
    TurnEnded {
        message_id: Option<String>,
        usage: Usage,
        reasoning_issuer: Option<String>,
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
///
/// A reply is exactly what Rig collects from its canonical block ends, and
/// nothing else. Deltas only feed [`TurnFold::partial`], a preview of the
/// reply while it streams, which never joins the history.
#[derive(Default)]
pub struct TurnFold {
    /// Rig's fold of the reply being streamed: its finished blocks, in the
    /// order they began.
    reply: CompletionFold,
    /// The reply as it shows while streaming, one entry per block Rig
    /// places, in its order. A block that has ended shows as Rig finalized
    /// it; one still open shows its deltas so far, and `None` until it has
    /// any.
    preview: Vec<Option<AssistantContent>>,
    /// The latest preview entry each block key holds, placed as Rig's fold
    /// places it so that nothing moves when a block ends.
    slots: HashMap<BlockId, usize>,
    /// Reasoning keys whose block began and has not ended.
    open_reasoning: HashSet<BlockId>,
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

    /// The message being folded, as far as it has come: a preview of the
    /// reply streaming, or once it has ended, the results of the tools that
    /// have returned. `None` while there is neither.
    ///
    /// The preview shows each block where the reply will have it: as Rig
    /// finalized it once it has ended, and as its deltas so far while it is
    /// open. The reply that joins the history is Rig's alone, so it can
    /// differ from the last preview where Rig's finalization does, as when
    /// it records the reasoning's issuer.
    pub fn partial(&self) -> Option<Message> {
        if !self.results.is_empty() {
            return Some(Message::User {
                content: self.results.clone(),
            });
        }
        let content = self.preview.iter().flatten().cloned().collect::<Vec<_>>();
        (!content.is_empty()).then_some(Message::Assistant { id: None, content })
    }

    pub fn apply(&mut self, event: &AgentEvent) -> Folded {
        match event {
            AgentEvent::Model(event) => {
                // Collecting a canonical event never fails.
                _ = self.reply.absorb(event);
                Folded {
                    message: None,
                    block: self.show(event),
                }
            }
            AgentEvent::TurnEnded {
                message_id,
                reasoning_issuer,
                ..
            } => {
                let mut content = std::mem::take(&mut self.reply).snapshot();
                if let Some(issuer) = reasoning_issuer {
                    content = stamp_reasoning(content, issuer);
                }
                self.preview.clear();
                self.slots.clear();
                self.open_reasoning.clear();
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

    /// Updates the preview with a model event, returning the block it
    /// finalized, if any. Blocks are placed as [`CompletionFold`] places
    /// them: text at its first content, reasoning when it begins, and tool
    /// calls and images at their end.
    fn show(&mut self, event: &StreamEvent) -> Option<AssistantContent> {
        match event {
            StreamEvent::BlockStart {
                id,
                kind:
                    BlockKind::Text {
                        additional_params: Some(_),
                    },
            }
            | StreamEvent::BlockDelta {
                id,
                delta: Delta::TextMeta { .. },
            } if !self.slots.contains_key(id) => {
                self.reserve(id);
            }
            StreamEvent::BlockStart {
                id,
                kind: BlockKind::Reasoning { .. },
            } if !self.open_reasoning.contains(id) => {
                self.open_reasoning.insert(id.clone());
                self.reserve(id);
            }
            StreamEvent::BlockDelta {
                id,
                delta: Delta::Text { text },
            } => {
                if !self.slots.contains_key(id) {
                    self.reserve(id);
                }
                let shown = &mut self.preview[self.slots[id]];
                match shown {
                    Some(AssistantContent::Text(part)) => part.text.push_str(text),
                    None => *shown = Some(AssistantContent::Text(Text::new(text))),
                    Some(_) => {}
                }
            }
            StreamEvent::BlockDelta {
                id,
                delta: Delta::Reasoning { text },
            } => {
                if self.open_reasoning.insert(id.clone()) {
                    self.reserve(id);
                }
                let shown = &mut self.preview[self.slots[id]];
                match shown {
                    Some(AssistantContent::Reasoning(part)) => {
                        if let Some(ReasoningContent::Text { text: body, .. }) =
                            part.content.last_mut()
                        {
                            body.push_str(text);
                        }
                    }
                    None => *shown = Some(AssistantContent::Reasoning(Reasoning::new(text))),
                    Some(_) => {}
                }
            }
            StreamEvent::BlockEnd {
                id,
                end,
                block: Some(block),
            } => {
                let slot = match end {
                    BlockClose::Text => self.slots.get(id).copied(),
                    BlockClose::Reasoning { .. } => {
                        self.open_reasoning.remove(id);
                        self.slots.get(id).copied()
                    }
                    BlockClose::ToolCall(_) | BlockClose::Image(_) => None,
                };
                let slot = slot.unwrap_or_else(|| self.reserve(id));
                self.preview[slot] = Some(block.clone());
                return Some(block.clone());
            }
            _ => {}
        }
        None
    }

    /// Holds the next place of the preview for block `id`.
    fn reserve(&mut self, id: &BlockId) -> usize {
        let slot = self.preview.len();
        self.slots.insert(id.clone(), slot);
        self.preview.push(None);
        slot
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
pub struct Agent {
    model: DynModel<Completion>,
    tools: ToolSet,
    preamble: Option<String>,
    additional_params: Option<serde_json::Value>,
}

impl Agent {
    pub fn new(model: DynModel<Completion>, tools: ToolSet) -> Self {
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

impl Agent {
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
            let mut request = CompletionRequest::new(prompt.clone())
                .messages(history.clone())
                .tools(self.tools.tool_definitions());

            if let Some(preamble) = &self.preamble {
                request = request.preamble(preamble.clone());
            }
            if let Some(params) = &self.additional_params {
                request = request.additional_params(params.clone());
            }
            history.push(prompt.clone());

            let mut stream = self
                .model
                .stream(request)
                .context("failed to start model stream")?;
            while let Some(event) = stream.next().await {
                let event = AgentEvent::Model(event.context("model stream failed")?);
                fold.apply(&event);
                emit(event);
            }
            let reasoning_issuer = stream.folded().reasoning_issuer().map(str::to_owned);
            let response = stream
                .finish()
                .context("model stream ended without a complete reply")?;
            let ended = AgentEvent::TurnEnded {
                message_id: response.message_id.clone(),
                usage: response.usage,
                reasoning_issuer,
            };
            let reply = fold.apply(&ended).message;
            debug_assert_eq!(
                reply.as_ref(),
                Some(&Message::from(response)),
                "the folded reply is the one Rig returned"
            );
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
        streaming::{ToolCallEnd, UnparseableToolInput},
        test_utils::{MockCompletionModel, MockStreamEvent, mock_final_with_total_tokens},
        tool::ToolOutput,
    };

    use crate::test_support::canonical;

    fn model(event: StreamEvent) -> AgentEvent {
        AgentEvent::Model(event)
    }

    /// A turn that thinks, answers and calls a tool, as Rig streams it.
    fn turn() -> Vec<AgentEvent> {
        let (thinking, text, tool) = (BlockId::from("r"), BlockId::from("t"), BlockId::from("c"));
        canonical([
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
                reasoning_issuer: None,
            },
        ])
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

    /// The reply is the blocks Rig finalized, not what streamed before them:
    /// an end that restates its block supersedes the deltas the preview
    /// showed.
    #[test]
    fn replies_hold_the_blocks_rig_finalized() {
        let thinking = BlockId::from("r");
        let events = canonical([
            model(StreamEvent::BlockDelta {
                id: thinking.clone(),
                delta: Delta::Reasoning { text: "Hm".into() },
            }),
            model(StreamEvent::BlockEnd {
                id: thinking.clone(),
                end: BlockClose::Reasoning {
                    reasoning: Some(Reasoning::new("Considered.")),
                    signature: None,
                    wire_sent: true,
                },
                block: None,
            }),
            AgentEvent::TurnEnded {
                message_id: None,
                usage: Usage::default(),
                reasoning_issuer: Some("ollama".into()),
            },
        ]);
        let mut fold = TurnFold::default();
        fold.apply(&events[0]);
        let Some(Message::Assistant { content, .. }) = fold.partial() else {
            panic!("expected the preview");
        };
        assert!(matches!(
            content.as_slice(),
            [AssistantContent::Reasoning(reasoning)] if reasoning.display_text() == "Hm"
        ));

        let Some(AssistantContent::Reasoning(finalized)) = fold.apply(&events[1]).block else {
            panic!("expected the finalized reasoning");
        };
        assert_eq!(finalized.display_text(), "Considered.");
        let Some(Message::Assistant { content, .. }) = fold.apply(&events[2]).message else {
            panic!("expected the reply");
        };
        // Stamped with the issuer, as Rig's response is.
        let [AssistantContent::Reasoning(reasoning)] = content.as_slice() else {
            panic!("expected the reasoning, got {content:?}");
        };
        assert_eq!(reasoning.display_text(), "Considered.");
        assert_eq!(reasoning.provider.as_deref(), Some("ollama"));
    }

    #[test]
    fn block_ends_allocate_or_reuse_preview_slots() {
        for event in turn() {
            let AgentEvent::Model(StreamEvent::BlockEnd {
                id,
                end,
                block: Some(block),
            }) = &event
            else {
                continue;
            };
            for reserved in [false, true] {
                let mut fold = TurnFold::default();
                if reserved {
                    assert_eq!(fold.reserve(id), 0);
                    if matches!(end, BlockClose::Reasoning { .. }) {
                        fold.open_reasoning.insert(id.clone());
                    }
                }
                let slot = usize::from(reserved && matches!(end, BlockClose::ToolCall(_)));
                assert_eq!(fold.apply(&event).block.as_ref(), Some(block));
                assert_eq!(fold.slots[id], slot);
                assert_eq!(fold.preview.len(), slot + 1);
                assert_eq!(fold.preview[slot].as_ref(), Some(block));
                assert!(!fold.open_reasoning.contains(id));
                if slot == 1 {
                    assert_eq!(fold.preview[0], None);
                }

                if matches!(end, BlockClose::Reasoning { .. }) {
                    for event in canonical([model(StreamEvent::BlockStart {
                        id: id.clone(),
                        kind: BlockKind::Reasoning { provider_id: None },
                    })]) {
                        fold.apply(&event);
                    }
                    assert_eq!(fold.slots[id], 1);
                    assert_eq!(fold.preview.len(), 2);
                    assert_eq!(fold.preview[0].as_ref(), Some(block));
                    assert_eq!(fold.preview[1], None);
                }
            }
        }
    }

    /// A block's place in the preview is its place in the reply, so blocks
    /// don't move when one ends: text that began before a tool call stays
    /// before it, even though the call ends first.
    #[test]
    fn blocks_keep_their_place_when_they_end() {
        let (text, tool) = (BlockId::from("t"), BlockId::from("c"));
        let events = canonical([
            model(StreamEvent::text(text.clone(), "Checking")),
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
                    arguments: "{}".into(),
                },
            }),
            model(StreamEvent::BlockEnd {
                id: tool,
                end: BlockClose::ToolCall(ToolCallEnd::new(UnparseableToolInput::Error)),
                block: None,
            }),
            model(StreamEvent::text(text, " twice.")),
            AgentEvent::TurnEnded {
                message_id: None,
                usage: Usage::default(),
                reasoning_issuer: None,
            },
        ]);
        let (turn_ended, streamed) = events.split_last().expect("a turn");
        let mut fold = TurnFold::default();
        for event in streamed {
            fold.apply(event);
            if let Some(Message::Assistant { content, .. }) = fold.partial()
                && content.len() == 2
            {
                assert!(matches!(
                    content.as_slice(),
                    [AssistantContent::Text(_), AssistantContent::ToolCall(_)]
                ));
            }
        }
        let Some(Message::Assistant { content, .. }) = fold.apply(turn_ended).message else {
            panic!("expected the reply");
        };
        let [AssistantContent::Text(text), AssistantContent::ToolCall(_)] = content.as_slice()
        else {
            panic!("expected the text, then the call, got {content:?}");
        };
        assert_eq!(text.text, "Checking twice.");
    }

    /// A run records the replies Rig returns (`run` asserts each against
    /// the response), and folding what it emitted, as someone it was sent
    /// to does, rebuilds that history exactly.
    #[tokio::test]
    async fn runs_record_the_replies_rig_returns() {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::ReasoningDelta {
                    id: "r".into(),
                    reasoning: "Add them.".into(),
                },
                MockStreamEvent::Text("Adding.".into()),
                MockStreamEvent::ToolCall {
                    id: "c".into(),
                    name: "calculate".into(),
                    arguments: serde_json::json!({"operation": "add", "a": 1, "b": 2}),
                    call_id: None,
                },
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
            ],
            vec![
                MockStreamEvent::Text("It is 3.".into()),
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(20)),
            ],
        ]);
        let mut tools = ToolSet::default();
        tools.add_tool(tools::Calculate);
        let mut history = Vec::new();
        let mut events = Vec::new();
        Agent::new(model.erase(), tools)
            .run(Message::user("1 + 2?"), &mut history, |event| {
                events.push(event);
            })
            .await
            .expect("the run");

        let [
            Message::User { .. },
            Message::Assistant { content, .. },
            Message::User { .. },
            Message::Assistant { .. },
        ] = history.as_slice()
        else {
            panic!("expected prompt, reply, tool result, reply, got {history:?}");
        };
        let [
            AssistantContent::Reasoning(reasoning),
            AssistantContent::Text(_),
            AssistantContent::ToolCall(_),
        ] = content.as_slice()
        else {
            panic!("expected reasoning, text and a call, got {content:?}");
        };
        assert!(reasoning.provider.is_some());

        let mut fold = TurnFold::default();
        let folded = events
            .iter()
            .map(|event| {
                serde_json::from_str::<AgentEvent>(&serde_json::to_string(event).expect("encode"))
                    .expect("decode")
            })
            .filter_map(|event| fold.apply(&event).message);
        let prompt = Message::user("1 + 2?");
        assert_eq!(
            std::iter::once(prompt).chain(folded).collect::<Vec<_>>(),
            history
        );
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
