//! Pure placement of a run's output and operational state in its agent
//! message. Items are references to facts, not agent events or another
//! shared history. Projection chooses ordering and collapse behavior;
//! renderers only draw it.

use crate::timeline::{AgentMessage, RunActivity};

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
