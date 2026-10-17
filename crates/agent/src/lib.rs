//! Rig's agent runs, reported as events anyone can fold into the history.
//!
//! [`run`] drives a Rig [`AgentRunner`] and reports what it does as
//! [`AgentEvent`]s, as it happens: every event of the model's stream, each
//! tool's result as soon as that tool returns, and each model turn's end.
//! Completed tool calls stream as soon as their parts close, while tools run
//! only after the complete model turn, keeping the assistant message and its
//! tool results in a valid history order.
//!
//! A run is fully described by its events: folding them with a [`TurnFold`]
//! rebuilds exactly the messages Rig commits to the run's history. Events are
//! serializable one at a time, so they can be sent elsewhere as they happen
//! and folded there with the same result.
//!
//! Each reply is the content the stream's part ends carry, in the parts'
//! positions, with the origin and stop Rig gave the turn; each tool result is
//! the message Rig commits for the call.

use std::collections::{BTreeMap, HashMap, HashSet};

use futures::{StreamExt as _, channel::mpsc};
use rig::{
    AgentRunner,
    agent::{AgentHook, HookContext, MultiTurnStreamItem, OutcomeAction, OutcomeEvent},
    completion::{AssistantContent, Message, Usage},
    message::{self, AssistantMessage, CallId, Origin, StopReason, ToolCall, UserContent},
    streaming::{Item, SequenceError, StreamEvent, Transcript},
    tool::ToolResult,
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
    /// A tool the last reply called returned, with the result the model
    /// reads. The call itself is in the reply.
    ToolResult(message::ToolResult),
}

impl AgentEvent {
    /// The report that `call`, of the tool `name`, returned `result`, which
    /// the model reads as Rig commits it.
    pub fn tool_result(call: CallId, name: message::ToolName, result: &ToolResult) -> Self {
        match rig::transcript::tool_result_output(call, name, result) {
            UserContent::ToolResult(result) => Self::ToolResult(result),
            _ => unreachable!("a tool's output is a tool result"),
        }
    }
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
            AgentEvent::ToolResult(result) => {
                let waiting = self.calls.iter().any(|call| call.id == result.call);
                let answered = self.results.iter().any(
                    |answered| matches!(answered, UserContent::ToolResult(answered) if answered.call == result.call),
                );
                if !waiting || answered {
                    return Ok(Folded::default());
                }
                self.results.push(UserContent::ToolResult(result.clone()));
                if self.results.len() < self.calls.len() {
                    return Ok(Folded::default());
                }
                // Results arrive as their tools return; Rig commits them in
                // the order the reply made the calls.
                let position = |result: &UserContent| match result {
                    UserContent::ToolResult(result) => {
                        self.calls.iter().position(|call| call.id == result.call)
                    }
                    _ => None,
                };
                self.results.sort_by_key(position);
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

/// Runs `runner` to its end, reporting what it does through `emit`, which is
/// called synchronously and should do little work itself; forwarding events
/// to a channel is a good default.
///
/// Rig's stream carries the model's events, but not a turn's origin and stop
/// reason, and tool results only once every call of a turn has returned. A
/// hook added to `runner` observes those as they happen instead, in the
/// order Rig settles them.
pub async fn run(runner: AgentRunner, mut emit: impl FnMut(AgentEvent)) -> anyhow::Result<()> {
    let (observed, mut observations) = mpsc::unbounded();
    let mut stream = runner.add_hook(Observer(observed)).stream().fuse();
    let mut relay = Relay::default();
    loop {
        let item = futures::select_biased! {
            event = observations.select_next_some() => {
                relay.observed(event, &mut emit);
                continue;
            }
            item = stream.next() => item,
        };
        // What the hook observed while Rig produced `item` happened before it.
        while let Ok(event) = observations.try_recv() {
            relay.observed(event, &mut emit);
        }
        match item {
            None | Some(Ok(MultiTurnStreamItem::FinalResponse(_))) => return Ok(()),
            Some(Ok(item)) => relay.item(item, &mut emit),
            Some(Err(error)) => return Err(error.into()),
        }
    }
}

/// Reports what [`Observer`] sees as it happens: each model turn's end, and
/// each tool's result as soon as that tool returns.
struct Observer(mpsc::UnboundedSender<AgentEvent>);

impl AgentHook for Observer {
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        let observed = if let Some(response) = event.completion() {
            let head = response.head();
            Some(AgentEvent::TurnEnded {
                origin: head.origin,
                stop: head.stop,
                usage: response.usage,
            })
        } else {
            match (event.call_id, event.tool_name(), event.tool_result()) {
                (Some(call), Some(name), Some(result)) => message::ToolName::new(name)
                    .ok()
                    .map(|name| AgentEvent::tool_result(call.clone(), name, result)),
                _ => None,
            }
        };
        if let Some(event) = observed {
            _ = self.0.unbounded_send(event);
        }
        OutcomeAction::Proceed
    }
}

/// Turns Rig's stream items and what the hook observed into events, each
/// once.
#[derive(Default)]
struct Relay {
    /// The calls of the turn routed to execution, by id.
    calls: HashMap<CallId, ToolCall>,
    /// The calls whose results were reported.
    answered: HashSet<CallId>,
}

impl Relay {
    fn observed(&mut self, event: AgentEvent, emit: &mut impl FnMut(AgentEvent)) {
        match &event {
            AgentEvent::TurnEnded { .. } => {
                self.calls.clear();
                self.answered.clear();
            }
            AgentEvent::ToolResult(result) => {
                self.answered.insert(result.call.clone());
            }
            AgentEvent::Model(_) => {}
        }
        emit(event);
    }

    fn item(&mut self, item: MultiTurnStreamItem, emit: &mut impl FnMut(AgentEvent)) {
        match item {
            MultiTurnStreamItem::StreamAssistantItem(Item::Event(event)) => {
                emit(AgentEvent::Model(event));
            }
            MultiTurnStreamItem::ToolCall { tool_call } => {
                self.calls.insert(tool_call.id.clone(), tool_call);
            }
            // Rig answers some calls without dispatching them, as one whose
            // arguments are not a JSON object; no hook sees those results.
            MultiTurnStreamItem::ToolResult { tool_result }
                if self.calls.contains_key(&tool_result.call)
                    && self.answered.insert(tool_result.call.clone()) =>
            {
                emit(AgentEvent::ToolResult(tool_result));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
