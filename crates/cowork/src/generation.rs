//! Running the agent for a thread, and forwarding what it does to everyone
//! in it; see `transcript.rs` for how that is folded.

use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use agent::{Agent as StreamingAgent, AgentEvent, ToolDecision};
use anyhow::Context as _;
use gpui::{App, Context, Entity};
use rig::{completion::Usage, message::CallId, providers::ollama::Ollama, tool::ToolSet};
use sandbox::{ListDirectory, ReadFile, RequestPermissions, RunCommand, WriteFile};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tools::{Calculate, RespondToComment};

use super::GenerationPlan;
use uuid::Uuid;

use crate::{
    Cowork,
    models::ModelProvider,
    participant::ParticipantId,
    prompt, protocol,
    thread::{ControlGeneration, Thread, ThreadOwnership, ThreadSharing},
    tool_approval::{self, ApprovalGate, RunUpdate},
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
fn summary_reduces_history(
    history: &[rig::completion::Message],
    replacement: &[rig::completion::Message],
) -> bool {
    match (serde_json::to_vec(history), serde_json::to_vec(replacement)) {
        (Ok(before), Ok(after)) => after.len() < before.len(),
        _ => false,
    }
}

/// Uses the same summary path before a run and at its tool boundaries.
/// Transcript positions count ordinary messages, never the smaller context.
struct GenerationCompaction {
    threshold: Option<u64>,
    transcript_base: u64,
    latest_context_tokens: Mutex<u64>,
    environment_note: String,
    updates: mpsc::UnboundedSender<RunUpdate>,
}

impl GenerationCompaction {
    async fn compact(
        &self,
        agent: &StreamingAgent,
        history: &[rig::completion::Message],
        through: u64,
        retain_environment: bool,
    ) -> Option<Vec<rig::completion::Message>> {
        _ = self.updates.send(RunUpdate::CompactionStarted);
        let result = agent
            .summarize(rig::completion::Message::user(COMPACTION_PROMPT), history)
            .await;
        let (context, replacement, usage) = match result {
            Ok((mut summary, usage)) => {
                // Between requests there is no new user prompt to restore
                // the environment note, so carry it with the summary.
                if retain_environment {
                    summary.push_str("\n\n");
                    summary.push_str(&self.environment_note);
                }
                let replacement = vec![rig::completion::Message::user(format!(
                    "Summary of the conversation so far:\n{summary}"
                ))];
                if summary_reduces_history(history, &replacement) {
                    *self.latest_context_tokens.lock().unwrap() =
                        (summary.len() as u64).div_ceil(4);
                    (
                        Some(crate::transcript::Compaction {
                            resume_at: crate::transcript::TranscriptIndex(through),
                            summary,
                        }),
                        Some(replacement),
                        usage,
                    )
                } else {
                    eprintln!("compaction skipped: summary did not reduce request history");
                    (None, None, usage)
                }
            }
            Err(error) => {
                eprintln!("compaction failed: {error:#}");
                (None, None, Usage::default())
            }
        };
        _ = self
            .updates
            .send(RunUpdate::CompactionFinished { context, usage });
        replacement
    }
}

impl agent::ContextHook for GenerationCompaction {
    fn before_request<'a>(
        &'a self,
        agent: &'a StreamingAgent,
        boundary: agent::RequestBoundary<'a>,
    ) -> futures::future::BoxFuture<'a, Option<Vec<rig::completion::Message>>> {
        Box::pin(async move {
            let threshold = self.threshold?;
            let context_tokens = if boundary.usage.is_reported() {
                usage_tokens(boundary.usage).saturating_add(boundary.tool_result_tokens)
            } else {
                // Interrupted streams have no final usage. Keep the last
                // count and estimate the completed messages instead.
                let estimate = serde_json::to_vec(boundary.history)
                    .map_or(0, |bytes| (bytes.len() as u64).div_ceil(4));
                (*self.latest_context_tokens.lock().unwrap()).max(estimate)
            };
            *self.latest_context_tokens.lock().unwrap() = context_tokens;
            if context_tokens < threshold {
                return None;
            }
            self.compact(
                agent,
                boundary.history,
                self.transcript_base + boundary.added_messages as u64,
                true,
            )
            .await
        })
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
    pub(super) steering: Arc<Mutex<Option<SteeringPayload>>>,
    pub(super) turn_comments: Arc<tools::TurnComments>,
}

pub(super) struct SteeringPayload {
    pub(super) group: crate::timeline::UserMessageGroup,
    pub(super) aliases: Vec<tools::CommentId>,
    pub(super) prompt: rig::completion::Message,
}

struct GenerationSteering {
    pending: Arc<Mutex<Option<SteeringPayload>>>,
    updates: mpsc::UnboundedSender<RunUpdate>,
}
impl agent::SteeringHook for GenerationSteering {
    fn has_pending(&self) -> bool {
        self.pending.lock().unwrap().is_some()
    }
    fn take_pending(&self) -> Option<rig::completion::Message> {
        let payload = self.pending.lock().unwrap().take()?;
        _ = self.updates.send(RunUpdate::SteeringConsumed {
            group: payload.group,
            aliases: payload
                .aliases
                .iter()
                .map(|alias| alias.as_str().to_owned())
                .collect(),
        });
        Some(payload.prompt)
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
    pub(crate) fn take_steering_for_test(
        &self,
    ) -> (
        crate::timeline::UserMessageGroup,
        Vec<String>,
        rig::completion::Message,
    ) {
        let payload = self
            .steering
            .lock()
            .unwrap()
            .take()
            .expect("pending steering");
        (
            payload.group,
            payload
                .aliases
                .iter()
                .map(|alias| alias.as_str().to_owned())
                .collect(),
            payload.prompt,
        )
    }

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
            steering: Arc::new(Mutex::new(None)),
            turn_comments: Arc::new(tools::TurnComments::new(0)),
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
            mut prompt,
            comment_group_id,
            turn_comments,
            scheduled,
        } = plan;
        let thread = self.thread_store.read(cx).thread(thread_id, cx)?;
        // Capture context and its original boundary together, before recording
        // the new run's prompt. A summary must never cover that later prompt.
        let mut history = thread.read(cx).request_history();
        let compact_through = thread.read(cx).transcript.end().0;
        let context_tokens = thread.read(cx).live_context_tokens().unwrap_or(0);
        let threshold = self
            .context_settings
            .threshold(thread.read(cx).max_tokens());
        let compact =
            threshold.is_some_and(|threshold| context_tokens >= threshold) && !history.is_empty();
        let message_id = Uuid::new_v4();
        let started_at = SystemTime::now();
        // Monotonic, so the duration survives clock changes.
        let started = Instant::now();
        let selected_model = thread
            .read(cx)
            .runnable_model()
            .map(|(model, info)| (model.clone(), info.max_tokens));
        // Only the host runs agents, so commands run on the host's computer
        // with its project for this thread, whoever asked for the run. Later
        // project changes reach it through `Cowork::sync_sandbox_project`,
        // and the model through the next run's note.
        self.sync_sandbox_project(&thread, cx);
        let environment_note = self.sandboxes.environment_note(&thread_id.to_string());
        prompt = prompt::with_environment_note(
            prompt,
            if compact { &[] } else { &history },
            environment_note.clone(),
        );
        thread.update(cx, |thread, cx| {
            // Recorded before the run starts, so a prompt stays in the
            // transcript even when the run is stopped before sending it.
            thread.emit(
                protocol::HostMessage::AgentStarted {
                    id: message_id.into_bytes(),
                    comment_group_id: comment_group_id.map(Uuid::into_bytes),
                    started_at,
                    prompt: protocol::Json::from_rig(&prompt),
                },
                cx,
            );
        });
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let compaction = Arc::new(GenerationCompaction {
            threshold,
            transcript_base: compact_through,
            latest_context_tokens: Mutex::new(context_tokens),
            environment_note,
            updates: sender.clone(),
        });
        let tools = tool_set(
            &self.sandboxes,
            &thread_id.to_string(),
            turn_comments.clone(),
        );
        let resumed_steering = {
            let state = thread.read(cx);
            state
                .pending
                .iter()
                .find(|pending| pending.mode == protocol::SubmissionMode::Steering)
                .map(|pending| {
                    let aliases = turn_comments
                        .extend(pending.group.comments.len())
                        .expect("comment state is available");
                    let preface = Cowork::comments_preface(
                        &pending.group.comments,
                        &aliases,
                        &state.timeline,
                        &state.prompt_names,
                    );
                    let prompt = prompt::agent_message(
                        preface.as_deref(),
                        &pending.group.blocks,
                        &state.draft().files,
                        &state.prompt_names,
                    );
                    SteeringPayload {
                        group: pending.group.clone(),
                        aliases,
                        prompt,
                    }
                })
        };
        let pending_steering = Arc::new(Mutex::new(resumed_steering));
        let steering = Arc::new(GenerationSteering {
            pending: pending_steering.clone(),
            updates: sender.clone(),
        });
        let gate = ApprovalGate::new(
            sender.clone(),
            self.sandboxes.clone(),
            thread_id.to_string(),
            scheduled,
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let active_gate = Arc::downgrade(&gate);
        let generation_task = self.tokio_handle.spawn(async move {
            let (selected_model, max_tokens) =
                selected_model.context("No available model selected")?;
            let model = match selected_model.provider {
                // `num_ctx` is a model option only the native `/api/chat`
                // accepts; the OpenAI-compatible route refuses the request.
                ModelProvider::Ollama => Ollama::new().native_completion(selected_model.id),
            };
            let agent = StreamingAgent::new(model.erase(), tools)
                .tool_hook(gate)
                .steering_hook(steering)
                .context_hook(compaction.clone())
                .preamble(system_prompt())
                .additional_params(json!({
                    "num_ctx": max_tokens,
                    "think": "medium"
                }));
            if compact
                && let Some(replacement) = compaction
                    .compact(&agent, &history, compact_through, false)
                    .await
            {
                history = replacement;
            }
            agent
                .run(prompt, &mut history, move |event| {
                    _ = sender.send(RunUpdate::Event(event));
                })
                .await?;
            Ok::<_, anyhow::Error>(())
        });
        self.active_generations.insert(
            thread_id,
            ActiveGeneration {
                message_id,
                abort_handle: generation_task.abort_handle(),
                cancelled: cancelled.clone(),
                gate: Some(active_gate),
                approvals: HashMap::new(),
                steering: pending_steering,
                turn_comments,
            },
        );

        cx.spawn(async move |this, cx| {
            let mut stream_completed = true;
            let mut turn_usage = Usage::default();
            while let Some(update) = receiver.recv().await {
                let event = match update {
                    RunUpdate::Event(event) => event,
                    RunUpdate::CompactionStarted => {
                        thread.update(cx, |thread, cx| {
                            thread.emit(protocol::HostMessage::CompactionStarted, cx)
                        });
                        _ = this.update(cx, |this, cx| this.thread_updated(thread_id, cx));
                        continue;
                    }
                    RunUpdate::CompactionFinished { context, usage } => {
                        turn_usage += usage;
                        thread.update(cx, |thread, cx| {
                            thread.emit(protocol::HostMessage::CompactionFinished { context }, cx)
                        });
                        _ = this.update(cx, |this, cx| this.thread_updated(thread_id, cx));
                        continue;
                    }
                    RunUpdate::SteeringConsumed { group, aliases } => {
                        thread.update(cx, |thread, cx| {
                            thread.emit(
                                protocol::HostMessage::PendingDelivered {
                                    id: group.id.into_bytes(),
                                    group: group.to_protocol(),
                                    agent_id: Some(message_id.into_bytes()),
                                    aliases,
                                },
                                cx,
                            )
                        });
                        continue;
                    }
                    RunUpdate::Approval { call, tool, decide } => {
                        if this
                            .update(cx, |this, cx| {
                                this.await_tool_approval(
                                    thread_id, message_id, call, &tool, decide, cx,
                                )
                            })
                            .is_err()
                        {
                            stream_completed = false;
                            break;
                        }
                        continue;
                    }
                };
                if let AgentEvent::TurnEnded { usage, .. } = &event {
                    turn_usage += *usage;
                }
                let event = protocol::Json::shared(event);
                thread.update(cx, |thread, cx| {
                    thread.emit(
                        protocol::HostMessage::AgentEvent {
                            id: message_id.into_bytes(),
                            event,
                        },
                        cx,
                    );
                });

                if this
                    .update(cx, |this, cx| this.thread_updated(thread_id, cx))
                    .is_err()
                {
                    stream_completed = false;
                    break;
                }
            }

            if stream_completed {
                let failed = |error: anyhow::Error| {
                    protocol::RunOutcome::Failed(format!(
                        "Unable to generate a response: {error:#}"
                    ))
                };
                let outcome = match generation_task.await {
                    Ok(Ok(())) => protocol::RunOutcome::Completed,
                    Ok(Err(error)) => failed(error),
                    Err(error) if error.is_cancelled() && cancelled.load(Ordering::Acquire) => {
                        protocol::RunOutcome::Stopped
                    }
                    Err(error) => failed(error.into()),
                };
                let duration = started.elapsed();
                let ended = this.update(cx, |this, cx| {
                    this.finish_generation(
                        &thread,
                        message_id,
                        outcome.clone(),
                        RunTotals {
                            started_at,
                            duration,
                            usage: turn_usage,
                        },
                        cx,
                    )
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
#[path = "generation_compaction_tests.rs"]
mod compaction_tests;

#[cfg(test)]
mod tests {
    use rig::tool::Tool as _;

    use super::*;

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
            steering: Arc::new(Mutex::new(None)),
            turn_comments: Arc::new(tools::TurnComments::new(0)),
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
        let names = first
            .iter()
            .map(|definition| definition.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                RespondToComment::NAME,
                Calculate::NAME,
                RunCommand::NAME,
                RequestPermissions::NAME,
                ListDirectory::NAME,
                ReadFile::NAME,
                WriteFile::NAME,
            ],
            "in the same order every time"
        );
        let text = serde_json::to_string(&first).unwrap();
        for host_specific in ["thread-0f1e", "folder-5c2d", &folder.to_string_lossy()] {
            assert!(!text.contains(host_specific), "names {host_specific}");
        }
    }
}
