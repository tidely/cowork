//! Pure placement of a run's output and operational state in its agent
//! message. Items are references to facts, not agent events or another
//! shared history. Projection chooses ordering and collapse behavior;
//! renderers only draw it.

use crate::timeline::{AgentMessage, AgentOutput, AgentStep, RunActivity};

#[derive(Debug, PartialEq)]
pub(crate) enum RunItem {
    Content(usize),
    Compaction(usize),
    Steering(uuid::Uuid),
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
        items.extend(Self::work_items(&message.ui.output, expanded));
        if activity == Some(RunActivity::Compacting) {
            items.push(RunItem::Activity(RunActivity::Compacting));
        }
        Self { items }
    }

    fn work_items(output: &AgentOutput, expanded: bool) -> Vec<RunItem> {
        let mut items = Vec::new();
        let response_start = output.response_start();
        // A steer can extend a run after the agent already answered. Walking
        // backwards identifies the answer before each steer: text after that
        // segment's last tool call. Later work must not hide those answers.
        let mut before_steering = false;
        for (step, content) in output.steps().iter().enumerate().rev() {
            match content {
                AgentStep::Compaction => items.push(RunItem::Compaction(step)),
                AgentStep::Steering(id) => {
                    before_steering = true;
                    items.push(RunItem::Steering(*id));
                }
                AgentStep::Text(_) if step >= response_start => {}
                AgentStep::Text(_) if before_steering => items.push(RunItem::Content(step)),
                content => {
                    if matches!(content, AgentStep::ToolCall(_)) {
                        before_steering = false;
                    }
                    if expanded {
                        items.push(RunItem::Content(step));
                    }
                }
            }
        }
        items.reverse();
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{AgentToolCall, tool_call};

    #[test]
    fn steering_keeps_earlier_answers_visible_but_not_tool_work() {
        let steering = uuid::Uuid::new_v4();
        let next_steering = uuid::Uuid::new_v4();
        let mut output = AgentOutput::for_test(
            vec![
                AgentStep::Text("Checking".into()),
                AgentStep::ToolCall(AgentToolCall {
                    call: tool_call("call", "calculate", serde_json::json!({})),
                    result: Some(Vec::new()),
                }),
                AgentStep::Thinking("Thinking".into()),
                AgentStep::Text("First answer".into()),
                AgentStep::Steering(steering),
                AgentStep::Compaction,
                AgentStep::Thinking("More thinking".into()),
                AgentStep::Text("Second answer".into()),
            ],
            String::new(),
            true,
        );
        let earlier = vec![
            RunItem::Content(3),
            RunItem::Steering(steering),
            RunItem::Compaction(5),
        ];
        // The current response is rendered separately; only earlier answers
        // appear among the work items, even while the next reply is streaming.
        assert_eq!(RunProjection::work_items(&output, false), earlier);
        assert_eq!(output.response(), "Second answer");
        output.push_steering(next_steering);
        let mut expected = earlier;
        expected.extend([RunItem::Content(7), RunItem::Steering(next_steering)]);
        assert_eq!(RunProjection::work_items(&output, false), expected);
        assert!(output.response().is_empty());
        assert_eq!(
            RunProjection::work_items(&output, true),
            [
                RunItem::Content(0),
                RunItem::Content(1),
                RunItem::Content(2),
                RunItem::Content(3),
                RunItem::Steering(steering),
                RunItem::Compaction(5),
                RunItem::Content(6),
                RunItem::Content(7),
                RunItem::Steering(next_steering),
            ]
        );
    }
}
