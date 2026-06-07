use std::{
    convert::Infallible,
    fs, io,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use rig_core::{
    agent::{StreamingError, StreamingResult},
    client::{CompletionClient, ProviderClient},
    completion::ToolDefinition,
    providers::ollama,
    schemars::{self, JsonSchema},
    streaming::StreamingPrompt,
    tool::{Tool, ToolEmbedding, ToolError},
};
use rig_derive::rig_tool;

use crate::{
    agent::{EventSink, pump_stream},
    app::{AgentAddr, AgentDepth, AgentEvent, RuntimeAgentKey, ThreadId},
    tui::{RuntimeEvent, RuntimeEventSender},
};

const MAX_READ_BYTES: u64 = 512 * 1024;
const SUBAGENT_MODEL: &str = "gemma4:31b";
const SUBAGENT_MAX_TURNS: usize = 1000;
const WORKER_AGENT_MAX_TURNS: usize = 1000;

static NEXT_RUNTIME_AGENT_KEY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ToolUiContext {
    thread_id: ThreadId,
    parent_key: Option<RuntimeAgentKey>,
    events: RuntimeEventSender,
}

impl ToolUiContext {
    pub fn new(thread_id: ThreadId, events: RuntimeEventSender) -> Self {
        Self {
            thread_id,
            parent_key: None,
            events,
        }
    }

    fn with_parent_key(&self, parent_key: RuntimeAgentKey) -> Self {
        Self {
            thread_id: self.thread_id,
            parent_key: Some(parent_key),
            events: self.events.clone(),
        }
    }
}

fn next_runtime_agent_key() -> RuntimeAgentKey {
    NEXT_RUNTIME_AGENT_KEY.fetch_add(1, Ordering::Relaxed)
}

impl EventSink for ToolUiContext {
    async fn emit(&self, event: AgentEvent) {
        let _ = self
            .events
            .send(RuntimeEvent::Agent(self.thread_id, event))
            .await;
    }
}

#[derive(Clone, Copy)]
enum SubagentDepth {
    FirstLevel,
    Worker,
}

impl SubagentDepth {
    fn label(self) -> &'static str {
        match self {
            SubagentDepth::FirstLevel => "subagent",
            SubagentDepth::Worker => "worker_agent",
        }
    }

    fn ui_depth(self) -> AgentDepth {
        match self {
            SubagentDepth::FirstLevel => AgentDepth::Subagent,
            SubagentDepth::Worker => AgentDepth::Worker,
        }
    }

    fn max_turns(self) -> usize {
        match self {
            SubagentDepth::FirstLevel => SUBAGENT_MAX_TURNS,
            SubagentDepth::Worker => WORKER_AGENT_MAX_TURNS,
        }
    }

    fn preamble(self) -> &'static str {
        match self {
            SubagentDepth::FirstLevel => {
                "You are a first-level task agent spawned by a top-level assistant. Own the delegated bounded task. \
                 Maximize parallelism: when there are many independent context-heavy chunks, issue separate worker_agent calls for each chunk instead of inspecting chunks yourself. \
                 Use one worker per independent chunk and run as many worker calls in parallel as possible; do not batch multiple independent chunks into one worker. \
                 Examples of chunks are one repository, one document, one subsystem, one account/resource, or one comparison item; the examples are not task-specific rules. \
                 Do not send the entire task, a long global list, or a batch of unrelated chunks to one worker. Give each worker only its slice-specific context. \
                 If the task requires continuous shared context rather than independent chunks, do the work yourself instead of spawning workers. \
                 Use tools when needed, do not modify files, and return a concise result useful to the top-level assistant."
            }
            SubagentDepth::Worker => {
                "You are a lowest-level worker agent. Do one independent context-heavy chunk and return a concise result. \
                 Use tools when needed. Do not modify files. \
                 If the task cannot be handled independently because it needs continuous shared context, say so briefly."
            }
        }
    }
}

fn tool_error(message: impl Into<String>) -> ToolError {
    ToolError::ToolCallError(Box::new(io::Error::other(message.into())))
}

fn resolve_path(path: &str) -> Result<PathBuf, ToolError> {
    let path = path.trim();
    let path = if path == "~" {
        std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| tool_error("HOME environment variable is not set"))?
    } else if let Some(rest) = path.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .map_err(|_| tool_error("HOME environment variable is not set"))?
    } else {
        PathBuf::from(path)
    };

    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| ToolError::ToolCallError(Box::new(error)))
    }
}

/// Read a UTF-8 text file from disk.
#[rig_tool]
pub fn read_file(
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    path: String,
) -> Result<String, ToolError> {
    let path = resolve_path(&path)?;
    let metadata =
        fs::metadata(&path).map_err(|error| ToolError::ToolCallError(Box::new(error)))?;

    if !metadata.is_file() {
        return Err(tool_error(format!("{} is not a file", path.display())));
    }

    if metadata.len() > MAX_READ_BYTES {
        return Err(tool_error(format!(
            "{} is too large to read ({} bytes, max {MAX_READ_BYTES})",
            path.display(),
            metadata.len()
        )));
    }

    fs::read_to_string(&path).map_err(|error| ToolError::ToolCallError(Box::new(error)))
}

impl ToolEmbedding for ReadFile {
    type InitError = Infallible;
    type Context = ();
    type State = ();

    fn embedding_docs(&self) -> Vec<String> {
        vec![
            "Read a local text file from disk by path.".to_string(),
            "Use when the user asks to inspect, open, view, summarize, or understand file contents."
                .to_string(),
        ]
    }

    fn context(&self) -> Self::Context {}

    fn init(_state: Self::State, _context: Self::Context) -> Result<Self, Self::InitError> {
        Ok(ReadFile)
    }
}

/// List files and directories inside a directory.
#[rig_tool]
pub fn list_directory(
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    path: String,
) -> Result<String, ToolError> {
    let path = resolve_path(&path)?;
    let mut entries = fs::read_dir(&path)
        .map_err(|error| ToolError::ToolCallError(Box::new(error)))?
        .map(|entry| {
            let entry = entry.map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
            let metadata = entry
                .metadata()
                .map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
            let kind = if metadata.is_dir() {
                "dir"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };

            Ok((
                entry.file_name().to_string_lossy().to_string(),
                kind.to_string(),
                metadata.len(),
            ))
        })
        .collect::<Result<Vec<_>, ToolError>>()?;

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    if entries.is_empty() {
        return Ok(format!("{} is empty", path.display()));
    }

    let output = entries
        .into_iter()
        .map(|(name, kind, size)| match kind.as_str() {
            "dir" => format!("{name}/"),
            "file" => format!("{name} ({size} bytes)"),
            _ => format!("{name} ({kind})"),
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(output)
}

impl ToolEmbedding for ListDirectory {
    type InitError = Infallible;
    type Context = ();
    type State = ();

    fn embedding_docs(&self) -> Vec<String> {
        vec![
            "List files and directories in a local directory by path.".to_string(),
            "Use when the user asks to browse, inspect, explore, or see the contents of a folder or directory."
                .to_string(),
        ]
    }

    fn context(&self) -> Self::Context {}

    fn init(_state: Self::State, _context: Self::Context) -> Result<Self, Self::InitError> {
        Ok(ListDirectory)
    }
}

/// Edit a text file by replacing one exact text span with another.
#[rig_tool]
pub fn edit_file(
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    path: String,
    /// Exact text to find. The edit fails unless this text appears exactly once.
    old_text: String,
    /// Replacement text.
    new_text: String,
) -> Result<String, ToolError> {
    if old_text.is_empty() {
        return Err(tool_error("old_text must not be empty"));
    }

    let path = resolve_path(&path)?;
    let original =
        fs::read_to_string(&path).map_err(|error| ToolError::ToolCallError(Box::new(error)))?;

    let matches = original.match_indices(&old_text).count();
    match matches {
        0 => Err(tool_error("old_text was not found in the file")),
        1 => {
            let updated = original.replacen(&old_text, &new_text, 1);
            fs::write(&path, updated).map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
            Ok(format!("Edited {}", path.display()))
        }
        count => Err(tool_error(format!(
            "old_text appears {count} times; provide a more specific span"
        ))),
    }
}

impl ToolEmbedding for EditFile {
    type InitError = Infallible;
    type Context = ();
    type State = ();

    fn embedding_docs(&self) -> Vec<String> {
        vec![
            "Edit a local text file by replacing exact old text with new text.".to_string(),
            "Use when the user asks to modify, patch, update, rewrite, or fix file contents."
                .to_string(),
        ]
    }

    fn context(&self) -> Self::Context {}

    fn init(_state: Self::State, _context: Self::Context) -> Result<Self, Self::InitError> {
        Ok(EditFile)
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

/// Pump a subagent's stream while preserving stream error type so callers can
/// distinguish transient network/provider failures from deterministic tool errors.
async fn run_nested_stream<R>(
    ui_context: &ToolUiContext,
    key: RuntimeAgentKey,
    stream: &mut StreamingResult<R>,
) -> Result<String, StreamingError> {
    pump_stream(ui_context, AgentAddr::Runtime(key), stream).await
}

fn require_nested_response(response: String) -> Result<String, ToolError> {
    if response.trim().is_empty() {
        Err(tool_error(
            "subagent produced no final response; retry with a narrower task",
        ))
    } else {
        Ok(response)
    }
}

async fn run_nested_agent_with_retries(
    client: &ollama::Client,
    ui_context: &ToolUiContext,
    key: RuntimeAgentKey,
    depth: SubagentDepth,
    prompt: String,
) -> Result<String, ToolError> {
    let max_attempts = crate::agent::prompt_retry_attempts();

    for attempt in 1..=max_attempts {
        let attempt_result = match depth {
            SubagentDepth::FirstLevel => {
                let agent = client
                    .agent(SUBAGENT_MODEL)
                    .preamble(depth.preamble())
                    .additional_params(serde_json::json!({ "think": true }))
                    .tool(ReadFile)
                    .tool(ListDirectory)
                    .tool(WorkerAgent::new(ui_context.with_parent_key(key)))
                    .default_max_turns(depth.max_turns())
                    .build();
                let mut stream = agent
                    .stream_prompt(prompt.clone())
                    .with_tool_concurrency(crate::agent::tool_concurrency())
                    .await;
                run_nested_stream(ui_context, key, &mut stream).await
            }
            SubagentDepth::Worker => {
                let agent = client
                    .agent(SUBAGENT_MODEL)
                    .preamble(depth.preamble())
                    .additional_params(serde_json::json!({ "think": true }))
                    .tool(ReadFile)
                    .tool(ListDirectory)
                    .default_max_turns(depth.max_turns())
                    .build();
                let mut stream = agent
                    .stream_prompt(prompt.clone())
                    .with_tool_concurrency(crate::agent::tool_concurrency())
                    .await;
                run_nested_stream(ui_context, key, &mut stream).await
            }
        };

        match attempt_result {
            Ok(response) => return require_nested_response(response),
            Err(error)
                if attempt < max_attempts && crate::agent::is_retryable_prompt_error(&error) =>
            {
                let backoff = crate::agent::prompt_retry_backoff(attempt);
                let message = format!(
                    "{} attempt {attempt}/{max_attempts} failed with a transient error: {error}. Retrying in {}s…",
                    depth.label(),
                    backoff.as_secs()
                );
                crate::debug_log::event(
                    "nested_agent_retry_scheduled",
                    [
                        ("kind", depth.label().to_string()),
                        ("attempt", attempt.to_string()),
                        ("max_attempts", max_attempts.to_string()),
                        ("backoff_secs", backoff.as_secs().to_string()),
                        ("error", error.to_string()),
                    ],
                );
                ui_context
                    .emit(AgentEvent::Status {
                        addr: AgentAddr::Runtime(key),
                        content: message,
                    })
                    .await;
                tokio::time::sleep(backoff).await;
            }
            Err(error) => return Err(tool_error(format!("subagent failed: {error}"))),
        }
    }

    unreachable!("retry loop always returns")
}

async fn run_subagent_at_depth(
    ui_context: ToolUiContext,
    depth: SubagentDepth,
    task: String,
    context: Option<String>,
) -> Result<String, ToolError> {
    let key = next_runtime_agent_key();
    ui_context
        .emit(AgentEvent::Spawned {
            key,
            parent: ui_context.parent_key,
            depth: depth.ui_depth(),
            task: task.clone(),
            context: context.clone(),
        })
        .await;

    crate::debug_log::event(
        "nested_agent_started",
        [
            ("kind", depth.label().to_string()),
            ("task_chars", task.chars().count().to_string()),
            (
                "context_chars",
                context
                    .as_deref()
                    .map(str::chars)
                    .map(Iterator::count)
                    .unwrap_or_default()
                    .to_string(),
            ),
            (
                "tool_concurrency",
                crate::agent::tool_concurrency().to_string(),
            ),
        ],
    );

    let client = ollama::Client::from_env()
        .map_err(|error| tool_error(format!("failed to create Ollama client: {error}")))?;
    let prompt = subagent_prompt(&task, context.as_deref());

    let result = run_nested_agent_with_retries(&client, &ui_context, key, depth, prompt).await;

    crate::debug_log::event(
        "nested_agent_finished",
        [
            ("kind", depth.label().to_string()),
            ("ok", result.is_ok().to_string()),
        ],
    );

    match &result {
        Ok(result) => {
            ui_context
                .emit(AgentEvent::Finished {
                    addr: AgentAddr::Runtime(key),
                    result: Some(result.clone()),
                })
                .await;
        }
        Err(error) => {
            ui_context
                .emit(AgentEvent::Error {
                    addr: AgentAddr::Runtime(key),
                    error: error.to_string(),
                })
                .await;
        }
    }

    result
}

#[derive(Clone)]
pub struct WorkerAgent {
    ui_context: ToolUiContext,
}

impl WorkerAgent {
    pub fn new(ui_context: ToolUiContext) -> Self {
        Self { ui_context }
    }
}

#[derive(Clone)]
pub struct Subagent {
    ui_context: ToolUiContext,
}

impl Subagent {
    pub fn new(ui_context: ToolUiContext) -> Self {
        Self { ui_context }
    }
}

#[derive(serde::Deserialize, JsonSchema)]
#[schemars(crate = "schemars")]
pub struct WorkerAgentParameters {
    /// The specific independent chunk this worker should complete. It should be narrow enough that the worker can finish it and return a concise result.
    task: String,
    /// Context, constraints, paths, service details, or prior findings needed for this chunk only. Do not include unrelated global task context.
    context: Option<String>,
}

#[derive(serde::Deserialize, JsonSchema)]
#[schemars(crate = "schemars")]
pub struct SubagentParameters {
    /// The bounded task for this first-level agent to own. This can be the full one-off user task when it can be split into independent chunks or completed without needing the top-level agent's persistent context. The first-level agent should split parallelizable work into one worker per chunk, not batches.
    task: String,
    /// Context, constraints, paths, service details, or prior findings the task agent needs. Include enough context to plan chunks, but avoid unrelated ongoing-conversation context.
    context: Option<String>,
}

impl Tool for WorkerAgent {
    const NAME: &'static str = "worker_agent";

    type Args = WorkerAgentParameters;
    type Output = String;
    type Error = ToolError;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        tool_definition(
            Self::NAME,
            "Spawn one lowest-level worker agent for one independent context-heavy chunk. This tool is for first-level subagents, not for delegating full tasks. Use it only when the current task can be split into independent chunks whose context can be discarded after a concise result.",
            serde_json::to_value(schemars::schema_for!(WorkerAgentParameters))
                .expect("schema serialization"),
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        run_subagent_at_depth(
            self.ui_context.clone(),
            SubagentDepth::Worker,
            args.task,
            args.context,
        )
        .await
    }
}

impl Tool for Subagent {
    const NAME: &'static str = "subagent";

    type Args = SubagentParameters;
    type Output = String;
    type Error = ToolError;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        tool_definition(
            Self::NAME,
            "Spawn one first-level task agent for a broad bounded task. Prefer this for one-off tasks with many independent context-heavy chunks; the task agent should maximize parallelism by calling worker_agent once per independent chunk, then synthesize their results. Do not use this for long-running continuous work where the top-level agent should preserve understanding across many user prompts.",
            serde_json::to_value(schemars::schema_for!(SubagentParameters))
                .expect("schema serialization"),
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        run_subagent_at_depth(
            self.ui_context.clone(),
            SubagentDepth::FirstLevel,
            args.task,
            args.context,
        )
        .await
    }
}

fn tool_definition(
    name: &str,
    description: &str,
    mut parameters: serde_json::Value,
) -> ToolDefinition {
    parameters["required"] = serde_json::json!(["task"]);
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}
