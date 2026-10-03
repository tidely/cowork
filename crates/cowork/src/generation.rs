//! Running the agent for a thread, and forwarding what it does to everyone
//! in it; see `transcript.rs` for how that is folded.

use std::{
    collections::hash_map::Entry,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use agent::{Agent as StreamingAgent, AgentEvent};
use anyhow::Context as _;
use gpui::{App, Context, Entity};
use rig::{completion::Usage, providers::ollama::Ollama, tool::ToolSet};
use serde_json::json;
use tokio::sync::mpsc;
use tools::{Calculate, RespondToComment};

use super::GenerationPlan;
use uuid::Uuid;

use crate::{
    Cowork,
    models::ModelProvider,
    participant::ParticipantId,
    protocol,
    thread::{ControlGeneration, Thread, ThreadOwnership, ThreadSharing},
    usage::{TokenActivity, usage_tokens},
};

/// The system prompt every run starts with. Kept in its own file so it reads
/// and diffs as prose; it is static, so it needs no templating.
const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

pub(crate) struct ActiveGeneration {
    message_id: Uuid,
    abort_handle: tokio::task::AbortHandle,
    cancelled: Arc<AtomicBool>,
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
        }
    }
}

impl Cowork {
    pub(super) fn start_generation(&mut self, plan: GenerationPlan, cx: &mut Context<Self>) {
        let GenerationPlan {
            thread_id,
            prompt,
            mut history,
            comment_group_id,
            turn_comments,
        } = plan;
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let message_id = Uuid::new_v4();
        let started_at = SystemTime::now();
        // Monotonic, so the duration survives clock changes.
        let started = Instant::now();
        let selected_model = thread
            .read(cx)
            .runnable_model()
            .map(|(model, info)| (model.clone(), info.max_tokens));
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
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation_task = self.tokio_handle.spawn(async move {
            let (selected_model, max_tokens) =
                selected_model.context("No available model selected")?;
            let model = match selected_model.provider {
                ModelProvider::Ollama => Ollama::new().completion(selected_model.id),
            };
            // The same tools on every run, whether or not a turn has comments:
            // the definitions are part of the prompt prefix, so changing them
            // would invalidate the model's prompt cache.
            let mut tools = ToolSet::default();
            tools.add_tool(RespondToComment::new(turn_comments));
            tools.add_tool(Calculate);
            StreamingAgent::new(model.erase(), tools)
                .preamble(SYSTEM_PROMPT)
                .additional_params(json!({
                    "num_ctx": max_tokens,
                    "think": "medium"
                }))
                .run(prompt, &mut history, move |event| {
                    _ = sender.send(event);
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
            },
        );

        cx.spawn(async move |this, cx| {
            let mut stream_completed = true;
            let mut turn_usage = Usage::default();
            while let Some(event) = receiver.recv().await {
                if let AgentEvent::TurnEnded { usage, .. } = &event {
                    turn_usage += *usage;
                }
                let Some(event) = protocol::Json::shared(event) else {
                    continue;
                };
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
                    protocol::RunOutcome::Failed(format!("Unable to generate a response: {error}"))
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
                _ = this.update(cx, |this, cx| {
                    this.record_turn_usage(&thread, turn_usage, started_at, duration, cx);
                    // The profile page's statistics may be showing.
                    cx.notify();
                    if let Entry::Occupied(entry) = this.active_generations.entry(thread_id)
                        && entry.get().message_id == message_id
                    {
                        entry.remove();
                    }
                    this.thread_updated(thread_id, cx);
                });
            }
        })
        .detach();
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
