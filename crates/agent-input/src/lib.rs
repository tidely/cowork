//! Host-owned input scheduling for an agent conversation.
//!
//! Sources and payloads are application-defined. An owner serializes mutations;
//! providers and tools never own these queues. Reservations stay here while a
//! host prepares model input asynchronously, and leave only on synchronous
//! commitment. This is an in-memory lifecycle, not a durable journal or an
//! authorization boundary. See the crate README for the host integration contract.

#![doc = include_str!("../README.md")]

use std::collections::VecDeque;

use thiserror::Error;
use uuid::Uuid;

macro_rules! identity {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

identity!(InputId);
identity!(RunId);
identity!(DeliveryId);

/// Delivery intent, independent of the producer's identity or trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Start a later run, preserving FIFO within this lane.
    Queue,
    /// Join the active run at a conversation-safe boundary.
    Steer,
    /// Like Steer, but ask a cooperative blocking wait to yield sooner.
    /// Does not authorize cancellation of an arbitrary tool or model request.
    Interject,
    /// Cancel the active run and start this input ahead of ordinary queued work.
    /// Multiple interrupts preserve their own admission order.
    Interrupt,
}

/// Queue presentation and editability are unrelated to delivery priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueAccess {
    Editable,
    Protected,
    Hidden,
}

/// What happens to an unconsumed input when its bound run ends (or is absent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    Queue,
    Discard,
}

/// Lifetime of pending work. Delivered work is the conversation's responsibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifetime {
    /// Keep pending work across producer closure and session shutdown.
    Retain,
    /// Withdraw if the host reports that its source has closed.
    ProducerBound,
    /// Disposable runtime context, withdrawn on source closure or shutdown.
    /// Unlike ProducerBound work, its producer need not be cancelled by the host.
    Ephemeral,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub access: QueueAccess,
    pub fallback: Fallback,
    pub lifetime: Lifetime,
}

/// S is a host-defined origin (participant, parent, tool, timer, extension, ...).
/// P may be a prepared language_model::Message or an unprepared submission.
/// Neither origin nor policy confers permissions; the host admits authorized work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input<S, P> {
    pub id: InputId,
    pub source: S,
    pub payload: P,
    pub policy: Policy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Queued,
    Active(RunId),
    /// The input was not accepted; ownership is returned to the host.
    Discarded,
}

#[derive(Debug)]
pub struct Admitted<S, P> {
    pub location: Admission,
    pub discarded: Option<Input<S, P>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Stopped,
    Failed,
}

/// Level-triggered requests, scoped to a run so late notifications cannot affect
/// its successor. The owner performs the effect and calls finish_run afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Cancel(RunId),
    YieldWait(RunId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    Start(RunId),
    Continue(RunId),
}

/// A stable snapshot. New arrivals cannot join a preparation already in progress.
#[derive(Debug, PartialEq, Eq)]
pub struct Batch<S, P> {
    pub id: DeliveryId,
    pub destination: Destination,
    pub inputs: Vec<Input<S, P>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum StateError {
    #[error("input identity is already pending")]
    Duplicate,
    #[error("run is no longer active")]
    StaleRun,
    #[error("delivery reservation is no longer active")]
    StaleDelivery,
    #[error("another delivery is being prepared")]
    Preparing,
    #[error("an input must be editable and unreserved")]
    NotEditable,
    #[error("active inputs must be committed before completing the run")]
    Undelivered,
}

#[derive(Debug)]
pub struct Rejected<S, P> {
    pub reason: StateError,
    pub input: Input<S, P>,
}

/// A source closing can invalidate preparation already running elsewhere. The
/// host must discard that preparation and use the replacement snapshot, if any.
#[derive(Debug)]
pub struct Withdrawn<S, P> {
    pub inputs: Vec<Input<S, P>>,
    pub invalidated: Option<DeliveryId>,
    pub replacement: Option<DeliveryId>,
}

struct Running {
    id: RunId,
    started: bool,
    cancel: bool,
}

/// Four FIFO lanes: ordinary future runs, interrupt follow-ups, interjections,
/// and steering. Interjections precede steering at each safe point. A single
/// reservation keeps prepared-but-uncommitted input owned across cancellation.
pub struct Inbox<S, P> {
    queued: VecDeque<Input<S, P>>,
    interrupts: VecDeque<Input<S, P>>,
    interjections: VecDeque<Input<S, P>>,
    steering: VecDeque<Input<S, P>>,
    preparing: Option<Batch<S, P>>,
    running: Option<Running>,
    paused: bool,
}

impl<S, P> Default for Inbox<S, P> {
    fn default() -> Self {
        Self {
            queued: VecDeque::new(),
            interrupts: VecDeque::new(),
            interjections: VecDeque::new(),
            steering: VecDeque::new(),
            preparing: None,
            running: None,
            paused: false,
        }
    }
}

impl<S, P> Inbox<S, P> {
    /// All pending inputs, including reserved ones. Lane order, not global arrival
    /// order: interrupt follow-ups, ordinary queue, interjections, steering, batch.
    pub fn pending(&self) -> impl Iterator<Item = &Input<S, P>> {
        self.interrupts
            .iter()
            .chain(&self.queued)
            .chain(&self.interjections)
            .chain(&self.steering)
            .chain(self.preparing.iter().flat_map(|batch| &batch.inputs))
    }

    pub fn run(&self) -> Option<RunId> {
        self.running.as_ref().map(|run| run.id)
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    pub fn admit(
        &mut self,
        input: Input<S, P>,
        delivery: Delivery,
    ) -> Result<Admitted<S, P>, Rejected<S, P>> {
        if self.pending().any(|pending| pending.id == input.id) {
            return Err(Rejected {
                reason: StateError::Duplicate,
                input,
            });
        }
        let location = match delivery {
            Delivery::Queue => {
                self.queued.push_back(input);
                Admission::Queued
            }
            Delivery::Interrupt => {
                self.interrupts.push_back(input);
                if let Some(run) = &mut self.running {
                    run.cancel = true;
                }
                // Explicit interrupt follow-ups request automatic handoff.
                self.paused = false;
                Admission::Queued
            }
            Delivery::Steer | Delivery::Interject => {
                if let Some(run) = self.running.as_ref().filter(|run| !run.cancel) {
                    let id = run.id;
                    match delivery {
                        Delivery::Interject => self.interjections.push_back(input),
                        _ => self.steering.push_back(input),
                    }
                    Admission::Active(id)
                } else if input.policy.fallback == Fallback::Queue {
                    self.queued.push_back(input);
                    Admission::Queued
                } else {
                    return Ok(Admitted {
                        location: Admission::Discarded,
                        discarded: Some(input),
                    });
                }
            }
        };
        Ok(Admitted {
            location,
            discarded: None,
        })
    }

    pub fn signal(&self) -> Option<Signal> {
        let run = self.running.as_ref()?;
        if run.cancel {
            Some(Signal::Cancel(run.id))
        } else if !self.interjections.is_empty() {
            Some(Signal::YieldWait(run.id))
        } else {
            None
        }
    }

    /// Stop is control, not model-visible text. Accepted work stays pending and
    /// the queue pauses until resume (or a subsequent explicit Interrupt).
    pub fn stop(&mut self, run: RunId) -> Result<(), StateError> {
        self.running_mut(run)?.cancel = true;
        self.paused = true;
        Ok(())
    }

    /// Reserve the next run's first input. Do not start inference until its
    /// transcript commit succeeds. While preparing it, arrivals can still steer.
    pub fn begin_next(&mut self) -> Result<Option<DeliveryId>, StateError> {
        if self.preparing.is_some() {
            return Err(StateError::Preparing);
        }
        if self.running.is_some() || self.paused {
            return Ok(None);
        }
        let Some(input) = self
            .interrupts
            .pop_front()
            .or_else(|| self.queued.pop_front())
        else {
            return Ok(None);
        };
        let run = RunId::new();
        self.running = Some(Running {
            id: run,
            started: false,
            cancel: false,
        });
        Ok(Some(self.reserve(Destination::Start(run), vec![input])))
    }

    /// Call only with a valid conversation boundary: all accepted tool calls
    /// settled, and no partially streamed assistant reply being committed.
    pub fn begin_safe_point(&mut self, run: RunId) -> Result<Option<DeliveryId>, StateError> {
        let running = self.running_mut(run)?;
        if running.cancel || !running.started {
            return Ok(None);
        }
        if self.preparing.is_some() {
            return Err(StateError::Preparing);
        }
        let inputs: Vec<_> = self
            .interjections
            .drain(..)
            .chain(self.steering.drain(..))
            .collect();
        if inputs.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.reserve(Destination::Continue(run), inputs)))
    }

    pub fn batch(&self, id: DeliveryId) -> Result<&Batch<S, P>, StateError> {
        self.preparing
            .as_ref()
            .filter(|batch| batch.id == id)
            .ok_or(StateError::StaleDelivery)
    }

    /// Commit model history and its journal together in this synchronous callback.
    /// On error the reservation remains owned here for retry or run settlement.
    /// The callback must be atomic on error; this layer cannot undo its effects.
    pub fn commit<E>(
        &mut self,
        id: DeliveryId,
        commit: impl FnOnce(&Batch<S, P>) -> Result<(), E>,
    ) -> Result<Result<Batch<S, P>, E>, StateError> {
        let batch = self.batch(id)?;
        if self.running.as_ref().is_none_or(|run| run.cancel) {
            return Err(StateError::StaleDelivery);
        }
        if let Err(error) = commit(batch) {
            return Ok(Err(error));
        }
        let batch = self.preparing.take().expect("validated reservation");
        self.running
            .as_mut()
            .expect("reservation has a run")
            .started = true;
        Ok(Ok(batch))
    }

    /// Settles the run, invalidates reservations, and returns explicitly discarded
    /// inputs. Completion is refused while admitted work still needs consumption.
    /// Stop/failure recover it according to fallback, ahead of the ordinary queue.
    pub fn finish_run(
        &mut self,
        run: RunId,
        outcome: RunOutcome,
    ) -> Result<Vec<Input<S, P>>, StateError> {
        self.running_mut(run)?;
        if outcome == RunOutcome::Completed
            && (self.preparing.is_some()
                || !self.interjections.is_empty()
                || !self.steering.is_empty())
        {
            return Err(StateError::Undelivered);
        }
        let cancelled = self.running.take().expect("validated run").cancel;
        let mut recovery = Vec::new();
        if let Some(batch) = self.preparing.take() {
            // An uncommitted initial prompt was never consumed; retain it even
            // when its mid-run fallback policy would have discarded it.
            let initial = matches!(batch.destination, Destination::Start(_));
            recovery.extend(batch.inputs.into_iter().map(|input| (input, initial)));
        }
        recovery.extend(
            self.interjections
                .drain(..)
                .chain(self.steering.drain(..))
                .map(|input| (input, false)),
        );
        let mut retained = VecDeque::new();
        let mut discarded = Vec::new();
        for (input, initial) in recovery {
            if initial || input.policy.fallback == Fallback::Queue {
                retained.push_back(input);
            } else {
                discarded.push(input);
            }
        }
        retained.append(&mut self.queued);
        self.queued = retained;
        // Interrupts hand off automatically; explicit stop keeps paused even if
        // an older interrupt is queued. Failures always require host resumption.
        if outcome == RunOutcome::Failed || (outcome == RunOutcome::Stopped && !cancelled) {
            self.paused = true;
        }
        Ok(discarded)
    }

    /// Owner-authorized queue edit. Source permissions/version checks belong to
    /// the host; reserved and run-bound inputs cannot be edited or removed here.
    pub fn edit(&mut self, id: InputId, payload: P) -> Result<(), StateError> {
        let input = self
            .interrupts
            .iter_mut()
            .chain(&mut self.queued)
            .find(|input| input.id == id && input.policy.access == QueueAccess::Editable)
            .ok_or(StateError::NotEditable)?;
        input.payload = payload;
        Ok(())
    }

    pub fn remove(&mut self, id: InputId) -> Result<Input<S, P>, StateError> {
        for lane in [&mut self.interrupts, &mut self.queued] {
            if let Some(index) = lane
                .iter()
                .position(|input| input.id == id && input.policy.access == QueueAccess::Editable)
            {
                return Ok(lane.remove(index).expect("existing queue position"));
            }
        }
        Err(StateError::NotEditable)
    }

    /// Change a held prompt's delivery intent atomically: it cannot remain queued
    /// as well as becoming a steer. Authorization is the same as a queue edit.
    pub fn redirect(
        &mut self,
        id: InputId,
        delivery: Delivery,
    ) -> Result<Admitted<S, P>, StateError> {
        let input = self.remove(id)?;
        Ok(self
            .admit(input, delivery)
            .unwrap_or_else(|_| unreachable!("removed identity is unique")))
    }

    /// Reorder the ordinary queue. Interrupt priority remains a separate lane.
    /// None places the row at the end; only the moved row must be editable.
    pub fn move_before(&mut self, id: InputId, before: Option<InputId>) -> Result<(), StateError> {
        let index = self
            .queued
            .iter()
            .position(|input| input.id == id && input.policy.access == QueueAccess::Editable)
            .ok_or(StateError::NotEditable)?;
        if before == Some(id) {
            return Ok(());
        }
        if before.is_some_and(|target| !self.queued.iter().any(|input| input.id == target)) {
            return Err(StateError::NotEditable);
        }
        let input = self.queued.remove(index).expect("existing queue position");
        let destination = before
            .and_then(|target| self.queued.iter().position(|input| input.id == target))
            .unwrap_or(self.queued.len());
        self.queued.insert(destination, input);
        Ok(())
    }

    pub fn preparation(&self) -> Option<&Batch<S, P>> {
        self.preparing.as_ref()
    }

    /// Remove only uncommitted, producer-scoped work. Never retract a committed
    /// tool result/message or cancel its run merely because its source closed.
    /// If a run's initial input is withdrawn before commit, request cancellation.
    pub fn producer_closed(&mut self, source: &S) -> Withdrawn<S, P>
    where
        S: PartialEq,
    {
        self.withdraw(|input| &input.source == source && input.policy.lifetime != Lifetime::Retain)
    }

    /// Stop the session and release non-retained work to the host for settlement
    /// (including cancellation of ProducerBound producers where necessary).
    /// Retained input stays here and may resume in this process. This is not disk
    /// persistence; dropping the Inbox still drops its contents.
    pub fn shutdown(&mut self) -> Withdrawn<S, P> {
        self.paused = true;
        if let Some(run) = &mut self.running {
            run.cancel = true;
        }
        self.withdraw(|input| input.policy.lifetime != Lifetime::Retain)
    }

    fn withdraw(&mut self, predicate: impl Fn(&Input<S, P>) -> bool) -> Withdrawn<S, P> {
        let mut inputs = Vec::new();
        for lane in [
            &mut self.interrupts,
            &mut self.queued,
            &mut self.interjections,
            &mut self.steering,
        ] {
            let mut retained = VecDeque::new();
            for input in lane.drain(..) {
                if predicate(&input) {
                    inputs.push(input);
                } else {
                    retained.push_back(input);
                }
            }
            *lane = retained;
        }
        let mut invalidated = None;
        let mut replacement = None;
        if self
            .preparing
            .as_ref()
            .is_some_and(|batch| batch.inputs.iter().any(&predicate))
        {
            let batch = self.preparing.take().expect("matched batch");
            invalidated = Some(batch.id);
            let mut retained = Vec::new();
            for input in batch.inputs {
                if predicate(&input) {
                    inputs.push(input);
                } else {
                    retained.push(input);
                }
            }
            if retained.is_empty() {
                if matches!(batch.destination, Destination::Start(_)) {
                    self.running
                        .as_mut()
                        .expect("initial reservation has a run")
                        .cancel = true;
                }
            } else {
                replacement = Some(self.reserve(batch.destination, retained));
            }
        }
        Withdrawn {
            inputs,
            invalidated,
            replacement,
        }
    }

    fn reserve(&mut self, destination: Destination, inputs: Vec<Input<S, P>>) -> DeliveryId {
        let id = DeliveryId::new();
        self.preparing = Some(Batch {
            id,
            destination,
            inputs,
        });
        id
    }

    fn running_mut(&mut self, id: RunId) -> Result<&mut Running, StateError> {
        self.running
            .as_mut()
            .filter(|run| run.id == id)
            .ok_or(StateError::StaleRun)
    }
}

#[cfg(test)]
mod tests;
