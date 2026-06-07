use std::{future::Future, time::Duration};

use futures_util::StreamExt;
use rig_core::{
    agent::{
        HookAction, MultiTurnStreamItem, PromptHook, StreamingError, StreamingResult,
        ToolCallHookAction,
    },
    client::{CompletionClient, ProviderClient},
    completion::CompletionModel,
    memory::{ConversationMemory, InMemoryConversationMemory},
    message::{AssistantContent, Message as RigMessage},
    providers::ollama,
    streaming::{StreamedAssistantContent, StreamingPrompt},
};

use crate::{
    app::{AgentAddr, AgentEvent, ThreadId},
    tui::{RuntimeEvent, RuntimeEventSender},
};

/// Receives UI events produced while pumping an agent's stream. Implemented by
/// the channel-backed sinks in this binary and faked in tests, so the
/// stream→event translation can be exercised without a live model.
pub trait EventSink: Sync {
    fn emit(&self, event: AgentEvent) -> impl Future<Output = ()> + Send;
}

/// `EventSink` that forwards events to the TUI runtime channel for a thread.
#[derive(Clone)]
pub struct ChannelSink {
    events: RuntimeEventSender,
    thread_id: ThreadId,
}

impl ChannelSink {
    pub fn new(events: RuntimeEventSender, thread_id: ThreadId) -> Self {
        Self { events, thread_id }
    }
}

impl EventSink for ChannelSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self
            .events
            .send(RuntimeEvent::Agent(self.thread_id, event))
            .await;
    }
}

#[derive(Clone)]
pub(crate) struct UiPromptHook<S> {
    sink: S,
    addr: AgentAddr,
}

impl<S> UiPromptHook<S> {
    pub(crate) fn new(sink: S, addr: AgentAddr) -> Self {
        Self { sink, addr }
    }
}

impl<S> UiPromptHook<S>
where
    S: EventSink,
{
    async fn emit_tool_call_event(
        &self,
        tool_name: &str,
        internal_call_id: &str,
        args: &str,
    ) -> ToolCallHookAction {
        let arguments = serde_json::from_str(args)
            .unwrap_or_else(|_| serde_json::Value::String(args.to_string()));
        self.sink
            .emit(AgentEvent::ToolCall {
                addr: self.addr,
                id: internal_call_id.to_string(),
                name: tool_name.to_string(),
                arguments,
            })
            .await;
        ToolCallHookAction::cont()
    }

    async fn emit_tool_result_event(&self, internal_call_id: &str, result: &str) -> HookAction {
        self.sink
            .emit(AgentEvent::ToolResult {
                addr: self.addr,
                id: internal_call_id.to_string(),
                content: result.to_string(),
            })
            .await;
        HookAction::cont()
    }
}

impl<M, S> PromptHook<M> for UiPromptHook<S>
where
    M: CompletionModel,
    S: EventSink + Clone + Send + Sync,
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

pub const MODEL: &str = "gemma4:31b";
pub(crate) const TOOL_CONCURRENCY: usize = 8;
pub(crate) const PROMPT_RETRY_ATTEMPTS: usize = 5;
pub(crate) const PROMPT_RETRY_BACKOFFS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

pub const MAIN_AGENT_PREAMBLE: &str = "You are the top-level user-facing assistant with persistent conversation context. Use tools when relevant. \
     Do continuous work yourself when future prompts depend on your accumulated understanding, such as ongoing work in the same codebase or project. \
     For broad bounded one-off tasks with many independent chunks, prefer calling subagent instead of manually iterating every chunk yourself. \
     A first-level subagent owns the bounded task, maximizes parallelism by splitting independent context-heavy chunks into one worker call per chunk, and synthesizes their results. \
     Do not use subagent for simple one- or two-tool tasks or tasks needing continuous shared context. \
     When delegating, pass the full bounded task plus enough context, constraints, and paths for the subagent to plan; after it returns, synthesize its result into the final answer or next action.";

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
        let sink = ChannelSink::new(events.clone(), thread_id);
        sink.emit(AgentEvent::Started).await;

        if let Err(error) =
            run_prompt_with_retries(thread_id, prompt, conversation_id, memory, events).await
        {
            sink.emit(AgentEvent::Error {
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
    let sink = ChannelSink::new(events.clone(), thread_id);
    for attempt in 1..=PROMPT_RETRY_ATTEMPTS {
        match run_prompt_once(
            thread_id,
            prompt.clone(),
            conversation_id.clone(),
            memory.clone(),
            events.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error)
                if attempt < PROMPT_RETRY_ATTEMPTS && is_retryable_prompt_error(error.as_ref()) =>
            {
                let backoff = PROMPT_RETRY_BACKOFFS
                    .get(attempt - 1)
                    .copied()
                    .unwrap_or_else(|| {
                        *PROMPT_RETRY_BACKOFFS
                            .last()
                            .expect("non-empty backoff list")
                    });
                let message = format!(
                    "Attempt {attempt}/{PROMPT_RETRY_ATTEMPTS} failed with a transient error: {error}. Retrying in {}s…",
                    backoff.as_secs()
                );

                sink.emit(AgentEvent::Status {
                    addr: AgentAddr::Main,
                    content: message,
                })
                .await;
                tokio::time::sleep(backoff).await;
            }
            Err(error) => {
                remember_failed_prompt(&memory, &conversation_id, &prompt, error.as_ref()).await;
                return Err(error);
            }
        }
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
    let agent = client
        .agent(MODEL)
        .preamble(MAIN_AGENT_PREAMBLE)
        .additional_params(serde_json::json!({ "think": true }))
        .memory(memory)
        .tool(crate::tools::ReadFile)
        .tool(crate::tools::ListDirectory)
        .tool(crate::tools::EditFile)
        .tool(crate::tools::Subagent::new(
            crate::tools::ToolUiContext::new(thread_id, events.clone()),
        ))
        .hook(UiPromptHook::new(
            ChannelSink::new(events.clone(), thread_id),
            AgentAddr::Main,
        ))
        .default_max_turns(1000)
        .build();

    let mut stream = agent
        .stream_prompt(prompt)
        .conversation(&conversation_id)
        .with_tool_concurrency(TOOL_CONCURRENCY)
        .await;

    let sink = ChannelSink::new(events, thread_id);
    pump_stream(&sink, AgentAddr::Main, &mut stream).await?;

    sink.emit(AgentEvent::Finished {
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
    sink: &impl EventSink,
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
                    sink.emit(event).await;
                }
            }
            MultiTurnStreamItem::StreamUserItem(_) => {}
            MultiTurnStreamItem::CompletionCall(call) => {
                if let Some(usage) = call.usage {
                    sink.emit(AgentEvent::Usage {
                        addr,
                        content: format!(
                            "completion {} tokens: input={}, output={}, total={}",
                            call.call_index,
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.total_tokens
                        ),
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
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct StaticError(&'static str);

    impl std::fmt::Display for StaticError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for StaticError {}

    #[derive(Clone, Default)]
    struct RecordingSink {
        events: Arc<Mutex<Vec<AgentEvent>>>,
    }

    impl RecordingSink {
        fn events(&self) -> Vec<AgentEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl EventSink for RecordingSink {
        async fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn retry_backoffs_use_fixed_slow_schedule() {
        assert_eq!(PROMPT_RETRY_BACKOFFS[0], Duration::from_secs(1));
        assert_eq!(PROMPT_RETRY_BACKOFFS[1], Duration::from_secs(3));
        assert_eq!(PROMPT_RETRY_BACKOFFS[2], Duration::from_secs(10));
        assert_eq!(PROMPT_RETRY_BACKOFFS[3], Duration::from_secs(30));
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
        let sink = RecordingSink::default();
        let hook = UiPromptHook::new(sink.clone(), AgentAddr::Main);

        let action = hook
            .emit_tool_call_event("read_file", "ic", r#"{"path":"/x"}"#)
            .await;

        assert_eq!(action, ToolCallHookAction::Continue);
        match sink.events().as_slice() {
            [
                AgentEvent::ToolCall {
                    addr: AgentAddr::Main,
                    id,
                    name,
                    arguments,
                },
            ] => {
                assert_eq!(id, "ic");
                assert_eq!(name, "read_file");
                assert_eq!(arguments, &json!({ "path": "/x" }));
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
        let sink = RecordingSink::default();
        let hook = UiPromptHook::new(sink.clone(), AgentAddr::Runtime(3));

        let action = hook.emit_tool_result_event("ic", "done").await;

        assert_eq!(action, HookAction::Continue);
        match sink.events().as_slice() {
            [
                AgentEvent::ToolResult {
                    addr: AgentAddr::Runtime(3),
                    id,
                    content,
                },
            ] => {
                assert_eq!(id, "ic");
                assert_eq!(content, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
