//! Pure placement of thread records and operational state in the timeline.
//! Items are references to facts, not agent events or another shared history.
//! Projection chooses ordering and collapse behavior; renderers only draw it.

use std::collections::HashSet;

use crate::{
    thread::submission::PendingSubmission,
    timeline::{AgentMessage, RunActivity, TimelineMessage},
    transcript::Compaction,
};

/// Accepted summaries are append-only, so their ordinal is a stable identity
/// even when several summaries cover the same transcript boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompactionId(pub(crate) usize);

pub(crate) enum TimelineItem {
    Message(usize),
    Compaction(CompactionId),
}

pub(crate) enum PendingItem {
    Submission(usize),
    DraftBoundary,
}

#[derive(Default)]
pub(crate) struct TimelineProjection {
    history: Vec<TimelineItem>,
    pending: Vec<PendingItem>,
}

impl TimelineProjection {
    pub(crate) fn history(&self) -> &[TimelineItem] {
        &self.history
    }
    pub(crate) fn pending(&self) -> &[PendingItem] {
        &self.pending
    }

    pub(crate) fn new(
        messages: &[TimelineMessage],
        compactions: &[Compaction],
        pending: &[PendingSubmission],
    ) -> Self {
        let steering = messages
            .iter()
            .filter_map(|entry| match entry {
                TimelineMessage::Agent(run) => Some(run.record.steering.iter().copied()),
                TimelineMessage::User(_) => None,
            })
            .flatten()
            .collect::<HashSet<_>>();
        let mut history = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            if matches!(message, TimelineMessage::User(group) if steering.contains(&group.record.id))
            {
                continue;
            }
            let run = match message {
                TimelineMessage::User(_) => messages.get(index + 1),
                TimelineMessage::Agent(_)
                    if index == 0 || !matches!(messages[index - 1], TimelineMessage::User(_)) =>
                {
                    Some(message)
                }
                _ => None,
            };
            if let Some(TimelineMessage::Agent(run)) = run {
                history.extend(
                    compactions
                        .iter()
                        .enumerate()
                        .filter(|(_, summary)| summary.resume_at == run.record.prompt)
                        .map(|(id, _)| TimelineItem::Compaction(CompactionId(id))),
                );
            }
            history.push(TimelineItem::Message(index));
        }
        let mut pending_order = (0..pending.len()).collect::<Vec<_>>();
        pending_order
            .sort_by_key(|&index| pending[index].mode != crate::protocol::SubmissionMode::Steering);
        let mut pending = pending_order
            .into_iter()
            .map(PendingItem::Submission)
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            pending.push(PendingItem::DraftBoundary);
        }
        Self { history, pending }
    }
}

pub(crate) enum RunItem {
    Content(usize),
    Steering(usize),
    Compaction(CompactionId),
    Activity(RunActivity),
    Response,
}

pub(crate) struct RunProjection {
    items: Vec<RunItem>,
}

impl RunProjection {
    pub(crate) fn into_items(self) -> impl Iterator<Item = RunItem> {
        self.items.into_iter()
    }

    pub(crate) fn new(
        index: usize,
        message: &AgentMessage,
        timeline: &[TimelineMessage],
        compactions: &[Compaction],
        activity: Option<RunActivity>,
    ) -> Self {
        let next_prompt = timeline[index + 1..].iter().find_map(|entry| match entry {
            TimelineMessage::Agent(run) => Some(run.record.prompt),
            TimelineMessage::User(_) => None,
        });
        let mut boundaries = compactions
            .iter()
            .enumerate()
            .filter(|(_, summary)| {
                summary.resume_at > message.record.prompt
                    && next_prompt.is_none_or(|end| summary.resume_at < end)
            })
            .map(|(id, summary)| {
                (
                    CompactionId(id),
                    message.ui.output.step_at_boundary(summary.boundary()),
                )
            })
            .peekable();
        let mut steering = message.record.steering.iter().filter_map(|id| {
            timeline.iter().position(
                |entry| matches!(entry, TimelineMessage::User(group) if group.record.id == *id),
            )
        });
        let mut items = Vec::new();
        if activity == Some(RunActivity::Working)
            && message.ui.output.steps().is_empty()
            && message.ui.output.text.is_empty()
        {
            items.push(RunItem::Activity(RunActivity::Working));
        }
        if message.ui.work_expanded || message.record.awaiting_approval.is_some() {
            for step in message.ui.output.work() {
                while boundaries.peek().is_some_and(|(_, before)| *before <= step) {
                    items.push(RunItem::Compaction(boundaries.next().unwrap().0));
                }
                if matches!(
                    message.ui.output.steps()[step],
                    crate::timeline::AgentStep::Steering
                ) {
                    if let Some(index) = steering.next() {
                        items.push(RunItem::Steering(index));
                    }
                } else {
                    items.push(RunItem::Content(step));
                }
            }
        }
        // Collapsed work and steering delivered before its prompt is folded
        // still show submissions. All hidden boundaries remain above the reply.
        items.extend(steering.map(RunItem::Steering));
        items.extend(boundaries.map(|(id, _)| RunItem::Compaction(id)));
        if activity == Some(RunActivity::Compacting) {
            items.push(RunItem::Activity(RunActivity::Compacting));
        }
        items.push(RunItem::Response);
        Self { items }
    }
}
