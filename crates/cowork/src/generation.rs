//! Running the agent for a thread, and forwarding what it does to everyone
//! in it; see `transcript.rs` for how that is folded.
//!
//! The host runs one agent session per thread ([`ThreadAgent`]), created
//! with the thread's first run and kept for its later ones, so the session
//! keeps what it told the model (such as the environment note) between
//! runs. Cowork keeps its own queue of pending submissions and hands the
//! session one input at a time, when no run is going.

use std::{
    collections::{HashMap, hash_map::Entry},
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use agent::{
    AgentDefinition, AgentEvent, AgentSession, ApprovalDecision, Configuration, ContextSection,
    Delivery, EventKind, Fallback, InputId, Lifetime, Policy, QueueAccess, Request, RequestKind,
    RunId, SessionHandle, SessionView,
    model::{
        ContentDelta, JsonObject, LanguageModel, LanguageModelProvider, Thinking, ToolCallId, Usage,
    },
    tool::ToolRegistry,
};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{App, Context, Entity};
use sandbox::{ListDirectory, Needs, ReadFile, RequestPermissions, RunCommand, WriteFile};
use tools::{Calculate, RespondToComment};
use uuid::Uuid;

use super::GenerationPlan;
use crate::{
    Cowork,
    models::{ModelInfo, ModelProvider, ModelRef},
    participant::ParticipantId,
    protocol::{self, Preview, PreviewDelta, RunEvent},
    thread::{ControlGeneration, Thread, ThreadOwnership, ThreadSharing},
    tool_approval::{self, CoworkHost},
    transcript::{RunSource, shared_compaction, shared_entry},
    usage::{TokenActivity, usage_tokens},
};

/// The system prompt every run starts with, before [`platform_prompt`]. Kept
/// in its own file so it reads and diffs as prose.
const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

/// Where the app finds Ollama.
const OLLAMA_URL: &str = "http://localhost:11434";

/// The tools every session offers, in this order. They are part of the
/// prompt prefix, so they never change with the thread, its comments, or
/// its project; what changes reaches the model in the environment note.
const TOOLS: [&str; 7] = [
    RespondToComment::NAME,
    Calculate::NAME,
    RunCommand::NAME,
    RequestPermissions::NAME,
    ListDirectory::NAME,
    ReadFile::NAME,
    WriteFile::NAME,
];

/// The system prompt with the platform the host's commands run on, which
/// decides their shell and tools. The same for every run on this computer, so
/// it keeps the prompt prefix cacheable.
fn system_prompt() -> &'static str {
    static PROMPT: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| format!("{SYSTEM_PROMPT}\n{}\n", platform_prompt()));
    &PROMPT
}

fn platform_prompt() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    };
    let shell = if cfg!(windows) {
        "the command line is started as a program, not through a shell, so run shell \
         commands through `cmd /c` or `powershell -Command`"
    } else {
        "commands run with /bin/sh -c"
    };
    format!(
        "The `run_command` tool runs on the computer of the thread's host, a {platform} \
         computer ({arch}): {shell}.",
        arch = std::env::consts::ARCH,
    )
}

/// What the prompt is made of for `model`: the system prompt, the tools,
/// medium thinking, and Ollama's `num_ctx`, which is a model option only the
/// native `/api/chat` accepts.
fn definition(model: &ModelRef, info: &ModelInfo) -> AgentDefinition {
    let mut language_model = LanguageModel::new(model.id.clone());
    language_model.name = Some(info.name.clone());
    language_model.context_window = Some(info.max_tokens);
    let mut options = JsonObject::new();
    options.insert(
        "options".into(),
        serde_json::json!({ "num_ctx": info.max_tokens }),
    );
    AgentDefinition {
        model: language_model,
        instructions: Some(system_prompt().to_owned()),
        tools: TOOLS.iter().map(|tool| (*tool).to_owned()).collect(),
        thinking: Some(Thinking::Effort("medium".into())),
        options,
    }
}

/// Every tool the sessions offer.
fn registry() -> ToolRegistry<CoworkHost, Needs> {
    let mut registry = ToolRegistry::default();
    let registered = [
        registry.register(RespondToComment),
        registry.register(Calculate),
        registry.register(RunCommand),
        registry.register(RequestPermissions),
        registry.register(ListDirectory),
        registry.register(ReadFile),
        registry.register(WriteFile),
    ];
    for result in registered {
        result.expect("tool names are unique");
    }
    registry
}

/// The connection that serves `provider`'s models.
pub(crate) fn provider(provider: ModelProvider) -> anyhow::Result<Arc<dyn LanguageModelProvider>> {
    match provider {
        ModelProvider::Ollama => Ok(Arc::new(agent::Ollama::new(OLLAMA_URL)?)),
    }
}

/// Where the thread's commands run, as the model is told: its project's
/// folders and mode, and the scratch folder. Rendered whenever it changed
/// since the model was last told, so a project change reaches the next
/// request.
struct EnvironmentSection;

impl ContextSection<CoworkHost> for EnvironmentSection {
    const ID: &'static str = "environment";
    type Snapshot = String;

    fn snapshot(&self, view: &SessionView<'_, CoworkHost>) -> String {
        let sandbox = view.host.sandbox();
        sandbox.sandboxes().environment_note(sandbox.thread())
    }

    fn render(&self, _: Option<&String>, current: &String) -> Option<String> {
        Some(current.clone())
    }
}

/// How queued inputs behave: Cowork hands the session one at a time.
const INPUT_POLICY: Policy = Policy {
    access: QueueAccess::Protected,
    fallback: Fallback::Queue,
    lifetime: Lifetime::Retain,
};

/// A thread's agent session on the host, which outlives its runs.
pub(crate) struct ThreadAgent {
    /// Tells this session's events apart from a later session's.
    id: u64,
    handle: SessionHandle<CoworkHost>,
    host: Arc<CoworkHost>,
    /// The model the session's definition names, and its window.
    model: (ModelRef, u64),
}

impl Drop for ThreadAgent {
    /// Stops the session and any run it has; its events still arrive.
    fn drop(&mut self) {
        _ = self
            .handle
            .post(Request::new(RunSource::Host, RequestKind::Shutdown));
    }
}

/// Starts the session of a thread whose conversation so far is
/// `conversation`, on the Tokio runtime. Its events reach `events`; the
/// callback also asks for compaction as the context fills up, as it sees
/// each request's usage before anything else can start.
fn start_session(
    host: Arc<CoworkHost>,
    provider: Arc<dyn LanguageModelProvider>,
    model: &ModelRef,
    info: &ModelInfo,
    conversation: agent::Conversation<RunSource>,
    events: mpsc::UnboundedSender<AgentEvent<CoworkHost>>,
    tokio: &tokio::runtime::Handle,
) -> anyhow::Result<SessionHandle<CoworkHost>> {
    let configuration = Configuration {
        // Ollama has no default output cap, and a reply cut short could not
        // run its calls.
        sampling: agent::Sampling {
            max_output_tokens: None,
            ..agent::Sampling::default()
        },
        max_steps: NonZeroUsize::MAX,
        ..Configuration::default()
    };
    let (session, handle) = AgentSession::new(
        host.clone(),
        provider,
        definition(model, info),
        configuration,
        registry(),
    )?;
    let session = session
        .with_section(EnvironmentSection)
        .with_conversation(conversation, 0)?;
    let requests = handle.clone();
    let request_compaction = move |host: &CoworkHost| {
        if !host.compaction.requested.swap(true, Ordering::AcqRel) {
            _ = requests.post(Request::new(RunSource::Host, RequestKind::Compact));
        }
    };
    tokio.spawn(session.run(move |event| {
        let compaction = &host.compaction;
        match &event.kind {
            EventKind::RunStarted => {
                host.run_started();
                if compaction.at_start.swap(false, Ordering::AcqRel) {
                    request_compaction(&host);
                }
            }
            EventKind::UsageReported(usage)
                if usage_tokens(usage) >= compaction.threshold.load(Ordering::Acquire) =>
            {
                request_compaction(&host);
            }
            EventKind::Compacted(_) | EventKind::CompactionFailed(_) => {
                compaction.requested.store(false, Ordering::Release);
            }
            EventKind::RunFinished { .. } => {
                host.sandbox().end_run();
                compaction.requested.store(false, Ordering::Release);
            }
            _ => {}
        }
        _ = events.unbounded_send(event);
    }));
    Ok(handle)
}

/// What a finished run took, for the usage statistics.
pub(crate) struct RunTotals {
    pub(crate) started_at: SystemTime,
    pub(crate) duration: Duration,
    pub(crate) usage: Usage,
}

/// A run of a local or hosted thread, from the submission that started it
/// until it ends.
pub(crate) struct ActiveGeneration {
    message_id: Uuid,
    cancelled: Arc<AtomicBool>,
    /// The thread's session, and who started the run; `None` in tests.
    session: Option<(SessionHandle<CoworkHost>, Arc<CoworkHost>, RunSource)>,
    /// The session's run producing the message, once it started.
    run: Option<RunId>,
    /// The calls the run waits on a decision for, and whether only the host
    /// may decide each; see [`tool_approval::HOST_ONLY`].
    approvals: HashMap<ToolCallId, bool>,
    started_at: SystemTime,
    /// Monotonic, so the duration survives clock changes.
    started: Instant,
    usage: Usage,
    /// The decisions handed to the run, for tests without a session.
    #[cfg(test)]
    decisions: Vec<(ToolCallId, bool)>,
}

impl ActiveGeneration {
    fn new(message_id: Uuid) -> Self {
        Self {
            message_id,
            cancelled: Arc::default(),
            session: None,
            run: None,
            approvals: HashMap::new(),
            started_at: SystemTime::now(),
            started: Instant::now(),
            usage: Usage::default(),
            #[cfg(test)]
            decisions: Vec::new(),
        }
    }

    /// What the run has been granted so far, for showing only new
    /// permissions in cards.
    pub(crate) fn granted(&self) -> Option<sandbox::Grant> {
        self.session
            .as_ref()
            .map(|(_, host, _)| host.sandbox().run_grant())
    }

    pub(crate) fn message_id(&self) -> Uuid {
        self.message_id
    }

    /// Whether the run waits for someone to decide one of its calls.
    pub(crate) fn is_waiting_for_approval(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// Whether `call`, if the run waits on it, is one only the host decides.
    pub(crate) fn approval_is_host_only(&self, call: &ToolCallId) -> bool {
        self.approvals.get(call).copied().unwrap_or(false)
    }

    /// Hands the session the decision on `call`, if the run still waits on
    /// it. Returns whether it did.
    pub(crate) fn decide(&mut self, call: &ToolCallId, allow: bool) -> bool {
        if self.approvals.remove(call).is_none() {
            return false;
        }
        #[cfg(test)]
        self.decisions.push((call.clone(), allow));
        if let Some((handle, _, source)) = &self.session {
            let decision = if allow {
                ApprovalDecision::AllowOnce
            } else {
                ApprovalDecision::Deny(tool_approval::DENIED.into())
            };
            _ = handle.post(Request::new(
                source.clone(),
                RequestKind::ResolveApproval {
                    call: call.clone(),
                    decision,
                },
            ));
        }
        true
    }

    /// Stops the run, or the run about to start.
    fn stop(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let (Some((handle, _, source)), Some(run)) = (&self.session, self.run) {
            _ = handle.post(Request::new(source.clone(), RequestKind::Stop { run }));
        }
    }

    fn totals(&self) -> RunTotals {
        RunTotals {
            started_at: self.started_at,
            duration: self.started.elapsed(),
            usage: self.usage.clone(),
        }
    }
}

#[cfg(test)]
impl ActiveGeneration {
    pub(crate) fn for_test(message_id: Uuid, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            ..Self::new(message_id)
        }
    }

    /// The decisions handed to the run so far.
    pub(crate) fn decisions_for_test(&self) -> &[(ToolCallId, bool)] {
        &self.decisions
    }
}

/// `usage` plus `more`, counting what either reported.
fn add_usage(usage: &mut Usage, more: &Usage) {
    let add = |total: &mut Option<u64>, more: Option<u64>| {
        if let Some(more) = more {
            *total = Some(total.unwrap_or(0) + more);
        }
    };
    add(&mut usage.input_tokens, more.input_tokens);
    add(&mut usage.output_tokens, more.output_tokens);
    add(&mut usage.total_tokens, more.total_tokens);
    add(&mut usage.cached_input_tokens, more.cached_input_tokens);
    add(&mut usage.reasoning_tokens, more.reasoning_tokens);
}

/// How a session's finished run ended, for everyone.
fn run_outcome(outcome: agent::RunOutcome, error: Option<agent::RunError>) -> protocol::RunOutcome {
    match outcome {
        agent::RunOutcome::Completed => protocol::RunOutcome::Completed,
        agent::RunOutcome::Stopped => protocol::RunOutcome::Stopped,
        agent::RunOutcome::Failed => protocol::RunOutcome::Failed(format!(
            "Unable to generate a response: {}",
            error.map_or_else(|| "the run failed".to_owned(), |error| error.to_string())
        )),
    }
}

impl Cowork {
    /// Starts the agent on a submission, returning the agent message it
    /// produces.
    pub(super) fn start_generation(
        &mut self,
        plan: GenerationPlan,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        let GenerationPlan {
            thread_id,
            prompt,
            comment_group_id,
            turn_comments,
            source,
        } = plan;
        let thread = self.thread_store.read(cx).thread(thread_id, cx)?;
        // Only the host runs agents, so commands run on the host's computer
        // with its project for this thread, whoever asked for the run. Later
        // project changes reach it through `Cowork::sync_sandbox_project`,
        // and the model through the environment note.
        self.sync_sandbox_project(&thread, cx);

        let message_id = Uuid::new_v4();
        let generation = ActiveGeneration::new(message_id);
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::AgentStarted {
                    id: message_id.into_bytes(),
                    comment_group_id: comment_group_id.map(Uuid::into_bytes),
                    started_at: generation.started_at,
                },
                cx,
            )
        });
        self.active_generations.insert(thread_id, generation);

        let started = self.thread_agent(&thread, cx).and_then(|(handle, host)| {
            let state = thread.read(cx);
            let threshold = self
                .context_settings
                .threshold(state.max_tokens())
                .unwrap_or(u64::MAX);
            host.compaction
                .threshold
                .store(threshold, Ordering::Release);
            // A context already full is summarized before the run's
            // first request.
            host.compaction.at_start.store(
                state.transcript.len() > 0 && state.live_context_tokens().unwrap_or(0) >= threshold,
                Ordering::Release,
            );
            host.comments().set(turn_comments);
            // Cowork pauses its own queue; a stop pauses the session's.
            handle.post(Request::new(source.clone(), RequestKind::Resume))?;
            handle.post(Request::new(
                source.clone(),
                RequestKind::Input {
                    id: InputId::new(),
                    payload: prompt,
                    delivery: Delivery::Queue,
                    policy: INPUT_POLICY,
                },
            ))?;
            Ok((handle, host))
        });
        match started {
            Ok((handle, host)) => {
                if let Some(generation) = self.active_generations.get_mut(&thread_id) {
                    generation.session = Some((handle, host, source));
                }
            }
            Err(error) => {
                let totals = self.active_generations[&thread_id].totals();
                let outcome = protocol::RunOutcome::Failed(format!(
                    "Unable to generate a response: {error:#}"
                ));
                self.finish_generation(&thread, message_id, outcome, totals, cx);
            }
        }
        Some(message_id)
    }

    /// The thread's agent session, started for its selected model or
    /// switched to it.
    fn thread_agent(
        &mut self,
        thread: &Entity<Thread>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<(SessionHandle<CoworkHost>, Arc<CoworkHost>)> {
        let state = thread.read(cx);
        let thread_id = state.instance_id;
        let (model, info) = state
            .runnable_model()
            .map(|(model, info)| (model.clone(), info.clone()))
            .ok_or_else(|| anyhow::anyhow!("No available model selected"))?;
        let key = (model.clone(), info.max_tokens);
        if let Some(agent) = self.agents.get_mut(&thread_id) {
            if agent.model != key {
                agent.handle.post(Request::new(
                    RunSource::Host,
                    RequestKind::ReplaceDefinition {
                        definition: definition(&model, &info),
                        provider: None,
                    },
                ))?;
                agent.model = key;
            }
            return Ok((agent.handle.clone(), agent.host.clone()));
        }

        let host = Arc::new(CoworkHost::new(Arc::new(sandbox::ThreadSandbox::new(
            self.sandboxes.clone(),
            thread_id.to_string(),
        ))));
        let (events, mut received) = mpsc::unbounded();
        let handle = start_session(
            host.clone(),
            provider(model.provider)?,
            &model,
            &info,
            state.transcript.conversation().clone(),
            events,
            &self.tokio_handle,
        )?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        self.agents.insert(
            thread_id,
            ThreadAgent {
                id,
                handle: handle.clone(),
                host: host.clone(),
                model: key,
            },
        );
        cx.spawn(async move |this, cx| {
            while let Some(event) = received.next().await {
                if this
                    .update(cx, |this, cx| this.agent_event(thread_id, event, cx))
                    .is_err()
                {
                    return;
                }
            }
            _ = this.update(cx, |this, cx| this.agent_session_ended(thread_id, id, cx));
        })
        .detach();
        Ok((handle, host))
    }

    /// Ends `thread_id`'s agent session, stopping any run it has.
    pub(crate) fn retire_agent(&mut self, thread_id: Uuid) {
        self.agents.remove(&thread_id);
    }

    /// Session `id` of `thread_id` has ended: a run it never finished
    /// failed, and the thread's next run starts a new session.
    fn agent_session_ended(&mut self, thread_id: Uuid, id: u64, cx: &mut Context<Self>) {
        if let Entry::Occupied(agent) = self.agents.entry(thread_id)
            && agent.get().id == id
        {
            agent.remove();
        }
        let Some(generation) = self.active_generations.get(&thread_id) else {
            return;
        };
        if generation.session.is_none() {
            return;
        }
        let message_id = generation.message_id;
        let totals = generation.totals();
        let thread = self
            .thread_store
            .read(cx)
            .thread(thread_id, cx)
            .or_else(|| self.thread_store.read(cx).archived_thread(thread_id, cx));
        if let Some(thread) = thread {
            self.finish_generation(
                &thread,
                message_id,
                protocol::RunOutcome::Failed("The agent stopped unexpectedly.".into()),
                totals,
                cx,
            );
        }
    }

    /// Tells everyone in `thread_id` what its session reported about the
    /// current run.
    fn agent_event(
        &mut self,
        thread_id: Uuid,
        event: AgentEvent<CoworkHost>,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self.active_generations.get_mut(&thread_id) else {
            return;
        };
        if matches!(event.kind, EventKind::RunStarted) && generation.run.is_none() {
            generation.run = event.run;
            if generation.cancelled.load(Ordering::Acquire) {
                generation.stop();
            }
            return;
        }
        if event.run.is_none() || event.run != generation.run {
            return;
        }
        let message_id = generation.message_id;
        let told = match event.kind {
            EventKind::Preview { index, delta } => {
                let delta = match delta {
                    ContentDelta::Text(text) => PreviewDelta::Text(text),
                    ContentDelta::Reasoning(text) => PreviewDelta::Reasoning(text),
                    ContentDelta::ToolCall { .. } => return,
                };
                RunEvent::Preview(Preview {
                    index: index as u32,
                    delta,
                })
            }
            EventKind::MessageCommitted { index, entry } => RunEvent::Committed {
                index: index as u64,
                entry: shared_entry(&entry),
            },
            EventKind::UsageReported(usage) => {
                add_usage(&mut generation.usage, &usage);
                if usage.total_tokens.is_none()
                    && usage.input_tokens.is_none()
                    && usage.output_tokens.is_none()
                {
                    return;
                }
                RunEvent::Usage {
                    tokens: usage_tokens(&usage),
                }
            }
            EventKind::CompactionStarted { .. } => RunEvent::CompactionStarted,
            EventKind::Compacted(compaction) => RunEvent::Compacted(shared_compaction(&compaction)),
            EventKind::CompactionFailed(error) => {
                eprintln!("compaction failed: {error}");
                RunEvent::CompactionFailed
            }
            EventKind::ApprovalRequested { call, tool, .. } => {
                self.await_tool_approval(thread_id, message_id, call.id, &tool, cx);
                return;
            }
            EventKind::RunFinished { outcome, error } => {
                let totals = generation.totals();
                let outcome = run_outcome(outcome, error);
                let thread = self
                    .thread_store
                    .read(cx)
                    .thread(thread_id, cx)
                    .or_else(|| self.thread_store.read(cx).archived_thread(thread_id, cx));
                if let Some(thread) = thread {
                    self.finish_generation(&thread, message_id, outcome, totals, cx);
                }
                return;
            }
            _ => return,
        };
        let thread = self
            .thread_store
            .read(cx)
            .thread(thread_id, cx)
            .or_else(|| self.thread_store.read(cx).archived_thread(thread_id, cx));
        if let Some(thread) = thread {
            thread.update(cx, |thread, cx| {
                thread.emit(
                    protocol::HostMessage::AgentEvent {
                        id: message_id.into_bytes(),
                        event: told,
                    },
                    cx,
                )
            });
        }
        self.thread_updated(thread_id, cx);
    }

    /// Ends the run producing `message_id` with `outcome`, telling everyone,
    /// counting its usage, and letting the scheduled run queue go on.
    pub(crate) fn finish_generation(
        &mut self,
        thread: &Entity<Thread>,
        message_id: Uuid,
        outcome: protocol::RunOutcome,
        totals: RunTotals,
        cx: &mut Context<Self>,
    ) {
        let RunTotals {
            started_at,
            duration,
            usage,
        } = totals;
        let thread_id = thread.read(cx).instance_id;
        if self
            .active_generations
            .get(&thread_id)
            .is_some_and(|active| active.message_id != message_id)
            || thread.read(cx).running_agent_message_id() != Some(message_id)
        {
            return;
        }
        // Stop can arrive after the provider finished but before its outcome
        // reaches the UI. The user's stop still pauses pending delivery.
        let outcome = if self
            .active_generations
            .get(&thread_id)
            .is_some_and(|active| active.cancelled.load(Ordering::Acquire))
        {
            protocol::RunOutcome::Stopped
        } else {
            outcome
        };
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::AgentEnded {
                    id: message_id.into_bytes(),
                    outcome: outcome.clone(),
                    duration,
                },
                cx,
            );
        });
        self.record_turn_usage(thread, usage, started_at, duration, cx);
        // The profile page's statistics may be showing.
        cx.notify();
        if let Entry::Occupied(entry) = self.active_generations.entry(thread_id)
            && entry.get().message_id == message_id
        {
            entry.remove();
        }
        if self.thread_store.read(cx).thread(thread_id, cx).is_none() {
            self.settle_retired_thread(thread, cx);
            self.save_statistics();
        }
        // A turn boundary, before the next pending submission starts.
        self.save_thread(thread, cx);
        self.thread_updated(thread_id, cx);
        if matches!(outcome, protocol::RunOutcome::Completed) {
            self.start_next_pending(thread, cx);
        }
        self.scheduled_generation_ended(thread_id, message_id, &outcome, cx);
    }

    /// Remembers that the run producing message `message_id` waits on a
    /// decision about `call` of `tool`, and tells everyone.
    pub(crate) fn await_tool_approval(
        &mut self,
        thread_id: Uuid,
        message_id: Uuid,
        call: ToolCallId,
        tool: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let Some(generation) = self
            .active_generations
            .get_mut(&thread_id)
            .filter(|generation| generation.message_id == message_id)
        else {
            return;
        };
        generation
            .approvals
            .insert(call.clone(), tool_approval::host_only(tool));
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::ToolApprovalRequested {
                    id: message_id.into_bytes(),
                    call,
                },
                cx,
            );
        });
        self.thread_updated(thread_id, cx);
        self.scheduled_run_waits(thread_id, message_id, tool, cx);
    }

    /// Adds a finished turn's usage to its thread, with when it was used:
    /// spread across the `duration` of the response it started at
    /// `started_at`. The profile's statistics add up the user's own threads;
    /// a turn ending after its thread was deleted is counted with the
    /// deleted ones.
    pub(crate) fn record_turn_usage(
        &mut self,
        thread: &Entity<Thread>,
        usage: Usage,
        started_at: SystemTime,
        duration: Duration,
        cx: &mut App,
    ) {
        let tokens = usage_tokens(&usage);
        let activity = (tokens > 0).then_some(TokenActivity {
            at: started_at,
            duration,
            tokens,
        });
        let (thread_id, ownership) = thread.update(cx, |thread, _| {
            thread.tokens_used += tokens;
            thread.token_activity.extend(activity);
            (thread.instance_id, thread.ownership())
        });
        let store = self.thread_store.read(cx);
        let deleted =
            store.thread(thread_id, cx).is_none() && store.archived_thread(thread_id, cx).is_none();
        if ownership == ThreadOwnership::Local && deleted {
            self.deleted.tokens_used += tokens;
            self.deleted.token_activity.extend(activity);
        }
    }

    /// Redraws the timeline after `thread_id` changed, staying pinned to the
    /// newest output unless the user has scrolled away.
    pub(crate) fn thread_updated(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        if self.active_thread_id != Some(thread_id) {
            return;
        }
        if self.follow_generation {
            self.timeline_scroll_handle.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Stops the active thread's agent run, asking the host to when the
    /// thread is mirrored.
    pub(crate) fn stop_generation(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.active_thread(cx) else {
            return;
        };
        let state = thread.read(cx);
        let actor = state.participant_id();
        if matches!(state.sharing, ThreadSharing::Connected { .. }) {
            let message_id = state.running_agent_message_id();
            if let Some(message_id) = message_id {
                _ = thread.update(cx, |thread, _| {
                    thread.with_authorized::<ControlGeneration, _>(actor, |auth| {
                        auth.request_stop(message_id)
                    })
                });
            }
            return;
        }
        let thread_id = state.instance_id;
        self.cancel_generation(thread_id, actor, None, cx);
    }

    /// Cancels the agent run of a local or hosted thread. With `message_id`,
    /// only a run still producing that message is cancelled, so a stale stop
    /// request cannot cancel the run that followed it.
    pub(crate) fn cancel_generation(
        &mut self,
        thread_id: Uuid,
        actor: ParticipantId,
        message_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self.active_generations.get(&thread_id) else {
            return;
        };
        if message_id.is_some_and(|message_id| message_id != generation.message_id) {
            return;
        }
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        if !thread.read(cx).is_host() {
            return;
        }
        // Stopped within the current-policy guard. No detached authorization
        // can survive revocation and later stop a different run.
        if thread
            .update(cx, |thread, _| {
                thread.with_authorized::<ControlGeneration, _>(actor, |_| generation.stop())
            })
            .is_ok()
        {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use agent::{
        ApprovalDecision, Origin, RunOutcome,
        model::{
            Completion, CompletionRequest, CompletionStream, Content, FinishReason,
            GenerationMetadata, Message, Role, StreamEvent, ToolArguments, ToolCall,
        },
    };
    use futures::future::BoxFuture;

    use super::*;

    /// A provider answering each request with the next of its replies, and
    /// keeping the requests.
    #[derive(Default)]
    struct Scripted {
        replies: parking_lot::Mutex<VecDeque<Message>>,
        requests: parking_lot::Mutex<Vec<CompletionRequest>>,
    }

    impl LanguageModelProvider for Scripted {
        fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, agent::model::Error>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn stream<'a>(
            &'a self,
            _: &'a LanguageModel,
            request: CompletionRequest,
        ) -> BoxFuture<'a, Result<CompletionStream, agent::model::Error>> {
            self.requests.lock().push(request);
            let message = self.replies.lock().pop_front().expect("a scripted reply");
            Box::pin(async move {
                let finished = StreamEvent::Finished(Box::new(Completion { message }));
                Ok(futures::stream::iter([Ok(finished)]).boxed())
            })
        }
    }

    fn reply(content: Vec<Content>, finish: FinishReason, tokens: u64) -> Message {
        let mut message = Message::new(Role::Assistant, content);
        message.generation = Some(GenerationMetadata {
            provider: "test".into(),
            model: None,
            finish_reason: Some(finish),
            usage: Some(Usage {
                total_tokens: Some(tokens),
                ..Usage::default()
            }),
            extra: JsonObject::new(),
        });
        message
    }

    /// A session tells the model where its commands run before the first
    /// prompt, asks before a write in a read-only project and reports the
    /// call refused once denied, and compacts once a reply reports the
    /// context past the threshold.
    #[tokio::test]
    async fn a_thread_session_asks_refuses_and_compacts() {
        let mut arguments = JsonObject::new();
        arguments.insert("path".into(), "scratch://notes.md".into());
        arguments.insert("content".into(), "hello".into());
        let write = ToolCall {
            id: ToolCallId::new(),
            provider_id: None,
            name: WriteFile::NAME.into(),
            arguments: ToolArguments::Object { value: arguments },
        };
        let provider = Arc::new(Scripted::default());
        provider.replies.lock().extend([
            reply(
                vec![Content::ToolCall(write.clone())],
                FinishReason::ToolCalls,
                10,
            ),
            reply(
                vec![Content::text("Done ".repeat(40))],
                FinishReason::Stop,
                100,
            ),
            reply(vec![Content::text("Summary.")], FinishReason::Stop, 5),
        ]);
        let host = Arc::new(CoworkHost::new(Arc::new(sandbox::ThreadSandbox::new(
            crate::test_support::unused_sandboxes(),
            "thread",
        ))));
        host.compaction.threshold.store(50, Ordering::Release);
        let model = ModelRef {
            provider: ModelProvider::Ollama,
            id: "model".into(),
        };
        let info = ModelInfo {
            name: "Model".into(),
            max_tokens: 1_024,
        };
        let (events, mut received) = mpsc::unbounded();
        let handle = start_session(
            host.clone(),
            provider.clone(),
            &model,
            &info,
            agent::Conversation::default(),
            events,
            &tokio::runtime::Handle::current(),
        )
        .unwrap();
        let source = RunSource::Participant([1; 16]);
        handle
            .post(Request::new(
                source.clone(),
                RequestKind::Input {
                    id: InputId::new(),
                    payload: vec![Content::text("Take a note. ".repeat(20))],
                    delivery: Delivery::Queue,
                    policy: INPUT_POLICY,
                },
            ))
            .unwrap();

        let mut committed = Vec::new();
        let mut asked = Vec::new();
        let mut compacted = false;
        let outcome = loop {
            let event = tokio::time::timeout(Duration::from_secs(5), received.next())
                .await
                .expect("the session goes on")
                .expect("the session runs");
            match event.kind {
                EventKind::MessageCommitted { entry, .. } => committed.push(entry),
                EventKind::ApprovalRequested { call, tool, .. } => {
                    asked.push(tool);
                    handle
                        .post(Request::new(
                            source.clone(),
                            RequestKind::ResolveApproval {
                                call: call.id,
                                decision: ApprovalDecision::Deny(tool_approval::DENIED.into()),
                            },
                        ))
                        .unwrap();
                }
                EventKind::Compacted(_) => compacted = true,
                EventKind::RunFinished { outcome, .. } => break outcome,
                _ => {}
            }
        };
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(asked, [WriteFile::NAME]);
        assert!(compacted, "the full context was summarized");
        assert!(matches!(
            &committed[0].origin,
            Origin::Context { sections } if sections == &["environment"]
        ));
        assert!(matches!(committed[1].origin, Origin::Input { .. }));
        let refused = committed
            .iter()
            .find_map(|entry| match entry.message.content.first() {
                Some(Content::ToolResult(result)) => Some(result.is_error),
                _ => None,
            });
        assert_eq!(refused, Some(true));
        let first = &provider.requests.lock()[0];
        assert_eq!(
            first
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            TOOLS
        );
        assert_eq!(first.instructions.as_deref(), Some(system_prompt()));
        _ = handle.post(Request::new(RunSource::Host, RequestKind::Shutdown));
    }

    #[test]
    fn usage_adds_what_either_reported() {
        let mut total = Usage::default();
        add_usage(
            &mut total,
            &Usage {
                input_tokens: Some(3),
                total_tokens: Some(5),
                ..Usage::default()
            },
        );
        add_usage(
            &mut total,
            &Usage {
                total_tokens: Some(7),
                ..Usage::default()
            },
        );
        assert_eq!(total.total_tokens, Some(12));
        assert_eq!(total.input_tokens, Some(3));
        assert_eq!(total.output_tokens, None);
    }

    /// Every tool the definition offers is registered, so a session can be
    /// built for any model.
    #[test]
    fn the_definition_offers_the_registered_tools() {
        let registry = registry();
        let model = ModelRef {
            provider: ModelProvider::Ollama,
            id: "model".into(),
        };
        let info = ModelInfo {
            name: "Model".into(),
            max_tokens: 1_024,
        };
        for tool in definition(&model, &info).tools {
            assert!(registry.contains(&tool), "{tool}");
        }
    }

    /// The app's session against a local Ollama: a prompt, a tool call, and
    /// a reply. Opt-in, as it needs the server and `OLLAMA_MODEL` (default
    /// `gemma4:e4b`):
    /// `cargo test -p cowork --lib live_ollama -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs a local Ollama server"]
    async fn a_thread_session_runs_on_live_ollama() {
        let model = ModelRef {
            provider: ModelProvider::Ollama,
            id: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gemma4:e4b".into()),
        };
        let info = ModelInfo {
            name: model.id.clone(),
            max_tokens: 16_384,
        };
        let host = Arc::new(CoworkHost::new(Arc::new(sandbox::ThreadSandbox::new(
            crate::test_support::unused_sandboxes(),
            "thread",
        ))));
        let (events, mut received) = mpsc::unbounded();
        let handle = start_session(
            host,
            provider(ModelProvider::Ollama).unwrap(),
            &model,
            &info,
            agent::Conversation::default(),
            events,
            &tokio::runtime::Handle::current(),
        )
        .unwrap();
        handle
            .post(Request::new(
                RunSource::Participant([1; 16]),
                RequestKind::Input {
                    id: InputId::new(),
                    payload: vec![Content::text(
                        "Use the calculate tool to multiply 17 by 23, then tell me the result.",
                    )],
                    delivery: Delivery::Queue,
                    policy: INPUT_POLICY,
                },
            ))
            .unwrap();
        let mut tools = Vec::new();
        let mut reply = String::new();
        let (outcome, error) = loop {
            let event = tokio::time::timeout(Duration::from_secs(120), received.next())
                .await
                .expect("the session goes on")
                .expect("the session runs");
            match event.kind {
                EventKind::ToolStarted { tool, .. } => tools.push(tool),
                EventKind::MessageCommitted { entry, .. } => {
                    for content in &entry.message.content {
                        if let Content::Text { text } = content
                            && entry.message.role == Role::Assistant
                        {
                            reply.push_str(text);
                        }
                    }
                }
                EventKind::RunFinished { outcome, error } => break (outcome, error),
                _ => {}
            }
        };
        println!("tools: {tools:?}\nreply: {reply}");
        assert_eq!(outcome, RunOutcome::Completed, "{error:?}");
        assert_eq!(tools, [Calculate::NAME]);
        assert!(reply.contains("391"), "{reply}");
        _ = handle.post(Request::new(RunSource::Host, RequestKind::Shutdown));
    }
}
