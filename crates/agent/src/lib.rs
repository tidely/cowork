//! A small agent loop built directly on Rig's low-level completion stream.
//!
//! Unlike Rig's multi-turn agent stream, this crate surfaces everything as it
//! streams, including a completed tool call as soon as the provider closes
//! that call's part. Tool execution still waits for the complete model turn,
//! keeping the assistant message and its tool results in a valid history
//! order.
//!
//! A run is fully described by the [`AgentEvent`]s it emits: folding them
//! with a [`TurnFold`] rebuilds exactly the history the loop records, because
//! the loop records it by folding them itself. Events are serializable one at
//! a time, so they can be sent elsewhere as they happen and folded there with
//! the same result.
//!
//! The history is Rig's own: each reply is the content the stream's part ends
//! carry, in the parts' positions, with the origin and stop Rig gave the
//! turn, which is the turn Rig's response appends.
//!
//! A [`ToolHook`] decides whether each call may run before its tool sees it,
//! so policy such as asking the user lives in one place rather than in every
//! tool. A call it denies is answered like any other, so the history stays
//! valid and the model learns why.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context as _, Result};
use futures::{StreamExt as _, future::BoxFuture};
use rig::{
    DynModel,
    completion::{AssistantContent, CompletionRequest, Message, Usage},
    message::{AssistantMessage, CallId, Origin, StopReason, ToolCall, UserContent},
    operation::Completion,
    streaming::{Item, SequenceError, StreamEvent, Transcript},
    tool::{ToolContext, ToolExecutionError, ToolResult, ToolSet},
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
    /// An event of Rig's completion stream: a part starts, grows, or ends
    /// with the content Rig finalized (the end of a tool call's part
    /// completes the call). Payloads Rig passes along unmodeled are not
    /// reported, as they are not part of the reply.
    Model(StreamEvent),
    /// A model turn ended cleanly, so its reply joins the history. Carries
    /// what the response reported besides its content: who produced the
    /// turn and how it ended, which the reply keeps, and the tokens the
    /// request used, which sum to the run's usage even when a later request
    /// fails or is cancelled.
    TurnEnded {
        origin: Option<Origin>,
        stop: Option<StopReason>,
        usage: Usage,
    },
    /// A tool the last reply called returned. The call itself is in the
    /// reply.
    ToolResult { call: CallId, result: ToolResult },
}

/// Folds a run's events into the messages they add to the history.
///
/// Each completed model turn adds its reply, and once every tool that reply
/// called has returned, their results add the prompt of the next request.
/// The initial prompt is the caller's, and never comes out of a fold.
///
/// A reply is exactly the content Rig's part ends carry, and nothing else.
/// Fragments only feed [`TurnFold::partial`], a preview of the reply while it
/// streams, which never joins the history.
///
/// Model events are checked as Rig checks a relayed stream (see
/// [`Transcript::push`]), so events from elsewhere that Rig's stream could
/// not have produced are refused rather than folded.
#[derive(Default)]
pub struct TurnFold {
    /// The reply's events so far, in the order Rig's stream yields them.
    transcript: Transcript,
    /// The parts of the reply that ended, by position: what the reply is.
    ended: BTreeMap<usize, AssistantContent>,
    /// The text and reasoning of parts still streaming, by position: shown
    /// in the preview only.
    streaming: BTreeMap<usize, AssistantContent>,
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
    /// A part of the reply it completed, such as a tool call.
    pub block: Option<AssistantContent>,
}

impl TurnFold {
    /// A fold resuming after `reply`, the last message of the history,
    /// waiting for the results of the tools it called.
    pub fn after(reply: &Message) -> Self {
        let calls = match reply {
            Message::Assistant(reply) => pending_calls(&reply.content, reply.stop.as_ref()),
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
    /// The preview shows each part where the reply will have it: as Rig
    /// finalized it once it has ended, and as its fragments so far while it
    /// is open.
    pub fn partial(&self) -> Option<Message> {
        if !self.results.is_empty() {
            return Some(Message::User {
                content: self.results.clone(),
            });
        }
        let mut parts = self.streaming.clone();
        parts.extend(self.ended.clone());
        let content = parts.into_values().collect::<Vec<_>>();
        (!content.is_empty()).then(|| Message::Assistant(AssistantMessage::new(content)))
    }

    /// Folds `event`, refusing a model event Rig's stream could not have
    /// produced after the turn's events so far.
    pub fn apply(&mut self, event: &AgentEvent) -> Result<Folded, SequenceError> {
        match event {
            AgentEvent::Model(event) => {
                self.transcript.push(Item::Event(event.clone()))?;
                Ok(Folded {
                    message: None,
                    block: self.show(event),
                })
            }
            AgentEvent::TurnEnded { origin, stop, .. } => {
                let content = std::mem::take(&mut self.ended)
                    .into_values()
                    .collect::<Vec<_>>();
                self.transcript = Transcript::default();
                self.streaming.clear();
                self.calls = pending_calls(&content, stop.as_ref());
                self.results.clear();
                // Rig appends no message for an empty reply.
                let message = (!content.is_empty()).then(|| {
                    Message::Assistant(
                        AssistantMessage::new(content)
                            .with_origin(origin.clone())
                            .with_stop(stop.clone()),
                    )
                });
                Ok(Folded {
                    message,
                    block: None,
                })
            }
            AgentEvent::ToolResult { call, result } => {
                let Some(call) = self.calls.iter().find(|pending| pending.id == *call) else {
                    return Ok(Folded::default());
                };
                self.results.push(rig::transcript::tool_result_output(
                    call.id.clone(),
                    call.function.name.clone(),
                    result,
                ));
                if self.results.len() < self.calls.len() {
                    return Ok(Folded::default());
                }
                self.calls.clear();
                Ok(Folded {
                    message: Some(Message::User {
                        content: std::mem::take(&mut self.results),
                    }),
                    block: None,
                })
            }
        }
    }

    /// Updates the preview with a model event, returning the part it
    /// finalized, if any.
    fn show(&mut self, event: &StreamEvent) -> Option<AssistantContent> {
        match event {
            StreamEvent::Text { part, text } => {
                if let AssistantContent::Text(shown) = self
                    .streaming
                    .entry(part.index())
                    .or_insert_with(|| AssistantContent::text(""))
                {
                    shown.text.push_str(text);
                }
            }
            StreamEvent::Reasoning { part, text } => {
                if let AssistantContent::Reasoning(shown) = self
                    .streaming
                    .entry(part.index())
                    .or_insert_with(|| AssistantContent::reasoning(""))
                {
                    shown.text.push_str(text);
                }
            }
            StreamEvent::End { part, content } => {
                self.streaming.remove(&part.index());
                self.ended.insert(part.index(), content.clone());
                return Some(content.clone());
            }
            StreamEvent::Start { .. } | StreamEvent::Arguments { .. } => {}
        }
        None
    }
}

/// The calls of a reply that ended with `stop` that wait for results: none
/// when the turn failed, as none of its calls run.
fn pending_calls(content: &[AssistantContent], stop: Option<&StopReason>) -> Vec<ToolCall> {
    if stop.is_some_and(StopReason::is_failure) {
        return Vec::new();
    }
    content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// Whether a tool call may run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    /// The call is answered with `reason` instead of running, which the
    /// model reads as the call's result.
    Deny {
        reason: String,
    },
}

/// Decides whether each tool call may run, before its tool sees it.
///
/// Called once per call, in the order the reply made them, and awaited: a
/// hook may take as long as it needs, such as while someone decides. Dropping
/// the run drops the pending decision with it. Calls whose arguments could not
/// be read never reach the hook, as they never run anyway.
pub trait ToolHook: Send + Sync {
    fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolDecision>;
}

/// A provider-independent, streaming tool loop.
pub struct Agent {
    model: DynModel<Completion>,
    tools: ToolSet,
    preamble: Option<String>,
    additional_params: Option<serde_json::Value>,
    /// `None` lets every call run.
    tool_hook: Option<Arc<dyn ToolHook>>,
}

impl Agent {
    pub fn new(model: DynModel<Completion>, tools: ToolSet) -> Self {
        Self {
            model,
            tools,
            preamble: None,
            additional_params: None,
            tool_hook: None,
        }
    }

    pub fn tool_hook(mut self, hook: Arc<dyn ToolHook>) -> Self {
        self.tool_hook = Some(hook);
        self
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
    async fn decide(&self, call: &ToolCall) -> ToolDecision {
        match &self.tool_hook {
            Some(hook) => hook.before_tool_call(call).await,
            None => ToolDecision::Allow,
        }
    }

    /// Runs until the model produces a turn with no tool calls.
    ///
    /// `emit` is called synchronously as stream events arrive. It should do
    /// little work itself; forwarding events to a channel is a good default.
    ///
    /// `history` gains `prompt` and then exactly the messages a [`TurnFold`]
    /// folds out of the emitted events. On failure, it ends with the prompt
    /// of the request that failed, or with the reply of a turn the provider
    /// failed, whose tool calls never run.
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
            while let Some(item) = stream.next().await {
                let Item::Event(event) = item.context("model stream failed")? else {
                    continue;
                };
                let event = AgentEvent::Model(event);
                fold.apply(&event)
                    .context("model stream yielded an event out of order")?;
                emit(event);
            }
            let response = stream
                .finish()
                .await
                .context("model stream ended without a complete reply")?;
            let head = response.head();
            let ended = AgentEvent::TurnEnded {
                origin: head.origin,
                stop: head.stop,
                usage: response.usage,
            };
            let reply = fold.apply(&ended)?.message;
            debug_assert_eq!(
                reply,
                response.message(),
                "the folded reply is the one Rig returned"
            );
            emit(ended);
            history.extend(reply);
            if let Some(failure) = rig::message::turn_failure(
                &response.choice,
                Some(&response.stop()),
                response.finish_reason().as_ref(),
            ) {
                anyhow::bail!(failure);
            }

            let calls = fold.pending_calls().to_vec();
            if calls.is_empty() {
                return Ok(());
            }
            for call in calls {
                let name = call.function.name.as_str();
                let result = match &call.function.invalid_arguments {
                    // The tool never sees arguments it cannot read; the
                    // model is told why and can call again.
                    Some(raw) => ToolResult::failed(ToolExecutionError::invalid_args(
                        rig::transcript::invalid_arguments_feedback(name, raw),
                    )),
                    None => match self.decide(&call).await {
                        ToolDecision::Allow => {
                            self.tools
                                .execute(
                                    name,
                                    call.function.arguments_value().to_string(),
                                    &mut ToolContext::new(),
                                )
                                .await
                        }
                        // Rig's result for a call runtime policy skipped.
                        ToolDecision::Deny { reason } => ToolResult::skipped(reason),
                    },
                };
                let event = AgentEvent::ToolResult {
                    call: call.id,
                    result,
                };
                if let Some(results) = fold.apply(&event)?.message {
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
        test_utils::{MockCompletionModel, MockStreamEvent, mock_final_with_total_tokens},
        tool::ToolOutput,
    };

    use crate::test_support::turn;

    /// A model event read from its JSON form, as one arrives from elsewhere.
    fn event(value: serde_json::Value) -> AgentEvent {
        AgentEvent::Model(serde_json::from_value(value).expect("a stream event"))
    }

    fn ended() -> AgentEvent {
        AgentEvent::TurnEnded {
            origin: None,
            stop: Some(StopReason::Stop),
            usage: Usage::default(),
        }
    }

    /// A turn that thinks, answers and calls a tool, as Rig streams it.
    fn thinking_turn() -> Vec<AgentEvent> {
        turn([
            MockStreamEvent::ReasoningDelta {
                id: "r".into(),
                reasoning: "Hmm.".into(),
            },
            MockStreamEvent::text("Checking."),
            MockStreamEvent::tool_call("c", "lookup", serde_json::json!({"q": 1})),
            MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
        ])
    }

    fn fold_all(fold: &mut TurnFold, events: &[AgentEvent]) -> Vec<Folded> {
        events
            .iter()
            .map(|event| fold.apply(event).expect("a valid event"))
            .collect()
    }

    #[test]
    fn folding_rebuilds_replies_and_tool_results() {
        let mut fold = TurnFold::default();
        let folded = fold_all(&mut fold, &thinking_turn());
        let blocks = folded
            .iter()
            .filter_map(|folded| folded.block.clone())
            .collect::<Vec<_>>();
        let messages = folded
            .into_iter()
            .filter_map(|folded| folded.message)
            .collect::<Vec<_>>();
        // The tool call is complete as soon as its part ends.
        let Some(AssistantContent::ToolCall(call)) = blocks
            .iter()
            .find(|block| matches!(block, AssistantContent::ToolCall(_)))
        else {
            panic!("expected the completed tool call, got {blocks:?}");
        };
        assert_eq!(call.function.name.as_str(), "lookup");
        let [Message::Assistant(reply)] = messages.as_slice() else {
            panic!("expected the reply");
        };
        assert!(reply.origin.is_some());
        assert_eq!(reply.stop, Some(StopReason::ToolUse));
        assert!(matches!(
            reply.content.as_slice(),
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
        let expected = fold
            .apply(&result)
            .expect("a result")
            .message
            .expect("the results prompt");
        assert_eq!(
            resumed.apply(&result).expect("a result").message,
            Some(expected.clone())
        );
        assert!(matches!(
            expected,
            Message::User { content } if matches!(content.as_slice(), [UserContent::ToolResult(_)])
        ));
    }

    /// Mid-turn, the partial message is the reply so far, and just before
    /// the turn ends it is the reply the turn adds.
    #[test]
    fn partial_messages_grow_into_the_messages_folded() {
        let events = thinking_turn();
        let (turn_ended, streamed) = events.split_last().expect("a turn");
        let mut fold = TurnFold::default();
        assert_eq!(fold.partial(), None);
        fold_all(&mut fold, &streamed[..2]);
        let Some(Message::Assistant(partial)) = fold.partial() else {
            panic!("expected the reply so far");
        };
        assert!(matches!(
            partial.content.as_slice(),
            [AssistantContent::Reasoning(_)]
        ));

        fold_all(&mut fold, &streamed[2..]);
        let Some(Message::Assistant(partial)) = fold.partial() else {
            panic!("expected the whole reply");
        };
        let Some(Message::Assistant(reply)) = fold.apply(turn_ended).expect("an end").message
        else {
            panic!("expected the reply");
        };
        assert_eq!(partial.content, reply.content);
    }

    /// The reply is the content Rig finalized, not what streamed before it:
    /// an end that states other content supersedes the fragments the
    /// preview showed.
    #[test]
    fn replies_hold_the_content_rig_finalized() {
        let events = [
            event(serde_json::json!({"event": "start", "part": 0, "kind": "reasoning"})),
            event(serde_json::json!({"event": "reasoning", "part": 0, "text": "Hm"})),
            event(serde_json::json!({"event": "end", "part": 0,
                "content": {"type": "reasoning", "text": "Considered."}})),
            ended(),
        ];
        let mut fold = TurnFold::default();
        fold_all(&mut fold, &events[..2]);
        let Some(Message::Assistant(partial)) = fold.partial() else {
            panic!("expected the preview");
        };
        assert!(matches!(
            partial.content.as_slice(),
            [AssistantContent::Reasoning(reasoning)] if reasoning.text == "Hm"
        ));

        let Some(AssistantContent::Reasoning(finalized)) =
            fold.apply(&events[2]).expect("an end").block
        else {
            panic!("expected the finalized reasoning");
        };
        assert_eq!(finalized.text, "Considered.");
        let Some(Message::Assistant(reply)) = fold.apply(&events[3]).expect("an end").message
        else {
            panic!("expected the reply");
        };
        let [AssistantContent::Reasoning(reasoning)] = reply.content.as_slice() else {
            panic!("expected the reasoning, got {:?}", reply.content);
        };
        assert_eq!(reasoning.text, "Considered.");
    }

    /// A part's place in the preview is its place in the reply, so parts
    /// don't move when one ends: text that began before a tool call stays
    /// before it, even though the call ends first.
    #[test]
    fn parts_keep_their_place_when_they_end() {
        let events = [
            event(serde_json::json!({"event": "start", "part": 0, "kind": "text"})),
            event(serde_json::json!({"event": "text", "part": 0, "text": "Checking"})),
            event(
                serde_json::json!({"event": "start", "part": 1, "kind": "tool_call",
                "name": "lookup"}),
            ),
            event(serde_json::json!({"event": "arguments", "part": 1, "json": "{}"})),
            event(serde_json::json!({"event": "end", "part": 1, "content": {
                "type": "toolcall", "id": {"provider": "c"},
                "function": {"name": "lookup", "arguments": {}}}})),
            event(serde_json::json!({"event": "text", "part": 0, "text": " twice."})),
            event(serde_json::json!({"event": "end", "part": 0,
                "content": {"type": "text", "text": "Checking twice."}})),
            ended(),
        ];
        let (turn_ended, streamed) = events.split_last().expect("a turn");
        let mut fold = TurnFold::default();
        for event in streamed {
            fold.apply(event).expect("a valid event");
            if let Some(Message::Assistant(partial)) = fold.partial()
                && partial.content.len() == 2
            {
                assert!(matches!(
                    partial.content.as_slice(),
                    [AssistantContent::Text(_), AssistantContent::ToolCall(_)]
                ));
            }
        }
        let Some(Message::Assistant(reply)) = fold.apply(turn_ended).expect("an end").message
        else {
            panic!("expected the reply");
        };
        let [AssistantContent::Text(text), AssistantContent::ToolCall(_)] =
            reply.content.as_slice()
        else {
            panic!("expected the text, then the call, got {:?}", reply.content);
        };
        assert_eq!(text.text, "Checking twice.");
    }

    /// Events Rig's stream could not have produced are refused, as Rig
    /// refuses them in a relayed stream: here, text for a part that never
    /// started, and a part that ends twice.
    #[test]
    fn events_out_of_order_are_refused() {
        let mut fold = TurnFold::default();
        assert!(
            fold.apply(&event(
                serde_json::json!({"event": "text", "part": 0, "text": "Hi"})
            ))
            .is_err()
        );

        let start = event(serde_json::json!({"event": "start", "part": 0, "kind": "text"}));
        let end = event(serde_json::json!({"event": "end", "part": 0,
            "content": {"type": "text", "text": "Hi"}}));
        let mut fold = TurnFold::default();
        fold_all(&mut fold, &[start.clone(), end.clone()]);
        assert!(fold.apply(&end).is_err());

        // Positions are per turn: the next turn starts over.
        fold.apply(&ended()).expect("an end");
        fold_all(&mut fold, &[start, end]);
    }

    /// A failed turn's calls never run, so nothing waits for their results.
    #[test]
    fn a_failed_turns_calls_are_not_pending() {
        let reply = Message::Assistant(
            AssistantMessage::new(vec![AssistantContent::ToolCall(ToolCall::from_wire(
                "c",
                rig::message::ToolFunction::new(
                    rig::message::ToolName::new("lookup").expect("a name"),
                    serde_json::json!({}),
                ),
            ))])
            .with_stop(StopReason::Error("refused".into())),
        );
        assert!(TurnFold::after(&reply).pending_calls().is_empty());
    }

    /// A run records the replies Rig returns (`run` asserts each against
    /// the response), and folding what it emitted, one event at a time from
    /// its JSON form as someone it was sent to does, rebuilds that history
    /// exactly.
    #[tokio::test]
    async fn runs_record_the_replies_rig_returns() {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::ReasoningDelta {
                    id: "r".into(),
                    reasoning: "Add them.".into(),
                },
                MockStreamEvent::Text("Adding.".into()),
                MockStreamEvent::tool_call(
                    "c",
                    "calculate",
                    serde_json::json!({"operation": "add", "a": 1, "b": 2}),
                ),
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
            Message::Assistant(reply),
            Message::User { content: results },
            Message::Assistant(_),
        ] = history.as_slice()
        else {
            panic!("expected prompt, reply, tool result, reply, got {history:?}");
        };
        assert!(matches!(
            reply.content.as_slice(),
            [
                AssistantContent::Reasoning(_),
                AssistantContent::Text(_),
                AssistantContent::ToolCall(_),
            ]
        ));
        assert!(reply.origin.is_some());
        assert!(matches!(
            results.as_slice(),
            [UserContent::ToolResult(result)] if !result.is_error
        ));

        let mut fold = TurnFold::default();
        let folded = events
            .iter()
            .map(|event| {
                serde_json::from_str::<AgentEvent>(&serde_json::to_string(event).expect("encode"))
                    .expect("decode")
            })
            .filter_map(|event| fold.apply(&event).expect("a valid event").message);
        let prompt = Message::user("1 + 2?");
        assert_eq!(
            std::iter::once(prompt).chain(folded).collect::<Vec<_>>(),
            history
        );
    }

    /// A call whose arguments are not a JSON object never reaches the tool:
    /// it is answered with an error telling the model why.
    #[tokio::test]
    async fn malformed_arguments_are_answered_without_running_the_tool() {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::ToolCallNameDelta {
                    id: "c".into(),
                    name: "calculate".into(),
                },
                MockStreamEvent::ToolCallArgumentsDelta {
                    id: "c".into(),
                    arguments: "{\"a\": ".into(),
                },
                MockStreamEvent::ToolCallEnd { id: "c".into() },
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
            ],
            vec![
                MockStreamEvent::Text("Sorry.".into()),
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(20)),
            ],
        ]);
        let mut tools = ToolSet::default();
        tools.add_tool(tools::Calculate);
        let mut history = Vec::new();
        Agent::new(model.erase(), tools)
            .run(Message::user("1 + ?"), &mut history, |_| {})
            .await
            .expect("the run");
        let Some(Message::User { content }) = history.get(2) else {
            panic!("expected the tool result, got {history:?}");
        };
        let [UserContent::ToolResult(result)] = content.as_slice() else {
            panic!("expected one result, got {content:?}");
        };
        assert!(result.is_error);
        assert!(
            result.content[0]
                .as_text()
                .is_some_and(|text| text.contains("not a JSON object")),
            "{result:?}"
        );
    }

    /// Denies every call, remembering which it was asked about.
    #[derive(Default)]
    struct DenyAll {
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl ToolHook for DenyAll {
        fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolDecision> {
            self.asked.lock().unwrap().push(call.id.to_string());
            Box::pin(async {
                ToolDecision::Deny {
                    reason: "Not now.".into(),
                }
            })
        }
    }

    fn calculation_turns() -> MockCompletionModel {
        MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call(
                    "c",
                    "calculate",
                    serde_json::json!({"operation": "add", "a": 1, "b": 2}),
                ),
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
            ],
            vec![
                MockStreamEvent::Text("I could not.".into()),
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(20)),
            ],
        ])
    }

    /// A call the hook denies is answered with its reason without running,
    /// and the run goes on to the model's next turn.
    #[tokio::test]
    async fn denied_calls_are_answered_without_running_the_tool() {
        let mut tools = ToolSet::default();
        tools.add_tool(tools::Calculate);
        let hook = Arc::new(DenyAll::default());
        let mut history = Vec::new();
        let mut events = Vec::new();
        Agent::new(calculation_turns().erase(), tools)
            .tool_hook(hook.clone())
            .run(Message::user("1 + 2?"), &mut history, |event| {
                events.push(event)
            })
            .await
            .expect("the run");

        assert_eq!(*hook.asked.lock().unwrap(), ["c"]);
        let results = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolResult { result, .. } => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        let [result] = results.as_slice() else {
            panic!("expected one result, got {events:?}");
        };
        assert!(result.is_skipped(), "{result:?}");
        let [_, _, Message::User { content }, Message::Assistant(_)] = history.as_slice() else {
            panic!("expected prompt, call, result, reply, got {history:?}");
        };
        let [UserContent::ToolResult(result)] = content.as_slice() else {
            panic!("expected one result, got {content:?}");
        };
        assert_eq!(result.content[0].as_text(), Some("Not now."));
    }

    /// What a run emits folds the same after each event crosses a JSON
    /// boundary on its own.
    #[test]
    fn events_fold_the_same_after_serialization() {
        let fold = |events: &[AgentEvent]| {
            let mut fold = TurnFold::default();
            events
                .iter()
                .filter_map(|event| fold.apply(event).expect("a valid event").message)
                .collect::<Vec<_>>()
        };
        let events = thinking_turn();
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
