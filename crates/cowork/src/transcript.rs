//! The transcript and the agent's events: on the wire, and folded into a
//! thread.
//!
//! Only the host runs the agent, as an agent session per thread (see
//! `generation.rs`). The transcript is that session's conversation, kept by
//! every participant: the host forwards what the session reports as
//! [`RunEvent`]s, and everyone, the host included, folds them into a replica
//! of the conversation, which checks each committed entry as the session's
//! own conversation did. Accepted summaries replace the older part of the
//! context for later requests; they never remove entries.
//!
//! What an agent message shows ([`AgentOutput`]) is never sent. It is a
//! function of transcript entries: the ones its run added after its prompt,
//! followed by the reply being sampled, as far as its previews have come.
//! Those previews are the run's pending events, which a snapshot carries raw,
//! so someone joining rebuilds the message exactly, and every copy shows the
//! same.
//!
//! Live, the transcript part never changes, so only the rest is redone per
//! event: a message rewinds its output to where the committed part ends,
//! adds an entry that has just joined the transcript, if any, and then the
//! preview.

use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
};

use agent::{
    Compaction, Conversation, Entry, Origin,
    model::{Content, Message, Reasoning, Role, ToolArguments, ToolCall, ToolCallId},
    persistence::record::{StoredCompaction, StoredEntry},
};
use anyhow::Context as _;
use gpui::AppContext;
use gpui_base::TextViewState;
use serde::{Deserialize, Serialize};
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

use crate::{
    protocol::{self, AgentRun, Json, Preview, PreviewDelta, RunEvent, RunOutcome},
    thread::Thread,
    timeline::{AgentCommentResponse, AgentMessage, RunActivity, TimelineMessage},
};

/// A rough average for estimating the tokens in text before the provider
/// reports the exact count.
pub(crate) const BYTES_PER_TOKEN: u64 = 4;

/// Who started a run, recorded with its input. Only provenance: the host
/// checked authority when it accepted the submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RunSource {
    /// A participant's submission.
    Participant(uuid::Bytes),
    /// A scheduled task, which nobody may be watching.
    Schedule,
    /// The host app itself, for requests nobody made, such as compacting a
    /// full context.
    Host,
}

/// An absolute position in the append-only sequence of transcript entries.
/// Compaction never changes these positions, and UUIDs identify runs rather
/// than individual entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TranscriptIndex(pub(crate) u64);

/// The conversation of a thread's agent session, and where each of its
/// compactions was accepted, so a run's output can show it in place.
///
/// All entries remain resident. A saved thread keeps them as
/// [`TranscriptLine`]s; see `thread_files.rs`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Transcript {
    conversation: Conversation<RunSource>,
    /// The number of entries when each compaction was accepted, in order.
    compacted_at: Vec<usize>,
}

impl PartialEq for Transcript {
    fn eq(&self, other: &Self) -> bool {
        self.conversation.entries() == other.conversation.entries()
            && self.conversation.compactions() == other.conversation.compactions()
            && self.compacted_at == other.compacted_at
    }
}

/// A transcript as it is sent: entries and compactions in the agent's
/// versioned records.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct TranscriptRecord {
    entries: Vec<StoredEntry>,
    /// Each compaction, after the number of entries it was accepted at.
    compactions: Vec<(u64, StoredCompaction)>,
}

impl Transcript {
    pub(crate) fn conversation(&self) -> &Conversation<RunSource> {
        &self.conversation
    }

    pub(crate) fn entries(&self) -> &[Entry<RunSource>] {
        self.conversation.entries()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries().len()
    }

    pub(crate) fn end(&self) -> TranscriptIndex {
        TranscriptIndex(self.len() as u64)
    }

    /// Adds `entry` at `index`, refusing one the host's conversation could
    /// not have committed there.
    pub(crate) fn commit(&mut self, index: u64, entry: Entry<RunSource>) -> anyhow::Result<()> {
        let index = usize::try_from(index).context("entry index does not fit in memory")?;
        self.conversation
            .apply(index, entry)
            .context("the host committed an entry its conversation could not hold")
    }

    pub(crate) fn compact(&mut self, compaction: Compaction) -> anyhow::Result<()> {
        self.conversation
            .apply_compaction(compaction)
            .context("the host accepted a compaction its conversation could not hold")?;
        self.compacted_at.push(self.len());
        Ok(())
    }

    /// Whether every tool call has its result.
    pub(crate) fn is_settled(&self) -> bool {
        self.conversation.is_settled()
    }

    /// Whether `call` waits for its result.
    pub(crate) fn is_pending(&self, call: &ToolCallId) -> bool {
        self.conversation
            .pending_calls()
            .iter()
            .any(|pending| pending.id == *call)
    }

    /// The tokens the context holds, estimated from its size, until the
    /// provider measures it again.
    pub(crate) fn estimated_tokens(&self) -> u64 {
        (self.conversation.context_bytes() as u64).div_ceil(BYTES_PER_TOKEN)
    }

    /// The compactions accepted with `range.start..=range.end` entries.
    fn compactions_in(&self, range: &Range<usize>) -> impl Iterator<Item = usize> + '_ {
        let range = range.clone();
        self.compacted_at
            .iter()
            .copied()
            .filter(move |at| range.contains(at) || *at == range.end)
    }

    pub(crate) fn to_protocol(&self) -> Json<TranscriptRecord> {
        let mut at = self.compacted_at.iter();
        let record = TranscriptRecord {
            entries: self
                .entries()
                .iter()
                .map(|entry| StoredEntry::encode(entry).expect("committed entries encode"))
                .collect(),
            compactions: self
                .conversation
                .compactions()
                .iter()
                .map(|compaction| {
                    (
                        *at.next().expect("every compaction has a position") as u64,
                        StoredCompaction::encode(compaction).expect("accepted summaries encode"),
                    )
                })
                .collect(),
        };
        Json::from_value(&record).expect("a transcript encodes as JSON")
    }
}

impl Json<TranscriptRecord> {
    /// Rebuilds a transcript from the host, checking every entry and
    /// compaction as they were checked when they were first applied.
    pub(crate) fn to_transcript(&self) -> anyhow::Result<Transcript> {
        let record = self.parse().context("invalid transcript from host")?;
        let mut transcript = Transcript::default();
        let mut entries = record.entries.iter();
        for (at, compaction) in &record.compactions {
            while (transcript.len() as u64) < *at {
                let entry = entries.next().context("compaction past the transcript")?;
                transcript.commit(transcript.len() as u64, entry.decode()?)?;
            }
            transcript.compact(compaction.decode()?)?;
        }
        for entry in entries {
            transcript.commit(transcript.len() as u64, entry.decode()?)?;
        }
        Ok(transcript)
    }
}

/// A transcript record as a saved thread keeps it, one per line of its
/// message log in the order they were accepted, so the log only grows.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranscriptLine {
    Entry(StoredEntry),
    /// Accepted with as many entries as the lines before it hold.
    Compaction(StoredCompaction),
}

/// How much of a transcript is saved: its first `entries` entries and
/// `compactions` compactions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptExtent {
    pub(crate) entries: usize,
    pub(crate) compactions: usize,
}

impl Transcript {
    pub(crate) fn extent(&self) -> TranscriptExtent {
        TranscriptExtent {
            entries: self.len(),
            compactions: self.compacted_at.len(),
        }
    }

    /// The records after `saved`, in the order they were accepted: a
    /// compaction accepted with `n` entries follows the `n`th entry.
    pub(crate) fn lines_after(&self, saved: TranscriptExtent) -> Vec<TranscriptLine> {
        let entry_line = |index: usize| {
            TranscriptLine::Entry(
                StoredEntry::encode(&self.entries()[index]).expect("committed entries encode"),
            )
        };
        let mut lines = Vec::new();
        let mut next = saved.entries;
        let compactions = self.conversation.compactions();
        for (index, at) in self.compacted_at.iter().enumerate().skip(saved.compactions) {
            lines.extend((next..*at).map(entry_line));
            next = next.max(*at);
            lines.push(TranscriptLine::Compaction(
                StoredCompaction::encode(&compactions[index]).expect("accepted summaries encode"),
            ));
        }
        lines.extend((next..self.len()).map(entry_line));
        lines
    }

    /// Adds the record after the ones applied so far, checking it as it was
    /// checked when it was first accepted.
    pub(crate) fn apply_line(&mut self, line: &TranscriptLine) -> anyhow::Result<()> {
        match line {
            TranscriptLine::Entry(entry) => self.commit(self.len() as u64, entry.decode()?),
            TranscriptLine::Compaction(compaction) => self.compact(compaction.decode()?),
        }
    }
}

/// An entry as it is sent.
pub(crate) fn shared_entry(entry: &Entry<RunSource>) -> Json<StoredEntry> {
    Json::from_value(&StoredEntry::encode(entry).expect("committed entries encode"))
        .expect("a stored entry encodes as JSON")
}

/// A compaction as it is sent.
pub(crate) fn shared_compaction(compaction: &Compaction) -> Json<StoredCompaction> {
    Json::from_value(&StoredCompaction::encode(compaction).expect("accepted summaries encode"))
        .expect("a stored compaction encodes as JSON")
}

/// The reply being sampled, as far as its previews have come: each part's
/// text or reasoning, by its position in the reply.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReplyPreview {
    parts: BTreeMap<u32, Content>,
}

impl ReplyPreview {
    pub(crate) fn apply(&mut self, preview: &Preview) {
        let part = self
            .parts
            .entry(preview.index)
            .or_insert_with(|| match &preview.delta {
                PreviewDelta::Text(_) => Content::text(""),
                PreviewDelta::Reasoning(_) => Content::Reasoning(Reasoning::Text {
                    text: String::new(),
                }),
            });
        match (part, &preview.delta) {
            (Content::Text { text }, PreviewDelta::Text(delta))
            | (Content::Reasoning(Reasoning::Text { text }), PreviewDelta::Reasoning(delta)) => {
                text.push_str(delta)
            }
            (part, PreviewDelta::Text(delta)) => *part = Content::text(delta.clone()),
            (part, PreviewDelta::Reasoning(delta)) => {
                *part = Content::Reasoning(Reasoning::Text {
                    text: delta.clone(),
                })
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// The reply as far as it has come, or `None` before its first preview.
    pub(crate) fn partial(&self) -> Option<Message> {
        (!self.is_empty())
            .then(|| Message::new(Role::Assistant, self.parts.values().cloned().collect()))
    }

    pub(crate) fn of(previews: &[Preview]) -> Self {
        let mut preview = Self::default();
        for delta in previews {
            preview.apply(delta);
        }
        preview
    }

    /// Previews that bring a reply as far as `partial`, a reply that
    /// [`ReplyPreview::partial`] gave: one for each part's text or reasoning.
    pub(crate) fn previews_of(partial: &Message) -> Vec<Preview> {
        partial
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(PreviewDelta::Text(text.clone())),
                Content::Reasoning(Reasoning::Text { text }) => {
                    Some(PreviewDelta::Reasoning(text.clone()))
                }
                _ => None,
            })
            .enumerate()
            .map(|(index, delta)| Preview {
                index: index as u32,
                delta,
            })
            .collect()
    }
}

/// Each run owns its entries up to the next run's start. Its output starts
/// after its first input, the prompt, which may follow context notes. Only
/// a run still starting can lack it.
fn run_output(transcript: &Transcript, range: Range<usize>) -> Option<Range<usize>> {
    transcript.entries()[range.clone()]
        .iter()
        .position(|entry| matches!(entry.origin, Origin::Input { .. }))
        .map(|prompt| range.start + prompt + 1..range.end)
}

/// Checks that a snapshot's agent messages, in timeline order, describe runs
/// `transcript` can hold: each starts within it, in order; only the last
/// run can still be generating; a run still starting has no output; and
/// the active run's live state is one its conversation allows.
pub(crate) fn validate_agent_runs(
    transcript: &Transcript,
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
        let generating = message.run.is_generating();
        anyhow::ensure!(
            index + 1 == messages.len() || !generating,
            "agent message {index} is generating, but is not the last"
        );
        if run_output(transcript, starts[index]..end).is_none() {
            anyhow::ensure!(generating, "only the active run can await its prompt");
            anyhow::ensure!(
                message.pending_events.is_empty() && message.awaiting_approval.is_none(),
                "an unstarted run has output"
            );
        }
        if !generating {
            continue;
        }
        anyhow::ensure!(
            message.pending_events.is_empty() || transcript.is_settled(),
            "agent message {index} previews a reply while calls wait for results"
        );
        if let Some(call) = &message.awaiting_approval {
            anyhow::ensure!(
                transcript.is_pending(call),
                "agent message {index} awaits approval of a call that is not waiting"
            );
        }
        if compacting {
            anyhow::ensure!(
                message.awaiting_approval.is_none()
                    && transcript.is_settled()
                    && message.pending_events.is_empty(),
                "compaction is not at a request boundary"
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
        event: RunEvent,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        let activity = self
            .run_activity(message_id)
            .context("event is not for the active run")?;
        let mut completed = None;
        match event {
            RunEvent::Preview(preview) => {
                anyhow::ensure!(
                    activity != RunActivity::Compacting && self.transcript.is_settled(),
                    "a reply previews while calls wait or the context is compacting"
                );
                let (PreviewDelta::Text(text) | PreviewDelta::Reasoning(text)) = &preview.delta;
                self.streamed_bytes += text.len() as u64;
                self.active_preview(message_id)?.apply(&preview);
                if let Some(message) = agent_message(&mut self.timeline, message_id) {
                    message.record.pending_events.push(preview);
                }
            }
            RunEvent::Committed { index, entry } => {
                anyhow::ensure!(
                    activity != RunActivity::Compacting,
                    "an entry was committed while compacting"
                );
                let entry = entry
                    .parse()
                    .context("invalid entry from host")?
                    .decode::<RunSource>()
                    .context("invalid entry from host")?;
                self.transcript.commit(index, entry.clone())?;
                if let Some(group_id) = steered_group(&entry, |id| user_group(&self.timeline, id))
                    && let Some(message) = agent_message(&mut self.timeline, message_id)
                {
                    message.ui.steered(group_id, cx);
                }
                if entry.message.role == Role::Assistant {
                    *self.active_preview(message_id)? = ReplyPreview::default();
                    if let Some(message) = agent_message(&mut self.timeline, message_id) {
                        message.record.pending_events.clear();
                    }
                }
                // A call that returned no longer waits, however the host
                // decided.
                if let Some(message) = agent_message(&mut self.timeline, message_id)
                    && message
                        .record
                        .awaiting_approval
                        .as_ref()
                        .is_some_and(|call| !self.transcript.is_pending(call))
                {
                    message.record.awaiting_approval = None;
                    self.approval_activity(message_id, None)?;
                }
                completed = Some(entry.message);
            }
            RunEvent::Usage { tokens } => {
                // A summary's usage measures the context it summarized.
                if activity != RunActivity::Compacting {
                    self.context_tokens = Some(tokens);
                    self.streamed_bytes = 0;
                }
                return Ok(());
            }
            RunEvent::CompactionStarted => {
                anyhow::ensure!(
                    activity == RunActivity::Working,
                    "compaction requires a working run"
                );
                anyhow::ensure!(
                    self.transcript.is_settled() && self.active_preview(message_id)?.is_empty(),
                    "compaction requires a request boundary"
                );
                self.set_run_activity(message_id, RunActivity::Compacting);
                return Ok(());
            }
            RunEvent::Compacted(compaction) => {
                anyhow::ensure!(
                    activity == RunActivity::Compacting,
                    "compaction is not running"
                );
                let compaction = compaction
                    .parse()
                    .context("invalid compaction from host")?
                    .decode()
                    .context("invalid compaction from host")?;
                self.transcript.compact(compaction)?;
                self.context_tokens = Some(self.transcript.estimated_tokens());
                self.streamed_bytes = 0;
                self.set_run_activity(message_id, RunActivity::Working);
                if let Some(message) = agent_message(&mut self.timeline, message_id) {
                    message.ui.compact(cx);
                }
                return Ok(());
            }
            RunEvent::CompactionFailed => {
                if activity == RunActivity::Compacting {
                    self.set_run_activity(message_id, RunActivity::Working);
                }
                return Ok(());
            }
        }
        let partial = self.active_preview(message_id)?.partial();
        let Some(message) = agent_message(&mut self.timeline, message_id) else {
            return Ok(());
        };
        let generating = message.is_generating();
        message
            .ui
            .advance(completed.as_ref(), partial.as_ref(), generating, cx);
        self.show_comment_responses(message_id, cx);
        Ok(())
    }

    /// Marks `call` as waiting for approval in the run producing message
    /// `message_id`, refusing a call the run is not waiting on.
    pub(crate) fn await_tool_approval(
        &mut self,
        message_id: Uuid,
        call: ToolCallId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.transcript.is_pending(&call),
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
    pub(crate) fn resolve_tool_approval(&mut self, message_id: Uuid, call: &ToolCallId) {
        if let Some(message) = agent_message(&mut self.timeline, message_id)
            && message.record.awaiting_approval.as_ref() == Some(call)
        {
            message.record.awaiting_approval = None;
            _ = self.approval_activity(message_id, None);
        }
    }

    /// Ends the run producing message `id`. What it previewed that never
    /// joined the transcript stays in the message's pending events.
    pub(crate) fn end_agent_run(
        &mut self,
        id: uuid::Bytes,
        outcome: RunOutcome,
        duration: std::time::Duration,
        cx: &mut impl AppContext,
    ) {
        let Some(preview) = self.take_active_preview(Uuid::from_bytes(id)) else {
            return;
        };
        let partial = preview.partial();
        // A reply that was stopped or failed before it completed never
        // joins the transcript.
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
    /// snapshot, and resumes previewing the running one. The snapshot must
    /// have passed [`validate_agent_runs`].
    pub(crate) fn restore_agent_output(&mut self, compacting: bool, cx: &mut impl AppContext) {
        let starts = self
            .timeline
            .iter()
            .filter_map(|entry| match entry {
                TimelineMessage::Agent(message) => Some(message.record.start.0 as usize),
                TimelineMessage::User(_) => None,
            })
            .collect::<Vec<_>>();
        let groups = self
            .timeline
            .iter()
            .filter_map(|entry| match entry {
                TimelineMessage::User(group) => Some(group.record.id),
                TimelineMessage::Agent(_) => None,
            })
            .collect::<HashSet<_>>();
        let mut active = None;
        let mut runs = 0;
        for entry in &mut self.timeline {
            let TimelineMessage::Agent(message) = entry else {
                continue;
            };
            let end = starts
                .get(runs + 1)
                .copied()
                .unwrap_or(self.transcript.len());
            let output = run_output(&self.transcript, starts[runs]..end)
                .unwrap_or(self.transcript.len()..self.transcript.len());
            runs += 1;
            let preview = ReplyPreview::of(&message.record.pending_events);
            let generating = message.is_generating();
            message.ui.restore(
                &self.transcript,
                output,
                |id| groups.contains(&id),
                preview.partial().as_ref(),
                generating,
                cx,
            );
            if generating {
                active = Some((message.record.id, preview));
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
        let ToolArguments::Object { value } = &call.arguments else {
            return;
        };
        if call.name != RespondToComment::NAME {
            return;
        }
        let Ok(args) = serde_json::from_value::<RespondToCommentArgs>(serde_json::Value::Object(
            value.clone(),
        )) else {
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
        let mut comments = self
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
        let mut aliases = TurnComments::new(comments.len()).comment_ids().to_vec();
        if let Some(message) = self.timeline.iter().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if message.record.id == message_id => Some(message),
            _ => None,
        }) {
            for step in message.ui.output.steps() {
                if let crate::timeline::AgentStep::Steering(id) = step
                    && let Some(group) = self.timeline.iter().find_map(|entry| match entry {
                        TimelineMessage::User(group) if group.record.id == *id => Some(group),
                        _ => None,
                    })
                {
                    aliases.extend(steering_comment_ids(*id, group.record.comments.len()));
                    comments.extend(group.record.comments.iter().map(|comment| comment.id));
                }
            }
        }
        let Some(comment_id) = aliases
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
            id: Uuid::new_v5(&message_id, call.id.0.as_bytes()),
            comment_id,
            response_view: cx.new(|cx| TextViewState::markdown(&args.response, cx)),
            response: args.response,
        });
    }
}

/// The user group that `entry`, from a run's output, brings into the run. A
/// steer joins its run as an input whose ID is its group's, and the group is
/// published before the entry, so `is_group` (whether the timeline has a
/// user group of that ID) holds for every steer. Live folding and replay
/// both decide with this, so every copy places the same groups, and any
/// other input shows nothing.
pub(crate) fn steered_group(
    entry: &Entry<RunSource>,
    is_group: impl Fn(Uuid) -> bool,
) -> Option<Uuid> {
    let Origin::Input { id, .. } = &entry.origin else {
        return None;
    };
    Some(id.as_uuid()).filter(|id| is_group(*id))
}

/// The aliases a steer's comments have in its run: `comment_<8 hex>_<n>`,
/// named by the first 8 hex digits of its group's ID. Short enough for a
/// small model to copy, and computed the same by every copy of the thread.
/// A steer's text is written before it joins, so numbering steers in order
/// would renumber later ones when an earlier one is cancelled. Two steers of
/// one run sharing a prefix (about one in four billion per pair) would make
/// the later one's aliases name the earlier one's comments.
pub(crate) fn steering_comment_ids(group: Uuid, count: usize) -> Vec<tools::CommentId> {
    let simple = group.simple().to_string();
    TurnComments::group_ids(&simple[..8], count)
}

fn user_group(timeline: &[TimelineMessage], id: Uuid) -> bool {
    timeline
        .iter()
        .any(|entry| matches!(entry, TimelineMessage::User(group) if group.record.id == id))
}

/// Agent message `id` of `timeline`, borrowed apart from the thread's other
/// fields.
fn agent_message(timeline: &mut [TimelineMessage], id: Uuid) -> Option<&mut AgentMessage> {
    timeline.iter_mut().find_map(|entry| match entry {
        TimelineMessage::Agent(message) if message.record.id == id => Some(message),
        _ => None,
    })
}

impl crate::timeline::AgentOutput {
    /// What a run's `output` entries of `transcript` show, with the
    /// compactions accepted among them in place, and the steers among them
    /// by [`steered_group`].
    pub(crate) fn from_transcript(
        transcript: &Transcript,
        output: Range<usize>,
        is_group: impl Fn(Uuid) -> bool,
    ) -> Self {
        let mut shown = Self::default();
        let mut compactions = transcript.compactions_in(&output).peekable();
        for index in output.clone() {
            while compactions.next_if(|at| *at == index).is_some() {
                shown.push_compaction();
            }
            let entry = &transcript.entries()[index];
            if let Some(group_id) = steered_group(entry, &is_group) {
                shown.push_steering(group_id);
            }
            shown.push(&entry.message);
        }
        for _ in compactions {
            shown.push_compaction();
        }
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::timeline::{AgentOutput, AgentStep};
    use agent::model::{FinishReason, GenerationMetadata, ToolResult, ToolResultContent};
    use agent::{DefinitionRevision, InputId};

    fn input(text: &str) -> Entry<RunSource> {
        Entry {
            message: Message::user(text),
            origin: Origin::Input {
                id: InputId::new(),
                source: RunSource::Schedule,
            },
        }
    }

    fn reply(content: Vec<Content>) -> Entry<RunSource> {
        let mut message = Message::new(Role::Assistant, content);
        message.generation = Some(GenerationMetadata {
            provider: "test".into(),
            model: None,
            finish_reason: Some(FinishReason::Stop),
            usage: None,
            extra: Default::default(),
        });
        Entry {
            message,
            origin: Origin::Model {
                revision: DefinitionRevision::default(),
            },
        }
    }

    fn commit(transcript: &mut Transcript, entry: Entry<RunSource>) {
        transcript.commit(transcript.len() as u64, entry).unwrap();
    }

    fn summary(end: usize, previous_revision: usize) -> Compaction {
        Compaction {
            previous_revision,
            end,
            summary: Message::user("Summary"),
        }
    }

    /// Accepted summaries show where they were accepted, and survive the
    /// wire with every entry, exactly.
    #[test]
    fn compactions_show_in_place_and_round_trip() {
        let mut transcript = Transcript::default();
        commit(&mut transcript, input("Request"));
        commit(
            &mut transcript,
            reply(vec![Content::text("Before ".repeat(50))]),
        );
        let more = input("More");
        let Origin::Input { id, .. } = &more.origin else {
            unreachable!()
        };
        let steering_id = id.as_uuid();
        commit(&mut transcript, more);
        transcript.compact(summary(2, 0)).unwrap();
        commit(&mut transcript, reply(vec![Content::text("After")]));
        let steps = |is_group: bool| {
            AgentOutput::from_transcript(&transcript, 1..4, |_| is_group)
                .steps()
                .to_vec()
        };
        assert_eq!(
            steps(true),
            [
                AgentStep::Text("Before ".repeat(50)),
                AgentStep::Steering(steering_id),
                AgentStep::Compaction,
                AgentStep::Text("After".into()),
            ]
        );
        // An input that names no user group isn't a steer, and shows nothing.
        assert!(!steps(false).contains(&AgentStep::Steering(steering_id)));
        let bytes = postcard::to_stdvec(&transcript.to_protocol()).unwrap();
        let wire: Json<TranscriptRecord> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(wire.to_transcript().unwrap(), transcript);
    }

    /// Entries the host's conversation could not have committed are
    /// refused: out of order, or a result no call waits for.
    #[test]
    fn invalid_entries_are_refused() {
        let mut transcript = Transcript::default();
        assert!(transcript.commit(1, input("Gap")).is_err());
        commit(&mut transcript, input("Request"));
        let stray = Entry {
            message: Message::new(
                Role::Tool,
                vec![Content::ToolResult(ToolResult {
                    call: ToolCallId::new(),
                    provider_id: None,
                    name: None,
                    content: vec![ToolResultContent::Text { text: "x".into() }],
                    is_error: false,
                })],
            ),
            origin: Origin::Tool,
        };
        assert!(transcript.commit(1, stray).is_err());
        assert_eq!(transcript.len(), 1);
    }

    #[test]
    fn previews_follow_their_parts() {
        let mut preview = ReplyPreview::default();
        for (index, delta) in [
            (0, PreviewDelta::Reasoning("Think".into())),
            (1, PreviewDelta::Text("Hel".into())),
            (0, PreviewDelta::Reasoning("ing.".into())),
            (1, PreviewDelta::Text("lo".into())),
        ] {
            preview.apply(&Preview { index, delta });
        }
        let partial = preview.partial().unwrap();
        assert_eq!(
            partial.content,
            [
                Content::Reasoning(Reasoning::Text {
                    text: "Thinking.".into()
                }),
                Content::text("Hello"),
            ]
        );
    }
}
