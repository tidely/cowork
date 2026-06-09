use std::time::Duration;

use futures::SinkExt;
use futures_util::StreamExt;
use rig_core::{
    agent::{
        AgentBuilder, HookAction, MultiTurnStreamItem, PromptHook, StreamingError, StreamingResult,
        ToolCallHookAction, WithBuilderTools,
    },
    client::{CompletionClient, ProviderClient},
    completion::CompletionModel,
    memory::{ConversationMemory, InMemoryConversationMemory},
    message::{AssistantContent, Message as RigMessage},
    providers::ollama,
    streaming::{StreamedAssistantContent, StreamingPrompt},
};
use tokio_util::sync::PollSender;

use crate::{
    app::{AgentAddr, AgentEvent, ThreadId},
    tui::{RuntimeEvent, RuntimeEventSender},
};

#[derive(Clone)]
pub(crate) struct AgentEventSink {
    events: PollSender<RuntimeEvent>,
    thread_id: ThreadId,
}

impl AgentEventSink {
    pub(crate) fn new(events: RuntimeEventSender, thread_id: ThreadId) -> Self {
        Self {
            events: PollSender::new(events),
            thread_id,
        }
    }
}

impl AgentEventSink {
    pub(crate) async fn send(&mut self, event: AgentEvent) {
        let _ = self
            .events
            .send(RuntimeEvent::Agent(self.thread_id, event))
            .await;
    }
}

#[derive(Clone)]
pub(crate) struct UiPromptHook {
    sink: AgentEventSink,
    addr: AgentAddr,
}

impl UiPromptHook {
    pub(crate) fn new(sink: AgentEventSink, addr: AgentAddr) -> Self {
        Self { sink, addr }
    }
    async fn emit_tool_call_event(
        &self,
        tool_name: &str,
        internal_call_id: &str,
        args: &str,
    ) -> ToolCallHookAction {
        let arguments = serde_json::from_str(args)
            .unwrap_or_else(|_| serde_json::Value::String(args.to_string()));
        let mut sink = self.sink.clone();
        sink.send(AgentEvent::ToolCall {
            addr: self.addr,
            id: internal_call_id.to_string(),
            name: tool_name.to_string(),
            arguments,
        })
        .await;
        ToolCallHookAction::cont()
    }

    async fn emit_tool_result_event(&self, internal_call_id: &str, result: &str) -> HookAction {
        let mut sink = self.sink.clone();
        sink.send(AgentEvent::ToolResult {
            addr: self.addr,
            id: internal_call_id.to_string(),
            content: result.to_string(),
        })
        .await;
        HookAction::cont()
    }
}

impl<M> PromptHook<M> for UiPromptHook
where
    M: CompletionModel,
{
    async fn on_tool_call(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        internal_call_id: &str,
        args: &str,
    ) -> ToolCallHookAction {
        self.emit_tool_call_event(tool_name, internal_call_id, args)
            .await
    }

    async fn on_tool_result(
        &self,
        _tool_name: &str,
        _tool_call_id: Option<String>,
        internal_call_id: &str,
        _args: &str,
        result: &str,
    ) -> HookAction {
        self.emit_tool_result_event(internal_call_id, result).await
    }
}

pub const MODEL: &str = "gemma4:31b-it-qat";
const DEFAULT_TOOL_CONCURRENCY: usize = 8;
const DEFAULT_CHILD_SUBAGENT_CONCURRENCY: usize = 1;
pub(crate) const AGENT_MAX_TURNS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutionPolicy {
    pub(crate) tool_concurrency: usize,
    pub(crate) child_subagent_concurrency: usize,
}

impl ExecutionPolicy {
    pub(crate) fn from_env() -> Self {
        Self {
            tool_concurrency: parse_env("COWORK_TOOL_CONCURRENCY", DEFAULT_TOOL_CONCURRENCY),
            child_subagent_concurrency: parse_env(
                "COWORK_SUBAGENT_CONCURRENCY",
                DEFAULT_CHILD_SUBAGENT_CONCURRENCY,
            ),
        }
    }
}

fn parse_env<R: std::str::FromStr>(name: &str, default: R) -> R {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Backoffs between successive retries, slowest last. We make one more attempt
/// than there are backoffs — the final attempt has nothing waiting after it — so
/// a failure on 1-based attempt `n` waits `PROMPT_RETRY_BACKOFFS[n - 1]`, and a
/// failure on the last attempt finds no entry and gives up.
pub(crate) const PROMPT_RETRY_BACKOFFS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
pub(crate) const PROMPT_RETRY_ATTEMPTS: usize = PROMPT_RETRY_BACKOFFS.len() + 1;

/// How long to wait before retrying a failed attempt, or `None` to give up —
/// either because the error is deterministic or because we are out of retry
/// budget. The `get` returning `None` past the schedule is the budget check.
pub(crate) fn retry_backoff(
    attempt: usize,
    error: &(dyn std::error::Error + Send + Sync),
) -> Option<Duration> {
    if is_retryable_prompt_error(error) {
        PROMPT_RETRY_BACKOFFS.get(attempt - 1).copied()
    } else {
        None
    }
}

/// Build an agent preconfigured with the shared model, reasoning, the read-only
/// filesystem tools, and a turn limit. Callers add memory, extra tools, and a
/// hook before `build()`. Keeping this in one place means a new shared tool is a
/// single-line change rather than one edit per agent level.
pub(crate) fn base_agent_builder(
    client: &ollama::Client,
    preamble: &str,
    max_turns: usize,
) -> AgentBuilder<ollama::CompletionModel, (), WithBuilderTools> {
    client
        .agent(MODEL)
        .preamble(preamble)
        .additional_params(serde_json::json!({ "think": true }))
        .default_max_turns(max_turns)
        .tool(crate::tools::ReadFile)
        .tool(crate::tools::ReadPdf)
        .tool(crate::tools::ListDirectory)
}

pub const MAIN_AGENT_PREAMBLE: &str = "You are the top-level user-facing assistant with persistent conversation context. Use tools when relevant. \
     Do continuous work yourself when future prompts depend on your accumulated understanding, such as ongoing work in the same codebase or project. \
     Do not use subagent for simple one- or two-tool tasks, single-file inspection, straightforward path reads/listing, or tasks needing continuous shared context. \
     For broad bounded one-off tasks with many known independent chunks, prefer calling subagent instead of manually iterating every chunk yourself. \
     A subagent owns only the bounded task you give it; it must not broaden scope, explore unrelated directories, or invent follow-up work. \
     When delegating, specify: the goal, why it matters, exact scope boundaries, known paths/resources, constraints, expected output shape, and what to do on failure. \
     If a delegated path is missing, too large, inaccessible, or otherwise blocks the task, instruct the subagent to stop and report the blocker plus what it tried rather than exploring elsewhere or spawning recovery subagents. \
     Only authorize subagents to spawn children when the delegated task explicitly contains multiple known independent chunks; otherwise they should do the task themselves and return a concise result.";

pub(crate) fn is_retryable_prompt_error(error: &(dyn std::error::Error + Send + Sync)) -> bool {
    let message = error.to_string().to_ascii_lowercase();

    // Tool and JSON/schema errors are usually deterministic. Retrying those can
    // duplicate tool side effects without improving the outcome.
    if message.contains("toolseterror")
        || message.contains("toolcallerror")
        || message.contains("toolnotfounderror")
        || message.contains("jsonerror")
        || message.contains("maxturnserror")
    {
        return false;
    }

    message.contains("httperror")
        || message.contains("providererror") && message.contains("status code")
        || message.contains("connection")
        || message.contains("timeout")
        || message.contains("timed out")
        || message.contains("network")
        || message.contains("broken pipe")
        || message.contains("connection reset")
        || message.contains("connection refused")
        || message.contains("temporarily unavailable")
        || message.contains("dns")
}

pub fn spawn_prompt_task(
    thread_id: ThreadId,
    prompt: String,
    conversation_id: String,
    memory: InMemoryConversationMemory,
    events: RuntimeEventSender,
) {
    tokio::spawn(async move {
        let mut sink = AgentEventSink::new(events.clone(), thread_id);
        sink.send(AgentEvent::Started).await;

        if let Err(error) =
            run_prompt_with_retries(thread_id, prompt, conversation_id, memory, events).await
        {
            sink.send(AgentEvent::Error {
                addr: AgentAddr::Main,
                error: error.to_string(),
            })
            .await;
        }
    });
}

async fn run_prompt_with_retries(
    thread_id: ThreadId,
    prompt: String,
    conversation_id: String,
    memory: InMemoryConversationMemory,
    events: RuntimeEventSender,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for attempt in 1..=PROMPT_RETRY_ATTEMPTS {
        let error = match run_prompt_once(
            thread_id,
            prompt.clone(),
            conversation_id.clone(),
            memory.clone(),
            events.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        let Some(backoff) = retry_backoff(attempt, error.as_ref()) else {
            remember_failed_prompt(&memory, &conversation_id, &prompt, error.as_ref()).await;
            return Err(error);
        };

        tokio::time::sleep(backoff).await;
    }

    unreachable!("retry loop always returns")
}

async fn remember_failed_prompt(
    memory: &InMemoryConversationMemory,
    conversation_id: &str,
    prompt: &str,
    error: &(dyn std::error::Error + Send + Sync),
) {
    let messages = vec![
        RigMessage::from(prompt),
        RigMessage::from(AssistantContent::from(format!(
            "The previous attempt failed before a final response was produced: {error}"
        ))),
    ];

    let _ = memory.append(conversation_id, messages).await;
}

async fn run_prompt_once(
    thread_id: ThreadId,
    prompt: String,
    conversation_id: String,
    memory: InMemoryConversationMemory,
    events: RuntimeEventSender,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = ollama::Client::from_env()?;
    let policy = ExecutionPolicy::from_env();
    let agent = base_agent_builder(&client, MAIN_AGENT_PREAMBLE, AGENT_MAX_TURNS)
        .memory(memory)
        .tool(crate::tools::EditFile)
        .tool(crate::tools::Subagent::new(
            crate::tools::ToolUiContext::new(thread_id, events.clone(), policy),
        ))
        .hook(UiPromptHook::new(
            AgentEventSink::new(events.clone(), thread_id),
            AgentAddr::Main,
        ))
        .build();

    let mut stream = agent
        .stream_prompt(prompt)
        .conversation(&conversation_id)
        .with_tool_concurrency(policy.tool_concurrency)
        .await;

    let mut sink = AgentEventSink::new(events, thread_id);
    pump_stream(&mut sink, AgentAddr::Main, &mut stream).await?;

    sink.send(AgentEvent::Finished {
        addr: AgentAddr::Main,
        result: None,
    })
    .await;
    Ok(())
}

/// Drive an agent's stream to completion, emitting UI events for `addr` through
/// `sink`. Returns the agent's final response text (used by subagents; ignored
/// by the top-level agent).
pub(crate) async fn pump_stream<R>(
    sink: &mut AgentEventSink,
    addr: AgentAddr,
    stream: &mut StreamingResult<R>,
) -> Result<String, StreamingError> {
    let mut streamed_text = String::new();
    let mut final_response = None;

    while let Some(item) = stream.next().await {
        match item? {
            MultiTurnStreamItem::StreamAssistantItem(content) => {
                if let StreamedAssistantContent::Text(text) = &content {
                    streamed_text.push_str(&text.text);
                }
                for event in assistant_events(addr, content) {
                    sink.send(event).await;
                }
            }
            MultiTurnStreamItem::StreamUserItem(_) => {}
            MultiTurnStreamItem::CompletionCall(call) => {
                if let Some(usage) = call.usage {
                    sink.send(AgentEvent::Usage {
                        addr,
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        total_tokens: usage.total_tokens,
                    })
                    .await;
                }
            }
            MultiTurnStreamItem::FinalResponse(response) => {
                final_response = Some(response.response().to_string());
            }
            _ => {}
        }
    }

    Ok(final_response.unwrap_or(streamed_text))
}

/// Translate a streamed assistant item into UI events. Pure (no I/O), so the
/// mapping is unit-tested directly.
fn assistant_events<R>(addr: AgentAddr, content: StreamedAssistantContent<R>) -> Vec<AgentEvent> {
    match content {
        StreamedAssistantContent::Text(text) => vec![AgentEvent::AssistantDelta {
            addr,
            delta: text.text,
        }],
        StreamedAssistantContent::Reasoning(reasoning) => vec![AgentEvent::ReasoningDelta {
            addr,
            delta: reasoning.display_text().to_string(),
        }],
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            vec![AgentEvent::ReasoningDelta {
                addr,
                delta: reasoning,
            }]
        }
        // Tool calls/results are emitted through UiPromptHook so this stream
        // mapper only handles assistant-facing content.
        StreamedAssistantContent::ToolCall { .. } => vec![],
        // ToolCallDelta and the provider-specific Final(R) carry no UI update.
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure stream-item → UI-event translation. The address
    //! is threaded through unchanged, and provider-only items yield nothing.
    use super::*;
    use rig_core::message::{ToolCall, ToolFunction};
    use serde_json::json;
    use tokio::sync::mpsc;

    #[derive(Debug)]
    struct StaticError(&'static str);

    impl std::fmt::Display for StaticError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for StaticError {}

    #[test]
    fn retry_backoffs_use_fixed_slow_schedule() {
        assert_eq!(PROMPT_RETRY_BACKOFFS[0], Duration::from_secs(1));
        assert_eq!(PROMPT_RETRY_BACKOFFS[1], Duration::from_secs(3));
        assert_eq!(PROMPT_RETRY_BACKOFFS[2], Duration::from_secs(10));
        assert_eq!(PROMPT_RETRY_BACKOFFS[3], Duration::from_secs(30));
    }

    #[test]
    fn retry_backoff_follows_schedule_then_gives_up() {
        let transient = StaticError("CompletionError: HttpError: connection reset by peer");
        let deterministic = StaticError("ToolSetError: ToolCallError: old_text was not found");

        // A retryable error within budget waits the scheduled backoff...
        assert_eq!(retry_backoff(1, &transient), Some(Duration::from_secs(1)));
        assert_eq!(retry_backoff(4, &transient), Some(Duration::from_secs(30)));
        // ...the last attempt has no budget left, and deterministic errors never
        // retry — both give up.
        assert_eq!(retry_backoff(PROMPT_RETRY_ATTEMPTS, &transient), None);
        assert_eq!(retry_backoff(1, &deterministic), None);
    }

    #[test]
    fn retry_classifier_allows_transient_network_errors() {
        assert!(is_retryable_prompt_error(&StaticError(
            "CompletionError: HttpError: connection reset by peer"
        )));
        assert!(is_retryable_prompt_error(&StaticError(
            "CompletionError: ProviderError: Got error status code trying to send a request to Ollama: 502 Bad Gateway"
        )));
    }

    #[test]
    fn retry_classifier_rejects_deterministic_errors() {
        assert!(!is_retryable_prompt_error(&StaticError(
            "ToolSetError: ToolCallError: old_text was not found"
        )));
        assert!(!is_retryable_prompt_error(&StaticError(
            "CompletionError: JsonError: expected value"
        )));
    }

    #[test]
    fn assistant_text_maps_to_assistant_delta() {
        let events = assistant_events(AgentAddr::Main, StreamedAssistantContent::<()>::text("hi"));
        match events.as_slice() {
            [
                AgentEvent::AssistantDelta {
                    addr: AgentAddr::Main,
                    delta,
                },
            ] => {
                assert_eq!(delta, "hi");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn assistant_reasoning_delta_maps_and_preserves_address() {
        let content = StreamedAssistantContent::<()>::ReasoningDelta {
            id: None,
            reasoning: "thinking".into(),
        };
        let events = assistant_events(AgentAddr::Runtime(7), content);
        match events.as_slice() {
            [
                AgentEvent::ReasoningDelta {
                    addr: AgentAddr::Runtime(7),
                    delta,
                },
            ] => {
                assert_eq!(delta, "thinking");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn assistant_tool_call_yields_no_stream_events() {
        let content = StreamedAssistantContent::<()>::ToolCall {
            tool_call: ToolCall::new(
                "call-1".into(),
                ToolFunction::new("read_file".into(), json!({ "path": "/x" })),
            ),
            internal_call_id: "ic".into(),
        };

        assert!(assistant_events(AgentAddr::Main, content).is_empty());
    }

    #[tokio::test]
    async fn hook_tool_call_maps_to_tool_call_event() {
        let (sender, mut receiver) = mpsc::channel(8);
        let hook = UiPromptHook::new(AgentEventSink::new(sender, 11), AgentAddr::Main);

        let action = hook
            .emit_tool_call_event("read_file", "ic", r#"{"path":"/x"}"#)
            .await;

        assert_eq!(action, ToolCallHookAction::Continue);
        match receiver.try_recv() {
            Ok(RuntimeEvent::Agent(
                11,
                AgentEvent::ToolCall {
                    addr: AgentAddr::Main,
                    id,
                    name,
                    arguments,
                },
            )) => {
                assert_eq!(id, "ic");
                assert_eq!(name, "read_file");
                assert_eq!(arguments, json!({ "path": "/x" }));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn assistant_provider_final_yields_no_events() {
        assert!(
            assistant_events(AgentAddr::Main, StreamedAssistantContent::<()>::Final(())).is_empty()
        );
    }

    #[tokio::test]
    async fn hook_tool_result_maps_to_tool_result_event() {
        let (sender, mut receiver) = mpsc::channel(8);
        let hook = UiPromptHook::new(AgentEventSink::new(sender, 11), AgentAddr::Runtime(3));

        let action = hook.emit_tool_result_event("ic", "done").await;

        assert_eq!(action, HookAction::Continue);
        match receiver.try_recv() {
            Ok(RuntimeEvent::Agent(
                11,
                AgentEvent::ToolResult {
                    addr: AgentAddr::Runtime(3),
                    id,
                    content,
                },
            )) => {
                assert_eq!(id, "ic");
                assert_eq!(content, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
