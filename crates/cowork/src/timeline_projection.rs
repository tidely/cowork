//! Pure placement of thread records and operational state in the timeline.
//! Items are references to facts, not agent events or another shared history.
//! Projection chooses ordering and collapse behavior; renderers only draw it.

use std::collections::HashSet;

use crate::{
    timeline::{AgentMessage, RunActivity, TimelineMessage},
    transcript::{Transcript, TranscriptIndex},
};

pub(crate) enum TimelineItem {
    Message(usize),
    Compaction(TranscriptIndex),
}

#[derive(Default)]
pub(crate) struct TimelineProjection {
    history: Vec<TimelineItem>,
}

impl TimelineProjection {
    pub(crate) fn history(&self) -> &[TimelineItem] {
        &self.history
    }
    pub(crate) fn new(messages: &[TimelineMessage], transcript: &Transcript) -> Self {
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
                    transcript
                        .leading_compactions(run.record.start)
                        .map(|index| TimelineItem::Compaction(TranscriptIndex(index as u64))),
                );
            }
            history.push(TimelineItem::Message(index));
        }
        Self { history }
    }
}

pub(crate) enum RunItem {
    Content(usize),
    Steering(usize),
    Compaction(usize),
    Activity(RunActivity),
}

pub(crate) struct RunProjection {
    items: Vec<RunItem>,
}

impl RunProjection {
    pub(crate) fn into_items(self) -> impl Iterator<Item = RunItem> {
        self.items.into_iter()
    }

    pub(crate) fn new(
        message: &AgentMessage,
        timeline: &[TimelineMessage],
        activity: Option<RunActivity>,
    ) -> Self {
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
        let expanded = message.ui.work_expanded || message.record.awaiting_approval.is_some();
        for step in message.ui.output.work() {
            if matches!(
                message.ui.output.steps()[step],
                crate::timeline::AgentStep::Compaction
            ) {
                items.push(RunItem::Compaction(step));
            } else if expanded {
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
        if activity == Some(RunActivity::Compacting) {
            items.push(RunItem::Activity(RunActivity::Compacting));
        }
        Self { items }
    }
}
