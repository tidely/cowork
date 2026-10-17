//! Pure placement of thread records and operational state in the timeline.
//! Items are references to facts, not agent events or another shared history.
//! Projection chooses ordering and collapse behavior; renderers only draw it.

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
        let mut history = Vec::new();
        for (index, message) in messages.iter().enumerate() {
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

    pub(crate) fn new(message: &AgentMessage, activity: Option<RunActivity>) -> Self {
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
                items.push(RunItem::Content(step));
            }
        }
        if activity == Some(RunActivity::Compacting) {
            items.push(RunItem::Activity(RunActivity::Compacting));
        }
        Self { items }
    }
}
