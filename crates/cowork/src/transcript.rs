//! The transcript and the agent's events: on the wire, and folded into a
//! thread.
//!
//! The transcript records ordinary agent runs exactly as sent, as Rig's own
//! messages. Prompts carry their files' content like any other request does.
//! Compaction's separate summary request is kept as request-context metadata;
//! it never rewrites the transcript used to render those runs.
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
use serde::{Deserialize, Serialize};
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

use crate::{
    protocol::{self, AgentRun, Json, RunOutcome},
    thread::Thread,
    timeline::{
        AgentCommentResponse, AgentMessage, AgentMessageUi, AgentOutput, AgentStep, AgentToolCall,
        OutputMark, StepView, TimelineMessage,
    },
    usage::usage_tokens,
};

/// An absolute position in the original sequence of completed Rig messages.
/// Compaction never changes these positions, and UUIDs identify runs rather
/// than individual messages in this sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TranscriptIndex(pub(crate) u64);

/// A complete replacement for the original prefix `[0, resume_at)`.
/// Summary requests themselves never join the original messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Compaction {
    pub(crate) resume_at: TranscriptIndex,
    pub(crate) summary: String,
}

/// LLM state only: original messages and successive summaries of their prefix.
/// Run identities, unfinished events, and local views belong to the timeline.
///
/// All messages remain resident today. Offloading could add a
/// `start: TranscriptIndex`, the absolute position of `messages[0]`, keeping a
/// contiguous tail. An absolute position would then map to the resident vector
/// at `index - start`; removing a persisted prefix would advance `start` without
/// changing run references or compaction boundaries. Missing ranges would need
/// loading before access, and the absolute end would be `start + messages.len()`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Transcript {
    pub(crate) messages: Vec<RigMessage>,
    compactions: Vec<Compaction>,
}

impl Transcript {
    pub(crate) fn end(&self) -> TranscriptIndex {
        TranscriptIndex(self.messages.len() as u64)
    }

    pub(crate) fn append(&mut self, message: RigMessage) -> TranscriptIndex {
        let index = self.end();
        self.messages.push(message);
        index
    }

    pub(crate) fn latest_compaction(&self) -> Option<&Compaction> {
        self.compactions.last()
    }

    pub(crate) fn compactions(&self) -> &[Compaction] {
        &self.compactions
    }

    /// Place summaries inside a run after the steps covered by their original
    /// transcript boundary. Tool-result messages update calls without adding a
    /// step; adjacent text and reasoning parts each share a step, as in `push`.
    /// Counting without constructing output keeps rendering independent of
    /// transcript text size and avoids creating any new views.
    pub(crate) fn compaction_steps(
        &self,
        prompt: TranscriptIndex,
        next_prompt: Option<TranscriptIndex>,
    ) -> Vec<(usize, usize)> {
        #[derive(Clone, Copy, PartialEq)]
        enum StepKind {
            Thinking,
            Text,
            ToolCall,
            Steering,
        }
        let mut markers = Vec::new();
        let mut cursor = prompt.0 as usize + 1;
        let mut steps = 0;
        let mut last = None;
        for (index, compaction) in self
            .compactions
            .iter()
            .enumerate()
            .filter(|(_, compaction)| {
                compaction.resume_at > prompt
                    && next_prompt.is_none_or(|end| compaction.resume_at < end)
            })
        {
            for message in &self.messages[cursor..compaction.resume_at.0 as usize] {
                let mut add = |kind: StepKind, empty: bool| {
                    if matches!(kind, StepKind::Thinking | StepKind::Text)
                        && (last == Some(kind) || empty)
                    {
                        return;
                    }
                    steps += 1;
                    last = Some(kind);
                };
                match message {
                    RigMessage::Assistant(reply) => {
                        for part in &reply.content {
                            match part {
                                AssistantContent::Reasoning(reasoning) => {
                                    add(StepKind::Thinking, reasoning.text.is_empty());
                                }
                                AssistantContent::Text(text) => {
                                    add(StepKind::Text, text.text.is_empty())
                                }
                                AssistantContent::ToolCall(_) => add(StepKind::ToolCall, false),
                                _ => {}
                            }
                        }
                    }
                    RigMessage::User { content }
                        if content
                            .iter()
                            .any(|item| !matches!(item, UserContent::ToolResult(_))) =>
                    {
                        add(StepKind::Steering, false);
                    }
                    _ => {}
                }
            }
            cursor = compaction.resume_at.0 as usize;
            markers.push((index, steps));
        }
        markers
    }

    /// Accept only nonempty summaries within the original transcript, with
    /// nondecreasing coverage. A replacement at the same boundary is valid.
    pub(crate) fn install_compaction(&mut self, compaction: Compaction) -> anyhow::Result<()> {
        compaction.validate(self.end(), self.latest_compaction())?;
        self.compactions.push(compaction);
        Ok(())
    }

    pub(crate) fn request_history(&self) -> Vec<RigMessage> {
        match self.latest_compaction() {
            Some(compaction) => std::iter::once(RigMessage::user(format!(
                "Summary of the conversation so far:\n{}",
                compaction.summary
            )))
            .chain(
                self.messages[compaction.resume_at.0 as usize..]
                    .iter()
                    .cloned(),
            )
            .collect(),
            None => self.messages.clone(),
        }
    }

    pub(crate) fn to_protocol(&self) -> Json<Self> {
        Json::from_value(self).expect("a transcript encodes as JSON")
    }
}

impl Compaction {
    fn validate(&self, end: TranscriptIndex, previous: Option<&Self>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.resume_at.0 > 0
                && self.resume_at <= end
                && previous.is_none_or(|previous| self.resume_at >= previous.resume_at)
                && !self.summary.trim().is_empty(),
            "invalid compacted context"
        );
        Ok(())
    }
}

impl Json<Transcript> {
    /// Decode the whole LLM state once and validate all prefix summaries before
    /// a snapshot is installed. JSON validity alone does not establish coverage.
    pub(crate) fn to_transcript(&self) -> anyhow::Result<Transcript> {
        let transcript = self.parse().context("invalid transcript from host")?;
        let mut previous = None;
        for compaction in &transcript.compactions {
            compaction.validate(transcript.end(), previous)?;
            previous = Some(compaction);
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
        .map(|message| {
            usize::try_from(message.prompt.0).context("transcript index does not fit in memory")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
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
        if let Some(call) = &message.awaiting_approval {
            let call = call.to_call_id()?;
            anyhow::ensure!(
                message.run.is_generating() && is_pending(&fold, &call),
                "agent message {index} awaits approval of a call that is not waiting"
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
        let decoded = event.to_agent()?;
        let folded = self
            .agent_turn
            .apply(&decoded)
            .context("the host sent an agent event out of order")?;
        let completed = folded.message.is_some();
        self.transcript.messages.extend(folded.message);
        self.measure_agent_event(&decoded);
        let partial = self.agent_turn.partial();
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
        let completed = completed.then(|| self.transcript.messages.last()).flatten();
        let generating = message.is_generating();
        message
            .ui
            .advance(completed, partial.as_ref(), generating, cx);
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
            is_pending(&self.agent_turn, &call),
            "the host asked to approve a call that is not waiting for its result"
        );
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
        let partial = self.agent_turn.partial();
        self.generating = false;
        // A request that was stopped or failed before it reported usage
        // never adds its partial output to the transcript.
        self.streamed_bytes = 0;
        self.agent_turn = TurnFold::default();
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
    pub(crate) fn restore_agent_output(&mut self, cx: &mut impl AppContext) {
        let (indices, prompts): (Vec<_>, Vec<_>) = self
            .timeline
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match entry {
                TimelineMessage::Agent(message) => Some((index, message.record.prompt.0 as usize)),
                TimelineMessage::User(_) => None,
            })
            .unzip();
        let outputs = run_outputs(&prompts, self.transcript.messages.len());
        self.agent_turn = TurnFold::default();
        for (index, output) in indices.into_iter().zip(outputs) {
            let TimelineMessage::Agent(message) = &mut self.timeline[index] else {
                unreachable!("an agent message's index");
            };
            let mut fold = fold_after(&self.transcript.messages, &output);
            for event in &message.record.pending_events {
                let event = event
                    .to_agent()
                    .expect("pending events validated with the snapshot");
                fold.apply(&event)
                    .expect("pending events validated with the snapshot");
            }
            let generating = message.is_generating();
            message.ui.restore(
                &self.transcript.messages[output],
                fold.partial().as_ref(),
                generating,
                cx,
            );
            let message_id = message.record.id;
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
                        _ => {}
                    }
                }
            }
            RigMessage::User { content } => {
                for item in content {
                    if let UserContent::ToolResult(result) = item {
                        self.record_tool_result(&result.call, &result.content);
                    }
                }
                // A run's own user messages carry tool results only; any
                // other content is a steering prompt delivered into it.
                if content
                    .iter()
                    .any(|item| !matches!(item, UserContent::ToolResult(_)))
                {
                    self.steps.push(AgentStep::Steering);
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
            AgentStep::Thinking(_) | AgentStep::Text(_) | AgentStep::Steering => None,
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

impl AgentMessageUi {
    /// Shows a run's output after its fold moved on: `completed`, a message
    /// it has just added to the transcript, if any, then `partial`, the
    /// message it is folding. Only the part after the committed mark is
    /// redone, and the text views only get what that part added.
    pub(crate) fn advance(
        &mut self,
        completed: Option<&RigMessage>,
        partial: Option<&RigMessage>,
        generating: bool,
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
                AgentStep::ToolCall(_) | AgentStep::Steering => None,
            })
            .collect::<Vec<_>>();
        self.output.rewind(from);
        if let Some(completed) = completed {
            self.output.push(completed);
            self.committed = self.output.mark();
        }
        self.output.extend(partial);
        self.set_thinking_complete(partial, generating);
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
                (Some(StepView::ToolCall { .. }), AgentStep::ToolCall(_))
                | (Some(StepView::Steering), AgentStep::Steering) => {}
                (Some(shown), step) => *shown = StepView::new(step, cx),
                (None, step) => self.step_views.push(StepView::new(step, cx)),
            }
        }
    }

    /// Shows `output`, a run's transcript entries, followed by `partial`, the
    /// message it is folding, in a message fresh from a snapshot. The text
    /// views are shown whole, as history rather than as a stream.
    pub(crate) fn restore(
        &mut self,
        output: &[RigMessage],
        partial: Option<&RigMessage>,
        generating: bool,
        cx: &mut impl AppContext,
    ) {
        for message in output {
            self.output.push(message);
        }
        self.committed = self.output.mark();
        self.output.extend(partial);
        self.set_thinking_complete(partial, generating);
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
    fn set_thinking_complete(&mut self, partial: Option<&RigMessage>, generating: bool) {
        self.output.thinking_complete = !(generating && AgentOutput::still_reasoning(partial));
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
        TimelineMessage::Agent(message) if message.record.id == id => Some(message),
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

    #[test]
    fn compaction_markers_follow_original_steps_and_keep_repeated_boundaries() {
        let call_id = CallId::from_wire("lookup");
        let tool = ToolName::new("read_file").unwrap();
        let mut transcript = Transcript::default();
        transcript.append(RigMessage::user("Earlier prompt"));
        transcript.append(RigMessage::assistant("Earlier reply"));
        let prompt = transcript.append(RigMessage::user("Continue"));
        transcript.append(RigMessage::Assistant(AssistantMessage::new(vec![
            AssistantContent::Reasoning(Reasoning::new("Check")),
            AssistantContent::Reasoning(Reasoning::new(" the file")),
            AssistantContent::text("Looking"),
            AssistantContent::text(" now"),
            AssistantContent::ToolCall(ToolCall::new(
                call_id.clone(),
                ToolFunction::new(tool.clone(), json!({"path": "file"})),
            )),
        ])));
        transcript.append(RigMessage::tool_result(call_id, tool, "Contents"));
        transcript.append(RigMessage::user("Steering"));
        transcript.append(RigMessage::assistant("Answer"));
        for boundary in [2, 5, 5, 6, 7] {
            transcript
                .install_compaction(Compaction {
                    resume_at: TranscriptIndex(boundary),
                    summary: "Summary".into(),
                })
                .unwrap();
        }
        // The pre-run boundary is handled outside the run. Tool results do
        // not add a step, repeated summaries each retain their own marker,
        // and a boundary at the next prompt belongs before that prompt.
        assert_eq!(
            transcript.compaction_steps(prompt, Some(TranscriptIndex(7))),
            vec![(1, 3), (2, 3), (3, 4)]
        );
        assert_eq!(
            transcript.compaction_steps(prompt, None),
            vec![(1, 3), (2, 3), (3, 4), (4, 5)]
        );
        let restored = transcript.to_protocol().to_transcript().unwrap();
        for (index, steps) in transcript.compaction_steps(prompt, None) {
            let mut output = AgentOutput::default();
            for message in &transcript.messages
                [prompt.0 as usize + 1..transcript.compactions[index].resume_at.0 as usize]
            {
                output.push(message);
            }
            assert_eq!(steps, output.steps.len());
        }
        assert_eq!(
            restored.compaction_steps(prompt, None),
            transcript.compaction_steps(prompt, None)
        );
    }

    #[test]
    fn compactions_keep_original_positions_and_round_trip_their_history() {
        let mut transcript = Transcript::default();
        let first = transcript.append(RigMessage::user("Original request"));
        let second = transcript.append(RigMessage::assistant("Original answer"));
        assert_eq!((first, second), (TranscriptIndex(0), TranscriptIndex(1)));

        let boundary = transcript.end();
        // A new conversation prompt may arrive before the summary finishes.
        let third = transcript.append(RigMessage::user("Continue"));
        transcript
            .install_compaction(Compaction {
                resume_at: boundary,
                summary: "Earlier conversation".into(),
            })
            .unwrap();
        assert_eq!(third, TranscriptIndex(2));
        assert_eq!(
            transcript.request_history(),
            vec![
                RigMessage::user("Summary of the conversation so far:\nEarlier conversation"),
                RigMessage::user("Continue"),
            ]
        );
        transcript
            .install_compaction(Compaction {
                resume_at: transcript.end(),
                summary: "Everything so far".into(),
            })
            .unwrap();
        assert_eq!(transcript.compactions.len(), 2);
        assert_eq!(
            transcript.messages,
            vec![
                RigMessage::user("Original request"),
                RigMessage::assistant("Original answer"),
                RigMessage::user("Continue"),
            ]
        );

        let bytes = postcard::to_stdvec(&transcript.to_protocol()).unwrap();
        // The entire value travels as one JSON string, with no per-message
        // wrappers or intermediate transcript representation.
        assert_eq!(
            bytes,
            postcard::to_stdvec(&serde_json::to_string(&transcript).unwrap()).unwrap()
        );
        let wire: Json<Transcript> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(wire.to_transcript().unwrap(), transcript);
    }

    #[test]
    fn invalid_compactions_leave_context_unchanged_and_are_rejected_in_snapshots() {
        let mut transcript = Transcript::default();
        transcript.append(RigMessage::user("Request"));
        transcript.append(RigMessage::assistant("Answer"));
        transcript
            .install_compaction(Compaction {
                resume_at: TranscriptIndex(2),
                summary: "Summary".into(),
            })
            .unwrap();
        let original = transcript.clone();
        for (boundary, summary) in [
            (0, "Invalid"),
            (1, "Backward"),
            (3, "Beyond end"),
            (2, "  "),
            (u64::MAX, "Overflow"),
        ] {
            let invalid = Compaction {
                resume_at: TranscriptIndex(boundary),
                summary: summary.into(),
            };
            assert!(transcript.install_compaction(invalid.clone()).is_err());
            assert_eq!(transcript, original);
            let mut invalid_transcript = transcript.clone();
            invalid_transcript.compactions.push(invalid);
            assert!(invalid_transcript.to_protocol().to_transcript().is_err());
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
