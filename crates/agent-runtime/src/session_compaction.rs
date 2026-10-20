use super::*;
use crate::{
    CompactionPlan,
    compaction::{finish_summary, summary_request},
};

pub(super) struct Compacting {
    plan: CompactionPlan,
    previous_phase: Phase,
}

impl<S: Clone + PartialEq + Send + 'static> AgentSession<S> {
    pub(super) fn maybe_compact(
        &mut self,
        automatic: bool,
        emit: &mut impl FnMut(AgentEvent<S>),
    ) -> bool {
        let manual = !self.compaction_requests.is_empty();
        // Steering admitted during summarization must not trigger another
        // summary before the model has had a chance to consume that input.
        if !manual
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.compaction_attempted)
        {
            return false;
        }
        let Some(policy) = self.compaction_policy.as_ref() else {
            if manual {
                self.reject_compaction(CompactionError::Disabled, emit);
            }
            return false;
        };
        let stamp = (
            self.conversation.messages().len(),
            self.conversation.compactions().len(),
        );
        if !manual
            && (!automatic
                || self.last_compaction_attempt == Some(stamp)
                || self.conversation.context_bytes() < policy.trigger_bytes.get())
        {
            return false;
        }
        self.last_compaction_attempt = Some(stamp);
        let plan = match self
            .conversation
            .plan_compaction(policy.keep_recent_messages.get())
        {
            Ok(plan) => plan,
            Err(error) => {
                if manual {
                    self.reject_compaction(error, emit);
                }
                return false;
            }
        };
        let request = summary_request(&plan, &self.request, policy);
        if let Some(active) = &mut self.active {
            active.compaction_attempted = true;
        }
        let previous_phase = self
            .active
            .as_ref()
            .map_or(Phase::Idle, |active| active.phase);
        if self.active.is_some() {
            self.phase(Phase::Compacting, emit);
        } else {
            self.emit(EventKind::PhaseChanged(Phase::Compacting), emit);
        }
        self.emit(
            EventKind::CompactionStarted {
                range: plan.range.clone(),
                previous_revision: plan.previous_revision,
            },
            emit,
        );
        self.compacting = Some(Compacting {
            plan,
            previous_phase,
        });
        self.work = Some(
            model::sample(self.provider.clone(), self.model.clone(), request)
                .filter_map(|event| {
                    futures::future::ready(match event {
                        WorkEvent::Model(result) => Some(WorkEvent::Compaction(result)),
                        WorkEvent::Preview { .. } => None,
                        _ => unreachable!("summary stream only samples"),
                    })
                })
                .boxed(),
        );
        true
    }

    pub(super) fn complete_compaction(
        &mut self,
        result: Result<Message, language_model::Error>,
        emit: &mut impl FnMut(AgentEvent<S>),
    ) {
        self.work = None;
        let compacting = self
            .compacting
            .take()
            .expect("summary belongs to compaction");
        if let Ok(message) = &result
            && let Some(usage) = message
                .generation
                .as_ref()
                .and_then(|meta| meta.usage.clone())
        {
            self.emit(EventKind::UsageReported(usage), emit);
        }
        let result = result
            .map_err(CompactionError::Model)
            .and_then(|reply| finish_summary(&compacting.plan, &reply))
            .and_then(|compaction| {
                self.conversation.apply_compaction(compaction.clone())?;
                Ok(compaction)
            });
        match result {
            Ok(compaction) => {
                self.emit(EventKind::Compacted(compaction.clone()), emit);
                for response in self.compaction_requests.drain(..) {
                    let _ = response.send(Ok(compaction.clone()));
                }
            }
            Err(error) => self.reject_compaction(error, emit),
        }
        if self.active.is_some() {
            self.phase(compacting.previous_phase, emit);
        } else {
            self.emit(EventKind::PhaseChanged(Phase::Idle), emit);
        }
    }

    pub(super) fn cancel_compaction(&mut self, emit: &mut impl FnMut(AgentEvent<S>)) {
        let running = self.compacting.take().is_some();
        if running {
            self.work = None;
        }
        if running || !self.compaction_requests.is_empty() {
            self.reject_compaction(CompactionError::Cancelled, emit);
        }
    }

    fn reject_compaction(&mut self, error: CompactionError, emit: &mut impl FnMut(AgentEvent<S>)) {
        self.emit(EventKind::CompactionFailed(error.clone()), emit);
        for response in self.compaction_requests.drain(..) {
            let _ = response.send(Err(error.clone()));
        }
    }
}
