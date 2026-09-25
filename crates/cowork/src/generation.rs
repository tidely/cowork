//! Running the agent for a thread and folding its stream into the
//! timeline.

use std::{
    collections::{HashSet, hash_map::Entry},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use agent::{Agent as StreamingAgent, AgentEvent};
use anyhow::Context as _;
use gpui::{App, Context, Entity};
use rig::{
    completion::{Message as RigMessage, Usage},
    prelude::*,
    providers::ollama::wire::Ollama,
    streaming::{BlockClose, Delta, StreamEvent},
    tool::ToolSet,
};
use serde_json::json;
use tokio::sync::mpsc;
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

use crate::{
    Cowork,
    models::ModelProvider,
    protocol,
    thread::{Thread, ThreadOwnership, ThreadSharing},
    usage::{TokenActivity, usage_tokens},
};

pub(crate) struct ActiveGeneration {
    pub(crate) message_id: Uuid,
    pub(crate) abort_handle: tokio::task::AbortHandle,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl Cowork {
    pub(crate) fn start_generation(
        &mut self,
        thread_id: Uuid,
        prompt: RigMessage,
        mut history: Vec<RigMessage>,
        comment_group_id: Option<Uuid>,
        comment_ids: Vec<Uuid>,
        turn_comments: Arc<TurnComments>,
        cx: &mut Context<Self>,
    ) {
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
            thread.transcript.push(prompt.clone());
            thread.emit(
                protocol::HostMessage::AgentStarted {
                    id: message_id.into_bytes(),
                    comment_group_id: comment_group_id.map(Uuid::into_bytes),
                    started_at,
                },
                cx,
            );
        });
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let tool_comments = turn_comments.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation_task = self.tokio_handle.spawn(async move {
            let (selected_model, max_tokens) =
                selected_model.context("No available model selected")?;
            let model = match selected_model.provider {
                ModelProvider::Ollama => Ollama::new().bound()?.completion(selected_model.id),
            };
            let mut tools = ToolSet::default();
            tools.add_tool(RespondToComment::new(tool_comments));
            StreamingAgent::new(model, tools)
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
            let mut published_comment_responses = HashSet::new();
            let mut turn_usage = Usage::default();
            while let Some(item) = receiver.recv().await {
                if let AgentEvent::HistoryAppended(message) = item {
                    thread.update(cx, |thread, _| thread.transcript.push(message));
                    continue;
                }
                if let AgentEvent::Usage(usage) = item {
                    turn_usage += usage;
                    // Each request sends the whole transcript, so its usage
                    // is how full the context is.
                    if usage.is_reported() {
                        thread.update(cx, |thread, cx| {
                            thread.emit(
                                protocol::HostMessage::ContextMeasured(usage_tokens(usage)),
                                cx,
                            );
                        });
                        _ = this.update(cx, |_, cx| cx.notify());
                    }
                    continue;
                }
                if let AgentEvent::ToolCall(call) = &item
                    && call.function.name == "respond_to_comment"
                    && let Ok(response) = serde_json::from_value::<RespondToCommentArgs>(
                        call.function.arguments.clone(),
                    )
                    && !response.response.trim().is_empty()
                    && published_comment_responses.insert(response.comment_id.clone())
                    && let Some(comment_id) = turn_comments
                        .comment_ids()
                        .iter()
                        .position(|comment_id| comment_id.as_str() == response.comment_id)
                        .and_then(|index| comment_ids.get(index))
                {
                    thread.update(cx, |thread, cx| {
                        thread.emit(
                            protocol::HostMessage::AgentCommentResponded {
                                id: message_id.into_bytes(),
                                response_id: Uuid::new_v4().into_bytes(),
                                comment_id: comment_id.into_bytes(),
                                response: response.response,
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

                let Some(event) = Self::agent_stream_event(message_id, item) else {
                    continue;
                };
                thread.update(cx, |thread, cx| thread.emit(event, cx));

                if this
                    .update(cx, |this, cx| this.thread_updated(thread_id, cx))
                    .is_err()
                {
                    stream_completed = false;
                    break;
                }
            }

            if stream_completed {
                let error = match generation_task.await {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(error) if error.is_cancelled() && cancelled.load(Ordering::Acquire) => None,
                    Err(error) => Some(error.into()),
                };
                let duration = started.elapsed();
                thread.update(cx, |thread, cx| {
                    thread.emit(
                        protocol::HostMessage::AgentEnded {
                            id: message_id.into_bytes(),
                            failure: error
                                .map(|error| format!("Unable to generate a response: {error}")),
                            duration,
                        },
                        cx,
                    );
                });
                _ = this.update(cx, |this, cx| {
                    this.record_turn_usage(&thread, turn_usage, started_at, duration, cx);
                    // The profile page's statistics may be showing.
                    cx.notify();
                    if let Entry::Occupied(entry) = this.active_generations.entry(thread_id) {
                        if entry.get().message_id == message_id {
                            entry.remove();
                        }
                    }
                    this.thread_updated(thread_id, cx);
                });
            }
        })
        .detach();
    }

    /// Translates one item of the agent's stream into the thread event it
    /// represents, or `None` for items that do not change the timeline.
    fn agent_stream_event(message_id: Uuid, item: AgentEvent) -> Option<protocol::HostMessage> {
        let id = message_id.into_bytes();
        match item {
            AgentEvent::Model(StreamEvent::BlockDelta {
                delta: Delta::Reasoning { text },
                ..
            }) => Some(protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Thinking,
                text,
            }),
            AgentEvent::Model(StreamEvent::BlockEnd {
                end: BlockClose::Reasoning { .. },
                ..
            }) => Some(protocol::HostMessage::AgentThinkingEnded { id }),
            AgentEvent::Model(StreamEvent::BlockDelta {
                delta: Delta::Text { text },
                ..
            }) => Some(protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Response,
                text,
            }),
            AgentEvent::Model(_)
            | AgentEvent::ToolCall(_)
            | AgentEvent::ToolResult { .. }
            | AgentEvent::HistoryAppended(_)
            | AgentEvent::Usage(_) => None,
        }
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
            thread.ownership
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
        let thread = thread.read(cx);
        if matches!(thread.sharing, ThreadSharing::Connected { .. }) {
            if let Some(message_id) = thread.running_agent_message_id() {
                thread.request(protocol::CollaboratorMessage::Stop {
                    message_id: message_id.into_bytes(),
                });
            }
            return;
        }
        let thread_id = thread.instance_id;
        self.cancel_generation(thread_id, None, cx);
    }

    /// Cancels the agent run of a local or hosted thread. With `message_id`,
    /// only a run still producing that message is cancelled, so a stale stop
    /// request cannot cancel the run that followed it.
    pub(crate) fn cancel_generation(
        &mut self,
        thread_id: Uuid,
        message_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self.active_generations.get(&thread_id) else {
            return;
        };
        if message_id.is_some_and(|message_id| message_id != generation.message_id) {
            return;
        }
        generation.cancelled.store(true, Ordering::Release);
        generation.abort_handle.abort();
        cx.notify();
    }
}
