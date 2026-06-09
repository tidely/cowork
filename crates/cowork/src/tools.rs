use std::{
    convert::Infallible,
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    agent::{AgentEventSink, ExecutionPolicy, pump_stream},
    app::{AgentAddr, AgentDepth, AgentEvent, RuntimeAgentKey, ThreadId},
    tui::RuntimeEventSender,
};
use rig_core::{
    agent::{StreamingError, StreamingResult},
    client::ProviderClient,
    completion::ToolDefinition,
    loaders::PdfFileLoader,
    providers::ollama,
    schemars::{self, JsonSchema},
    streaming::StreamingPrompt,
    tool::{Tool, ToolEmbedding, ToolError},
};
use rig_derive::rig_tool;
use tokio::sync::Semaphore;

const MAX_READ_BYTES: u64 = 512 * 1024;
const MAX_PDF_BYTES: u64 = 25 * 1024 * 1024;

/// Maximum agent nesting depth. The top-level assistant is depth 0; each
/// `subagent` call spawns a child one level deeper. An agent below this depth
/// receives the `subagent` tool and can delegate further; an agent at this depth
/// is a leaf that does its chunk itself. Bounds runaway recursion.
const MAX_AGENT_DEPTH: AgentDepth = 4;

static NEXT_RUNTIME_AGENT_KEY: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolCapability {
    ReadFilesystem,
    WriteFilesystem,
    Delegate,
}

pub(crate) trait ToolAccessMetadata {
    const CAPABILITY: ToolCapability;
}

pub(crate) fn tool_capability(tool_name: &str) -> Option<ToolCapability> {
    match tool_name {
        <ReadFile as Tool>::NAME => Some(<ReadFile as ToolAccessMetadata>::CAPABILITY),
        <ReadPdf as Tool>::NAME => Some(<ReadPdf as ToolAccessMetadata>::CAPABILITY),
        <ListDirectory as Tool>::NAME => Some(<ListDirectory as ToolAccessMetadata>::CAPABILITY),
        <EditFile as Tool>::NAME => Some(<EditFile as ToolAccessMetadata>::CAPABILITY),
        <WriteFile as Tool>::NAME => Some(<WriteFile as ToolAccessMetadata>::CAPABILITY),
        Subagent::NAME => Some(<Subagent as ToolAccessMetadata>::CAPABILITY),
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) struct ToolUiContext {
    thread_id: ThreadId,
    /// Runtime key of the agent that owns these tools, and therefore the parent
    /// of any agent they spawn. `None` for the top-level assistant.
    parent_key: Option<RuntimeAgentKey>,
    /// Depth of the agent that owns these tools (top-level assistant is 0).
    depth: AgentDepth,
    events: RuntimeEventSender,
    policy: ExecutionPolicy,
    /// FIFO gate for this agent's immediate child subagents. Holding the permit
    /// for the full child run serializes sibling subtrees without preventing the
    /// parent from emitting multiple `subagent` tool calls in one turn.
    child_subagent_permits: Arc<Semaphore>,
}

impl ToolUiContext {
    pub(crate) fn new(
        thread_id: ThreadId,
        events: RuntimeEventSender,
        policy: ExecutionPolicy,
    ) -> Self {
        Self {
            thread_id,
            parent_key: None,
            depth: 0,
            events,
            policy,
            child_subagent_permits: Arc::new(Semaphore::new(policy.child_subagent_concurrency)),
        }
    }

    /// Context for the tools handed to a freshly spawned child agent: the child
    /// becomes the parent of its own children, one level deeper.
    fn child_context(&self, child_key: RuntimeAgentKey, child_depth: AgentDepth) -> Self {
        Self {
            thread_id: self.thread_id,
            parent_key: Some(child_key),
            depth: child_depth,
            events: self.events.clone(),
            policy: self.policy,
            child_subagent_permits: Arc::new(Semaphore::new(
                self.policy.child_subagent_concurrency,
            )),
        }
    }
}

fn next_runtime_agent_key() -> RuntimeAgentKey {
    NEXT_RUNTIME_AGENT_KEY.fetch_add(1, Ordering::Relaxed)
}

/// Preamble for a spawned subagent. Agents that can still delegate are steered
/// toward parallel fan-out; leaf agents (at `MAX_AGENT_DEPTH`) are told to do the
/// chunk themselves.
fn subagent_preamble(can_delegate: bool) -> &'static str {
    if can_delegate {
        "You are a task agent in an assistant hierarchy. Own only the delegated bounded task and stay within its stated scope. \
         Treat the parent's instructions as a contract: goal, context, paths/resources, constraints, expected output, and failure policy. Do not infer permission to broaden scope. \
         Do not explore unrelated directories, search for alternative targets, or invent follow-up work just because the direct path is blocked. \
         If a required file/path/resource is missing, too large to read, inaccessible, ambiguous, or otherwise blocks the task, stop and return a concise blocker report: what failed, what you tried, and what decision/input the parent should provide. \
         Spawn child subagents only when the parent explicitly delegated multiple known independent chunks or clearly authorized further fan-out; never spawn children to recover from a blocker or to explore outside scope. \
         When spawning children, give each child one slice-specific goal, context, paths/resources, constraints, expected output, and failure policy. \
         If the task requires continuous shared context rather than independent chunks, do the work yourself instead of spawning children. \
         Use tools when needed, do not modify files, and return a concise result useful to your parent."
    } else {
        "You are a leaf agent at the maximum delegation depth and cannot spawn further agents. \
         Do only the delegated bounded task and stay within its stated scope. \
         Do not explore unrelated directories, search for alternative targets, or invent follow-up work just because the direct path is blocked. \
         If a required file/path/resource is missing, too large to read, inaccessible, ambiguous, or otherwise blocks the task, stop and return a concise blocker report: what failed, what you tried, and what decision/input the parent should provide. \
         Use tools when needed. Do not modify files. \
         If the task cannot be handled independently because it needs continuous shared context, say so briefly."
    }
}

fn tool_error(message: impl Into<String>) -> ToolError {
    ToolError::ToolCallError(Box::new(io::Error::other(message.into())))
}

fn truncate_to_char_boundary(value: &mut String, max_len: usize) {
    if value.len() <= max_len {
        return;
    }

    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_len)
        .last()
        .unwrap_or(0);
    value.truncate(boundary);
}

fn home_dir() -> Result<PathBuf, ToolError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            if drive.is_empty() || path.is_empty() {
                return None;
            }

            let mut home = drive;
            home.push(path);
            Some(PathBuf::from(home))
        })
        .ok_or_else(|| tool_error("home directory environment variables are not set"))
}

fn normalize_path_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.replace('\\', "/")
    }
}

fn resolve_path(path: &str) -> Result<PathBuf, ToolError> {
    let path = path.trim();
    let path = if path == "~" {
        home_dir()?
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home_dir()?.join(normalize_path_separators(rest))
    } else {
        PathBuf::from(normalize_path_separators(path))
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

impl ToolAccessMetadata for ReadFile {
    const CAPABILITY: ToolCapability = ToolCapability::ReadFilesystem;
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

/// Convert a PDF file into markdown text extracted page-by-page.
#[rig_tool]
pub fn read_pdf(
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    path: String,
) -> Result<String, ToolError> {
    let path = resolve_path(&path)?;
    let metadata =
        fs::metadata(&path).map_err(|error| ToolError::ToolCallError(Box::new(error)))?;

    if !metadata.is_file() {
        return Err(tool_error(format!("{} is not a file", path.display())));
    }

    if metadata.len() > MAX_PDF_BYTES {
        return Err(tool_error(format!(
            "{} is too large to convert as PDF ({} bytes, max {MAX_PDF_BYTES})",
            path.display(),
            metadata.len()
        )));
    }

    let bytes = fs::read(&path).map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
    let document = PdfFileLoader::from_bytes(bytes)
        .load()
        .into_iter()
        .next()
        .ok_or_else(|| tool_error(format!("{} produced no PDF document", path.display())))?
        .map_err(|error| tool_error(format!("failed to load PDF {}: {error}", path.display())))?;

    let mut output = format!("# {}\n\n", path.display());
    let mut text_found = false;
    let mut truncated = false;

    for (page_index, _) in document.page_iter().enumerate() {
        let page_number = page_index + 1;
        let page_text = document
            .extract_text(&[page_number as u32])
            .map_err(|error| {
                tool_error(format!("failed to extract page {page_number}: {error}"))
            })?;

        let page_text = page_text.trim();
        text_found |= !page_text.is_empty();

        if !output.ends_with("\n\n") {
            output.push('\n');
        }
        output.push_str(&format!("## Page {page_number}\n\n"));
        output.push_str(page_text);
        output.push_str("\n\n");

        if output.len() as u64 > MAX_READ_BYTES {
            truncate_to_char_boundary(&mut output, MAX_READ_BYTES as usize);
            output.push_str("\n\n...\n\nPDF output truncated; ask for a narrower file or split the PDF before reading more.\n");
            truncated = true;
            break;
        }
    }

    if !text_found {
        return Ok(format!(
            "# {}\n\nNo extractable text was found in this PDF. It may be scanned or image-only.",
            path.display()
        ));
    }

    if !truncated {
        output.push_str("Converted from PDF with Rig's PDF document loader.\n");
    }

    Ok(output)
}

impl ToolAccessMetadata for ReadPdf {
    const CAPABILITY: ToolCapability = ToolCapability::ReadFilesystem;
}

impl ToolEmbedding for ReadPdf {
    type InitError = Infallible;
    type Context = ();
    type State = ();

    fn embedding_docs(&self) -> Vec<String> {
        vec![
            "Convert a local PDF file into markdown text by path.".to_string(),
            "Use when the user asks to inspect, read, summarize, or understand a PDF document."
                .to_string(),
        ]
    }

    fn context(&self) -> Self::Context {}

    fn init(_state: Self::State, _context: Self::Context) -> Result<Self, Self::InitError> {
        Ok(ReadPdf)
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

impl ToolAccessMetadata for ListDirectory {
    const CAPABILITY: ToolCapability = ToolCapability::ReadFilesystem;
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

impl ToolAccessMetadata for EditFile {
    const CAPABILITY: ToolCapability = ToolCapability::WriteFilesystem;
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

/// Create a text file, or overwrite an existing text file when explicitly allowed.
#[rig_tool]
pub fn write_file(
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    path: String,
    /// Full contents to write to the file.
    content: String,
    /// Set to true to replace an existing file. If false, existing files are left untouched.
    overwrite: bool,
) -> Result<String, ToolError> {
    let path = resolve_path(&path)?;

    if let Ok(metadata) = fs::metadata(&path) {
        if !metadata.is_file() {
            return Err(tool_error(format!("{} is not a file", path.display())));
        }
        if !overwrite {
            return Err(tool_error(format!(
                "{} already exists; set overwrite to true to replace it",
                path.display()
            )));
        }
    } else if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        return Err(tool_error(format!(
            "parent directory {} does not exist",
            parent.display()
        )));
    }

    fs::write(&path, content).map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
    Ok(format!("Wrote {}", path.display()))
}

impl ToolAccessMetadata for WriteFile {
    const CAPABILITY: ToolCapability = ToolCapability::WriteFilesystem;
}

impl ToolEmbedding for WriteFile {
    type InitError = Infallible;
    type Context = ();
    type State = ();

    fn embedding_docs(&self) -> Vec<String> {
        vec![
            "Create or overwrite a local text file with full contents.".to_string(),
            "Use when the user asks to create a new file, write a complete file, or replace a file's full contents."
                .to_string(),
        ]
    }

    fn context(&self) -> Self::Context {}

    fn init(_state: Self::State, _context: Self::Context) -> Result<Self, Self::InitError> {
        Ok(WriteFile)
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
    let mut sink = AgentEventSink::new(ui_context.events.clone(), ui_context.thread_id);
    pump_stream(&mut sink, AgentAddr::Runtime(key), stream).await
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
    parent_context: &ToolUiContext,
    key: RuntimeAgentKey,
    child_depth: AgentDepth,
    prompt: String,
) -> Result<String, ToolError> {
    let can_delegate = child_depth < MAX_AGENT_DEPTH;

    for attempt in 1..=crate::agent::PROMPT_RETRY_ATTEMPTS {
        // Every level shares the same base build. Agents that can still delegate
        // also get the recursive `subagent` tool, wired so their own children
        // land one level deeper.
        let mut builder = crate::agent::base_agent_builder(
            client,
            subagent_preamble(can_delegate),
            crate::agent::AGENT_MAX_TURNS,
        )
        .hook(crate::agent::UiPromptHook::new(
            AgentEventSink::new(parent_context.events.clone(), parent_context.thread_id),
            AgentAddr::Runtime(key),
        ));
        if can_delegate {
            builder = builder.tool(Subagent::new(
                parent_context.child_context(key, child_depth),
            ));
        }

        let agent = builder.build();
        let mut stream = agent
            .stream_prompt(&prompt)
            .with_tool_concurrency(parent_context.policy.tool_concurrency)
            .await;

        let error = match run_nested_stream(parent_context, key, &mut stream).await {
            Ok(response) => return require_nested_response(response),
            Err(error) => error,
        };

        let Some(backoff) = crate::agent::retry_backoff(attempt, &error) else {
            return Err(tool_error(format!("subagent failed: {error}")));
        };

        tokio::time::sleep(backoff).await;
    }

    unreachable!("retry loop always returns")
}

/// Spawn a child agent one level below `parent_context` for `task`, stream its
/// run into the UI tree, and return its final response.
async fn run_child_agent(
    parent_context: ToolUiContext,
    task: String,
    context: Option<String>,
) -> Result<String, ToolError> {
    let key = next_runtime_agent_key();
    let child_depth = parent_context.depth + 1;

    let prompt = subagent_prompt(&task, context.as_deref());

    let mut sink = AgentEventSink::new(parent_context.events.clone(), parent_context.thread_id);
    sink.send(AgentEvent::Spawned {
        key,
        parent: parent_context.parent_key,
        depth: child_depth,
        task,
        context,
    })
    .await;

    let _permit = parent_context
        .child_subagent_permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| tool_error("subagent scheduler was closed"))?;

    let client = ollama::Client::from_env()
        .map_err(|error| tool_error(format!("failed to create Ollama client: {error}")))?;

    let result =
        run_nested_agent_with_retries(&client, &parent_context, key, child_depth, prompt).await;

    match &result {
        Ok(result) => {
            sink.send(AgentEvent::Finished {
                addr: AgentAddr::Runtime(key),
                result: Some(result.clone()),
            })
            .await;
        }
        Err(error) => {
            sink.send(AgentEvent::Error {
                addr: AgentAddr::Runtime(key),
                error: error.to_string(),
            })
            .await;
        }
    }

    result
}

#[derive(Clone)]
pub(crate) struct Subagent {
    ui_context: ToolUiContext,
}

impl Subagent {
    pub(crate) fn new(ui_context: ToolUiContext) -> Self {
        Self { ui_context }
    }
}

#[derive(serde::Deserialize, JsonSchema)]
#[schemars(crate = "schemars")]
pub struct SubagentParameters {
    /// The bounded, independent sub-task for this child agent to own. Include the goal, expected output, and exact scope boundaries. Make it narrow enough that the child can finish it and return a concise result whose context can then be discarded.
    task: String,
    /// Slice-specific context only: why the goal matters, known paths/resources, constraints, relevant prior findings, and what to do if a path/resource is missing, too large, inaccessible, ambiguous, or otherwise blocks the task. Prefer telling the child to stop and report the blocker rather than explore outside scope.
    context: Option<String>,
}

impl ToolAccessMetadata for Subagent {
    const CAPABILITY: ToolCapability = ToolCapability::Delegate;
}

impl Tool for Subagent {
    const NAME: &'static str = "subagent";

    type Args = SubagentParameters;
    type Output = String;
    type Error = ToolError;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        tool_definition(
            Self::NAME,
            "Spawn a child agent to own one bounded, independent sub-task. Do not use it for simple one- or two-tool steps, single-file inspection, straightforward path reads/listing, or work that needs your continuous shared context. Give the child a clear goal, goal context, exact scope boundaries, known paths/resources, constraints, expected output shape, and failure policy. If a file/path/resource is missing, too large, inaccessible, ambiguous, or otherwise blocks the task, tell the child to stop and report the blocker rather than explore elsewhere or spawn recovery agents. Only delegate fan-out when there are multiple known independent chunks whose context can be discarded after a concise result.",
            serde_json::to_value(schemars::schema_for!(SubagentParameters))
                .expect("schema serialization"),
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        run_child_agent(self.ui_context.clone(), args.task, args.context).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_separators_accepts_either_slash_style() {
        let normalized = normalize_path_separators("parent/child\\leaf");

        if cfg!(windows) {
            assert_eq!(normalized, "parent\\child\\leaf");
        } else {
            assert_eq!(normalized, "parent/child/leaf");
        }
    }

    #[test]
    fn resolve_path_accepts_backslash_relative_paths() {
        let path = resolve_path("parent\\child").expect("path resolves");

        assert!(path.ends_with(PathBuf::from("parent").join("child")));
    }

    #[test]
    fn resolve_path_accepts_backslash_tilde_paths() {
        let path = resolve_path("~\\child").expect("path resolves");

        assert!(path.ends_with("child"));
        assert!(!path.to_string_lossy().contains('~'));
    }

    #[test]
    fn tools_declare_capabilities_without_profile_policy() {
        assert_eq!(
            tool_capability(EditFile::NAME),
            Some(ToolCapability::WriteFilesystem)
        );
        assert_eq!(
            tool_capability(WriteFile::NAME),
            Some(ToolCapability::WriteFilesystem)
        );
        assert_eq!(
            tool_capability(ReadFile::NAME),
            Some(ToolCapability::ReadFilesystem)
        );
        assert_eq!(
            tool_capability(ListDirectory::NAME),
            Some(ToolCapability::ReadFilesystem)
        );
        assert_eq!(
            tool_capability(Subagent::NAME),
            Some(ToolCapability::Delegate)
        );
    }

    #[test]
    fn write_file_creates_new_file() {
        let path = unique_temp_path("new.txt");
        let _ = fs::remove_file(&path);

        let result = write_file(path.to_string_lossy().to_string(), "hello".into(), false)
            .expect("file is written");

        assert!(result.contains("Wrote"));
        assert_eq!(fs::read_to_string(&path).expect("read file"), "hello");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn write_file_refuses_to_overwrite_without_flag() {
        let path = unique_temp_path("existing.txt");
        fs::write(&path, "original").expect("seed file");

        let error = write_file(path.to_string_lossy().to_string(), "updated".into(), false)
            .expect_err("overwrite is rejected");

        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read_to_string(&path).expect("read file"), "original");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn write_file_overwrites_when_allowed() {
        let path = unique_temp_path("overwrite.txt");
        fs::write(&path, "original").expect("seed file");

        write_file(path.to_string_lossy().to_string(), "updated".into(), true)
            .expect("overwrite succeeds");

        assert_eq!(fs::read_to_string(&path).expect("read file"), "updated");
        let _ = fs::remove_file(path);
    }

    fn unique_temp_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "cowork-write-file-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(name)
    }
}
