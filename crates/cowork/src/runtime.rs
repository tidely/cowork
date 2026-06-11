//! Adapter that runs a top-level prompt through the new in-workspace agent
//! stack (`agent::AgentRuntime` + `ollama::OllamaProvider` + `llm` memory/tools)
//! and forwards its generic events into the app's `AgentEvent` tree.
//!
//! This owns prompt execution for the TUI: top-level agent plus recursive
//! subagents, deterministic filesystem/PDF tools, and profile-driven permission
//! prompts.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::future::BoxFuture;
use llm::{
    ChatMessage, ConversationMemory, ConversationStore, Tool, ToolCall, ToolError, ToolOutput,
    ToolRegistry, ToolSchemaFormat, parse_args,
};
use ollama::OllamaProvider;

use crate::{
    app::{AgentAddr, AgentDepth, AgentEvent, RuntimeAgentKey, ThreadId, ToolPermissionResponse},
    config::{
        AGENT_MAX_TURNS, MAIN_AGENT_PREAMBLE, MODEL, PROMPT_RETRY_ATTEMPTS, PROMPT_RETRY_BACKOFFS,
    },
    events::AgentEventSink,
    permissions::{AgentProfile, ToolPermissionMode},
    tui::RuntimeEventSender,
};

use agent::{
    AgentConfig, AgentEvent as RuntimeAgentEvent, AgentRunError, AgentRuntime, EventSink,
    ToolPermission, ToolPermissionPolicy,
};

const MAX_AGENT_DEPTH: AgentDepth = 4;

static NEXT_RUNTIME_AGENT_KEY: AtomicU64 = AtomicU64::new(1);

/// Bridges the generic `agent::EventSink` to the app's runtime channel. Generic
/// events are translated to `app::AgentEvent`s addressed to one agent node and
/// forwarded through the existing `AgentEventSink`.
pub(crate) struct ChannelEventSink {
    inner: AgentEventSink,
    addr: AgentAddr,
}

impl ChannelEventSink {
    fn new(events: RuntimeEventSender, thread_id: ThreadId, addr: AgentAddr) -> Self {
        Self {
            inner: AgentEventSink::new(events, thread_id),
            addr,
        }
    }

    /// A sink for the lifecycle events the spawner emits directly (Started, and
    /// the terminal Finished/Error derived from the run result).
    fn lifecycle(&self) -> AgentEventSink {
        self.inner.clone()
    }
}

impl EventSink for ChannelEventSink {
    fn emit(&mut self, event: RuntimeAgentEvent) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(app_event) = map_event(event, self.addr) {
                self.inner.send(app_event).await;
            }
        })
    }
}

/// Translate a generic runtime event into an app event addressed to one agent
/// node. Pure (no I/O) so the mapping is unit-tested directly.
///
/// Lifecycle events (`Started`/`Finished`/`Error`) return `None`: the spawner
/// owns those, emitting `Finished`/`Error` from the run's result so a provider
/// error that bubbles out of the run is reported exactly once. Provider/queue
/// notices and incremental tool-call deltas have no distinct UI today.
fn map_event(event: RuntimeAgentEvent, addr: AgentAddr) -> Option<AgentEvent> {
    match event {
        RuntimeAgentEvent::AssistantDelta { delta } => {
            Some(AgentEvent::AssistantDelta { addr, delta })
        }
        RuntimeAgentEvent::ReasoningDelta { delta } => {
            Some(AgentEvent::ReasoningDelta { addr, delta })
        }
        RuntimeAgentEvent::ToolCallFinished(call) => Some(AgentEvent::ToolCall {
            addr,
            id: call.id,
            name: call.name,
            arguments: call.arguments,
        }),
        RuntimeAgentEvent::ToolResult { call, output } => Some(AgentEvent::ToolResult {
            addr,
            id: call.id,
            content: output.content,
            is_error: output.is_error,
        }),
        RuntimeAgentEvent::Usage(usage) => Some(AgentEvent::Usage {
            addr,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        }),
        RuntimeAgentEvent::Started
        | RuntimeAgentEvent::Queued { .. }
        | RuntimeAgentEvent::ProviderStarted
        | RuntimeAgentEvent::ToolCallStarted { .. }
        | RuntimeAgentEvent::ToolCallArgumentDelta { .. }
        | RuntimeAgentEvent::ToolCallArgumentParseError(_)
        | RuntimeAgentEvent::Finished { .. }
        | RuntimeAgentEvent::Error { .. } => None,
    }
}

/// Applies the active [`AgentProfile`] before each tool call: read/delegate tools
/// run, profile-denied tools are refused without a prompt, and the rest route
/// through the app's permission UI.
struct UiPermissionPolicy {
    /// Cloned per request; `request_tool_permission` needs `&mut` on a sink.
    sink: AgentEventSink,
    profile: AgentProfile,
    addr: AgentAddr,
}

impl UiPermissionPolicy {
    fn new(sink: AgentEventSink, profile: AgentProfile, addr: AgentAddr) -> Self {
        Self {
            sink,
            profile,
            addr,
        }
    }
}

impl ToolPermissionPolicy for UiPermissionPolicy {
    fn decide(&self, call: &ToolCall) -> BoxFuture<'_, ToolPermission> {
        let id = call.id.clone();
        let name = call.name.clone();
        let arguments = call.arguments.clone();
        let mode = self.profile.permission_for_tool(&name);
        let mut sink = self.sink.clone();

        Box::pin(async move {
            match mode {
                ToolPermissionMode::Allow => ToolPermission::Allow,
                ToolPermissionMode::Deny => ToolPermission::Deny {
                    reason: format!("Tool call rejected by active profile: {name} is not allowed"),
                },
                ToolPermissionMode::Ask => match sink
                    .request_tool_permission(self.addr, id, name, arguments)
                    .await
                {
                    Some(ToolPermissionResponse::Allow | ToolPermissionResponse::AllowAlways) => {
                        ToolPermission::Allow
                    }
                    Some(ToolPermissionResponse::Reject { reason }) => {
                        ToolPermission::Deny { reason }
                    }
                    None => ToolPermission::Deny {
                        reason: "Tool call rejected because the permission UI is unavailable"
                            .into(),
                    },
                },
            }
        })
    }
}

/// Build an agent runtime backed by the shared model, preamble, deterministic
/// filesystem/PDF tools, optional recursive subagent tool, and the app's
/// permission UI.
fn build_runtime(
    store: ConversationStore,
    permission: Arc<dyn ToolPermissionPolicy>,
    preamble: &'static str,
    subagent_context: Option<SubagentContext>,
) -> AgentRuntime {
    let provider = Arc::new(OllamaProvider::from_env());

    let mut tools = ToolRegistry::new();
    let mut registrations = vec![
        tools.insert(agent_tools::fs::ReadFile),
        tools.insert(agent_tools::fs::ReadPdf),
        tools.insert(agent_tools::fs::ListDirectory),
        tools.insert(agent_tools::fs::EditFile),
        tools.insert(agent_tools::fs::WriteFile),
    ];
    if let Some(context) = subagent_context {
        registrations.push(tools.insert(SubagentTool::new(context)));
    }

    for result in registrations {
        result.expect("filesystem tool names are unique");
    }

    let mut config = AgentConfig::new(MODEL);
    config.preamble = Some(preamble.to_string());
    config.max_turns = AGENT_MAX_TURNS;

    AgentRuntime::new(provider, Arc::new(store), tools, config).with_permission(permission)
}

/// Run `prompt` in a background task, forwarding events to the UI for
/// `thread_id`. Emits `Started` up front and a terminal `Finished`/`Error` from
/// the run result.
pub(crate) fn spawn_prompt_task(
    thread_id: ThreadId,
    prompt: String,
    conversation_id: String,
    store: ConversationStore,
    events: RuntimeEventSender,
) {
    tokio::spawn(async move {
        let mut sink = ChannelEventSink::new(events.clone(), thread_id, AgentAddr::Main);
        let mut lifecycle = sink.lifecycle();
        lifecycle.send(AgentEvent::Started).await;

        let permission = Arc::new(UiPermissionPolicy::new(
            sink.lifecycle(),
            AgentProfile::from_env(),
            AgentAddr::Main,
        ));
        let runtime = build_runtime(
            store.clone(),
            permission,
            MAIN_AGENT_PREAMBLE,
            Some(SubagentContext::root(thread_id, events, store.clone())),
        );
        match run_prompt_with_retries(&prompt, &conversation_id, &store, &runtime, &mut sink).await
        {
            Ok(_) => {
                lifecycle
                    .send(AgentEvent::Finished {
                        addr: AgentAddr::Main,
                        result: None,
                    })
                    .await;
            }
            Err(error) => {
                lifecycle
                    .send(AgentEvent::Error {
                        addr: AgentAddr::Main,
                        error: error.to_string(),
                    })
                    .await;
            }
        }
    });
}

async fn run_prompt_with_retries(
    prompt: &str,
    conversation_id: &str,
    store: &ConversationStore,
    runtime: &AgentRuntime,
    events: &mut impl EventSink,
) -> Result<String, AgentRunError> {
    let initial_history = store.load(conversation_id).await;

    for attempt in 1..=PROMPT_RETRY_ATTEMPTS {
        store
            .replace(conversation_id, initial_history.clone())
            .await;

        let error = match runtime
            .run(conversation_id, prompt.to_string(), events)
            .await
        {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };

        let Some(backoff) = retry_backoff(attempt, &error) else {
            store.replace(conversation_id, initial_history).await;
            remember_failed_prompt(store, conversation_id, prompt, &error).await;
            return Err(error);
        };

        tokio::time::sleep(backoff).await;
    }

    unreachable!("retry loop always returns")
}

fn retry_backoff(attempt: usize, error: &AgentRunError) -> Option<Duration> {
    if is_retryable_agent_error(error) {
        PROMPT_RETRY_BACKOFFS.get(attempt - 1).copied()
    } else {
        None
    }
}

fn is_retryable_agent_error(error: &AgentRunError) -> bool {
    matches!(error, AgentRunError::Llm(error) if error.is_retryable())
}

async fn remember_failed_prompt(
    store: &ConversationStore,
    conversation_id: &str,
    prompt: &str,
    error: &AgentRunError,
) {
    store
        .append(
            conversation_id,
            vec![
                ChatMessage::user(prompt),
                ChatMessage::assistant(format!(
                    "The previous attempt failed before a final response was produced: {error}"
                )),
            ],
        )
        .await;
}

#[derive(Clone)]
struct SubagentContext {
    thread_id: ThreadId,
    parent_key: Option<RuntimeAgentKey>,
    depth: AgentDepth,
    events: RuntimeEventSender,
    store: ConversationStore,
}

impl SubagentContext {
    fn root(thread_id: ThreadId, events: RuntimeEventSender, store: ConversationStore) -> Self {
        Self {
            thread_id,
            parent_key: None,
            depth: 0,
            events,
            store,
        }
    }

    fn child_context(&self, child_key: RuntimeAgentKey, child_depth: AgentDepth) -> Self {
        Self {
            thread_id: self.thread_id,
            parent_key: Some(child_key),
            depth: child_depth,
            events: self.events.clone(),
            store: self.store.clone(),
        }
    }
}

#[derive(Clone)]
struct SubagentTool {
    context: SubagentContext,
}

impl SubagentTool {
    fn new(context: SubagentContext) -> Self {
        Self { context }
    }
}

#[derive(serde::Deserialize)]
struct SubagentInput {
    task: String,
    context: Option<String>,
}

impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn description(&self) -> &'static str {
        "Spawn a child agent to own one bounded, independent sub-task. Do not use it for simple one- or two-tool steps, single-file inspection, straightforward path reads/listing, or work that needs your continuous shared context. Give the child a clear goal, goal context, exact scope boundaries, known paths/resources, constraints, expected output shape, and failure policy. If a file/path/resource is missing, too large, inaccessible, ambiguous, or otherwise blocks the task, tell the child to stop and report the blocker rather than explore elsewhere or spawn recovery agents. Only delegate fan-out when there are multiple known independent chunks whose context can be discarded after a concise result."
    }

    fn parameters_schema(&self, _format: ToolSchemaFormat) -> Result<serde_json::Value, ToolError> {
        Ok(serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The bounded, independent sub-task for this child agent to own. Include the goal, expected output, and exact scope boundaries. Make it narrow enough that the child can finish it and return a concise result whose context can then be discarded."
                },
                "context": {
                    "type": "string",
                    "description": "Slice-specific context only: why the goal matters, known paths/resources, constraints, relevant prior findings, and what to do if a path/resource is missing, too large, inaccessible, ambiguous, or otherwise blocks the task. Prefer telling the child to stop and report the blocker rather than explore outside scope."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }))
    }

    fn call(&self, arguments: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let input: SubagentInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let context = self.context.clone();

        Box::pin(async move {
            match run_child_agent(context, input.task, input.context).await {
                Ok(response) => Ok(ToolOutput::text(response)),
                Err(error) => Ok(ToolOutput::error(error)),
            }
        })
    }
}

fn next_runtime_agent_key() -> RuntimeAgentKey {
    NEXT_RUNTIME_AGENT_KEY.fetch_add(1, Ordering::Relaxed)
}

fn subagent_preamble(can_delegate: bool) -> &'static str {
    if can_delegate {
        include_str!("../prompts/subagent-delegating.md")
    } else {
        include_str!("../prompts/subagent-leaf.md")
    }
}

fn subagent_prompt(task: &str, context: Option<&str>) -> String {
    let mut prompt = String::new();
    if let Some(context) = context.map(str::trim).filter(|context| !context.is_empty()) {
        prompt.push_str("Context:\n");
        prompt.push_str(context);
        prompt.push_str("\n\n");
    }
    prompt.push_str("Task:\n");
    prompt.push_str(task.trim());
    prompt
}

fn require_nested_response(response: String) -> Result<String, String> {
    if response.trim().is_empty() {
        Err("subagent produced no final response; retry with a narrower task".into())
    } else {
        Ok(response)
    }
}

async fn run_child_agent(
    parent_context: SubagentContext,
    task: String,
    context: Option<String>,
) -> Result<String, String> {
    let key = next_runtime_agent_key();
    let child_depth = parent_context.depth + 1;
    let child_addr = AgentAddr::Runtime(key);
    let prompt = subagent_prompt(&task, context.as_deref());

    let mut lifecycle =
        AgentEventSink::new(parent_context.events.clone(), parent_context.thread_id);
    lifecycle
        .send(AgentEvent::Spawned {
            key,
            parent: parent_context.parent_key,
            depth: child_depth,
            task,
            context,
        })
        .await;

    let can_delegate = child_depth < MAX_AGENT_DEPTH;
    let permission = Arc::new(UiPermissionPolicy::new(
        lifecycle.clone(),
        AgentProfile::from_env(),
        child_addr,
    ));
    let runtime = build_runtime(
        parent_context.store.clone(),
        permission,
        subagent_preamble(can_delegate),
        can_delegate.then(|| parent_context.child_context(key, child_depth)),
    );
    let mut sink = ChannelEventSink::new(
        parent_context.events.clone(),
        parent_context.thread_id,
        child_addr,
    );
    let conversation_id = format!("runtime-agent-{key}");

    let result = run_prompt_with_retries(
        &prompt,
        &conversation_id,
        &parent_context.store,
        &runtime,
        &mut sink,
    )
    .await
    .map_err(|error| format!("subagent failed: {error}"))
    .and_then(require_nested_response);

    match &result {
        Ok(response) => {
            lifecycle
                .send(AgentEvent::Finished {
                    addr: child_addr,
                    result: Some(response.clone()),
                })
                .await;
        }
        Err(error) => {
            lifecycle
                .send(AgentEvent::Error {
                    addr: child_addr,
                    error: error.clone(),
                })
                .await;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    //! Pin the generic-event → app-event translation. The address is always the
    //! main agent, content/usage carry through, and lifecycle/provider notices
    //! are dropped because the spawner owns them.
    use super::*;
    use crate::tui::RuntimeEvent;
    use llm::{LlmError, TokenUsage, ToolArgumentParseError, ToolCall, ToolError, ToolOutput};
    use serde_json::json;
    use tokio::sync::mpsc;

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            raw_arguments: r#"{"path":"/x"}"#.into(),
            arguments: json!({ "path": "/x" }),
        }
    }

    fn call_named(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: name.into(),
            raw_arguments: "{}".into(),
            arguments: json!({}),
        }
    }

    fn policy(profile: AgentProfile) -> (UiPermissionPolicy, mpsc::Receiver<RuntimeEvent>) {
        let (sender, receiver) = mpsc::channel(8);
        (
            UiPermissionPolicy::new(AgentEventSink::new(sender, 11), profile, AgentAddr::Main),
            receiver,
        )
    }

    #[test]
    fn assistant_and_reasoning_map_to_main_deltas() {
        match map_event(
            RuntimeAgentEvent::AssistantDelta { delta: "hi".into() },
            AgentAddr::Main,
        ) {
            Some(AgentEvent::AssistantDelta {
                addr: AgentAddr::Main,
                delta,
            }) => assert_eq!(delta, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
        match map_event(
            RuntimeAgentEvent::ReasoningDelta {
                delta: "think".into(),
            },
            AgentAddr::Main,
        ) {
            Some(AgentEvent::ReasoningDelta {
                addr: AgentAddr::Main,
                delta,
            }) => assert_eq!(delta, "think"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn events_can_target_runtime_agent_nodes() {
        match map_event(
            RuntimeAgentEvent::AssistantDelta {
                delta: "child".into(),
            },
            AgentAddr::Runtime(42),
        ) {
            Some(AgentEvent::AssistantDelta {
                addr: AgentAddr::Runtime(42),
                delta,
            }) => assert_eq!(delta, "child"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn finished_tool_call_maps_to_tool_call_event() {
        match map_event(
            RuntimeAgentEvent::ToolCallFinished(tool_call()),
            AgentAddr::Main,
        ) {
            Some(AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id,
                name,
                arguments,
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "read_file");
                assert_eq!(arguments, json!({ "path": "/x" }));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_result_maps_to_call_id_and_content() {
        let event = RuntimeAgentEvent::ToolResult {
            call: tool_call(),
            output: ToolOutput::text("file contents"),
        };
        match map_event(event, AgentAddr::Main) {
            Some(AgentEvent::ToolResult {
                addr: AgentAddr::Main,
                id,
                content,
                is_error,
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn usage_carries_token_counts() {
        let event = RuntimeAgentEvent::Usage(TokenUsage::from_input_output(10, 5));
        match map_event(event, AgentAddr::Main) {
            Some(AgentEvent::Usage {
                addr: AgentAddr::Main,
                input_tokens,
                output_tokens,
                total_tokens,
            }) => {
                assert_eq!((input_tokens, output_tokens, total_tokens), (10, 5, 15));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn retry_backoff_uses_shared_schedule_for_retryable_llm_errors() {
        let transient = AgentRunError::Llm(LlmError::Transport("connection reset".into()));

        assert_eq!(retry_backoff(1, &transient), Some(Duration::from_secs(1)));
        assert_eq!(retry_backoff(4, &transient), Some(Duration::from_secs(30)));
        assert_eq!(retry_backoff(PROMPT_RETRY_ATTEMPTS, &transient), None);
    }

    #[test]
    fn retry_backoff_rejects_deterministic_agent_errors() {
        let decode = AgentRunError::Llm(LlmError::Decode("bad json".into()));
        let tool_setup = AgentRunError::ToolSetup(ToolError::UnknownTool("missing".into()));
        let max_turns = AgentRunError::MaxTurns { max_turns: 1 };

        assert_eq!(retry_backoff(1, &decode), None);
        assert_eq!(retry_backoff(1, &tool_setup), None);
        assert_eq!(retry_backoff(1, &max_turns), None);
    }

    #[tokio::test]
    async fn failed_prompt_note_is_recorded_in_conversation_memory() {
        let store = ConversationStore::new();
        store
            .append("conv", vec![ChatMessage::user("earlier")])
            .await;
        let error = AgentRunError::Llm(LlmError::Transport("network down".into()));

        remember_failed_prompt(&store, "conv", "try this", &error).await;

        let history = store.load("conv").await;
        assert_eq!(history.len(), 3);
        assert!(matches!(history[0], ChatMessage::User { ref content } if content == "earlier"));
        assert!(matches!(history[1], ChatMessage::User { ref content } if content == "try this"));
        assert!(
            matches!(history[2], ChatMessage::Assistant { ref content, .. } if content.contains("network down"))
        );
    }

    #[test]
    fn lifecycle_and_incremental_events_are_dropped() {
        let dropped = [
            RuntimeAgentEvent::Started,
            RuntimeAgentEvent::Queued { position: 1 },
            RuntimeAgentEvent::ProviderStarted,
            RuntimeAgentEvent::ToolCallStarted {
                id: "call-1".into(),
                name: "read_file".into(),
            },
            RuntimeAgentEvent::ToolCallArgumentDelta {
                id: "call-1".into(),
                delta: "{".into(),
            },
            RuntimeAgentEvent::ToolCallArgumentParseError(ToolArgumentParseError {
                id: "call-1".into(),
                name: "read_file".into(),
                raw_arguments: "{".into(),
                error: "eof".into(),
            }),
            RuntimeAgentEvent::Finished {
                response: "done".into(),
            },
            RuntimeAgentEvent::Error {
                error: "boom".into(),
            },
        ];
        for event in dropped {
            assert!(
                map_event(event.clone(), AgentAddr::Main).is_none(),
                "expected drop: {event:?}"
            );
        }
    }

    // ----- permission policy -----

    #[tokio::test]
    async fn read_tool_is_allowed_without_a_prompt() {
        let (policy, mut receiver) = policy(AgentProfile::ask_for_writes());

        assert_eq!(
            policy.decide(&call_named("read_file")).await,
            ToolPermission::Allow
        );
        assert!(receiver.try_recv().is_err(), "read tools are not prompted");
    }

    #[tokio::test]
    async fn write_tool_is_denied_under_read_only_without_a_prompt() {
        let (policy, mut receiver) = policy(AgentProfile::read_only());

        match policy.decide(&call_named("write_file")).await {
            ToolPermission::Deny { reason } => assert!(reason.contains("write_file")),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            receiver.try_recv().is_err(),
            "denied tools are not prompted"
        );
    }

    #[tokio::test]
    async fn write_tool_asks_and_allows_on_accept() {
        let (policy, mut receiver) = policy(AgentProfile::ask_for_writes());
        let pending = tokio::spawn(async move { policy.decide(&call_named("edit_file")).await });

        match receiver.recv().await {
            Some(RuntimeEvent::ToolPermissionRequest(request)) => {
                assert_eq!(request.name, "edit_file");
                request.respond(ToolPermissionResponse::Allow);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(pending.await.unwrap(), ToolPermission::Allow);
    }

    #[tokio::test]
    async fn write_tool_ask_reject_denies_with_user_reason() {
        let (policy, mut receiver) = policy(AgentProfile::ask_for_writes());
        let pending = tokio::spawn(async move { policy.decide(&call_named("edit_file")).await });

        match receiver.recv().await {
            Some(RuntimeEvent::ToolPermissionRequest(request)) => {
                request.respond(ToolPermissionResponse::Reject {
                    reason: "not now".into(),
                });
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(
            pending.await.unwrap(),
            ToolPermission::Deny {
                reason: "not now".into()
            }
        );
    }
}
