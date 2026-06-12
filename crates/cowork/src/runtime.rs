//! Adapter that runs a top-level prompt through the new in-workspace agent
//! stack (`agent::AgentRuntime` + `ollama::OllamaProvider` + `llm` memory/tools)
//! and forwards its generic stream events into the app's `ThreadEvent` tree.
//!
//! This owns prompt execution for the TUI: top-level agent plus recursive
//! subagents, deterministic filesystem/PDF tools, and profile-driven permission
//! prompts.

use std::{borrow::Cow, sync::Arc, time::Duration};

use async_trait::async_trait;
use llm::{
    ChatMessage, ConversationMemory, ConversationStore, Tool, ToolCall, ToolError, ToolOutput,
    ToolRegistry, parse_args,
};
use ollama::OllamaProvider;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    app::{
        AgentAddr, AgentCompletion, AgentControl, AgentDepth, AgentNodeEvent, RuntimeAgentKey,
        ThreadEvent, ThreadId, ToolPermissionResponse,
    },
    config::{
        AGENT_MAX_TURNS, MAIN_AGENT_PREAMBLE, MODEL, PROMPT_RETRY_ATTEMPTS, PROMPT_RETRY_BACKOFFS,
    },
    events::ThreadEventSink,
    permissions::{AgentProfile, ToolPermissionMode},
    tui::RuntimeEventSender,
};

use agent::{
    AgentConfig, AgentRunError, AgentRuntime, AgentStreamEvent as CoreAgentEvent, EventSink,
    ToolPermission, ToolPermissionPolicy,
};

const MAX_AGENT_DEPTH: AgentDepth = 4;

/// Bridges the generic `agent::EventSink` to the app's runtime channel. Core
/// stream events are translated to addressed `app::AgentNodeEvent`s and forwarded
/// as `ThreadEvent::Agent`.
pub(crate) struct AgentNodeEventSink {
    inner: ThreadEventSink,
    addr: AgentAddr,
}

impl AgentNodeEventSink {
    fn new(events: RuntimeEventSender, thread_id: ThreadId, addr: AgentAddr) -> Self {
        Self {
            inner: ThreadEventSink::new(events, thread_id),
            addr,
        }
    }

    /// A thread-level sender for events that are not core stream events, such as
    /// permission requests or run lifecycle transitions.
    fn thread_sink(&self) -> ThreadEventSink {
        self.inner.clone()
    }

    /// Rewind this agent's visible transcript to its pre-run baseline. Called
    /// before a retry so the next attempt's output does not stack on top of the
    /// failed attempt's partial output. Model memory is reset separately.
    async fn reset(&mut self) {
        self.send(AgentNodeEvent::Reset).await;
    }

    async fn send(&mut self, event: AgentNodeEvent) {
        self.inner
            .send(ThreadEvent::Agent {
                addr: self.addr,
                event,
            })
            .await;
    }
}

#[async_trait]
impl EventSink for AgentNodeEventSink {
    async fn emit(&mut self, event: CoreAgentEvent) {
        if let Some(app_event) = map_event(event, self.addr) {
            self.inner.send(app_event).await;
        }
    }
}

/// Translate a core stream event into a thread event addressed to one visible
/// agent node. Pure (no I/O) so the mapping is unit-tested directly. Provider,
/// queue, and incremental tool-call notices have no distinct UI today.
fn map_event(event: CoreAgentEvent, addr: AgentAddr) -> Option<ThreadEvent> {
    let event = match event {
        CoreAgentEvent::AssistantDelta { delta } => AgentNodeEvent::AssistantDelta { delta },
        CoreAgentEvent::ReasoningDelta { delta } => AgentNodeEvent::ReasoningDelta { delta },
        CoreAgentEvent::UserMessageInjected { text } => AgentNodeEvent::UserMessage { text },
        CoreAgentEvent::ToolCallFinished(call) => AgentNodeEvent::ToolCall {
            id: call.id,
            name: call.name,
            arguments: call.arguments,
        },
        CoreAgentEvent::ToolResult { call, output } => AgentNodeEvent::ToolResult {
            id: call.id,
            content: output.content,
            is_error: output.is_error,
        },
        // A call whose arguments failed to parse still gets a tool-call card so
        // the run loop's follow-up error result (matched by id) has something to
        // mark failed, rather than dangling as a bare result.
        CoreAgentEvent::ToolCallArgumentParseError(error) => AgentNodeEvent::ToolCall {
            id: error.id,
            name: error.name,
            arguments: serde_json::json!({
                "raw_arguments": error.raw_arguments,
                "parse_error": error.error,
            }),
        },
        CoreAgentEvent::Usage(usage) => AgentNodeEvent::Usage(usage),
        CoreAgentEvent::Queued { .. }
        | CoreAgentEvent::ProviderStarted
        | CoreAgentEvent::ToolCallStarted { .. }
        | CoreAgentEvent::ToolCallArgumentDelta { .. } => return None,
    };

    Some(ThreadEvent::Agent { addr, event })
}

/// Applies the active [`AgentProfile`] before each tool call: read/delegate tools
/// run, profile-denied tools are refused without a prompt, and the rest route
/// through the app's permission UI.
struct UiPermissionPolicy {
    /// Cloned per request; `request_tool_permission` needs `&mut` on a sink.
    sink: ThreadEventSink,
    profile: AgentProfile,
    addr: AgentAddr,
}

impl UiPermissionPolicy {
    fn new(sink: ThreadEventSink, profile: AgentProfile, addr: AgentAddr) -> Self {
        Self {
            sink,
            profile,
            addr,
        }
    }
}

#[async_trait]
impl ToolPermissionPolicy for UiPermissionPolicy {
    async fn decide(&self, call: &ToolCall) -> ToolPermission {
        let mode = self.profile.permission_for_tool(&call.name);

        match mode {
            ToolPermissionMode::Allow => ToolPermission::Allow,
            ToolPermissionMode::Deny => ToolPermission::Deny {
                reason: format!(
                    "Tool call rejected by active profile: {} is not allowed",
                    call.name
                ),
            },
            ToolPermissionMode::Ask => {
                let mut sink = self.sink.clone();
                match sink
                    .request_tool_permission(
                        self.addr,
                        call.id.clone(),
                        call.name.clone(),
                        call.arguments.clone(),
                    )
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
                }
            }
        }
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
        tools.insert(agent_tools::terminal::Terminal),
    ];
    if let Some(context) = subagent_context {
        registrations.push(tools.insert(SubagentTool::new(context)));
    }

    for result in registrations {
        result.expect("built-in tool names are unique");
    }

    let mut config = AgentConfig::new(MODEL);
    config.preamble = Some(preamble.to_string());
    config.max_turns = AGENT_MAX_TURNS;

    AgentRuntime::new(provider, Arc::new(store), tools, config).with_permission(permission)
}

struct VisibleAgentRun {
    thread_id: ThreadId,
    addr: AgentAddr,
    prompt: String,
    conversation_id: String,
    preamble: &'static str,
    subagent_context: Option<SubagentContext>,
    queued_messages: Option<mpsc::UnboundedReceiver<String>>,
}

async fn run_visible_agent(
    mut spec: VisibleAgentRun,
    store: ConversationStore,
    events: RuntimeEventSender,
) -> Result<String, AgentRunError> {
    let mut sink = AgentNodeEventSink::new(events.clone(), spec.thread_id, spec.addr);
    let permission = Arc::new(UiPermissionPolicy::new(
        sink.thread_sink(),
        AgentProfile::from_env(),
        spec.addr,
    ));
    let runtime = build_runtime(
        store.clone(),
        permission,
        spec.preamble,
        spec.subagent_context,
    );

    run_prompt_with_retries(
        &spec.prompt,
        &spec.conversation_id,
        &store,
        &runtime,
        &mut sink,
        spec.queued_messages.as_mut(),
    )
    .await
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
    cancel: CancellationToken,
    queued_messages: mpsc::UnboundedReceiver<String>,
) {
    tokio::spawn(async move {
        let mut lifecycle = ThreadEventSink::new(events.clone(), thread_id);
        lifecycle.send(ThreadEvent::MainStarted).await;

        let run = VisibleAgentRun {
            thread_id,
            addr: AgentAddr::Main,
            prompt,
            conversation_id,
            preamble: MAIN_AGENT_PREAMBLE,
            subagent_context: Some(SubagentContext::root(
                thread_id,
                events.clone(),
                store.clone(),
            )),
            // The main agent now has a queue too: messages typed while it runs
            // are drained between turns, exactly like a subagent's queue.
            queued_messages: Some(queued_messages),
        };

        // Race the whole run (retries, sleeps, and the nested subagent tree)
        // against cancellation. Losing the race drops the run future, which
        // tears down every in-flight provider request and subagent beneath it.
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = run_visible_agent(run, store, events) => Some(result),
        };

        let terminal = match outcome {
            Some(Ok(_)) => ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::Finished(AgentCompletion::Done),
            },
            Some(Err(error)) => ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::Error(error.to_string()),
            },
            None => ThreadEvent::Cancelled,
        };
        lifecycle.send(terminal).await;
    });
}

async fn run_prompt_with_retries(
    prompt: &str,
    conversation_id: &str,
    store: &ConversationStore,
    runtime: &AgentRuntime,
    sink: &mut AgentNodeEventSink,
    mut queued_messages: Option<&mut mpsc::UnboundedReceiver<String>>,
) -> Result<String, AgentRunError> {
    let initial_history = store.load(conversation_id).await;

    for attempt in 1..=PROMPT_RETRY_ATTEMPTS {
        store
            .replace(conversation_id, initial_history.clone())
            .await;

        let error = match runtime
            .run_with_user_messages(conversation_id, prompt.to_string(), &mut *sink, || {
                drain_queued_messages(queued_messages.as_deref_mut())
            })
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

        // Model memory is rewound at the top of the next iteration; rewind the
        // visible transcript to match so the retry starts from a clean slate.
        sink.reset().await;
        tokio::time::sleep(backoff).await;
    }

    unreachable!("retry loop always returns")
}

fn drain_queued_messages(receiver: Option<&mut mpsc::UnboundedReceiver<String>>) -> Vec<String> {
    let Some(receiver) = receiver else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    while let Ok(message) = receiver.try_recv() {
        messages.push(message);
    }
    messages
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

    /// Context handed to a freshly spawned child. From the child's perspective
    /// `parent_key` is *its own* runtime key — it becomes the parent of any
    /// grandchildren the child later spawns.
    fn child_context(&self, parent_key: RuntimeAgentKey, depth: AgentDepth) -> Self {
        Self {
            thread_id: self.thread_id,
            parent_key: Some(parent_key),
            depth,
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

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> Cow<'static, str> {
        "subagent".into()
    }

    fn description(&self) -> Cow<'static, str> {
        "Spawn a child agent to own one bounded, independent sub-task. Do not use it for simple one- or two-tool steps, single-file inspection, straightforward path reads/listing, or work that needs your continuous shared context. Give the child a clear goal, goal context, exact scope boundaries, known paths/resources, constraints, expected output shape, and failure policy. If a file/path/resource is missing, too large, inaccessible, ambiguous, or otherwise blocks the task, tell the child to stop and report the blocker rather than explore elsewhere or spawn recovery agents. Only delegate fan-out when there are multiple known independent chunks whose context can be discarded after a concise result.".into()
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
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

    async fn call(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let input: SubagentInput = parse_args(arguments)?;
        match run_child_agent(self.context.clone(), input.task, input.context).await {
            Ok(response) => Ok(ToolOutput::text(response)),
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

/// A random, effectively-collision-free id for a spawned agent (a UUIDv4 as a
/// `u128`). Random rather than a growing counter so persisted
/// `runtime-agent-{key}` conversation ids stay distinct across sessions once
/// persistence lands, instead of restarting at the same values every run.
fn next_runtime_agent_key() -> RuntimeAgentKey {
    RuntimeAgentKey::new(uuid::Uuid::new_v4().as_u128())
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

    let cancel = CancellationToken::new();
    let (queue_tx, queue_rx) = mpsc::unbounded_channel();

    let mut lifecycle =
        ThreadEventSink::new(parent_context.events.clone(), parent_context.thread_id);
    lifecycle
        .send(ThreadEvent::SpawnedSubagent {
            key,
            parent: parent_context.parent_key,
            depth: child_depth,
            task,
            context,
            control: AgentControl::new(cancel.clone(), queue_tx),
        })
        .await;

    let can_delegate = child_depth < MAX_AGENT_DEPTH;
    let run = VisibleAgentRun {
        thread_id: parent_context.thread_id,
        addr: child_addr,
        prompt,
        conversation_id: format!("runtime-agent-{key}"),
        preamble: subagent_preamble(can_delegate),
        subagent_context: can_delegate.then(|| parent_context.child_context(key, child_depth)),
        queued_messages: Some(queue_rx),
    };

    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("subagent cancelled by user".to_string()),
        result = run_visible_agent(
            run,
            parent_context.store.clone(),
            parent_context.events.clone(),
        ) => result
            .map_err(|error| format!("subagent failed: {error}"))
            .and_then(require_nested_response),
    };

    match &result {
        Ok(response) => {
            lifecycle
                .send(ThreadEvent::Agent {
                    addr: child_addr,
                    event: AgentNodeEvent::Finished(AgentCompletion::Returned(response.clone())),
                })
                .await;
        }
        Err(error) if error == "subagent cancelled by user" => {
            lifecycle
                .send(ThreadEvent::Agent {
                    addr: child_addr,
                    event: AgentNodeEvent::Cancelled(error.clone()),
                })
                .await;
        }
        Err(error) => {
            lifecycle
                .send(ThreadEvent::Agent {
                    addr: child_addr,
                    event: AgentNodeEvent::Error(error.clone()),
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
            UiPermissionPolicy::new(
                ThreadEventSink::new(sender, ThreadId::new(11)),
                profile,
                AgentAddr::Main,
            ),
            receiver,
        )
    }

    #[test]
    fn assistant_and_reasoning_map_to_main_deltas() {
        match map_event(
            CoreAgentEvent::AssistantDelta { delta: "hi".into() },
            AgentAddr::Main,
        ) {
            Some(ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::AssistantDelta { delta },
            }) => assert_eq!(delta, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
        match map_event(
            CoreAgentEvent::ReasoningDelta {
                delta: "think".into(),
            },
            AgentAddr::Main,
        ) {
            Some(ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ReasoningDelta { delta },
            }) => assert_eq!(delta, "think"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn events_can_target_runtime_agent_nodes() {
        match map_event(
            CoreAgentEvent::AssistantDelta {
                delta: "child".into(),
            },
            AgentAddr::Runtime(RuntimeAgentKey::new(42)),
        ) {
            Some(ThreadEvent::Agent {
                addr: AgentAddr::Runtime(key),
                event: AgentNodeEvent::AssistantDelta { delta },
            }) if key == RuntimeAgentKey::new(42) => assert_eq!(delta, "child"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn finished_tool_call_maps_to_tool_call_event() {
        match map_event(
            CoreAgentEvent::ToolCallFinished(tool_call()),
            AgentAddr::Main,
        ) {
            Some(ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event:
                    AgentNodeEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    },
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
        let event = CoreAgentEvent::ToolResult {
            call: tool_call(),
            output: ToolOutput::text("file contents"),
        };
        match map_event(event, AgentAddr::Main) {
            Some(ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event:
                    AgentNodeEvent::ToolResult {
                        id,
                        content,
                        is_error,
                    },
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn runtime_agent_keys_are_unique_and_non_sequential() {
        let keys: Vec<RuntimeAgentKey> = (0..100).map(|_| next_runtime_agent_key()).collect();
        let unique: std::collections::HashSet<_> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len(), "ids must not collide");
        // A growing counter would have produced a contiguous run; random ids
        // should not be 1, 2, 3, ...
        assert!(
            keys.windows(2)
                .any(|pair| pair[1] != pair[0].wrapping_add(1)),
            "ids should not be sequential"
        );
    }

    #[test]
    fn argument_parse_error_maps_to_a_tool_call_card() {
        let event = CoreAgentEvent::ToolCallArgumentParseError(ToolArgumentParseError {
            id: "call-1".into(),
            name: "read_file".into(),
            raw_arguments: "{bad".into(),
            error: "eof".into(),
        });
        match map_event(event, AgentAddr::Main) {
            Some(ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event:
                    AgentNodeEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    },
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "read_file");
                assert_eq!(arguments["raw_arguments"], "{bad");
                assert_eq!(arguments["parse_error"], "eof");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn usage_carries_token_counts() {
        let event = CoreAgentEvent::Usage(TokenUsage::from_input_output(10, 5));
        match map_event(event, AgentAddr::Main) {
            Some(ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::Usage(usage),
            }) => {
                assert_eq!(
                    (usage.input_tokens, usage.output_tokens, usage.total_tokens),
                    (10, 5, 15)
                );
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
    fn provider_and_incremental_tool_events_are_dropped() {
        let dropped = [
            CoreAgentEvent::Queued { position: 1 },
            CoreAgentEvent::ProviderStarted,
            CoreAgentEvent::ToolCallStarted {
                id: "call-1".into(),
                name: "read_file".into(),
            },
            CoreAgentEvent::ToolCallArgumentDelta {
                id: "call-1".into(),
                delta: "{".into(),
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
