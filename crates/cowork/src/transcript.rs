//! The transcript and the agent's events: on the wire, and folded into a
//! thread.
//!
//! The transcript is everything the agent has been sent and has replied,
//! exactly as sent, as Rig's own messages. Prompts carry their files' content
//! like any other request does.
//!
//! The host forwards what its agent loop reports ([`AgentEvent`]s) as it
//! happens, and every participant, the host included, folds them with the
//! agent crate's [`TurnFold`] into the transcript, which ends up exactly as
//! the loop recorded it: each reply is the content Rig finalized, which the
//! part ends carry. The fold checks the events' order as Rig checks a relayed
//! stream, so a host's events that Rig could not have produced are refused.
//!
//! What an agent message shows ([`AgentOutput`]) is never sent. It is a
//! function of Rig messages alone: the ones its run added to the transcript,
//! followed by the message it is folding, as far as it has come
//! ([`TurnFold::partial`]: the parts Rig has finalized, and a preview of
//! the ones still streaming). That message is the fold of the run's pending
//! events, the events since its output last joined the transcript, which a
//! snapshot carries raw. So someone joining rebuilds it exactly, and every
//! copy shows the same.
//!
//! Live, the transcript part never changes, so only the rest is redone per
//! event: a message rewinds its output to where the committed part ends,
//! adds a message that has just joined the transcript, if any, and then the
//! partial one. An event costs as much as the turn in progress, not the run.

use std::ops::Range;

use agent::{AgentEvent, TurnFold};
use anyhow::Context as _;
use gpui::AppContext;
use gpui_base::TextViewState;
use rig::{
    completion::{
        AssistantContent, Message as RigMessage,
        message::{ToolResultContent, UserContent},
    },
    message::{CallId, ToolCall},
    streaming::StreamEvent,
    tool::Tool as _,
};
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

use crate::{
    protocol::{self, AgentRun, Json, RunOutcome},
    thread::Thread,
    timeline::{
        AgentCommentResponse, AgentMessage, AgentOutput, AgentStep, AgentToolCall, OutputMark,
        StepView, TimelineMessage,
    },
    usage::usage_tokens,
};

impl Json<RigMessage> {
    pub(crate) fn from_rig(message: &RigMessage) -> Self {
        // Rig's messages are plain data, which always encodes.
        Self::from_value(message).expect("a Rig message encodes as JSON")
    }

    pub(crate) fn to_rig(&self) -> anyhow::Result<RigMessage> {
        self.parse()
            .context("failed to decode a transcript message")
    }
}

impl Json<AgentEvent> {
    /// The event as it is sent. A part's end keeps the content Rig
    /// finalized, which is what the reply is made of, even though its
    /// fragments already streamed: rebuilding it from them would be a second
    /// accumulation that could disagree.
    pub(crate) fn shared(event: AgentEvent) -> Self {
        // Rig's events are plain data, which always encodes.
        Self::from_value(&event).expect("an agent event encodes as JSON")
    }

    pub(crate) fn to_agent(&self) -> anyhow::Result<AgentEvent> {
        self.parse().context("failed to decode an agent event")
    }
}

/// The transcript entries each run output, given where each run's prompt is,
/// in timeline order: those after its prompt, up to the next run's. Prompts
/// must be increasing and within the transcript; see [`validate_agent_runs`].
fn run_outputs(prompts: &[usize], transcript_len: usize) -> Vec<Range<usize>> {
    prompts
        .iter()
        .enumerate()
        .map(|(index, prompt)| {
            let end = prompts.get(index + 1).copied().unwrap_or(transcript_len);
            prompt + 1..end
        })
        .collect()
}

/// A fold resuming after a run's output that joined the transcript, `output`
/// of [`run_outputs`]. With no output yet, that is after its prompt.
fn fold_after(transcript: &[RigMessage], output: &Range<usize>) -> TurnFold {
    TurnFold::after(&transcript[output.end - 1])
}

/// Checks that a snapshot's agent messages, in timeline order, describe runs
/// `transcript` can hold: each prompt is a user message there, in order;
/// only the last run can still be generating; and each run's pending events
/// continue its output without completing a message, which would have
/// joined the transcript.
pub(crate) fn validate_agent_runs(
    transcript: &[RigMessage],
    messages: &[&protocol::AgentMessage],
) -> anyhow::Result<()> {
    let prompts = messages
        .iter()
        .map(|message| message.prompt)
        .collect::<Vec<_>>();
    for (index, prompt) in prompts.iter().enumerate() {
        anyhow::ensure!(
            matches!(transcript.get(*prompt), Some(RigMessage::User { .. })),
            "agent message {index}'s prompt is not a user message of the transcript"
        );
        anyhow::ensure!(
            index == 0 || prompts[index - 1] < *prompt,
            "agent message {index}'s prompt comes before the previous one's"
        );
    }
    for (index, (message, output)) in messages
        .iter()
        .zip(run_outputs(&prompts, transcript.len()))
        .enumerate()
    {
        anyhow::ensure!(
            index + 1 == messages.len() || !message.run.is_generating(),
            "agent message {index} is generating, but is not the last"
        );
        let mut fold = fold_after(transcript, &output);
        for (event_index, event) in message.pending_events.iter().enumerate() {
            let invalid =
                || format!("invalid pending event {event_index} of agent message {index}");
            let event = event.to_agent().with_context(invalid)?;
            anyhow::ensure!(
                fold.apply(&event).with_context(invalid)?.message.is_none(),
                "agent message {index}'s pending events complete a message the transcript lacks"
            );
        }
    }
    Ok(())
}

impl Thread {
    /// Folds an event of the agent producing message `message_id` into the
    /// transcript, and shows the message's output as it now stands.
    pub(crate) fn apply_agent_event(
        &mut self,
        message_id: Uuid,
        event: Json<AgentEvent>,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        let decoded = event.to_agent()?;
        let folded = self
            .agent_turn
            .apply(&decoded)
            .context("the host sent an agent event out of order")?;
        let completed = folded.message.is_some();
        self.transcript.extend(folded.message);
        self.measure_agent_event(&decoded);
        let partial = self.agent_turn.partial();
        let Some(message) = agent_message(&mut self.timeline, message_id) else {
            return Ok(());
        };
        if completed {
            message.pending_events.clear();
        } else {
            message.pending_events.push(event);
        }
        let completed = completed.then(|| self.transcript.last()).flatten();
        message.advance(completed, partial.as_ref(), cx);
        self.show_comment_responses(message_id, cx);
        Ok(())
    }

    /// Ends the run producing message `id`. What it streamed that never
    /// joined the transcript stays in the message's pending events.
    pub(crate) fn end_agent_run(
        &mut self,
        id: uuid::Bytes,
        outcome: RunOutcome,
        duration: std::time::Duration,
        cx: &mut impl AppContext,
    ) {
        let partial = self.agent_turn.partial();
        self.generating = false;
        // A request that was stopped or failed before it reported usage
        // never adds its partial output to the transcript.
        self.streamed_bytes = 0;
        self.agent_turn = TurnFold::default();
        let Some(message) = self.agent_message_mut(id) else {
            return;
        };
        message.run = AgentRun::Ended { outcome, duration };
        // The work collapses under its summary, leaving the response.
        message.work_expanded = false;
        message.advance(None, partial.as_ref(), cx);
    }

    /// Shows the output of every agent message of a timeline fresh from a
    /// snapshot, and resumes folding the running one. A run's partial message
    /// is rebuilt by folding its pending events, as it was where they were
    /// folded. The snapshot must have passed [`validate_agent_runs`].
    pub(crate) fn restore_agent_output(&mut self, cx: &mut impl AppContext) {
        let (indices, prompts): (Vec<_>, Vec<_>) = self
            .timeline
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match entry {
                TimelineMessage::Agent(message) => Some((index, message.prompt)),
                TimelineMessage::User(_) => None,
            })
            .unzip();
        let outputs = run_outputs(&prompts, self.transcript.len());
        self.agent_turn = TurnFold::default();
        for (index, output) in indices.into_iter().zip(outputs) {
            let TimelineMessage::Agent(message) = &mut self.timeline[index] else {
                unreachable!("an agent message's index");
            };
            let mut fold = fold_after(&self.transcript, &output);
            for event in &message.pending_events {
                let event = event
                    .to_agent()
                    .expect("pending events validated with the snapshot");
                fold.apply(&event)
                    .expect("pending events validated with the snapshot");
            }
            message.restore(&self.transcript[output], fold.partial().as_ref(), cx);
            let message_id = message.id;
            if message.is_generating() {
                self.agent_turn = fold;
            }
            self.show_comment_responses(message_id, cx);
        }
    }

    /// Counts an event towards the context window estimate; see
    /// [`Thread::live_context_tokens`]. Only live events count: a snapshot
    /// carries the counts.
    fn measure_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Model(
                StreamEvent::Text { text, .. } | StreamEvent::Reasoning { text, .. },
            ) => self.streamed_bytes += text.len() as u64,
            // Each request sends the whole transcript, so its usage is how
            // full the context is.
            AgentEvent::TurnEnded { usage, .. } if usage.is_reported() => {
                self.context_tokens = Some(usage_tokens(*usage));
                self.streamed_bytes = 0;
            }
            _ => {}
        }
    }

    /// Shows the replies to comments that agent message `message_id`'s
    /// `respond_to_comment` calls make, once per comment, checking only the
    /// calls added since it last looked.
    fn show_comment_responses(&mut self, message_id: Uuid, cx: &mut impl AppContext) {
        let Some(message) = self.agent_message_mut(message_id.into_bytes()) else {
            return;
        };
        let calls = message
            .output
            .tool_calls()
            .skip(message.comment_calls_checked)
            .map(|call| call.call.clone())
            .collect::<Vec<_>>();
        message.comment_calls_checked = message.output.tool_calls().count();
        for call in &calls {
            self.show_comment_response(message_id, call, cx);
        }
    }

    fn show_comment_response(
        &mut self,
        message_id: Uuid,
        call: &ToolCall,
        cx: &mut impl AppContext,
    ) {
        // A call whose arguments are not a JSON object never ran.
        if call.function.name.as_str() != RespondToComment::NAME
            || call.function.invalid_arguments.is_some()
        {
            return;
        }
        let Ok(args) =
            serde_json::from_value::<RespondToCommentArgs>(call.function.arguments_value())
        else {
            return;
        };
        if args.response.trim().is_empty() {
            return;
        }
        // The prompt named the submission's comments by their position.
        let comment_group = self.timeline.iter().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if message.id == message_id => message.comment_group_id,
            _ => None,
        });
        let comments = self
            .timeline
            .iter()
            .find_map(|entry| match entry {
                TimelineMessage::User(group) if Some(group.id) == comment_group => {
                    Some(group.comments.iter().map(|comment| comment.id).collect())
                }
                _ => None,
            })
            .unwrap_or_else(Vec::new);
        let Some(comment_id) = TurnComments::new(comments.len())
            .comment_ids()
            .iter()
            .position(|alias| alias.as_str() == args.comment_id)
            .and_then(|index| comments.get(index).copied())
        else {
            return;
        };
        let Some(message) = self.agent_message_mut(message_id.into_bytes()) else {
            return;
        };
        if message
            .comment_responses
            .iter()
            .any(|response| response.comment_id == comment_id)
        {
            return;
        }
        message.comment_responses.push(AgentCommentResponse {
            // Derived rather than random, so every participant names the
            // reply alike and comments on it reach the same one.
            id: Uuid::new_v5(&message_id, call.id.to_string().as_bytes()),
            comment_id,
            response_view: cx.new(|cx| TextViewState::markdown(&args.response, cx)),
            response: args.response,
        });
    }
}

impl AgentOutput {
    /// Adds what `message`, the next message of a run's output, shows.
    fn push(&mut self, message: &RigMessage) {
        match message {
            RigMessage::Assistant(reply) => {
                for part in &reply.content {
                    match part {
                        AssistantContent::Reasoning(reasoning) => {
                            self.push_thinking(&reasoning.text);
                        }
                        AssistantContent::Text(text) => self.push_text(&text.text),
                        AssistantContent::ToolCall(call) => {
                            self.steps.push(AgentStep::ToolCall(AgentToolCall {
                                call: call.clone(),
                                result: None,
                            }));
                        }
                        AssistantContent::Image(_) | AssistantContent::Opaque(_) => {}
                    }
                }
            }
            RigMessage::User { content } => {
                for item in content {
                    if let UserContent::ToolResult(result) = item {
                        self.record_tool_result(&result.call, &result.content);
                    }
                }
            }
            RigMessage::System { .. } => {}
        }
    }

    /// Continues the thinking the output ends with, or starts new thinking
    /// if it ends with something else.
    fn push_thinking(&mut self, text: &str) {
        match self.steps.last_mut() {
            Some(AgentStep::Thinking(thinking)) => thinking.push_str(text),
            _ if text.is_empty() => {}
            _ => self.steps.push(AgentStep::Thinking(text.to_owned())),
        }
    }

    /// Continues the text the output ends with, or starts new text.
    fn push_text(&mut self, text: &str) {
        match self.steps.last_mut() {
            Some(AgentStep::Text(existing)) => existing.push_str(text),
            _ if text.is_empty() => {}
            _ => self.steps.push(AgentStep::Text(text.to_owned())),
        }
    }

    fn extend(&mut self, message: Option<&RigMessage>) {
        if let Some(message) = message {
            self.push(message);
        }
    }

    fn tool_calls_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut AgentToolCall> {
        self.steps.iter_mut().filter_map(|step| match step {
            AgentStep::ToolCall(call) => Some(call),
            AgentStep::Thinking(_) | AgentStep::Text(_) => None,
        })
    }

    /// Records what the tool returned for `call`. Results answer the latest
    /// reply, and Rig may mint the same id for id-less calls of different
    /// replies, so the latest call with that id is the one answered.
    fn record_tool_result(&mut self, call: &CallId, content: &[ToolResultContent]) {
        if let Some(tool_call) = self
            .tool_calls_mut()
            .rfind(|tool_call| tool_call.call.id == *call)
        {
            tool_call.result = Some(content.to_vec());
        };
    }

    /// Where the output ends now, as its committed part.
    fn mark(&self) -> OutputMark {
        let answered = self
            .tool_calls()
            .take_while(|call| call.result.is_some())
            .count();
        debug_assert!(
            self.tool_calls()
                .skip(answered)
                .all(|call| call.result.is_none()),
            "committed calls are answered in order"
        );
        OutputMark {
            steps: self.steps.len(),
            tail: match self.steps.last() {
                Some(AgentStep::Thinking(text) | AgentStep::Text(text)) => text.len(),
                _ => 0,
            },
            answered,
        }
    }

    /// Drops everything after `mark`, the committed part: what the message a
    /// run is folding added, including results for committed calls.
    fn rewind(&mut self, mark: OutputMark) {
        self.steps.truncate(mark.steps);
        if let Some(AgentStep::Thinking(text) | AgentStep::Text(text)) = self.steps.last_mut() {
            text.truncate(mark.tail);
        }
        for call in self.tool_calls_mut().skip(mark.answered) {
            call.result = None;
        }
    }

    /// Whether `partial`, the message a run is folding, shows the model
    /// still reasoning: it is the reply streaming, ending in reasoning that
    /// has some text, which is then the output's last step.
    fn still_reasoning(partial: Option<&RigMessage>) -> bool {
        matches!(
            partial,
            Some(RigMessage::Assistant(reply))
                if matches!(
                    reply.content.last(),
                    Some(AssistantContent::Reasoning(reasoning))
                        if !reasoning.text.is_empty()
                )
        )
    }
}

impl AgentMessage {
    /// Shows a run's output after its fold moved on: `completed`, a message
    /// it has just added to the transcript, if any, then `partial`, the
    /// message it is folding. Only the part after the committed mark is
    /// redone, and the text views only get what that part added.
    fn advance(
        &mut self,
        completed: Option<&RigMessage>,
        partial: Option<&RigMessage>,
        cx: &mut impl AppContext,
    ) {
        let from = self.committed;
        // Only the last committed step, if it is thinking or text, and the
        // steps after it can change what they show.
        let first_changed = from.steps.saturating_sub(1);
        let old_texts = self.output.steps[first_changed..]
            .iter()
            .map(|step| match step {
                AgentStep::Thinking(text) | AgentStep::Text(text) => Some(text.clone()),
                AgentStep::ToolCall(_) => None,
            })
            .collect::<Vec<_>>();
        self.output.rewind(from);
        if let Some(completed) = completed {
            self.output.push(completed);
            self.committed = self.output.mark();
        }
        self.output.extend(partial);
        self.set_thinking_complete(partial);
        self.show_steps(first_changed, &old_texts, cx);
        let response = self.output.response();
        let old_response = std::mem::replace(&mut self.output.text, response);
        show_tail(&self.text_view, &self.output.text, 0, &old_response, cx);
    }

    /// Brings the step views, which showed steps `..first` as they are now
    /// followed by steps whose thinking or text was `old`, to showing
    /// `output.steps`. A step that stays what it was keeps its view state.
    fn show_steps(&mut self, first: usize, old: &[Option<String>], cx: &mut impl AppContext) {
        self.step_views.truncate(self.output.steps.len());
        for (index, step) in self.output.steps.iter().enumerate().skip(first) {
            match (self.step_views.get_mut(index), step) {
                (Some(StepView::Thinking { view, .. }), AgentStep::Thinking(text))
                | (Some(StepView::Text { view }), AgentStep::Text(text)) => {
                    match old.get(index - first) {
                        Some(Some(old)) => show_tail(view, text, 0, old, cx),
                        _ => view.update(cx, |view, cx| view.set_text(text, cx)),
                    }
                }
                (Some(StepView::ToolCall { .. }), AgentStep::ToolCall(_)) => {}
                (Some(shown), step) => *shown = StepView::new(step, cx),
                (None, step) => self.step_views.push(StepView::new(step, cx)),
            }
        }
    }

    /// Shows `output`, a run's transcript entries, followed by `partial`, the
    /// message it is folding, in a message fresh from a snapshot. The text
    /// views are shown whole, as history rather than as a stream.
    fn restore(
        &mut self,
        output: &[RigMessage],
        partial: Option<&RigMessage>,
        cx: &mut impl AppContext,
    ) {
        for message in output {
            self.output.push(message);
        }
        self.committed = self.output.mark();
        self.output.extend(partial);
        self.set_thinking_complete(partial);
        self.output.text = self.output.response();
        self.step_views = self
            .output
            .steps
            .iter()
            .map(|step| StepView::new(step, cx))
            .collect();
        self.text_view = cx.new(|cx| TextViewState::markdown(&self.output.text, cx));
    }

    /// Thinking is complete unless the model is still reasoning. Thinking in
    /// progress cannot be expanded by hand, so it completes collapsed.
    fn set_thinking_complete(&mut self, partial: Option<&RigMessage>) {
        self.output.thinking_complete =
            !(self.is_generating() && AgentOutput::still_reasoning(partial));
    }
}

/// Brings `view` from showing `new[..from]` followed by `old_tail` to showing
/// `new`, appending when `new` extends that, as it does while a run streams.
fn show_tail(
    view: &gpui::Entity<TextViewState>,
    new: &str,
    from: usize,
    old_tail: &str,
    cx: &mut impl AppContext,
) {
    match new[from..].strip_prefix(old_tail) {
        Some("") => {}
        Some(added) => view.update(cx, |view, cx| view.push_str(added, cx)),
        None => view.update(cx, |view, cx| view.set_text(new, cx)),
    }
}

/// Agent message `id` of `timeline`, borrowed apart from the thread's other
/// fields.
fn agent_message(timeline: &mut [TimelineMessage], id: Uuid) -> Option<&mut AgentMessage> {
    timeline.iter_mut().find_map(|entry| match entry {
        TimelineMessage::Agent(message) if message.id == id => Some(message),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rig::message::{
        AssistantMessage, ImageMediaType, Origin, Reasoning, StopReason, ToolCall, ToolFunction,
        ToolName, UserContent,
    };
    use serde_json::json;

    /// A prompt with an image, a reply with reasoning and a tool call, and
    /// the tool's result come back from the wire exactly as they were
    /// recorded.
    #[test]
    fn messages_round_trip_exactly() {
        let call_id = CallId::from_wire("call_1");
        let tool = ToolName::new("respond_to_comment").expect("a valid name");
        let messages = [
            RigMessage::User {
                content: vec![
                    UserContent::text("Ada:\nWhat is this?"),
                    UserContent::image_base64("AQID", Some(ImageMediaType::PNG), None),
                ],
            },
            RigMessage::Assistant(AssistantMessage {
                content: vec![
                    AssistantContent::Reasoning(Reasoning::new("Look it up first.")),
                    AssistantContent::text("Checking."),
                    AssistantContent::ToolCall(ToolCall::new(
                        call_id.clone(),
                        ToolFunction::new(
                            tool.clone(),
                            json!({"comment_id": "comment_1", "response": "Yes", "n": 1.5}),
                        ),
                    )),
                ],
                origin: Some(Origin::new("ollama.chat", "ollama", "qwen")),
                stop: Some(StopReason::ToolUse),
            }),
            RigMessage::tool_result(call_id, tool, "Recorded"),
        ];
        for message in messages {
            let wire = postcard::to_stdvec(&Json::from_rig(&message)).expect("encode");
            let received: Json<RigMessage> = postcard::from_bytes(&wire).expect("decode");
            assert_eq!(received.to_rig().expect("a message"), message);
        }
    }
}
