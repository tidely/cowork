//! Running the agent for a thread, and forwarding what it does to everyone
//! in it; see `transcript.rs` for how that is folded.

use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use agent::AgentEvent;
use anyhow::Context as _;
use futures::StreamExt as _;
use gpui::{App, Context, Entity};
use rig::{
    AgentBuilder, AgentRunner, DynModel,
    agent::tool::server::ToolServer,
    completion::{AssistantContent, CompletionRequest, Message, ToolDefinition, Usage},
    message::CallId,
    operation::Completion,
    providers::ollama::Ollama,
    run::UnhandledInvalidToolCall,
    streaming::{Item, StreamEvent},
    tool::ToolSet,
};
use sandbox::{CallApprovals, ListDirectory, ReadFile, RequestPermissions, RunCommand, WriteFile};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tools::{Calculate, RespondToComment};

use super::GenerationPlan;
use uuid::Uuid;

use crate::{
    Cowork,
    models::{ModelProvider, ModelRef},
    participant::ParticipantId,
    prompt, protocol,
    thread::{ControlGeneration, Thread, ThreadOwnership, ThreadSharing},
    tool_approval::{self, ApprovalGate, GateHook, RunUpdate, ToolDecision},
    usage::{TokenActivity, usage_tokens},
};

/// The system prompt every run starts with, before [`platform_prompt`]. Kept
/// in its own file so it reads and diffs as prose.
const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

/// Kept separate for prompt iteration; appended as a user message, never
/// substituted for the stable system and tool prefix.
const COMPACTION_PROMPT: &str = include_str!("../prompts/compaction.md");

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

/// The tools of a run of `thread`. The same definitions, in the same order,
/// on every run, whatever the turn's comments, the thread, or its project:
/// they are part of the prompt prefix, so changing them would invalidate the
/// model's prompt cache. What changes reaches the model in the environment
/// note instead.
fn tool_set(
    sandboxes: &Arc<sandbox::Sandboxes>,
    thread: &str,
    turn_comments: Arc<tools::TurnComments>,
) -> ToolSet {
    let mut tools = ToolSet::default();
    tools.add_tool(RespondToComment::new(turn_comments));
    tools.add_tool(Calculate);
    tools.add_tool(RunCommand::new(sandboxes.clone(), thread));
    tools.add_tool(RequestPermissions);
    tools.add_tool(ListDirectory::new(sandboxes.clone(), thread));
    tools.add_tool(ReadFile::new(sandboxes.clone(), thread));
    tools.add_tool(WriteFile::new(sandboxes.clone(), thread));
    tools
}

/// Compare like-for-like encoded messages rather than provider token totals,
/// which include fixed system and tool overhead that a summary cannot shrink.
fn summary_reduces_history(history: &[Message], replacement: &[Message]) -> bool {
    match (serde_json::to_vec(history), serde_json::to_vec(replacement)) {
        (Ok(before), Ok(after)) => after.len() < before.len(),
        _ => false,
    }
}

/// What every request of a run starts with: the system prompt, the tool
/// definitions, and the model's parameters. A summary request starts with
/// the same, so the provider's prompt cache serves both.
struct RequestPrefix {
    model: DynModel<Completion>,
    tools: Vec<Arc<ToolDefinition>>,
    params: serde_json::Value,
}

impl RequestPrefix {
    fn new(model: DynModel<Completion>, tools: &ToolSet, max_tokens: u64) -> Self {
        Self {
            model,
            tools: tools.tool_definitions(),
            params: json!({
                "num_ctx": max_tokens,
                "think": "medium"
            }),
        }
    }

    /// The run of `prompt` after `history`, with `tools`, which are the
    /// tools [`RequestPrefix::new`] was given.
    fn runner(&self, tools: ToolSet, prompt: Message, history: Vec<Message>) -> AgentRunner {
        let handle = ToolServer::new().run();
        handle.add_tools(tools);
        AgentBuilder::new(self.model.clone())
            .preamble(system_prompt())
            .additional_params(self.params.clone())
            .tool_server_handle(handle)
            .build()
            .prompt(prompt)
            .history(history)
            .max_turns(usize::MAX)
            // A call to a tool that doesn't exist is dropped from its reply.
            .unhandled_invalid_tool_call(UnhandledInvalidToolCall::Ignore)
    }

    /// Makes one summary request after `history`. Tool calls are never
    /// dispatched, and invalidate the summary.
    async fn summarize(&self, history: &[Message]) -> anyhow::Result<(String, Usage)> {
        let started = Instant::now();
        let mut first_output = None;
        let request = CompletionRequest::new(Message::user(COMPACTION_PROMPT))
            .messages(history.to_vec())
            .tools(self.tools.clone())
            .preamble(system_prompt())
            .additional_params(self.params.clone());
        let mut stream = self
            .model
            .stream(request)
            .context("failed to start summary")?;
        while let Some(item) = stream.next().await {
            let item = item.context("summary stream failed")?;
            if matches!(
                item,
                Item::Event(StreamEvent::Text { .. } | StreamEvent::Reasoning { .. })
            ) {
                first_output.get_or_insert_with(|| started.elapsed());
            }
        }
        let response = stream.finish().await.context("summary did not finish")?;
        // Input counts include cached tokens. Report the actual cache counter
        // and split prompt evaluation from new reasoning/text generation;
        // these metrics contain no conversation content.
        let milliseconds = |key| {
            response
                .raw
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .map(|ns| ns as f64 / 1_000_000.)
        };
        eprintln!(
            "compaction metrics: input_tokens={:?} cached_input_tokens={:?} output_tokens={:?} first_output_ms={:?} load_ms={:?} prompt_eval_ms={:?} generation_ms={:?} total_ms={}",
            response.usage.input_tokens,
            response.usage.cached_input_tokens,
            response.usage.output_tokens,
            first_output.map(|time| time.as_millis()),
            milliseconds("load_duration"),
            milliseconds("prompt_eval_duration"),
            milliseconds("eval_duration"),
            started.elapsed().as_millis(),
        );
        if let Some(failure) = rig::message::turn_failure(
            &response.choice,
            Some(&response.stop()),
            response.finish_reason().as_ref(),
        ) {
            anyhow::bail!(failure);
        }
        let mut text = String::new();
        for content in response.choice.iter() {
            match content {
                AssistantContent::Text(part) => text.push_str(&part.text),
                AssistantContent::ToolCall(_) => anyhow::bail!("summary attempted to call a tool"),
                _ => {}
            }
        }
        anyhow::ensure!(!text.trim().is_empty(), "summary was empty");
        Ok((text, response.usage))
    }

    /// Summarizes `history` before a run, telling everyone through
    /// `updates`. Returns the history the run starts from: the summary, or
    /// `history` itself when summarizing failed or saves nothing.
    async fn compact(
        &self,
        history: Vec<Message>,
        updates: &mpsc::UnboundedSender<RunUpdate>,
    ) -> Vec<Message> {
        _ = updates.send(RunUpdate::CompactionStarted);
        let (context, usage, replacement) = match self.summarize(&history).await {
            Ok((summary, usage)) => {
                let replacement = vec![Message::user(format!(
                    "Summary of the conversation so far:\n{summary}"
                ))];
                if summary_reduces_history(&history, &replacement) {
                    (Some(summary), usage, Some(replacement))
                } else {
                    eprintln!("compaction skipped: summary did not reduce request history");
                    (None, usage, None)
                }
            }
            Err(error) => {
                eprintln!("compaction failed: {error:#}");
                (None, Usage::default(), None)
            }
        };
        _ = updates.send(RunUpdate::CompactionFinished { context, usage });
        replacement.unwrap_or(history)
    }
}

/// What a finished run took, for the usage statistics.
pub(crate) struct RunTotals {
    pub(crate) started_at: SystemTime,
    pub(crate) duration: Duration,
    pub(crate) usage: Usage,
}

pub(crate) struct ActiveGeneration {
    message_id: Uuid,
    abort_handle: tokio::task::AbortHandle,
    cancelled: Arc<AtomicBool>,
    /// The host's live run grants, used to show only new permissions in cards.
    gate: Option<std::sync::Weak<ApprovalGate>>,
    /// How to answer each call the run is waiting on a decision for.
    /// Dropping one, as when the run ends, denies its call.
    approvals: HashMap<CallId, PendingApproval>,
}

/// Builds the agent for `model` and runs it on `prompt`, first summarizing
/// `history` when `compact`. Everything it does reaches `updates`; the
/// channel closes when it returns, dropping the agent and its hooks.
async fn run_agent(
    model: Option<(ModelRef, u64)>,
    tools: ToolSet,
    gate: Arc<ApprovalGate>,
    compact: bool,
    prompt: Message,
    mut history: Vec<Message>,
    updates: mpsc::UnboundedSender<RunUpdate>,
) -> anyhow::Result<()> {
    let (model, max_tokens) = model.context("No available model selected")?;
    let model = match model.provider {
        // `num_ctx` is a model option only the native `/api/chat` accepts;
        // the OpenAI-compatible route refuses the request.
        ModelProvider::Ollama => Ollama::new().native_completion(model.id).erase(),
    };
    let prefix = RequestPrefix::new(model, &tools, max_tokens);
    if compact {
        history = prefix.compact(history, &updates).await;
    }
    let approvals = Arc::new(CallApprovals::default());
    let runner = prefix
        .runner(tools, prompt, history)
        // One call at a time, so each approval reaches its own call.
        .tool_concurrency(1)
        .tool_context(CallApprovals::run_context(&approvals))
        .add_hook(GateHook::new(gate, approvals));
    agent::run(runner, move |event| {
        _ = updates.send(RunUpdate::Event(event));
    })
    .await
}

/// How a run ended, from its task's result. Aborting is a stop only when
/// someone asked for one; any other abort is a failure.
fn run_outcome(
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
    cancelled: bool,
) -> protocol::RunOutcome {
    let failed = |error: anyhow::Error| {
        protocol::RunOutcome::Failed(format!("Unable to generate a response: {error:#}"))
    };
    match result {
        Ok(Ok(())) => protocol::RunOutcome::Completed,
        Ok(Err(error)) => failed(error),
        Err(error) if error.is_cancelled() && cancelled => protocol::RunOutcome::Stopped,
        Err(error) => failed(error.into()),
    }
}

/// Turns what a run reports into what everyone in its thread is told, in
/// order, and adds up what it used. No GPUI: the caller emits the messages.
///
/// With pre-run compaction, the run first announces `AgentPreparing` and
/// holds `AgentStarted` back until the compaction finishes, so its summary
/// joins the transcript before the prompt it was made for.
struct RunRelay {
    message_id: Uuid,
    pending_start: Option<protocol::HostMessage>,
    usage: Usage,
}

enum Relayed {
    /// Messages to emit, in order.
    Tell(Vec<protocol::HostMessage>),
    /// A call waits for a decision, which only `Cowork` can arrange; see
    /// [`Cowork::await_tool_approval`].
    AwaitApproval {
        call: CallId,
        tool: String,
        decide: oneshot::Sender<ToolDecision>,
    },
}

impl RunRelay {
    /// The relay of a run producing `message_id`, and the message announcing
    /// the run.
    fn start(
        message_id: Uuid,
        comment_group_id: Option<Uuid>,
        started_at: SystemTime,
        prompt: &rig::completion::Message,
        compact: bool,
    ) -> (Self, protocol::HostMessage) {
        let id = message_id.into_bytes();
        let comment_group_id = comment_group_id.map(Uuid::into_bytes);
        let started = protocol::HostMessage::AgentStarted {
            id,
            comment_group_id,
            started_at,
            prompt: protocol::Json::from_rig(prompt),
        };
        let (pending_start, announcement) = if compact {
            let preparing = protocol::HostMessage::AgentPreparing {
                id,
                comment_group_id,
                started_at,
            };
            (Some(started), preparing)
        } else {
            (None, started)
        };
        let relay = Self {
            message_id,
            pending_start,
            usage: Usage::default(),
        };
        (relay, announcement)
    }

    fn relay(&mut self, update: RunUpdate) -> Relayed {
        let message = match update {
            RunUpdate::Event(event) => {
                if let AgentEvent::TurnEnded { usage, .. } = &event {
                    self.usage += *usage;
                }
                protocol::HostMessage::AgentEvent {
                    id: self.message_id.into_bytes(),
                    event: protocol::Json::shared(event),
                }
            }
            RunUpdate::CompactionStarted => protocol::HostMessage::CompactionStarted,
            RunUpdate::CompactionFinished { context, usage } => {
                self.usage += usage;
                let finished = protocol::HostMessage::CompactionFinished { context };
                return Relayed::Tell(
                    std::iter::once(finished)
                        .chain(self.pending_start.take())
                        .collect(),
                );
            }

            RunUpdate::Approval { call, tool, decide } => {
                return Relayed::AwaitApproval { call, tool, decide };
            }
        };
        Relayed::Tell(vec![message])
    }

    /// What to emit once the run's updates end, before its outcome. A stop
    /// or a provider setup failure can end pre-run compaction before it
    /// reports finishing; the submitted prompt is still recorded.
    fn end(&mut self, still_compacting: bool) -> Vec<protocol::HostMessage> {
        let Some(started) = self.pending_start.take() else {
            return Vec::new();
        };
        let unfinished =
            still_compacting.then_some(protocol::HostMessage::CompactionFinished { context: None });
        unfinished.into_iter().chain([started]).collect()
    }

    /// Everything the run's requests and summaries used.
    fn usage(&self) -> Usage {
        self.usage
    }
}

struct PendingApproval {
    /// Whether only the host may decide the call; see
    /// [`tool_approval::HOST_ONLY`].
    host_only: bool,
    decide: oneshot::Sender<ToolDecision>,
}

impl ActiveGeneration {
    pub(crate) fn granted(&self) -> Option<sandbox::Grant> {
        self.gate
            .as_ref()
            .and_then(|gate| gate.upgrade())
            .map(|gate| gate.granted())
    }

    pub(crate) fn message_id(&self) -> Uuid {
        self.message_id
    }

    /// Takes how to answer `call`, if the run is still waiting on it.
    pub(crate) fn take_approval(&mut self, call: &CallId) -> Option<oneshot::Sender<ToolDecision>> {
        self.approvals.remove(call).map(|pending| pending.decide)
    }

    /// Whether the run waits for someone to decide one of its calls.
    pub(crate) fn is_waiting_for_approval(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// Whether `call`, if the run waits on it, is one only the host decides.
    pub(crate) fn approval_is_host_only(&self, call: &CallId) -> bool {
        self.approvals
            .get(call)
            .is_some_and(|pending| pending.host_only)
    }
}

#[cfg(test)]
impl ActiveGeneration {
    pub(crate) fn for_test(
        message_id: Uuid,
        abort_handle: tokio::task::AbortHandle,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            message_id,
            abort_handle,
            cancelled,
            gate: None,
            approvals: HashMap::new(),
        }
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
            scheduled,
        } = plan;
        let thread = self.thread_store.read(cx).thread(thread_id, cx)?;
        let state = thread.read(cx);
        let history = state.request_history();
        let context_tokens = state.live_context_tokens().unwrap_or(0);
        let threshold = self.context_settings.threshold(state.max_tokens());
        // Pre-run compaction uses the old context. Its accepted summary must
        // join the transcript before the upcoming prompt.
        let compact =
            threshold.is_some_and(|threshold| context_tokens >= threshold) && !history.is_empty();
        let model = state
            .runnable_model()
            .map(|(model, info)| (model.clone(), info.max_tokens));

        // Only the host runs agents, so commands run on the host's computer
        // with its project for this thread, whoever asked for the run. Later
        // project changes reach it through `Cowork::sync_sandbox_project`,
        // and the model through the next run's note.
        self.sync_sandbox_project(&thread, cx);
        let environment_note = self.sandboxes.environment_note(&thread_id.to_string());
        let prompt = prompt::with_environment_note(
            prompt,
            if compact { &[] } else { &history },
            environment_note,
        );

        let message_id = Uuid::new_v4();
        let started_at = SystemTime::now();
        // Monotonic, so the duration survives clock changes.
        let started = Instant::now();
        let (mut relay, announcement) =
            RunRelay::start(message_id, comment_group_id, started_at, &prompt, compact);
        thread.update(cx, |thread, cx| thread.emit(announcement, cx));

        let (updates, mut receiver) = mpsc::unbounded_channel();
        let gate = ApprovalGate::new(
            updates.clone(),
            self.sandboxes.clone(),
            thread_id.to_string(),
            scheduled,
        );
        let active_gate = Arc::downgrade(&gate);
        let tools = tool_set(&self.sandboxes, &thread_id.to_string(), turn_comments);
        let generation_task = self.tokio_handle.spawn(run_agent(
            model, tools, gate, compact, prompt, history, updates,
        ));
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active_generations.insert(
            thread_id,
            ActiveGeneration {
                message_id,
                abort_handle: generation_task.abort_handle(),
                cancelled: cancelled.clone(),
                gate: Some(active_gate),
                approvals: HashMap::new(),
            },
        );

        cx.spawn(async move |this, cx| {
            // Without `Cowork` the app is closing, and nobody is left to
            // finish the run.
            while let Some(update) = receiver.recv().await {
                let messages = match relay.relay(update) {
                    Relayed::Tell(messages) => messages,
                    Relayed::AwaitApproval { call, tool, decide } => {
                        let waiting = this.update(cx, |this, cx| {
                            this.await_tool_approval(thread_id, message_id, call, &tool, decide, cx)
                        });
                        if waiting.is_err() {
                            return;
                        }
                        continue;
                    }
                };
                thread.update(cx, |thread, cx| {
                    for message in messages {
                        thread.emit(message, cx);
                    }
                });
                if this
                    .update(cx, |this, cx| this.thread_updated(thread_id, cx))
                    .is_err()
                {
                    return;
                }
            }

            let outcome = run_outcome(generation_task.await, cancelled.load(Ordering::Acquire));
            thread.update(cx, |thread, cx| {
                for message in relay.end(thread.is_compacting()) {
                    thread.emit(message, cx);
                }
            });
            let duration = started.elapsed();
            let totals = RunTotals {
                started_at,
                duration,
                usage: relay.usage(),
            };
            let ended = this.update(cx, |this, cx| {
                this.finish_generation(&thread, message_id, outcome.clone(), totals, cx)
            });
            if ended.is_err() {
                // Recorded anyway, so the transcript stays complete.
                thread.update(cx, |thread, cx| {
                    thread.emit(
                        protocol::HostMessage::AgentEnded {
                            id: message_id.into_bytes(),
                            outcome,
                            duration,
                        },
                        cx,
                    );
                });
            }
        })
        .detach();
        Some(message_id)
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
        }
        self.save_statistics(cx);
        self.thread_updated(thread_id, cx);
        if matches!(outcome, protocol::RunOutcome::Completed) {
            self.start_next_pending(thread, cx);
        }
        self.scheduled_generation_ended(thread_id, message_id, &outcome, cx);
    }

    /// Remembers how to answer `call`, which the run producing message
    /// `message_id` waits on, and tells everyone it is waiting. A run that is
    /// no longer the thread's drops `decide`, which denies the call.
    pub(crate) fn await_tool_approval(
        &mut self,
        thread_id: Uuid,
        message_id: Uuid,
        call: CallId,
        tool: &str,
        decide: oneshot::Sender<ToolDecision>,
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
        generation.approvals.insert(
            call.clone(),
            PendingApproval {
                host_only: tool_approval::host_only(tool),
                decide,
            },
        );
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::ToolApprovalRequested {
                    id: message_id.into_bytes(),
                    call: protocol::Json::call_id(&call),
                },
                cx,
            );
        });
        self.thread_updated(thread_id, cx);
        self.scheduled_run_waits(thread_id, message_id, tool, cx);
    }

    /// Adds a finished turn's usage to its thread and, unless the thread was
    /// joined from someone else, to the global count and the activity log,
    /// spread across the `duration` of the response it started at
    /// `started_at`.
    pub(crate) fn record_turn_usage(
        &mut self,
        thread: &Entity<Thread>,
        usage: Usage,
        started_at: SystemTime,
        duration: Duration,
        cx: &mut App,
    ) {
        let tokens = usage_tokens(usage);
        let ownership = thread.update(cx, |thread, _| {
            thread.tokens_used += tokens;
            thread.ownership()
        });
        if ownership == ThreadOwnership::Local {
            self.tokens_used += tokens;
            if tokens > 0 {
                self.token_activity.push(TokenActivity {
                    at: started_at,
                    duration,
                    tokens,
                });
            }
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
        // Abort within the current-policy guard. No detached authorization can
        // survive revocation and later stop a different run.
        if thread
            .update(cx, |thread, _| {
                thread.with_authorized::<ControlGeneration, _>(actor, |_| {
                    generation.cancelled.store(true, Ordering::Release);
                    generation.abort_handle.abort();
                })
            })
            .is_ok()
        {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use rig::{
        message::UserContent,
        test_utils::{MockCompletionModel, MockStreamEvent, mock_final_with_total_tokens},
    };

    use super::*;

    fn calculator() -> ToolSet {
        let mut tools = ToolSet::default();
        tools.add_tool(Calculate);
        tools
    }

    /// A summary request starts exactly like the run's requests, so the
    /// provider's prompt cache serves both, and leaves the history alone.
    #[tokio::test]
    async fn summaries_keep_the_request_prefix_and_the_history() {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::Text("Summary.".into()),
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
            ],
            vec![
                MockStreamEvent::Text("Reply.".into()),
                MockStreamEvent::FinalResponse(mock_final_with_total_tokens(20)),
            ],
        ]);
        let prefix = RequestPrefix::new(model.clone().erase(), &calculator(), 131_072);
        let history = vec![Message::user("Earlier conversation")];
        let (summary, _) = prefix.summarize(&history).await.unwrap();
        assert_eq!(summary, "Summary.");
        agent::run(
            prefix.runner(calculator(), Message::user("Continue"), history),
            |_| {},
        )
        .await
        .unwrap();
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].system_instructions(),
            requests[1].system_instructions()
        );
        // Rig combines adjacent user messages while retaining content order.
        let [system, user] = requests[0].chat_history.as_slice() else {
            panic!("unexpected summary request");
        };
        let (Message::System { .. }, Message::User { content }) = (&**system, &**user) else {
            panic!("unexpected summary request");
        };
        assert_eq!(
            content
                .iter()
                .filter_map(|part| match part {
                    UserContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["Earlier conversation", COMPACTION_PROMPT]
        );
        assert!(requests[0].tool_choice.is_none());
        assert_eq!(requests[0].tools, requests[1].tools);
        assert_eq!(requests[0].additional_params, requests[1].additional_params);
        assert!(!requests[0].tools.is_empty());
    }

    /// A summary that calls a tool, even beside text, or says nothing, is no
    /// summary; the call is never dispatched.
    #[tokio::test]
    async fn summaries_refuse_empty_and_tool_output() {
        for text in [None, Some("A mixed tool summary")] {
            let mut turn = vec![MockStreamEvent::tool_call(
                "c",
                "calculate",
                serde_json::json!({"operation":"add", "a":1, "b":2}),
            )];
            if let Some(text) = text {
                turn.push(MockStreamEvent::Text(text.into()));
            }
            turn.push(MockStreamEvent::FinalResponse(
                mock_final_with_total_tokens(10),
            ));
            let model = MockCompletionModel::from_stream_turns([turn]);
            let prefix = RequestPrefix::new(model.clone().erase(), &calculator(), 131_072);
            assert!(
                prefix
                    .summarize(&[Message::user("Original")])
                    .await
                    .is_err()
            );
            assert_eq!(model.requests().len(), 1);
        }
        let model = MockCompletionModel::from_stream_turns([vec![MockStreamEvent::FinalResponse(
            mock_final_with_total_tokens(10),
        )]]);
        let prefix = RequestPrefix::new(model.erase(), &ToolSet::default(), 131_072);
        assert!(prefix.summarize(&[]).await.is_err());
    }

    #[test]
    fn summary_must_reduce_encoded_history_including_wrapper() {
        let history = vec![rig::completion::Message::user("A short conversation")];
        let wrapped = vec![rig::completion::Message::user(
            "Summary of the conversation so far:\nShort",
        )];
        assert!(!summary_reduces_history(&history, &wrapped));
        let history = vec![rig::completion::Message::user(
            "Long conversation ".repeat(100),
        )];
        assert!(summary_reduces_history(&history, &wrapped));
    }

    fn total(tokens: u64) -> Usage {
        Usage::new().total_tokens(Some(tokens))
    }

    fn turn_ended(tokens: u64) -> RunUpdate {
        RunUpdate::Event(AgentEvent::TurnEnded {
            origin: None,
            stop: None,
            usage: total(tokens),
        })
    }

    fn told(relayed: Relayed) -> Vec<protocol::HostMessage> {
        match relayed {
            Relayed::Tell(messages) => messages,
            Relayed::AwaitApproval { .. } => panic!("expected messages"),
        }
    }

    /// A relay starting with pre-run compaction, and the `AgentStarted` it
    /// holds back.
    fn compacting_relay() -> (RunRelay, protocol::HostMessage) {
        let prompt = rig::completion::Message::user("Prompt");
        let (relay, announcement) =
            RunRelay::start(Uuid::new_v4(), None, SystemTime::now(), &prompt, true);
        assert!(matches!(
            announcement,
            protocol::HostMessage::AgentPreparing { .. }
        ));
        let started = relay.pending_start.clone().expect("held back");
        assert!(matches!(
            started,
            protocol::HostMessage::AgentStarted { .. }
        ));
        (relay, started)
    }

    /// The summary must join the transcript before the prompt it was made
    /// for, and the run counts what the summary used.
    #[test]
    fn relay_holds_the_start_until_pre_run_compaction_finishes() {
        let (mut relay, started) = compacting_relay();
        assert_eq!(
            told(relay.relay(RunUpdate::CompactionStarted)),
            [protocol::HostMessage::CompactionStarted]
        );
        let finished = relay.relay(RunUpdate::CompactionFinished {
            context: Some("Summary".into()),
            usage: total(3),
        });
        assert_eq!(
            told(finished),
            [
                protocol::HostMessage::CompactionFinished {
                    context: Some("Summary".into())
                },
                started,
            ]
        );
        let event = told(relay.relay(turn_ended(5)));
        assert!(matches!(
            event.as_slice(),
            [protocol::HostMessage::AgentEvent { .. }]
        ));
        assert_eq!(relay.end(false), []);
        assert_eq!(usage_tokens(relay.usage()), 8);
    }

    /// A stop or setup failure can end the run before compaction reports;
    /// the prompt is still recorded, after closing the compaction if it
    /// started.
    #[test]
    fn relay_records_the_start_when_compaction_never_finishes() {
        let (mut relay, started) = compacting_relay();
        assert_eq!(
            relay.end(true),
            [
                protocol::HostMessage::CompactionFinished { context: None },
                started.clone(),
            ]
        );
        assert_eq!(relay.end(true), []);

        let (mut relay, started) = compacting_relay();
        assert_eq!(relay.end(false), [started]);
    }

    #[tokio::test]
    async fn an_abort_is_a_stop_only_when_someone_asked() {
        let aborted = || async {
            let task = tokio::spawn(std::future::pending::<anyhow::Result<()>>());
            task.abort();
            task.await
        };
        assert_eq!(
            run_outcome(aborted().await, true),
            protocol::RunOutcome::Stopped
        );
        assert!(matches!(
            run_outcome(aborted().await, false),
            protocol::RunOutcome::Failed(_)
        ));
    }

    /// Displaying grants must not keep the update channel alive after the
    /// agent finishes; the receiver closing is what completes the run.
    #[tokio::test]
    async fn displaying_run_grants_does_not_keep_the_run_alive() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let gate = ApprovalGate::new(
            sender,
            crate::test_support::unused_sandboxes(),
            "thread",
            false,
        );
        let task = tokio::spawn(async {});
        let generation = ActiveGeneration {
            message_id: Uuid::new_v4(),
            abort_handle: task.abort_handle(),
            cancelled: Arc::new(AtomicBool::new(false)),
            gate: Some(Arc::downgrade(&gate)),
            approvals: HashMap::new(),
        };
        assert!(generation.granted().is_some());
        drop(gate);
        assert!(generation.granted().is_none());
        assert!(receiver.recv().await.is_none());
        task.await.unwrap();
    }

    /// The definitions are the prompt prefix's, so nothing a conversation
    /// changes may reach them: not the thread, its comments, its project's
    /// folders or mode, nor where the scratch folders are.
    #[test]
    fn tool_definitions_never_change_within_a_session() {
        let sandboxes = crate::test_support::unused_sandboxes();
        let definitions = |sandboxes: &Arc<sandbox::Sandboxes>, thread: &str, comments| {
            tool_set(
                sandboxes,
                thread,
                Arc::new(tools::TurnComments::new(comments)),
            )
            .tool_definitions()
        };
        let first = definitions(&sandboxes, "thread-0f1e", 0);

        let folder = std::env::temp_dir().join(format!(
            "cowork-tool-definitions-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        sandboxes.set_project(
            "thread-0f1e",
            sandbox::Project {
                folders: vec![sandbox::ProjectFolder {
                    name: "folder-5c2d".into(),
                    path: folder.clone(),
                }],
                access: sandbox::Access::Write,
            },
        );
        let changed = [
            definitions(&sandboxes, "thread-0f1e", 0),
            definitions(&sandboxes, "thread-0f1e", 3),
            definitions(&sandboxes, "thread-9a8b", 1),
            definitions(&crate::test_support::unused_sandboxes(), "thread-0f1e", 0),
        ];
        std::fs::remove_dir_all(&folder).unwrap();

        for definitions in changed {
            assert_eq!(definitions, first);
        }
        let text = serde_json::to_string(&first).unwrap();
        for host_specific in ["thread-0f1e", "folder-5c2d", &folder.to_string_lossy()] {
            assert!(!text.contains(host_specific), "names {host_specific}");
        }
    }
}
