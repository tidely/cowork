//! The transcript and the agent's events: on the wire, and folded into a
//! thread.
//!
//! The transcript records ordinary agent runs exactly as sent, as Rig's own
//! messages. Prompts carry their files' content like any other request does.
//! Accepted compaction summaries are entries in the same ordered sequence;
//! they replace request context without rewriting the original messages.
//!
//! The host forwards what its agent loop reports ([`AgentEvent`]s) as it
//! happens, and every participant, the host included, folds them with the
//! agent crate's [`TurnFold`] into the transcript, which ends up exactly as
//! the loop recorded it: each reply is the content Rig finalized, which the
//! part ends carry. The fold checks the events' order as Rig checks a relayed
//! stream, so a host's events that Rig could not have produced are refused.
//!
//! What an agent message shows ([`AgentOutput`]) is never sent. It is a
//! function of transcript entries: the ones its run added to the transcript,
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
    completion::Message as RigMessage,
    message::{CallId, ToolCall},
    streaming::StreamEvent,
    tool::Tool as _,
};
use serde::{Deserialize, Serialize};
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

use crate::{
    protocol::{self, AgentRun, Json, RunOutcome},
    thread::Thread,
    timeline::{AgentCommentResponse, AgentMessage, TimelineMessage},
    usage::usage_tokens,
};

/// An absolute position in the append-only sequence of transcript entries.
/// Compaction never changes these positions, and UUIDs identify runs rather
/// than individual messages in this sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TranscriptIndex(pub(crate) u64);

/// A summary replaces everything before its position for future requests,
/// while the original messages remain available for display and replay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum TranscriptEntry {
    Message(RigMessage),
    Compaction(String),
}

/// LLM state only: original messages and successive summaries of their prefix.
/// Run identities, unfinished events, and local views belong to the timeline.
///
/// All messages remain resident today. Offloading could add a
/// `start: TranscriptIndex`, the absolute position of `entries[0]`, keeping a
/// contiguous tail. An absolute position would then map to the resident vector
/// at `index - start`; removing a persisted prefix would advance `start` without
/// changing run references or summary positions. Missing ranges would need
/// loading before access, and the absolute end would be `start + entries.len()`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Transcript {
    pub(crate) entries: Vec<TranscriptEntry>,
}

impl Transcript {
    pub(crate) fn end(&self) -> TranscriptIndex {
        TranscriptIndex(self.entries.len() as u64)
    }

    pub(crate) fn append(&mut self, message: RigMessage) {
        self.entries.push(TranscriptEntry::Message(message));
    }

    #[cfg(test)]
    pub(crate) fn latest_compaction(&self) -> Option<&String> {
        self.entries.iter().rev().find_map(|entry| match entry {
            TranscriptEntry::Compaction(summary) => Some(summary),
            TranscriptEntry::Message(_) => None,
        })
    }

    /// A run's leading summaries precede its prompt and submitted user group.
    pub(crate) fn leading_compactions(&self, start: TranscriptIndex) -> Range<usize> {
        let start = start.0 as usize;
        let count = self.entries[start..]
            .iter()
            .take_while(|entry| matches!(entry, TranscriptEntry::Compaction(_)))
            .count();
        start..start + count
    }

    pub(crate) fn has_prompt(&self, start: TranscriptIndex) -> bool {
        self.leading_compactions(start).end < self.entries.len()
    }

    /// Coverage follows from append order; summaries cannot name an older or
    /// future prefix. Repeated summaries are distinct entries.
    pub(crate) fn install_compaction(&mut self, summary: String) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.entries.is_empty() && !summary.trim().is_empty(),
            "invalid compacted context"
        );
        self.entries.push(TranscriptEntry::Compaction(summary));
        Ok(())
    }

    pub(crate) fn request_history(&self) -> Vec<RigMessage> {
        let start = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry, TranscriptEntry::Compaction(_)))
            .unwrap_or(0);
        self.entries[start..]
            .iter()
            .map(|entry| match entry {
                TranscriptEntry::Message(message) => message.clone(),
                TranscriptEntry::Compaction(summary) => {
                    RigMessage::user(format!("Summary of the conversation so far:\n{summary}"))
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn messages(&self) -> Vec<RigMessage> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Message(message) => Some(message.clone()),
                TranscriptEntry::Compaction(_) => None,
            })
            .collect()
    }

    pub(crate) fn to_protocol(&self) -> Json<Self> {
        Json::from_value(self).expect("a transcript encodes as JSON")
    }
}

impl Json<Transcript> {
    /// Decode the whole LLM state once and validate all prefix summaries before
    /// a snapshot is installed. JSON validity alone does not establish coverage.
    pub(crate) fn to_transcript(&self) -> anyhow::Result<Transcript> {
        let transcript = self.parse().context("invalid transcript from host")?;
        for (index, entry) in transcript.entries.iter().enumerate() {
            if let TranscriptEntry::Compaction(summary) = entry {
                anyhow::ensure!(
                    index > 0 && !summary.trim().is_empty(),
                    "invalid compacted context"
                );
            }
        }
        Ok(transcript)
    }
}

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

impl Json<CallId> {
    /// A call id as it is sent. Through JSON rather than its text, since an
    /// id rig minted cannot be rebuilt from its text.
    pub(crate) fn call_id(call: &CallId) -> Self {
        // A call id is plain data, which always encodes.
        Self::from_value(call).expect("a call id encodes as JSON")
    }

    pub(crate) fn to_call_id(&self) -> anyhow::Result<CallId> {
        self.parse().context("failed to decode a tool call id")
    }
}

/// Each run owns its entries up to the next run's start. Leading summaries
/// belong above its user group; output starts after its first ordinary entry,
/// the prompt. Only a preparing run can lack that prompt.
fn run_output(transcript: &[TranscriptEntry], range: Range<usize>) -> Option<Range<usize>> {
    transcript[range.clone()]
        .iter()
        .position(|entry| matches!(entry, TranscriptEntry::Message(_)))
        .map(|prompt| range.start + prompt + 1..range.end)
}

/// A fold resuming after a run's output that joined the transcript, `output`
/// of [`run_output`]. With no output yet, that is after its prompt.
fn fold_after(transcript: &[TranscriptEntry], output: &Range<usize>) -> TurnFold {
    let message = transcript[..output.end]
        .iter()
        .rev()
        .find_map(|entry| match entry {
            TranscriptEntry::Message(message) => Some(message),
            TranscriptEntry::Compaction(_) => None,
        })
        .expect("a run has a recorded prompt");
    TurnFold::after(message)
}

/// Checks that a snapshot's agent messages, in timeline order, describe runs
/// `transcript` can hold: each prompt is a user message there, in order;
/// only the last run can still be generating; and each run's pending events
/// continue its output without completing a message, which would have
/// joined the transcript.
pub(crate) fn validate_agent_runs(
    transcript: &[TranscriptEntry],
    messages: &[&protocol::AgentMessage],
    compacting: bool,
) -> anyhow::Result<()> {
    let starts = messages
        .iter()
        .map(|message| {
            usize::try_from(message.start.0).context("transcript index does not fit in memory")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (index, start) in starts.iter().enumerate() {
        anyhow::ensure!(
            *start <= transcript.len() && (index == 0 || starts[index - 1] < *start),
            "agent message {index}'s start is outside the transcript or out of order"
        );
    }
    for (index, message) in messages.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(transcript.len());
        let output = run_output(transcript, starts[index]..end);
        anyhow::ensure!(
            index + 1 == messages.len() || !message.run.is_generating(),
            "agent message {index} is generating, but is not the last"
        );
        let mut fold = if let Some(output) = output {
            anyhow::ensure!(
                matches!(
                    transcript.get(output.start - 1),
                    Some(TranscriptEntry::Message(RigMessage::User { .. }))
                ),
                "agent message {index}'s prompt is not a user message of the transcript"
            );
            fold_after(transcript, &output)
        } else {
            anyhow::ensure!(
                message.run.is_generating(),
                "only the active run can await its prompt"
            );
            anyhow::ensure!(
                message.pending_events.is_empty()
                    && message.awaiting_approval.is_none()
                    && message.steering.is_empty(),
                "an unstarted run has output"
            );
            TurnFold::default()
        };
        for (event_index, event) in message.pending_events.iter().enumerate() {
            let invalid =
                || format!("invalid pending event {event_index} of agent message {index}");
            let event = event.to_agent().with_context(invalid)?;
            anyhow::ensure!(
                fold.apply(&event).with_context(invalid)?.message.is_none(),
                "agent message {index}'s pending events complete a message the transcript lacks"
            );
        }
        if let Some(call) = &message.awaiting_approval {
            let call = call.to_call_id()?;
            anyhow::ensure!(
                message.run.is_generating() && is_pending(&fold, &call),
                "agent message {index} awaits approval of a call that is not waiting"
            );
        }
        if compacting && message.run.is_generating() {
            anyhow::ensure!(
                message.awaiting_approval.is_none()
                    && fold.pending_calls().is_empty()
                    && fold.partial().is_none(),
                "compaction is not at a request boundary"
            );
        }
    }
    Ok(())
}

/// Whether `call` is one of the calls `fold` is waiting for the results of.
fn is_pending(fold: &TurnFold, call: &CallId) -> bool {
    fold.pending_calls()
        .iter()
        .any(|pending| pending.id == *call)
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
        let start = self
            .agent_message_mut(message_id.into_bytes())
            .context("event has no run")?
            .record
            .start;
        anyhow::ensure!(
            self.transcript.has_prompt(start),
            "run has no recorded prompt"
        );
        let decoded = event.to_agent()?;
        let folded = self
            .active_turn(message_id)?
            .apply(&decoded)
            .context("the host sent an agent event out of order")?;
        let completed = folded.message.is_some();
        if let Some(message) = folded.message {
            self.transcript.append(message);
        }
        self.measure_agent_event(&decoded);
        let partial = self.active_turn(message_id)?.partial();
        let Some(message) = agent_message(&mut self.timeline, message_id) else {
            return Ok(());
        };
        if completed {
            message.record.pending_events.clear();
        } else {
            message.record.pending_events.push(event);
        }
        // A call that returned no longer waits, however the host decided.
        if let AgentEvent::ToolResult { call, .. } = &decoded
            && message.record.awaiting_approval.as_ref() == Some(call)
        {
            message.record.awaiting_approval = None;
        }
        let completed = completed.then(|| match self.transcript.entries.last().unwrap() {
            TranscriptEntry::Message(message) => message,
            _ => unreachable!(),
        });
        let generating = message.is_generating();
        message
            .ui
            .advance(completed, partial.as_ref(), generating, cx);
        if self.run_activity(message_id) == Some(crate::timeline::RunActivity::AwaitingApproval)
            && self
                .agent_message_mut(message_id.into_bytes())
                .is_some_and(|run| run.record.awaiting_approval.is_none())
        {
            self.approval_activity(message_id, None)?;
        }
        self.show_comment_responses(message_id, cx);
        Ok(())
    }

    /// Marks `call` as waiting for approval in the run producing message
    /// `message_id`, refusing a call the run is not waiting on.
    pub(crate) fn await_tool_approval(
        &mut self,
        message_id: Uuid,
        call: CallId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            is_pending(self.active_turn(message_id)?, &call),
            "the host asked to approve a call that is not waiting for its result"
        );
        self.approval_activity(message_id, Some(&call))?;
        let message = agent_message(&mut self.timeline, message_id)
            .filter(|message| message.is_generating())
            .context("the host asked to approve a call of a run that is not going")?;
        message.record.awaiting_approval = Some(call);
        Ok(())
    }

    /// Stops showing `call` as waiting for approval; its result follows.
    pub(crate) fn resolve_tool_approval(&mut self, message_id: Uuid, call: &CallId) {
        if let Some(message) = agent_message(&mut self.timeline, message_id)
            && message.record.awaiting_approval.as_ref() == Some(call)
        {
            message.record.awaiting_approval = None;
            _ = self.approval_activity(message_id, None);
        }
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
        let Some(turn) = self.take_active_turn(Uuid::from_bytes(id)) else {
            return;
        };
        let partial = turn.partial();
        // A request that was stopped or failed before it reported usage
        // never adds its partial output to the transcript.
        self.streamed_bytes = 0;
        let Some(message) = self.agent_message_mut(id) else {
            return;
        };
        message.record.run = AgentRun::Ended { outcome, duration };
        // A run that ends no longer waits for anything.
        message.record.awaiting_approval = None;
        // The work collapses under its summary, leaving the response.
        message.ui.work_expanded = false;
        message.ui.advance(None, partial.as_ref(), false, cx);
    }

    /// Shows the output of every agent message of a timeline fresh from a
    /// snapshot, and resumes folding the running one. A run's partial message
    /// is rebuilt by folding its pending events, as it was where they were
    /// folded. The snapshot must have passed [`validate_agent_runs`].
    pub(crate) fn restore_agent_output(&mut self, compacting: bool, cx: &mut impl AppContext) {
        let mut runs = self
            .timeline
            .iter_mut()
            .filter_map(|entry| match entry {
                TimelineMessage::Agent(message) => Some(message),
                TimelineMessage::User(_) => None,
            })
            .peekable();
        let mut active = None;
        while let Some(message) = runs.next() {
            let end = runs.peek().map_or(self.transcript.entries.len(), |next| {
                next.record.start.0 as usize
            });
            let output = run_output(
                &self.transcript.entries,
                message.record.start.0 as usize..end,
            );
            let mut fold = output
                .as_ref()
                .map(|output| fold_after(&self.transcript.entries, output))
                .unwrap_or_default();
            for event in &message.record.pending_events {
                let event = event
                    .to_agent()
                    .expect("pending events validated with the snapshot");
                fold.apply(&event)
                    .expect("pending events validated with the snapshot");
            }
            let generating = message.is_generating();
            let output =
                output.unwrap_or(self.transcript.entries.len()..self.transcript.entries.len());
            message.ui.restore(
                &self.transcript.entries[output.clone()],
                fold.partial().as_ref(),
                generating,
                cx,
            );
            let message_id = message.record.id;
            if message.is_generating() {
                active = Some((message_id, fold));
            }
        }
        self.restore_execution(active, compacting);
        // Comment replies can refer to other timeline entries, so reconcile
        // them after releasing the mutable run traversal.
        for index in 0..self.timeline.len() {
            if let TimelineMessage::Agent(message) = &self.timeline[index] {
                self.show_comment_responses(message.record.id, cx);
            }
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
            // Each request sends the current request context (possibly a
            // summary plus newer messages), so its usage measures fullness.
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
            .ui
            .output
            .tool_calls()
            .skip(message.ui.comment_calls_checked)
            .map(|call| call.call.clone())
            .collect::<Vec<_>>();
        message.ui.comment_calls_checked = message.ui.output.tool_calls().count();
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
            TimelineMessage::Agent(message) if message.record.id == message_id => {
                message.record.comment_group_id
            }
            _ => None,
        });
        let comments = self
            .timeline
            .iter()
            .find_map(|entry| match entry {
                TimelineMessage::User(group) if Some(group.record.id) == comment_group => Some(
                    group
                        .record
                        .comments
                        .iter()
                        .map(|comment| comment.id)
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_else(Vec::new);
        let initial_comment_id = TurnComments::new(comments.len())
            .comment_ids()
            .iter()
            .position(|alias| alias.as_str() == args.comment_id)
            .and_then(|index| comments.get(index).copied());
        let Some(comment_id) = initial_comment_id.or_else(|| {
            self.steered_comments.get(&message_id).and_then(|aliases| {
                aliases
                    .iter()
                    .find(|(alias, _)| alias == &args.comment_id)
                    .map(|(_, id)| *id)
            })
        }) else {
            return;
        };
        let Some(message) = self.agent_message_mut(message_id.into_bytes()) else {
            return;
        };
        if message
            .ui
            .comment_responses
            .iter()
            .any(|response| response.comment_id == comment_id)
        {
            return;
        }
        message.ui.comment_responses.push(AgentCommentResponse {
            // Derived rather than random, so every participant names the
            // reply alike and comments on it reach the same one.
            id: Uuid::new_v5(&message_id, call.id.to_string().as_bytes()),
            comment_id,
            response_view: cx.new(|cx| TextViewState::markdown(&args.response, cx)),
            response: args.response,
        });
    }
}

/// Agent message `id` of `timeline`, borrowed apart from the thread's other
/// fields.
fn agent_message(timeline: &mut [TimelineMessage], id: Uuid) -> Option<&mut AgentMessage> {
    timeline.iter_mut().find_map(|entry| match entry {
        TimelineMessage::Agent(message) if message.record.id == id => Some(message),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::timeline::AgentOutput;
    use rig::completion::AssistantContent;
    use rig::message::{
        AssistantMessage, ImageMediaType, Origin, Reasoning, StopReason, ToolCall, ToolFunction,
        ToolName, UserContent,
    };
    use serde_json::json;

    #[test]
    fn compactions_fold_in_order_and_separate_adjacent_text() {
        use crate::timeline::AgentStep;
        let mut transcript = Transcript::default();
        transcript.append(RigMessage::user("Request"));
        transcript.append(RigMessage::assistant("Before"));
        transcript
            .install_compaction("First summary".into())
            .unwrap();
        transcript
            .install_compaction("Second summary".into())
            .unwrap();
        transcript.append(RigMessage::assistant("After"));
        let output = AgentOutput::from_transcript(&transcript.entries[1..]);
        assert_eq!(
            output.steps(),
            &[
                AgentStep::Text("Before".into()),
                AgentStep::Compaction,
                AgentStep::Compaction,
                AgentStep::Text("After".into()),
            ]
        );
        assert_eq!(
            transcript.request_history(),
            vec![
                RigMessage::user("Summary of the conversation so far:\nSecond summary"),
                RigMessage::assistant("After"),
            ]
        );
    }

    #[test]
    fn summaries_precede_the_next_prompt_and_preserve_original_messages() {
        let mut transcript = Transcript::default();
        transcript.append(RigMessage::user("Original request"));
        transcript.append(RigMessage::assistant("Original answer"));
        transcript
            .install_compaction("Earlier conversation".into())
            .unwrap();
        transcript
            .install_compaction("Everything so far".into())
            .unwrap();
        let prompt = transcript.end();
        transcript.append(RigMessage::user("Continue"));
        assert_eq!(prompt, TranscriptIndex(4));
        assert_eq!(transcript.leading_compactions(TranscriptIndex(2)), 2..4);
        assert_eq!(run_output(&transcript.entries, 0..2), Some(1..2));
        assert_eq!(run_output(&transcript.entries, 2..5), Some(5..5));
        assert_eq!(
            transcript.messages(),
            vec![
                RigMessage::user("Original request"),
                RigMessage::assistant("Original answer"),
                RigMessage::user("Continue"),
            ]
        );
        assert_eq!(
            transcript.request_history(),
            vec![
                RigMessage::user("Summary of the conversation so far:\nEverything so far"),
                RigMessage::user("Continue"),
            ]
        );
        let bytes = postcard::to_stdvec(&transcript.to_protocol()).unwrap();
        let wire: Json<Transcript> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(wire.to_transcript().unwrap(), transcript);
    }

    #[test]
    fn empty_or_leading_summaries_are_rejected() {
        let mut transcript = Transcript::default();
        assert!(transcript.install_compaction("No history".into()).is_err());
        transcript.append(RigMessage::user("Request"));
        let original = transcript.clone();
        assert!(transcript.install_compaction("  ".into()).is_err());
        assert_eq!(transcript, original);
        for entries in [
            vec![TranscriptEntry::Compaction("No history".into())],
            vec![
                TranscriptEntry::Message(RigMessage::user("Request")),
                TranscriptEntry::Compaction("  ".into()),
            ],
        ] {
            let invalid = Transcript { entries };
            assert!(invalid.to_protocol().to_transcript().is_err());
        }
    }

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
            RigMessage::Assistant(
                AssistantMessage::new(vec![
                    AssistantContent::Reasoning(Reasoning::new("Look it up first.")),
                    AssistantContent::text("Checking."),
                    AssistantContent::ToolCall(ToolCall::new(
                        call_id.clone(),
                        ToolFunction::new(
                            tool.clone(),
                            json!({"comment_id": "comment_1", "response": "Yes", "n": 1.5}),
                        ),
                    )),
                ])
                .with_origin(Origin::new("ollama.chat", "ollama", "qwen"))
                .with_stop(StopReason::ToolUse),
            ),
            RigMessage::tool_result(call_id, tool, "Recorded"),
        ];
        for message in messages {
            let wire = postcard::to_stdvec(&Json::from_rig(&message)).expect("encode");
            let received: Json<RigMessage> = postcard::from_bytes(&wire).expect("decode");
            assert_eq!(received.to_rig().expect("a message"), message);
        }
    }
}
