use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use agent_input::{
    Admission, Delivery, DeliveryId, Inbox, Input, InputId, RunId, RunOutcome, Signal,
};
use futures::{
    FutureExt, StreamExt,
    channel::{mpsc, oneshot},
    stream::BoxStream,
};
use language_model::{
    CachePoint, CompletionRequest, Content, ContentDelta, FinishReason, LanguageModelProvider,
    Message, Role, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolResultContent, Usage,
};
use tool_runtime::{CallControl, ToolError, ToolErrorKind, ToolProgress, TypedToolOutput};

use crate::{
    AgentDefinition, AgentHost, ApprovalDecision, Command, Compaction, CompactionError,
    CompletionResponder, Configuration, ConfigurationChange, Conversation, ConversationError,
    DefinitionRevision, DynamicToolChange, Entry, Grant, Origin, PromptError, RenderInput, Reply,
    Request, RequestError, RequestKind, Responder, Response, SessionHandle, Snapshot,
    context::{
        self, ContextSection, ContextUpdate, DynamicToolsSection, ErasedSection, ModelSection,
        SessionView, ToolPolicySection,
    },
    executor::{self, CallScope, ExecutionEvent},
    model,
    tools::{BUILTIN_TOOLS, HostRegistry, ToolSnapshot},
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
    #[error("the run reached its step limit")]
    StepLimit,
}

/// Why inputs left the inbox without being committed. With admissions and
/// committed `Origin::Input` entries, this lets an observer mirror the inbox
/// from events alone. A refused `RemoveInput` emits nothing: the input was
/// already committed or discarded, and that event came first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardReason {
    /// A `RemoveInput` request withdrew it.
    Removed,
    /// It had no run to join, and its policy's fallback discards it.
    Fallback,
    /// Its producer-bound or ephemeral source closed.
    SourceClosed,
    /// The session shut down.
    Shutdown,
}

/// The session could not be built as configured.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("invalid session configuration: {0}")]
pub struct SessionError(pub String);

/// Ordered observations of the single session owner. Committed-entry indexes
/// can be applied to a replica with the same history. A runtime vocabulary,
/// not a frozen wire or persistence schema.
#[derive(Clone, Debug)]
pub struct AgentEvent<H: AgentHost> {
    pub sequence: u64,
    pub run: Option<RunId>,
    pub kind: EventKind<H>,
}

#[derive(Clone, Debug)]
pub enum EventKind<H: AgentHost> {
    InputAdmitted {
        id: InputId,
        source: H::Source,
        delivery: Delivery,
        location: Admission,
    },
    /// Inputs leaving the inbox without being committed, for producer receipts.
    InputsDiscarded {
        inputs: Vec<Input<H::Source, H::Input>>,
        reason: DiscardReason,
    },
    RunStarted,
    PhaseChanged(Phase),
    Preview {
        index: usize,
        delta: ContentDelta,
    },
    MessageCommitted {
        index: usize,
        entry: Entry<H::Source>,
    },
    UsageReported(Usage),
    CompactionStarted {
        end: usize,
        previous_revision: usize,
    },
    Compacted(Compaction),
    CompactionFailed(CompactionError),
    /// Answer with a `ResolveApproval` request naming this call.
    ApprovalRequested {
        call: ToolCall,
        tool: String,
        access: H::Access,
    },
    /// `tool` is the real tool, also when the model used `call_tool`.
    ToolStarted {
        call: ToolCall,
        tool: String,
    },
    ToolProgress {
        call: ToolCall,
        progress: ToolProgress,
    },
    ToolFinished {
        call: ToolCall,
        /// `None` when the call never resolved to a tool.
        tool: Option<String>,
        result: Result<TypedToolOutput, ToolError>,
    },
    DefinitionReplaced {
        revision: DefinitionRevision,
    },
    ConfigurationChanged(ConfigurationChange<H::Permissions>),
    DynamicToolAdded(String),
    DynamicToolRemoved(String),
    RunFinished {
        outcome: RunOutcome,
        error: Option<RunError>,
    },
}

pub struct SessionResult<H: AgentHost> {
    pub conversation: Conversation<H::Source>,
    /// Retained unconsumed work, returned on shutdown or a closed paused session.
    pub pending: Vec<Input<H::Source, H::Input>>,
}

enum Next {
    Sample,
    Tools,
    Complete,
}

/// What one model request offered, for running the calls in its reply.
struct Step<H: AgentHost> {
    tools: Arc<ToolSnapshot<H>>,
    revision: DefinitionRevision,
}

struct PendingApproval<A> {
    call: ToolCallId,
    access: A,
    respond: oneshot::Sender<ApprovalDecision>,
}

struct Active<H: AgentHost> {
    id: RunId,
    /// Who sent the input that started this run.
    origin: H::Source,
    phase: Phase,
    steps: usize,
    next: Next,
    step: Option<Step<H>>,
    calls: VecDeque<ToolCall>,
    current: Option<ToolCall>,
    current_tool: Option<String>,
    dispatched: bool,
    approval: Option<PendingApproval<H::Access>>,
    grants: Vec<Grant<H::Access>>,
    compaction_attempted: bool,
    control: CallControl,
    completions: HashMap<InputId, CompletionResponder>,
    reply: Option<Message>,
}

impl<H: AgentHost> Drop for Active<H> {
    fn drop(&mut self) {
        self.control.cancellation.cancel();
    }
}

pub(crate) enum WorkEvent<A> {
    Preview { index: usize, delta: ContentDelta },
    Model(Result<Message, language_model::Error>),
    Compaction(Result<Message, language_model::Error>),
    Tool(ExecutionEvent<A>),
}

/// A single-owner session driver. The host polls `run` (or spawns and joins
/// it), sends requests through its handle, and forwards the synchronous event
/// callback to UI, network, or journal channels.
pub struct AgentSession<H: AgentHost> {
    host: Arc<H>,
    provider: Arc<dyn LanguageModelProvider>,
    definition: AgentDefinition,
    revision: DefinitionRevision,
    configuration: Configuration<H::Permissions>,
    /// The base tools, offered in every request; fixed for the session's life.
    registry: HostRegistry<H>,
    /// Whether `search_tools` and `call_tool` are offered, reaching `dynamic`.
    dynamic_tools: bool,
    dynamic: HostRegistry<H>,
    /// The current tool snapshot; each step captures it.
    tools: Arc<ToolSnapshot<H>>,
    sections: Vec<Box<dyn ErasedSection<H>>>,
    conversation: Conversation<H::Source>,
    inbox: Inbox<H::Source, H::Input>,
    commands: mpsc::UnboundedReceiver<Command<H>>,
    active: Option<Active<H>>,
    work: Option<BoxStream<'static, WorkEvent<H::Access>>>,
    sequence: u64,
    closing: bool,
    compacting: Option<Compacting>,
    compaction_requests: Vec<Responder<H>>,
    last_compaction_attempt: Option<(usize, usize)>,
    pending_completions: HashMap<InputId, CompletionResponder>,
    /// Results settled while restoring, reported once the driver starts.
    recovered: Vec<(usize, Entry<H::Source>)>,
}

impl<H: AgentHost> AgentSession<H> {
    /// `registry` holds the session's base tools: each is offered in every
    /// request, in registration order, after `search_tools` and `call_tool`,
    /// for the session's whole life, so the prompt prefix stays cacheable.
    /// Tools that come and go are dynamic tools; see
    /// [`without_dynamic_tools`](Self::without_dynamic_tools). The built-in
    /// context sections (tool policy, dynamic tools, model) are installed;
    /// add others with `with_section`.
    pub fn new(
        host: Arc<H>,
        provider: Arc<dyn LanguageModelProvider>,
        definition: AgentDefinition,
        configuration: Configuration<H::Permissions>,
        registry: HostRegistry<H>,
    ) -> Result<(Self, SessionHandle<H>), SessionError> {
        definition
            .model
            .validate()
            .map_err(|error| SessionError(error.to_string()))?;
        if let Some(name) = BUILTIN_TOOLS.iter().find(|name| registry.contains(name)) {
            return Err(SessionError(format!("{name} is a built-in tool name")));
        }
        let dynamic = HostRegistry::<H>::default();
        let tools = ToolSnapshot::new(&registry, true, dynamic.clone());
        let (commands, receiver) = mpsc::unbounded();
        Ok((
            Self {
                host,
                provider,
                definition,
                revision: DefinitionRevision::default(),
                configuration,
                registry,
                dynamic_tools: true,
                dynamic,
                tools: Arc::new(tools),
                sections: vec![
                    Box::new(ToolPolicySection),
                    Box::new(DynamicToolsSection),
                    Box::new(ModelSection),
                ],
                conversation: Conversation::new(),
                inbox: Inbox::default(),
                commands: receiver,
                active: None,
                work: None,
                sequence: 0,
                closing: false,
                compacting: None,
                compaction_requests: Vec::new(),
                last_compaction_attempt: None,
                pending_completions: HashMap::new(),
                recovered: Vec::new(),
            },
            SessionHandle { commands },
        ))
    }

    /// Leaves out `search_tools` and `call_tool`, so the session has only its
    /// base tools and refuses `DynamicTools` requests.
    pub fn without_dynamic_tools(mut self) -> Self {
        self.dynamic_tools = false;
        self.tools = Arc::new(ToolSnapshot::new(
            &self.registry,
            false,
            self.dynamic.clone(),
        ));
        self
    }

    /// Adds a host context section, rendered after the built-in ones.
    pub fn with_section(mut self, section: impl ContextSection<H>) -> Self {
        self.sections.push(Box::new(section));
        self
    }

    /// Restore a saved conversation before starting the owner. Unresolved
    /// historical calls require explicit recovery, never re-execution.
    pub fn with_conversation(
        mut self,
        conversation: Conversation<H::Source>,
        next_sequence: u64,
    ) -> Result<Self, SessionError> {
        if !conversation.is_settled() {
            return Err(SessionError(ConversationError::UnsettledCalls.to_string()));
        }
        self.conversation = conversation;
        self.sequence = next_sequence;
        Ok(self)
    }

    /// Restore a conversation whose session stopped without settling its
    /// calls (a crash). A call that never started, because it was waiting for
    /// approval or behind another call, is settled as not run: authorization
    /// comes before dispatch, so it had no effect. A call that may have
    /// started is refused; it needs explicit recovery, never re-execution.
    /// `started` says which calls were dispatched (see
    /// `LoadedSession::started_calls`).
    pub fn with_interrupted_conversation(
        mut self,
        mut conversation: Conversation<H::Source>,
        next_sequence: u64,
        started: impl Fn(&ToolCallId) -> bool,
    ) -> Result<Self, SessionError> {
        let pending = conversation.pending_calls();
        if let Some(call) = pending.iter().find(|call| started(&call.id)) {
            return Err(SessionError(format!(
                "call {} to {} may have run before the session stopped; it needs explicit recovery",
                call.id.0, call.name
            )));
        }
        for call in pending {
            let error = ToolError::new(
                ToolErrorKind::Cancelled,
                "the session stopped before this call ran",
            )
            .with_details(serde_json::json!({"execution_may_have_started": false}));
            let entry = result_entry(&call, &Err(error));
            let index = conversation
                .append(vec![entry.clone()])
                .map_err(|error| SessionError(error.to_string()))?;
            self.recovered.push((index, entry));
        }
        self.conversation = conversation;
        self.sequence = next_sequence;
        Ok(self)
    }

    pub fn conversation(&self) -> &Conversation<H::Source> {
        &self.conversation
    }

    #[cfg(test)]
    pub(crate) fn configuration_mut(&mut self) -> &mut Configuration<H::Permissions> {
        &mut self.configuration
    }

    /// Closing all handles drains accepted runnable work, then returns. Explicit
    /// shutdown cancels the active run and returns retained queued work. Aborting
    /// this future is hard teardown, not a settled stop; join after shutdown.
    pub async fn run(mut self, mut emit: impl FnMut(AgentEvent<H>)) -> SessionResult<H> {
        for (index, entry) in std::mem::take(&mut self.recovered) {
            self.emit(EventKind::MessageCommitted { index, entry }, &mut emit);
        }
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
                    futures::future::Either::Left(Some(command)) => {
                        self.command(command, &mut emit)
                    }
                    futures::future::Either::Left(None) => disconnected = true,
                    futures::future::Either::Right(Some(event)) => self.observe(event, &mut emit),
                    // Every internal stream ends with a terminal event, and
                    // observing that event removes the stream itself.
                    futures::future::Either::Right(None) => {
                        unreachable!("work stream ended without settlement")
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

    fn command(&mut self, command: Command<H>, emit: &mut impl FnMut(AgentEvent<H>)) {
        match command {
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
                    committed_entries: self.conversation.entries().len(),
                    revision: self.revision,
                });
            }
            Command::Request {
                request,
                response,
                completion,
            } => {
                if self.closing {
                    let _ = response.send(Err(RequestError::Closed));
                    return;
                }
                if let Err(reason) = self.host.admit(&request) {
                    let _ = response.send(Err(RequestError::Rejected(reason)));
                    return;
                }
                let Request { source, kind } = *request;
                if let RequestKind::Compact = kind {
                    // Answered when the summary is applied or fails.
                    self.compaction_requests.push(response);
                    return;
                }
                let result = self.request(source, kind, completion, emit);
                let _ = response.send(result);
            }
        }
    }

    fn request(
        &mut self,
        source: H::Source,
        kind: RequestKind<H>,
        completion: Option<CompletionResponder>,
        emit: &mut impl FnMut(AgentEvent<H>),
    ) -> Result<Response<H>, RequestError> {
        match kind {
            RequestKind::Input {
                id,
                payload,
                delivery,
                policy,
            } => {
                if completion.is_some() && self.inbox.is_paused() {
                    return Err(RequestError::Rejected(
                        "session is paused; resume before requesting another run".into(),
                    ));
                }
                let rendered = Message::new(Role::User, payload.render());
                if rendered.content.is_empty() {
                    return Err(RequestError::Rejected("input renders no content".into()));
                }
                rendered
                    .validate()
                    .map_err(|error| RequestError::Rejected(error.to_string()))?;
                let input = Input {
                    id,
                    source: source.clone(),
                    payload,
                    policy,
                };
                let admitted = self
                    .inbox
                    .admit(input, delivery)
                    .map_err(|rejected| RequestError::State(rejected.reason))?;
                if let Some(completion) = completion {
                    self.bind_completion(id, admitted.location, completion);
                }
                self.emit(
                    EventKind::InputAdmitted {
                        id,
                        source,
                        delivery,
                        location: admitted.location,
                    },
                    emit,
                );
                self.wake_waits();
                if let Some(discarded) = &admitted.discarded {
                    self.discard(vec![discarded.clone()], DiscardReason::Fallback, emit);
                }
                Ok(Response::Admitted(admitted))
            }
            RequestKind::EditInput { id, payload } => {
                self.inbox.edit(id, payload)?;
                Ok(Response::Done)
            }
            RequestKind::RemoveInput { id } => {
                let input = self.inbox.remove(id)?;
                self.discard(vec![input.clone()], DiscardReason::Removed, emit);
                self.release_waiting(emit);
                Ok(Response::Removed(input))
            }
            RequestKind::RedirectInput { id, delivery } => {
                let admitted = self.inbox.redirect(id, delivery)?;
                if let Some(completion) = self.pending_completions.remove(&id) {
                    self.bind_completion(id, admitted.location, completion);
                }
                self.wake_waits();
                if let Some(discarded) = &admitted.discarded {
                    self.discard(vec![discarded.clone()], DiscardReason::Fallback, emit);
                }
                self.release_waiting(emit);
                Ok(Response::Admitted(admitted))
            }
            RequestKind::MoveInput { id, before } => {
                self.inbox.move_before(id, before)?;
                Ok(Response::Done)
            }
            RequestKind::SourceClosed => {
                let withdrawn = self.inbox.producer_closed(&source);
                self.discard(withdrawn.inputs.clone(), DiscardReason::SourceClosed, emit);
                self.release_waiting(emit);
                Ok(Response::Withdrawn(withdrawn))
            }
            RequestKind::Stop { run } => {
                self.inbox.stop(run)?;
                Ok(Response::Done)
            }
            RequestKind::Resume => {
                self.inbox.resume();
                Ok(Response::Done)
            }
            RequestKind::Compact => unreachable!("compaction requests are deferred"),
            RequestKind::Shutdown => {
                self.closing = true;
                self.commands.close();
                self.cancel_compaction(emit);
                let withdrawn = self.inbox.shutdown();
                self.discard(withdrawn.inputs, DiscardReason::Shutdown, emit);
                Ok(Response::Done)
            }
            RequestKind::ReplaceDefinition {
                definition,
                provider,
            } => {
                definition
                    .model
                    .validate()
                    .map_err(|error| RequestError::Rejected(error.to_string()))?;
                self.definition = definition;
                if let Some(provider) = provider {
                    self.provider = provider;
                }
                self.revision.0 += 1;
                let revision = self.revision;
                self.emit(EventKind::DefinitionReplaced { revision }, emit);
                Ok(Response::DefinitionReplaced(revision))
            }
            RequestKind::Configure(change) => {
                self.emit(EventKind::ConfigurationChanged(change.clone()), emit);
                self.configuration.apply(change);
                Ok(Response::Done)
            }
            RequestKind::DynamicTools(change) => {
                if !self.dynamic_tools {
                    return Err(RequestError::Rejected(
                        "the session has no dynamic tools".into(),
                    ));
                }
                let event = match change {
                    DynamicToolChange::Add(tool) => {
                        let tool = tool_runtime::RegisteredTool::new(tool)
                            .map_err(|error| RequestError::Rejected(error.message))?;
                        let name = tool.descriptor.name().to_owned();
                        if BUILTIN_TOOLS.contains(&name.as_str()) || self.registry.contains(&name) {
                            return Err(RequestError::Rejected(format!(
                                "{name} is already offered directly"
                            )));
                        }
                        self.dynamic
                            .insert(tool)
                            .map_err(|error| RequestError::Rejected(error.message))?;
                        EventKind::DynamicToolAdded(name)
                    }
                    DynamicToolChange::Remove(name) => {
                        self.dynamic.remove(&name).ok_or_else(|| {
                            RequestError::Rejected(format!("no dynamic tool named {name}"))
                        })?;
                        EventKind::DynamicToolRemoved(name)
                    }
                };
                self.tools = Arc::new(ToolSnapshot::new(
                    &self.registry,
                    self.dynamic_tools,
                    self.dynamic.clone(),
                ));
                self.emit(event, emit);
                Ok(Response::Done)
            }
            RequestKind::ResolveApproval { call, decision } => {
                let active = self
                    .active
                    .as_mut()
                    .filter(|active| {
                        active
                            .approval
                            .as_ref()
                            .is_some_and(|pending| pending.call == call)
                    })
                    .ok_or(RequestError::NoApproval(call))?;
                let pending = active.approval.take().expect("matched approval");
                if decision == ApprovalDecision::AllowForRun {
                    active.grants.push(Grant {
                        tool: active
                            .current_tool
                            .clone()
                            .expect("approval has a resolved tool"),
                        access: pending.access,
                    });
                }
                let _ = pending.respond.send(decision);
                Ok(Response::Done)
            }
        }
    }

    fn bind_completion(
        &mut self,
        id: InputId,
        location: Admission,
        completion: CompletionResponder,
    ) {
        match location {
            // A steer waiting for the next run moves into it as it starts.
            Admission::Queued | Admission::Next => {
                self.pending_completions.insert(id, completion);
            }
            Admission::Active(_) => {
                self.active
                    .as_mut()
                    .expect("active admission")
                    .completions
                    .insert(id, completion);
            }
            Admission::Discarded => {
                let _ = completion.send(Err(PromptError::Discarded));
            }
        }
    }

    /// An interjection wakes only the current call's cooperative wait.
    fn wake_waits(&self) {
        if matches!(self.inbox.signal(), Some(Signal::YieldWait(_)))
            && let Some(active) = &self.active
        {
            active.control.yield_wait.cancel();
        }
    }

    fn advance(&mut self, emit: &mut impl FnMut(AgentEvent<H>)) {
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
            let first = &self
                .inbox
                .batch(delivery)
                .expect("initial reservation")
                .inputs[0];
            let (input_id, origin) = (first.id, first.source.clone());
            let joining: Vec<_> = std::iter::once(input_id)
                .chain(self.inbox.waiting().map(|input| input.id))
                .collect();
            let completions = joining
                .into_iter()
                .filter_map(|id| {
                    self.pending_completions
                        .remove(&id)
                        .map(|response| (id, response))
                })
                .collect();
            self.active = Some(Active {
                id,
                origin,
                phase: Phase::Idle,
                steps: 0,
                next: Next::Sample,
                step: None,
                calls: VecDeque::new(),
                current: None,
                current_tool: None,
                dispatched: false,
                approval: None,
                grants: Vec::new(),
                compaction_attempted: false,
                control: CallControl::default(),
                completions,
                reply: None,
            });
            self.emit(EventKind::RunStarted, emit);
            if let Err(error) = self.deliver(delivery, emit) {
                self.finish(RunOutcome::Failed, Some(error.into()), emit);
            }
            // Return to command processing before inference can start.
            return;
        }
        let id = self.active.as_ref().expect("active run").id;
        if matches!(self.active.as_ref().expect("active run").next, Next::Tools) {
            if let Some(call) = self.active.as_mut().expect("active run").calls.pop_front() {
                self.start_call(call, emit);
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
        if self.active.as_ref().expect("active run").steps >= self.configuration.max_steps.get() {
            self.finish(RunOutcome::Failed, Some(RunError::StepLimit), emit);
            return;
        }
        // Changes since the last step reach the model before it samples.
        let update = self.context_update();
        if let Err(error) = self.commit_context(update, emit) {
            self.finish(RunOutcome::Failed, Some(error.into()), emit);
            return;
        }
        let request = self.completion_request();
        let active = self.active.as_mut().expect("active run");
        active.steps += 1;
        active.step = Some(Step {
            tools: self.tools.clone(),
            revision: self.revision,
        });
        self.work = Some(model::sample(
            self.provider.clone(),
            self.definition.model.clone(),
            request,
        ));
        self.phase(Phase::Sampling, emit);
    }

    fn start_call(&mut self, call: ToolCall, emit: &mut impl FnMut(AgentEvent<H>)) {
        let yield_now = matches!(self.inbox.signal(), Some(Signal::YieldWait(_)));
        let active = self.active.as_mut().expect("active run");
        active.current = Some(call.clone());
        active.current_tool = None;
        active.dispatched = false;
        active.control = CallControl::default();
        if yield_now {
            active.control.yield_wait.cancel();
        }
        let scope = CallScope {
            host: self.host.clone(),
            tools: active
                .step
                .as_ref()
                .expect("calls follow a step")
                .tools
                .clone(),
            origin: active.origin.clone(),
            permissions: self.configuration.permissions.clone(),
            grants: active.grants.clone(),
        };
        self.work = Some(
            executor::execute(scope, call, active.control.clone())
                .map(WorkEvent::Tool)
                .boxed(),
        );
        self.phase(Phase::ExecutingTools, emit);
    }

    /// The request for the next step, assembled from the definition, the
    /// configuration, the tools, and the conversation. Never stored.
    pub(crate) fn completion_request(&self) -> CompletionRequest {
        let messages = self.conversation.model_messages();
        let tools = self.tools.definitions();
        let instructions = self
            .definition
            .instructions
            .clone()
            .filter(|text| !text.trim().is_empty());
        let mut cache = Vec::new();
        if !tools.is_empty() {
            cache.push(CachePoint::AfterTools);
        }
        if instructions.is_some() {
            cache.push(CachePoint::AfterInstructions);
        }
        if let Some(last) = messages.last() {
            cache.push(CachePoint::AfterMessage(last.id.clone()));
        }
        let sampling = &self.configuration.sampling;
        CompletionRequest {
            instructions,
            messages,
            tools,
            cache,
            thinking: self.definition.thinking.clone(),
            max_output_tokens: sampling.max_output_tokens,
            temperature: sampling.temperature,
            top_p: sampling.top_p,
            stop: sampling.stop.clone(),
            seed: sampling.seed,
            options: self.definition.options.clone(),
        }
    }

    fn context_update(&self) -> ContextUpdate<H::Source> {
        let tools = self.tools.base_descriptors();
        let dynamic_tools = self.tools.dynamic_descriptors();
        let view = SessionView {
            host: &*self.host,
            definition: &self.definition,
            revision: self.revision,
            configuration: &self.configuration,
            tools: &tools,
            dynamic_tools: &dynamic_tools,
        };
        context::update(&self.sections, &view, &self.conversation)
    }

    fn commit_context(
        &mut self,
        update: ContextUpdate<H::Source>,
        emit: &mut impl FnMut(AgentEvent<H>),
    ) -> Result<(), ConversationError> {
        if update.is_empty() {
            return Ok(());
        }
        let at = self.conversation.entries().len();
        if let Some(entry) = update.entry {
            self.append(entry, emit)?;
        }
        for (section, snapshot) in update.snapshots {
            self.conversation.deliver(section, snapshot, at);
        }
        Ok(())
    }

    /// Commits a reservation: pending context notes first, then the inputs,
    /// in one atomic append.
    fn deliver(
        &mut self,
        delivery: DeliveryId,
        emit: &mut impl FnMut(AgentEvent<H>),
    ) -> Result<(), ConversationError> {
        self.phase(Phase::PreparingInput, emit);
        let update = self.context_update();
        let start = self.conversation.entries().len();
        let conversation = &mut self.conversation;
        let context_entry = update.entry.clone();
        let batch = self
            .inbox
            .commit(delivery, |batch| {
                let entries = context_entry
                    .into_iter()
                    .chain(batch.inputs.iter().map(|input| Entry {
                        message: Message::new(Role::User, input.payload.render()),
                        origin: Origin::Input {
                            id: input.id,
                            source: input.source.clone(),
                        },
                    }))
                    .collect();
                conversation.append(entries).map(|_| ())
            })
            .expect("owned delivery")?;
        // Before anything else can run, so tools in the next step see the
        // host's records of what the model now has.
        self.host.inputs_committed(&batch.inputs);
        for (section, snapshot) in update.snapshots {
            self.conversation.deliver(section, snapshot, start);
        }
        let committed: Vec<_> = self.conversation.entries()[start..].to_vec();
        for (offset, entry) in committed.into_iter().enumerate() {
            self.emit(
                EventKind::MessageCommitted {
                    index: start + offset,
                    entry,
                },
                emit,
            );
        }
        Ok(())
    }

    fn observe(&mut self, event: WorkEvent<H::Access>, emit: &mut impl FnMut(AgentEvent<H>)) {
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
                let revision = self
                    .active
                    .as_ref()
                    .and_then(|active| active.step.as_ref())
                    .expect("model reply belongs to a step")
                    .revision;
                let entry = Entry {
                    message: message.clone(),
                    origin: Origin::Model { revision },
                };
                if let Err(error) = self.append(entry, emit) {
                    self.finish(RunOutcome::Failed, Some(error.into()), emit);
                    return;
                }
                let active = self.active.as_mut().expect("model belongs to run");
                active.reply = Some(message);
                active.next = if calls.is_empty() {
                    Next::Complete
                } else {
                    Next::Tools
                };
                active.calls = calls;
            }
            WorkEvent::Tool(event) => {
                let active = self.active.as_mut().expect("tool belongs to run");
                let call = active.current.clone().expect("tool belongs to run");
                match event {
                    ExecutionEvent::Resolved(tool) => active.current_tool = Some(tool),
                    ExecutionEvent::AwaitingApproval { access, respond } => {
                        active.approval = Some(PendingApproval {
                            call: call.id.clone(),
                            access: access.clone(),
                            respond,
                        });
                        let tool = active
                            .current_tool
                            .clone()
                            .expect("resolved before approval");
                        self.phase(Phase::AwaitingApproval, emit);
                        self.emit(EventKind::ApprovalRequested { call, tool, access }, emit);
                    }
                    ExecutionEvent::Started => {
                        active.dispatched = true;
                        active.approval = None;
                        let tool = active.current_tool.clone().expect("resolved before start");
                        self.phase(Phase::ExecutingTools, emit);
                        self.emit(EventKind::ToolStarted { call, tool }, emit);
                    }
                    ExecutionEvent::Progress(progress) => {
                        self.emit(EventKind::ToolProgress { call, progress }, emit)
                    }
                    ExecutionEvent::Finished(result) => {
                        self.work = None;
                        let tool = active.current_tool.take();
                        active.current = None;
                        active.approval = None;
                        self.settle(call, tool, result, emit);
                    }
                }
            }
        }
    }

    fn append(
        &mut self,
        entry: Entry<H::Source>,
        emit: &mut impl FnMut(AgentEvent<H>),
    ) -> Result<(), ConversationError> {
        let index = self.conversation.append(vec![entry.clone()])?;
        self.emit(EventKind::MessageCommitted { index, entry }, emit);
        Ok(())
    }

    fn settle(
        &mut self,
        call: ToolCall,
        tool: Option<String>,
        result: Result<TypedToolOutput, ToolError>,
        emit: &mut impl FnMut(AgentEvent<H>),
    ) {
        let entry = result_entry(&call, &result);
        self.append(entry, emit)
            .expect("owned outstanding tool call");
        self.emit(EventKind::ToolFinished { call, tool, result }, emit);
    }

    fn finish(
        &mut self,
        outcome: RunOutcome,
        error: Option<RunError>,
        emit: &mut impl FnMut(AgentEvent<H>),
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
            // Dropping the work drops a parked approval, so a late answer
            // finds no call to resolve.
            self.work = None;
            for call in self.conversation.pending_calls() {
                let active = self.active.as_mut().expect("active run");
                active.approval = None;
                let current = active
                    .current
                    .as_ref()
                    .is_some_and(|current| current.id == call.id);
                let may_have_started = active.dispatched && current;
                let tool = if current {
                    active.current_tool.clone()
                } else {
                    None
                };
                let error = ToolError::new(
                    ToolErrorKind::Cancelled,
                    if may_have_started {
                        "run stopped while awaiting tool; external effects may have occurred"
                    } else {
                        "run ended before this call was dispatched"
                    },
                )
                .with_details(serde_json::json!({"execution_may_have_started": may_have_started}));
                self.settle(call, tool, Err(error), emit);
            }
        }
        let discarded = self
            .inbox
            .finish_run(id, outcome)
            .expect("completion checked at serialized safe point");
        self.discard(discarded, DiscardReason::Fallback, emit);
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

    /// Steers that waited behind queued work no longer starting a run follow
    /// their fallback; see [`Inbox::release_waiting`].
    fn release_waiting(&mut self, emit: &mut impl FnMut(AgentEvent<H>)) {
        let released = self.inbox.release_waiting();
        self.discard(released, DiscardReason::Fallback, emit);
    }

    fn discard(
        &mut self,
        inputs: Vec<Input<H::Source, H::Input>>,
        reason: DiscardReason,
        emit: &mut impl FnMut(AgentEvent<H>),
    ) {
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
            self.emit(EventKind::InputsDiscarded { inputs, reason }, emit);
        }
    }

    fn phase(&mut self, phase: Phase, emit: &mut impl FnMut(AgentEvent<H>)) {
        let active = self.active.as_mut().expect("phase has a run");
        if active.phase != phase {
            active.phase = phase;
            self.emit(EventKind::PhaseChanged(phase), emit);
        }
    }

    fn emit(&mut self, kind: EventKind<H>, emit: &mut impl FnMut(AgentEvent<H>)) {
        let sequence = self.sequence;
        self.sequence += 1;
        emit(AgentEvent {
            sequence,
            run: self.active.as_ref().map(|active| active.id),
            kind,
        });
    }
}

/// The correlated result entry for a call; errors become JSON the model reads.
fn result_entry<S>(call: &ToolCall, result: &Result<TypedToolOutput, ToolError>) -> Entry<S> {
    let (content, is_error) = match result {
        Ok(output) => (output.model_output.clone(), false),
        Err(error) => (
            vec![ToolResultContent::Json {
                value: serde_json::to_value(error).expect("tool error is serializable"),
            }],
            true,
        ),
    };
    Entry {
        message: Message::new(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call: call.id.clone(),
                provider_id: call.provider_id.clone(),
                name: Some(call.name.clone()),
                content,
                is_error,
            })],
        ),
        origin: Origin::Tool,
    }
}

/// The model's reply may run its calls only when it finished normally and
/// every call's arguments are a JSON object.
fn usable(message: &Message) -> Result<(), RunError> {
    let reason = message
        .generation
        .as_ref()
        .and_then(|metadata| metadata.finish_reason.as_ref());
    if !matches!(reason, Some(FinishReason::Stop | FinishReason::ToolCalls)) {
        return Err(RunError::Completion(format!("finish reason {reason:?}")));
    }
    if message.content.iter().any(|content| {
        matches!(content, Content::ToolCall(call) if matches!(call.arguments, ToolArguments::Invalid { .. }))
    }) {
        return Err(RunError::Completion("malformed tool arguments".into()));
    }
    Ok(())
}
