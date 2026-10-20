use std::{
    collections::{HashMap, VecDeque},
    num::NonZeroUsize,
    sync::Arc,
};

use agent_input::{
    Admission, Admitted, Delivery, Inbox, Input, InputId, RunId, RunOutcome, Signal, StateError,
};
use futures::{
    FutureExt, StreamExt,
    channel::{mpsc, oneshot},
    stream::BoxStream,
};
use language_model::{
    CompletionRequest, Content, ContentDelta, LanguageModel, LanguageModelProvider, Message, Role,
    ToolArguments, ToolCall, ToolResult, ToolResultContent, Usage,
};
use tool_runtime::{ToolError, ToolErrorKind, ToolProgress, TypedToolOutput};

use crate::{
    Compaction, CompactionError, CompactionPolicy, Conversation, ConversationError,
    executor::{CallControl, ExecutionEvent, ToolExecutor},
    model,
};

#[path = "session_compaction.rs"]
mod compaction_control;
use compaction_control::Compacting;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    PreparingInput,
    Sampling,
    Compacting,
    AwaitingApproval,
    WaitingForTool,
    ExecutingTools,
    Stopping,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Model(#[from] language_model::Error),
    #[error(transparent)]
    Conversation(#[from] ConversationError),
    #[error("unusable completion: {0}")]
    Completion(String),
    #[error("model turn budget exhausted")]
    TurnLimit,
}

#[derive(Clone, Debug)]
pub struct InputOrigin<S> {
    pub id: InputId,
    pub source: S,
}

/// Ordered observations of the single session owner. Committed-message indexes
/// can be applied to a replica seeded with the same initial history. This is a
/// runtime vocabulary, not a frozen wire/persistence schema.
#[derive(Clone, Debug)]
pub struct AgentEvent<S> {
    pub sequence: u64,
    pub run: Option<RunId>,
    pub kind: EventKind<S>,
}

#[derive(Clone, Debug)]
pub enum EventKind<S> {
    InputAdmitted {
        input: InputOrigin<S>,
        delivery: Delivery,
        location: Admission,
    },
    InputsDiscarded {
        inputs: Vec<Input<S, Message>>,
    },
    RunStarted,
    PhaseChanged(Phase),
    Preview {
        index: usize,
        delta: ContentDelta,
    },
    MessageCommitted {
        index: usize,
        message: Message,
        input: Option<InputOrigin<S>>,
    },
    UsageReported(Usage),
    CompactionStarted {
        range: std::ops::Range<usize>,
        previous_revision: usize,
    },
    Compacted(Compaction),
    CompactionFailed(CompactionError),
    ToolStarted(ToolCall),
    ToolProgress {
        call: ToolCall,
        progress: ToolProgress,
    },
    ToolFinished {
        call: ToolCall,
        result: Result<TypedToolOutput, ToolError>,
    },
    RunFinished {
        outcome: RunOutcome,
        error: Option<RunError>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("session owner has closed")]
    Closed,
    #[error(transparent)]
    State(#[from] StateError),
}

/// Completion of a particular run, rather than an admission acknowledgement.
#[derive(Clone, Debug)]
pub struct Reply {
    pub run: RunId,
    pub message: Message,
}

impl Reply {
    pub fn text(&self) -> String {
        self.message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PromptError {
    #[error("session owner has closed")]
    Closed,
    #[error("input rejected: {0}")]
    Rejected(String),
    #[error("input was withdrawn before delivery")]
    Discarded,
    #[error("run {0} was stopped")]
    Stopped(RunId),
    #[error(transparent)]
    Failed(#[from] RunError),
}

type CompletionResponse = oneshot::Sender<Result<Reply, PromptError>>;

/// Rejection retains the submitted work. If a caller drops a submission future
/// after sending, admission can still happen; observe events before retrying.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct SubmitError<S> {
    pub reason: String,
    pub input: Box<Input<S, Message>>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub run: Option<RunId>,
    pub phase: Phase,
    pub paused: bool,
    pub pending_inputs: usize,
    pub committed_messages: usize,
}

enum Command<S> {
    Submit {
        input: Box<Input<S, Message>>,
        delivery: Delivery,
        response: oneshot::Sender<Result<Admitted<S, Message>, SubmitError<S>>>,
        completion: Option<CompletionResponse>,
    },
    Stop {
        run: RunId,
        response: oneshot::Sender<Result<(), StateError>>,
    },
    Resume,
    Snapshot(oneshot::Sender<Snapshot>),
    Compact(oneshot::Sender<Result<Compaction, CompactionError>>),
    ProducerClosed {
        source: S,
        response: oneshot::Sender<agent_input::Withdrawn<S, Message>>,
    },
    Shutdown,
}

/// A command endpoint, not a shared mutable session. All producers route through
/// the same owner, which serializes admission with final completion.
#[derive(Clone)]
pub struct SessionHandle<S> {
    commands: mpsc::UnboundedSender<Command<S>>,
}

impl<S: Clone> SessionHandle<S> {
    pub async fn submit(
        &self,
        input: Input<S, Message>,
        delivery: Delivery,
    ) -> Result<Admitted<S, Message>, SubmitError<S>> {
        self.send(input, delivery, None).await
    }

    /// Wait for the run consuming this input. The owner binds the receipt before
    /// starting any work, so fast completion cannot race an external event watcher.
    /// A dropped future does not retract an already admitted input.
    pub async fn run_input(
        &self,
        input: Input<S, Message>,
        delivery: Delivery,
    ) -> Result<Reply, PromptError> {
        let (completion, reply) = oneshot::channel();
        self.send(input, delivery, Some(completion))
            .await
            .map_err(|error| PromptError::Rejected(error.reason))?;
        reply.await.map_err(|_| PromptError::Closed)?
    }

    async fn send(
        &self,
        input: Input<S, Message>,
        delivery: Delivery,
        completion: Option<CompletionResponse>,
    ) -> Result<Admitted<S, Message>, SubmitError<S>> {
        let backup = Box::new(input.clone());
        let (response, reply) = oneshot::channel();
        if let Err(error) = self.commands.unbounded_send(Command::Submit {
            input: Box::new(input),
            delivery,
            response,
            completion,
        }) {
            let Command::Submit { input, .. } = error.into_inner() else {
                unreachable!()
            };
            return Err(SubmitError {
                reason: "session owner has closed".into(),
                input,
            });
        }
        reply.await.unwrap_or_else(|_| {
            Err(SubmitError {
                reason: "session owner dropped before receipt; admission is unconfirmed".into(),
                input: backup,
            })
        })
    }

    pub async fn stop(&self, run: RunId) -> Result<(), ControlError> {
        let (response, reply) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Stop { run, response })
            .map_err(|_| ControlError::Closed)?;
        reply
            .await
            .map_err(|_| ControlError::Closed)?
            .map_err(ControlError::State)
    }

    pub fn resume(&self) -> Result<(), ControlError> {
        self.commands
            .unbounded_send(Command::Resume)
            .map_err(|_| ControlError::Closed)
    }

    pub async fn snapshot(&self) -> Result<Snapshot, ControlError> {
        let (response, reply) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Snapshot(response))
            .map_err(|_| ControlError::Closed)?;
        reply.await.map_err(|_| ControlError::Closed)
    }

    pub fn shutdown(&self) -> Result<(), ControlError> {
        self.commands
            .unbounded_send(Command::Shutdown)
            .map_err(|_| ControlError::Closed)
    }

    /// Schedule at the next safe boundary. A summary request never dispatches
    /// tools and failure leaves both transcript and model context unchanged.
    pub async fn compact(&self) -> Result<Compaction, CompactionError> {
        let (response, reply) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Compact(response))
            .map_err(|_| CompactionError::Closed)?;
        reply.await.map_err(|_| CompactionError::Closed)?
    }

    pub async fn producer_closed(
        &self,
        source: S,
    ) -> Result<agent_input::Withdrawn<S, Message>, ControlError> {
        let (response, reply) = oneshot::channel();
        self.commands
            .unbounded_send(Command::ProducerClosed { source, response })
            .map_err(|_| ControlError::Closed)?;
        reply.await.map_err(|_| ControlError::Closed)
    }
}

pub struct SessionResult<S> {
    pub conversation: Conversation,
    /// Retained unconsumed work, returned on shutdown or a closed paused session.
    pub pending: Vec<Input<S, Message>>,
}

enum Next {
    Sample,
    Tools,
    Complete,
}

struct Active {
    id: RunId,
    phase: Phase,
    turns: usize,
    next: Next,
    calls: VecDeque<ToolCall>,
    current: Option<ToolCall>,
    dispatched: bool,
    compaction_attempted: bool,
    control: CallControl,
    completions: HashMap<InputId, CompletionResponse>,
    reply: Option<Message>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.control.cancellation.cancel();
    }
}

pub(crate) enum WorkEvent {
    Preview { index: usize, delta: ContentDelta },
    Model(Result<Message, language_model::Error>),
    Compaction(Result<Message, language_model::Error>),
    Tool(ExecutionEvent),
}

/// A single-owner session driver. The host polls run (or spawns and joins it),
/// sends authorized inputs through its handle, and forwards its synchronous
/// event callback to UI/network/journal channels. No global runtime is required.
pub struct AgentSession<S> {
    provider: Arc<dyn LanguageModelProvider>,
    model: LanguageModel,
    executor: ToolExecutor,
    request: CompletionRequest,
    max_model_turns: NonZeroUsize,
    conversation: Conversation,
    inbox: Inbox<S, Message>,
    commands: mpsc::UnboundedReceiver<Command<S>>,
    active: Option<Active>,
    work: Option<BoxStream<'static, WorkEvent>>,
    sequence: u64,
    closing: bool,
    compaction_policy: Option<CompactionPolicy>,
    compacting: Option<Compacting>,
    compaction_requests: Vec<oneshot::Sender<Result<Compaction, CompactionError>>>,
    last_compaction_attempt: Option<(usize, usize)>,
    pending_completions: HashMap<InputId, CompletionResponse>,
}

impl<S: Clone + PartialEq + Send + 'static> AgentSession<S> {
    /// request.messages seeds the conversation. Tools come from the executor;
    /// conflicting explicit definitions are rejected. Other request controls are
    /// preserved for each sample. No automatic retries or compaction are applied.
    pub fn new(
        provider: Arc<dyn LanguageModelProvider>,
        model: LanguageModel,
        executor: ToolExecutor,
        mut request: CompletionRequest,
    ) -> Result<(Self, SessionHandle<S>), RunError> {
        model.validate()?;
        let tools = executor.definitions();
        if !request.tools.is_empty() && request.tools != tools {
            return Err(RunError::Completion(
                "request tools differ from the executor snapshot".into(),
            ));
        }
        request.tools = tools;
        request.validate()?;
        let conversation = Conversation::new(std::mem::take(&mut request.messages))?;
        if !conversation.is_settled() {
            return Err(ConversationError::UnsettledCalls.into());
        }
        let (commands, receiver) = mpsc::unbounded();
        Ok((
            Self {
                provider,
                model,
                executor,
                request,
                max_model_turns: NonZeroUsize::new(64).expect("positive turn budget"),
                conversation,
                inbox: Inbox::default(),
                commands: receiver,
                active: None,
                work: None,
                sequence: 0,
                closing: false,
                compaction_policy: None,
                compacting: None,
                compaction_requests: Vec::new(),
                last_compaction_attempt: None,
                pending_completions: HashMap::new(),
            },
            SessionHandle { commands },
        ))
    }

    pub fn with_max_model_turns(mut self, limit: NonZeroUsize) -> Self {
        self.max_model_turns = limit;
        self
    }

    /// Initial state for observers before moving the owner into its driver.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub fn with_compaction(mut self, policy: CompactionPolicy) -> Self {
        self.compaction_policy = Some(policy);
        self
    }

    /// Restore full history and its compacted context before starting the owner.
    /// Unresolved historical calls require explicit recovery, never re-execution.
    pub fn with_conversation(
        mut self,
        conversation: Conversation,
        next_sequence: u64,
    ) -> Result<Self, RunError> {
        if !conversation.is_settled() {
            return Err(ConversationError::UnsettledCalls.into());
        }
        self.conversation = conversation;
        self.sequence = next_sequence;
        Ok(self)
    }

    /// Closing all handles drains accepted runnable work, then returns. Explicit
    /// shutdown cancels the active run and returns retained queued work. Aborting
    /// this future is hard teardown, not a settled stop; join after shutdown.
    pub async fn run(mut self, mut emit: impl FnMut(AgentEvent<S>)) -> SessionResult<S> {
        let mut disconnected = false;
        loop {
            loop {
                match self.commands.try_recv() {
                    Ok(command) => self.command(command, &mut emit),
                    Err(mpsc::TryRecvError::Closed) => {
                        disconnected = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            if matches!(self.inbox.signal(), Some(Signal::Cancel(_))) && self.active.is_some() {
                self.finish(RunOutcome::Stopped, None, &mut emit);
            }
            if !self.closing && self.work.is_none() {
                self.advance(&mut emit);
            }
            if self.active.is_none()
                && self.work.is_none()
                && !self.closing
                && !self.inbox.is_paused()
                && self.inbox.pending().next().is_some()
            {
                continue;
            }
            if self.active.is_none() && self.work.is_none() && (self.closing || disconnected) {
                return SessionResult {
                    pending: self.inbox.pending().cloned().collect(),
                    conversation: self.conversation,
                };
            }
            if let Some(work) = &mut self.work {
                let wake = {
                    let command = if disconnected {
                        futures::future::pending().boxed()
                    } else {
                        self.commands.next().boxed()
                    }
                    .fuse();
                    let next = work.next().fuse();
                    futures::pin_mut!(command, next);
                    futures::select_biased! {
                        command = command => futures::future::Either::Left(command),
                        event = next => futures::future::Either::Right(event),
                    }
                };
                match wake {
                    futures::future::Either::Left(command) => match command {
                        Some(command) => self.command(command, &mut emit),
                        None => disconnected = true,
                    },
                    futures::future::Either::Right(event) => {
                        if let Some(event) = event {
                            self.observe(event, &mut emit);
                        } else {
                            // Every internal stream ends with a terminal event;
                            // observing that event removes the stream itself.
                            unreachable!("work stream ended without settlement");
                        }
                    }
                }
            } else if self.active.is_none() {
                match self.commands.next().await {
                    Some(command) => self.command(command, &mut emit),
                    None => disconnected = true,
                }
            }
        }
    }

    fn command(&mut self, command: Command<S>, emit: &mut impl FnMut(AgentEvent<S>)) {
        match command {
            Command::Submit {
                input,
                delivery,
                response,
                completion,
            } => {
                let input = *input;
                let invalid = if self.closing {
                    Some("session owner has closed".into())
                } else if completion.is_some() && self.inbox.is_paused() {
                    Some("session is paused; resume before requesting another run".into())
                } else if !matches!(
                    input.payload.role,
                    Role::User | Role::System | Role::Developer
                ) {
                    Some("inputs must be user, system, or developer messages".into())
                } else if self.conversation.contains_message_id(&input.payload.id)
                    || self
                        .inbox
                        .pending()
                        .any(|pending| pending.payload.id == input.payload.id)
                {
                    Some("input message identity is already accepted".into())
                } else {
                    input
                        .payload
                        .validate()
                        .err()
                        .map(|error| error.to_string())
                };
                if let Some(reason) = invalid {
                    let _ = response.send(Err(SubmitError {
                        reason,
                        input: Box::new(input),
                    }));
                    return;
                }
                let origin = InputOrigin {
                    id: input.id,
                    source: input.source.clone(),
                };
                match self.inbox.admit(input, delivery) {
                    Ok(admitted) => {
                        if let Some(completion) = completion {
                            match admitted.location {
                                Admission::Queued => {
                                    self.pending_completions.insert(origin.id, completion);
                                }
                                Admission::Active(_) => {
                                    self.active
                                        .as_mut()
                                        .expect("active admission")
                                        .completions
                                        .insert(origin.id, completion);
                                }
                                Admission::Discarded => {
                                    let _ = completion.send(Err(PromptError::Discarded));
                                }
                            }
                        }
                        self.emit(
                            EventKind::InputAdmitted {
                                input: origin,
                                delivery,
                                location: admitted.location,
                            },
                            emit,
                        );
                        // Interject wakes only the current call's cooperative wait.
                        if matches!(self.inbox.signal(), Some(Signal::YieldWait(_)))
                            && let Some(active) = &self.active
                        {
                            active.control.yield_wait.cancel();
                        }
                        if let Some(discarded) = &admitted.discarded {
                            self.discard(vec![discarded.clone()], emit);
                        }
                        let _ = response.send(Ok(admitted));
                    }
                    Err(rejected) => {
                        let _ = response.send(Err(SubmitError {
                            reason: rejected.reason.to_string(),
                            input: Box::new(rejected.input),
                        }));
                    }
                }
            }
            Command::Stop { run, response } => {
                let _ = response.send(self.inbox.stop(run));
            }
            Command::Resume if !self.closing => self.inbox.resume(),
            Command::Resume => {}
            Command::Snapshot(response) => {
                let _ = response.send(Snapshot {
                    run: self.inbox.run(),
                    phase: if self.compacting.is_some() {
                        Phase::Compacting
                    } else {
                        self.active
                            .as_ref()
                            .map_or(Phase::Idle, |active| active.phase)
                    },
                    paused: self.inbox.is_paused(),
                    pending_inputs: self.inbox.pending().count(),
                    committed_messages: self.conversation.messages().len(),
                });
            }
            Command::Compact(response) => {
                if self.closing {
                    let _ = response.send(Err(CompactionError::Closed));
                } else {
                    self.compaction_requests.push(response);
                }
            }
            Command::ProducerClosed { source, response } => {
                let withdrawn = self.inbox.producer_closed(&source);
                self.discard(withdrawn.inputs.clone(), emit);
                let _ = response.send(withdrawn);
            }
            Command::Shutdown => {
                self.closing = true;
                self.commands.close();
                self.cancel_compaction(emit);
                let withdrawn = self.inbox.shutdown();
                self.discard(withdrawn.inputs, emit);
            }
        }
    }

    fn advance(&mut self, emit: &mut impl FnMut(AgentEvent<S>)) {
        if self.active.is_none() {
            if self.maybe_compact(false, emit) {
                return;
            }
            let Some(delivery) = self
                .inbox
                .begin_next()
                .expect("idle inbox has no reservation")
            else {
                return;
            };
            let id = self.inbox.run().expect("reserved run");
            let input_id = self
                .inbox
                .batch(delivery)
                .expect("initial reservation")
                .inputs[0]
                .id;
            let completions = self
                .pending_completions
                .remove(&input_id)
                .map(|response| (input_id, response))
                .into_iter()
                .collect();
            self.active = Some(Active {
                id,
                phase: Phase::Idle,
                turns: 0,
                next: Next::Sample,
                calls: VecDeque::new(),
                current: None,
                dispatched: false,
                compaction_attempted: false,
                control: CallControl::default(),
                completions,
                reply: None,
            });
            self.emit(EventKind::RunStarted, emit);
            if let Err(error) = self.deliver(delivery, emit) {
                self.finish(RunOutcome::Failed, Some(error.into()), emit);
                return;
            }
            // Return to command processing before inference can start.
            return;
        }
        let id = self.active.as_ref().expect("active run").id;
        if matches!(self.active.as_ref().expect("active run").next, Next::Tools) {
            if let Some(call) = self.active.as_mut().expect("active run").calls.pop_front() {
                let active = self.active.as_mut().expect("active run");
                active.current = Some(call.clone());
                active.dispatched = false;
                active.control = CallControl::default();
                if matches!(self.inbox.signal(), Some(Signal::YieldWait(_))) {
                    active.control.yield_wait.cancel();
                }
                self.work = Some(
                    self.executor
                        .execute(id, call, active.control.clone())
                        .map(WorkEvent::Tool)
                        .boxed(),
                );
                self.phase(Phase::ExecutingTools, emit);
                return;
            }
            self.active.as_mut().expect("active run").next = Next::Sample;
        }
        if let Some(delivery) = self
            .inbox
            .begin_safe_point(id)
            .expect("active settled boundary")
        {
            if let Err(error) = self.deliver(delivery, emit) {
                self.finish(RunOutcome::Failed, Some(error.into()), emit);
                return;
            }
            self.active.as_mut().expect("active run").next = Next::Sample;
            return;
        }
        if matches!(
            self.active.as_ref().expect("active run").next,
            Next::Complete
        ) {
            if self.maybe_compact(false, emit) {
                return;
            }
            self.finish(RunOutcome::Completed, None, emit);
            return;
        }
        if self.maybe_compact(true, emit) {
            return;
        }
        let active = self.active.as_mut().expect("active run");
        if active.turns >= self.max_model_turns.get() {
            self.finish(RunOutcome::Failed, Some(RunError::TurnLimit), emit);
            return;
        }
        active.turns += 1;
        let mut request = self.request.clone();
        request.messages = self.conversation.model_messages();
        self.work = Some(model::sample(
            self.provider.clone(),
            self.model.clone(),
            request,
        ));
        self.phase(Phase::Sampling, emit);
    }

    fn deliver(
        &mut self,
        delivery: agent_input::DeliveryId,
        emit: &mut impl FnMut(AgentEvent<S>),
    ) -> Result<(), ConversationError> {
        self.phase(Phase::PreparingInput, emit);
        let conversation = &mut self.conversation;
        let batch = self
            .inbox
            .commit(delivery, |batch| {
                conversation
                    .append(
                        batch
                            .inputs
                            .iter()
                            .map(|input| input.payload.clone())
                            .collect(),
                    )
                    .map(|_| ())
            })
            .expect("owned delivery")?;
        let start = self.conversation.messages().len() - batch.inputs.len();
        for (offset, input) in batch.inputs.into_iter().enumerate() {
            self.emit(
                EventKind::MessageCommitted {
                    index: start + offset,
                    message: input.payload,
                    input: Some(InputOrigin {
                        id: input.id,
                        source: input.source,
                    }),
                },
                emit,
            );
        }
        Ok(())
    }

    fn observe(&mut self, event: WorkEvent, emit: &mut impl FnMut(AgentEvent<S>)) {
        match event {
            WorkEvent::Compaction(result) => self.complete_compaction(result, emit),
            WorkEvent::Preview { index, delta } => {
                self.emit(EventKind::Preview { index, delta }, emit)
            }
            WorkEvent::Model(result) => {
                self.work = None;
                self.active
                    .as_mut()
                    .expect("model belongs to run")
                    .compaction_attempted = false;
                let message = match result {
                    Ok(message) => message,
                    Err(error) => {
                        self.finish(RunOutcome::Failed, Some(error.into()), emit);
                        return;
                    }
                };
                // A completed request consumed tokens even when its stop reason
                // or calls are unusable for execution.
                if let Some(usage) = message
                    .generation
                    .as_ref()
                    .and_then(|metadata| metadata.usage.clone())
                {
                    self.emit(EventKind::UsageReported(usage), emit);
                }
                if let Err(error) = usable(&message) {
                    self.finish(RunOutcome::Failed, Some(error), emit);
                    return;
                }
                let calls = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolCall(call) => Some(call.clone()),
                        _ => None,
                    })
                    .collect::<VecDeque<_>>();
                let reply = message.clone();
                if let Err(error) = self.append(message, emit) {
                    self.finish(RunOutcome::Failed, Some(error.into()), emit);
                    return;
                }
                let active = self.active.as_mut().expect("model belongs to run");
                active.reply = Some(reply);
                active.next = if calls.is_empty() {
                    Next::Complete
                } else {
                    Next::Tools
                };
                active.calls = calls;
            }
            WorkEvent::Tool(event) => {
                let call = self
                    .active
                    .as_ref()
                    .and_then(|active| active.current.clone())
                    .expect("tool belongs to run");
                match event {
                    ExecutionEvent::AwaitingApproval => self.phase(Phase::AwaitingApproval, emit),
                    ExecutionEvent::WaitingForCapacity => self.phase(Phase::WaitingForTool, emit),
                    ExecutionEvent::Started => {
                        self.active.as_mut().expect("active run").dispatched = true;
                        self.phase(Phase::ExecutingTools, emit);
                        self.emit(EventKind::ToolStarted(call), emit);
                    }
                    ExecutionEvent::Progress(progress) => {
                        self.emit(EventKind::ToolProgress { call, progress }, emit)
                    }
                    ExecutionEvent::Finished(result) => {
                        self.work = None;
                        self.settle(call, result, emit);
                        self.active.as_mut().expect("active run").current = None;
                    }
                }
            }
        }
    }

    fn append(
        &mut self,
        message: Message,
        emit: &mut impl FnMut(AgentEvent<S>),
    ) -> Result<(), ConversationError> {
        let index = self.conversation.append(vec![message.clone()])?;
        self.emit(
            EventKind::MessageCommitted {
                index,
                message,
                input: None,
            },
            emit,
        );
        Ok(())
    }

    fn settle(
        &mut self,
        call: ToolCall,
        result: Result<TypedToolOutput, ToolError>,
        emit: &mut impl FnMut(AgentEvent<S>),
    ) {
        let (content, is_error) = match &result {
            Ok(output) => (output.model_output.clone(), false),
            Err(error) => (
                vec![ToolResultContent::Json {
                    value: serde_json::to_value(error).expect("tool error is serializable"),
                }],
                true,
            ),
        };
        let message = Message::new(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call: call.id.clone(),
                provider_id: call.provider_id.clone(),
                name: Some(call.name.clone()),
                content,
                is_error,
            })],
        );
        self.append(message, emit)
            .expect("owned outstanding tool call");
        self.emit(EventKind::ToolFinished { call, result }, emit);
    }

    fn finish(
        &mut self,
        outcome: RunOutcome,
        error: Option<RunError>,
        emit: &mut impl FnMut(AgentEvent<S>),
    ) {
        let id = self.active.as_ref().expect("finishing active run").id;
        if outcome != RunOutcome::Completed {
            self.cancel_compaction(emit);
            self.phase(Phase::Stopping, emit);
            self.active
                .as_ref()
                .expect("active run")
                .control
                .cancellation
                .cancel();
            self.work = None;
            for call in self.conversation.pending_calls() {
                let active = self.active.as_ref().expect("active run");
                let may_have_started = active.dispatched
                    && active
                        .current
                        .as_ref()
                        .is_some_and(|current| current.id == call.id);
                let error = ToolError::new(
                    ToolErrorKind::Cancelled,
                    if may_have_started {
                        "run stopped while awaiting tool; external effects may have occurred"
                    } else {
                        "run ended before this call was dispatched"
                    },
                )
                .with_details(serde_json::json!({"execution_may_have_started": may_have_started}));
                self.settle(call, Err(error), emit);
            }
        }
        let discarded = self
            .inbox
            .finish_run(id, outcome)
            .expect("completion checked at serialized safe point");
        self.discard(discarded, emit);
        let response = match outcome {
            RunOutcome::Completed => Ok(Reply {
                run: id,
                message: self
                    .active
                    .as_ref()
                    .and_then(|active| active.reply.clone())
                    .expect("completed run has an answer"),
            }),
            RunOutcome::Stopped => Err(PromptError::Stopped(id)),
            RunOutcome::Failed => Err(PromptError::Failed(
                error.clone().expect("failed run has an error"),
            )),
        };
        self.emit(EventKind::RunFinished { outcome, error }, emit);
        for (_, completion) in self
            .active
            .as_mut()
            .expect("active run")
            .completions
            .drain()
        {
            let _ = completion.send(response.clone());
        }
        self.active = None;
    }

    fn discard(&mut self, inputs: Vec<Input<S, Message>>, emit: &mut impl FnMut(AgentEvent<S>)) {
        for input in &inputs {
            if let Some(response) = self.pending_completions.remove(&input.id) {
                let _ = response.send(Err(PromptError::Discarded));
            }
            if let Some(active) = &mut self.active
                && let Some(response) = active.completions.remove(&input.id)
            {
                let _ = response.send(Err(PromptError::Discarded));
            }
        }
        if !inputs.is_empty() {
            self.emit(EventKind::InputsDiscarded { inputs }, emit);
        }
    }

    fn phase(&mut self, phase: Phase, emit: &mut impl FnMut(AgentEvent<S>)) {
        let active = self.active.as_mut().expect("phase has a run");
        if active.phase != phase {
            active.phase = phase;
            self.emit(EventKind::PhaseChanged(phase), emit);
        }
    }

    fn emit(&mut self, kind: EventKind<S>, emit: &mut impl FnMut(AgentEvent<S>)) {
        let sequence = self.sequence;
        self.sequence += 1;
        emit(AgentEvent {
            sequence,
            run: self.active.as_ref().map(|active| active.id),
            kind,
        });
    }
}

fn usable(message: &Message) -> Result<(), RunError> {
    let reason = message
        .generation
        .as_ref()
        .and_then(|metadata| metadata.finish_reason.as_deref());
    if !matches!(
        reason,
        Some("stop" | "tool_calls" | "end_turn" | "tool_use")
    ) {
        return Err(RunError::Completion(format!("finish reason {reason:?}")));
    }
    if message.content.iter().any(|content| matches!(content, Content::ToolCall(call) if matches!(call.arguments, ToolArguments::Invalid { .. }))) {
        return Err(RunError::Completion("malformed tool arguments".into()));
    }
    Ok(())
}
