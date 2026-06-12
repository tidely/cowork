use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use llm::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ThreadId(usize);

impl ThreadId {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    fn next(&mut self) -> Self {
        let current = *self;
        self.0 += 1;
        current
    }

    fn display_number(self) -> usize {
        self.0 + 1
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct AgentId(usize);

impl AgentId {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    fn next(&mut self) -> Self {
        let current = *self;
        self.0 += 1;
        current
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A random, per-spawn agent id (a UUIDv4 as a `u128`). Random rather than a
/// counter so persisted `runtime-agent-{key}` conversation ids never collide,
/// in a session or across sessions once persistence lands.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct RuntimeAgentKey(u128);

impl RuntimeAgentKey {
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(crate) fn wrapping_add(self, rhs: u128) -> Self {
        Self(self.0.wrapping_add(rhs))
    }
}

impl fmt::Display for RuntimeAgentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub const MAIN_AGENT_ID: AgentId = AgentId::new(0);

/// Nesting depth of an agent in the hierarchy. The top-level assistant is 0;
/// each `subagent` call spawns a child one level deeper. A plain count rather
/// than a fixed enum so the tree can recurse to an arbitrary (bounded) depth.
pub type AgentDepth = usize;

pub const MAIN_AGENT_DEPTH: AgentDepth = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Running,
    Complete,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Reasoning,
    ToolCall,
    ToolResult,
    Error,
}

impl MessageRole {
    pub fn label(self) -> &'static str {
        match self {
            MessageRole::System => "System",
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Reasoning => "Thinking",
            MessageRole::ToolCall => "Tool call",
            MessageRole::ToolResult => "Tool result",
            MessageRole::Error => "Error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    AwaitingPermission,
    Running,
    Finished,
    Failed,
}

impl ToolStatus {
    pub fn is_done(self) -> bool {
        !matches!(self, ToolStatus::AwaitingPermission | ToolStatus::Running)
    }
}

/// A tool-call card. Carries the call's display state structurally — the
/// header summary and pretty-printed arguments, plus the live status and
/// result — so nothing has to be packed into and re-parsed out of a string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDisplay {
    pub call_id: String,
    /// Header label, e.g. `read_file /tmp` or `subagent <task>`.
    pub summary: String,
    /// Pretty-printed JSON arguments.
    pub arguments: String,
    pub status: ToolStatus,
    pub result: Option<String>,
    pub collapsed: bool,
}

/// One visible entry in an agent's transcript. Each variant carries exactly the
/// data its kind needs: only `Reasoning` and `ToolCall` are collapsible, and
/// only `ToolCall` has a status/result — so a plain text message cannot express
/// a tool status, and a `User` message cannot be "collapsed".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    System(String),
    User(String),
    Assistant(String),
    Reasoning { content: String, collapsed: bool },
    ToolCall(ToolDisplay),
    ToolResult(String),
    Error(String),
}

impl Message {
    fn system(content: impl Into<String>) -> Self {
        Message::System(content.into())
    }

    fn user(content: impl Into<String>) -> Self {
        Message::User(content.into())
    }

    fn tool_result(content: impl Into<String>) -> Self {
        Message::ToolResult(content.into())
    }

    fn error(content: impl Into<String>) -> Self {
        Message::Error(content.into())
    }

    fn tool_call(id: String, name: String, arguments: &Value) -> Self {
        Message::ToolCall(ToolDisplay {
            call_id: id,
            summary: tool_call_summary(&name, arguments),
            arguments: pretty_json(arguments),
            status: ToolStatus::Running,
            result: None,
            collapsed: true,
        })
    }

    /// The kind discriminant, for styling, labels, and the streaming-delta
    /// coalescing check.
    pub fn role(&self) -> MessageRole {
        match self {
            Message::System(_) => MessageRole::System,
            Message::User(_) => MessageRole::User,
            Message::Assistant(_) => MessageRole::Assistant,
            Message::Reasoning { .. } => MessageRole::Reasoning,
            Message::ToolCall(_) => MessageRole::ToolCall,
            Message::ToolResult(_) => MessageRole::ToolResult,
            Message::Error(_) => MessageRole::Error,
        }
    }

    /// The text body of a plain message; `None` for a tool call, which renders
    /// from its structured fields instead.
    pub fn text(&self) -> Option<&str> {
        match self {
            Message::System(content)
            | Message::User(content)
            | Message::Assistant(content)
            | Message::ToolResult(content)
            | Message::Error(content)
            | Message::Reasoning { content, .. } => Some(content),
            Message::ToolCall(_) => None,
        }
    }

    /// The text body as a growable buffer, for appending streaming deltas.
    fn text_mut(&mut self) -> Option<&mut String> {
        match self {
            Message::System(content)
            | Message::User(content)
            | Message::Assistant(content)
            | Message::ToolResult(content)
            | Message::Error(content)
            | Message::Reasoning { content, .. } => Some(content),
            Message::ToolCall(_) => None,
        }
    }

    pub fn tool(&self) -> Option<&ToolDisplay> {
        match self {
            Message::ToolCall(tool) => Some(tool),
            _ => None,
        }
    }

    fn tool_mut(&mut self) -> Option<&mut ToolDisplay> {
        match self {
            Message::ToolCall(tool) => Some(tool),
            _ => None,
        }
    }

    /// Whether this message can be collapsed/expanded in the conversation view.
    pub fn is_collapsible(&self) -> bool {
        matches!(self, Message::Reasoning { .. } | Message::ToolCall(_))
    }

    pub fn collapsed(&self) -> bool {
        match self {
            Message::Reasoning { collapsed, .. } => *collapsed,
            Message::ToolCall(tool) => tool.collapsed,
            _ => false,
        }
    }

    /// Set the collapsed flag; a no-op on a non-collapsible message.
    fn set_collapsed(&mut self, value: bool) {
        match self {
            Message::Reasoning { collapsed, .. } => *collapsed = value,
            Message::ToolCall(tool) => tool.collapsed = value,
            _ => {}
        }
    }
}

#[cfg(test)]
impl Message {
    fn content(&self) -> &str {
        self.text().unwrap_or_default()
    }

    fn tool_status(&self) -> ToolStatus {
        self.tool().expect("a tool-call message").status
    }

    fn tool_result_text(&self) -> Option<&str> {
        self.tool().and_then(|tool| tool.result.as_deref())
    }

    fn tool_call_id(&self) -> Option<&str> {
        self.tool().map(|tool| tool.call_id.as_str())
    }

    fn tool_summary(&self) -> Option<&str> {
        self.tool().map(|tool| tool.summary.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNode {
    pub id: AgentId,
    pub parent_id: Option<AgentId>,
    pub runtime_key: Option<RuntimeAgentKey>,
    pub label: String,
    pub depth: AgentDepth,
    pub status: AgentStatus,
    pub expanded: bool,
    pub token_usage: TokenUsage,
    pub messages: Vec<Message>,
    /// Number of messages present before the current run began. A retry rewinds
    /// the transcript to this point so a failed attempt's partial output does
    /// not stack under the next attempt's. Per-run transient: never persisted,
    /// and reset to the message count on restore.
    #[serde(default, skip_serializing)]
    message_baseline: usize,
}

impl AgentNode {
    fn main() -> Self {
        let messages = initial_main_messages();
        Self {
            id: MAIN_AGENT_ID,
            parent_id: None,
            runtime_key: None,
            label: "Main Agent".to_string(),
            depth: MAIN_AGENT_DEPTH,
            status: AgentStatus::Idle,
            expanded: true,
            token_usage: TokenUsage::default(),
            message_baseline: messages.len(),
            messages,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadState {
    pub id: ThreadId,
    pub title: String,
    pub conversation_id: String,
    pub expanded: bool,
    pub main_agent: AgentNode,
    pub subagents: Vec<AgentNode>,
}

impl ThreadState {
    pub fn total_token_usage(&self) -> TokenUsage {
        let mut usage = self.main_agent.token_usage;
        for agent in &self.subagents {
            usage += agent.token_usage;
        }
        usage
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Conversation,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
}

#[derive(Debug, Clone)]
pub struct InputState {
    pub value: String,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct SidebarItem {
    pub thread_id: ThreadId,
    pub agent_id: Option<AgentId>,
    pub label: String,
    pub depth: usize,
    pub expanded: bool,
    pub selected: bool,
    pub status: Option<AgentStatus>,
    pub has_children: bool,
}

#[derive(Debug)]
pub struct AppState {
    pub threads: Vec<ThreadState>,
    pub selected: Selection,
    pub focus: Focus,
    pub input: InputState,
    pub should_quit: bool,
    pub conversation_scroll: u16,
    pub conversation_cursor: usize,
    pending_tool_permissions: VecDeque<PendingToolPermission>,
    always_allowed_tools: HashSet<String>,
    /// Live control handles (cancel token + message queue) for every running
    /// agent, the main agent included. Keyed by address so the cancel keybind and
    /// the outgoing-message queue share one lookup. Transient runtime state, not
    /// persisted; an entry is removed when its agent reaches a terminal state.
    agent_controls: HashMap<(ThreadId, AgentAddr), AgentControl>,
    next_thread_id: ThreadId,
    next_agent_id: AgentId,
}

#[derive(Debug)]
pub enum SubmitResult {
    None,
    Submitted {
        thread_id: ThreadId,
        conversation_id: String,
        prompt: String,
        /// Cancellation token for this run; the runtime races it so the cancel
        /// keybind can stop the thread.
        cancel: CancellationToken,
        /// Receiver the run drains between turns for messages queued while it is
        /// in flight. The matching sender lives in `agent_controls`.
        queued_messages: mpsc::UnboundedReceiver<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPermissionResponse {
    Allow,
    AllowAlways,
    Reject { reason: String },
}

/// Live control handles for one running agent — the main agent or a subagent.
/// `cancel` stops the run; `queue` carries user messages to be delivered at the
/// next turn boundary (the unified path for both a mid-run prompt to the main
/// agent and a message to a subagent). Transient runtime state, never persisted.
#[derive(Debug, Clone)]
pub struct AgentControl {
    cancel: CancellationToken,
    queue: mpsc::UnboundedSender<String>,
}

impl AgentControl {
    pub(crate) fn new(cancel: CancellationToken, queue: mpsc::UnboundedSender<String>) -> Self {
        Self { cancel, queue }
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Queue a user message for delivery at the run's next turn boundary.
    fn enqueue(&self, message: String) -> Result<(), mpsc::error::SendError<String>> {
        self.queue.send(message)
    }
}

#[derive(Debug)]
pub struct PendingToolPermission {
    pub thread_id: ThreadId,
    pub addr: AgentAddr,
    pub id: String,
    pub name: String,
    pub arguments: Value,
    respond_to: oneshot::Sender<ToolPermissionResponse>,
}

impl PendingToolPermission {
    pub fn new(
        thread_id: ThreadId,
        addr: AgentAddr,
        id: String,
        name: String,
        arguments: Value,
        respond_to: oneshot::Sender<ToolPermissionResponse>,
    ) -> Self {
        Self {
            thread_id,
            addr,
            id,
            name,
            arguments,
            respond_to,
        }
    }

    pub fn summary(&self) -> String {
        tool_call_summary(&self.name, &self.arguments)
    }

    pub(crate) fn respond(self, response: ToolPermissionResponse) {
        _ = self.respond_to.send(response);
    }
}

/// Identifies which agent within a thread an event targets. The top-level agent
/// is `Main`; spawned subagents are addressed by their runtime key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentAddr {
    Main,
    Runtime(RuntimeAgentKey),
}

#[derive(Debug, Clone)]
pub enum ThreadEvent {
    /// The top-level prompt run has begun.
    MainStarted,
    /// A subagent stream has begun; creates its node in the tree.
    SpawnedSubagent {
        key: RuntimeAgentKey,
        parent: Option<RuntimeAgentKey>,
        depth: AgentDepth,
        task: String,
        context: Option<String>,
        control: AgentControl,
    },
    /// An event for one visible agent node in the thread.
    Agent {
        addr: AgentAddr,
        event: AgentNodeEvent,
    },
    /// The top-level run was cancelled by the user. Marks the main agent and any
    /// still-running subagents of the thread as cancelled. Thread-wide, so it
    /// carries no agent address — the thread id routes it.
    Cancelled,
}

struct SpawnNested {
    key: RuntimeAgentKey,
    parent: Option<RuntimeAgentKey>,
    depth: AgentDepth,
    task: String,
    context: Option<String>,
    control: AgentControl,
}

#[derive(Debug, Clone)]
pub enum AgentNodeEvent {
    AssistantDelta {
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    /// A queued user message reached the conversation at a turn boundary. Pushed
    /// into the visible transcript here so it lands between turns rather than
    /// mid-stream. The model context already holds it (the agent loop appended
    /// it before emitting this), so this is display-only.
    UserMessage {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Usage(TokenUsage),
    Finished(AgentCompletion),
    Cancelled(String),
    Error(String),
    /// Rewind an agent's visible transcript to its pre-run baseline. Emitted
    /// before a retry so the partial output of the failed attempt is discarded.
    Reset,
}

#[derive(Debug, Clone)]
pub enum AgentCompletion {
    Done,
    Returned(String),
}

impl AppState {
    pub fn new() -> Self {
        Self {
            threads: vec![ThreadState {
                id: ThreadId::new(0),
                title: "Thread 1".to_string(),
                conversation_id: "agent-thread-0".to_string(),
                expanded: true,
                main_agent: AgentNode::main(),
                subagents: Vec::new(),
            }],
            selected: Selection {
                thread_id: ThreadId::new(0),
                agent_id: MAIN_AGENT_ID,
            },
            focus: Focus::Input,
            input: InputState {
                value: String::new(),
                cursor: 0,
            },
            should_quit: false,
            conversation_scroll: 0,
            conversation_cursor: 0,
            pending_tool_permissions: VecDeque::new(),
            always_allowed_tools: HashSet::new(),
            agent_controls: HashMap::new(),
            next_thread_id: ThreadId::new(1),
            next_agent_id: AgentId::new(1),
        }
    }

    /// Rebuild an app from a persisted session. Falls back to a fresh app when
    /// the snapshot carries no threads. In-flight statuses are normalized so the
    /// restored session is immediately usable (see [`normalize_after_restore`]).
    pub fn restored(
        threads: Vec<ThreadState>,
        next_thread_id: ThreadId,
        next_agent_id: AgentId,
        always_allowed_tools: Vec<String>,
    ) -> Self {
        let mut app = Self::new();
        let Some(first_thread_id) = threads.first().map(|thread| thread.id) else {
            return app;
        };

        app.threads = threads;
        app.next_thread_id = next_thread_id;
        app.next_agent_id = next_agent_id;
        app.always_allowed_tools = always_allowed_tools.into_iter().collect();
        app.selected = Selection {
            thread_id: first_thread_id,
            agent_id: MAIN_AGENT_ID,
        };
        app.normalize_after_restore();
        app
    }

    /// Clone of the persistable thread tree. The transient runtime state
    /// (pending permissions, cancellation tokens, focus/scroll/input) is left
    /// out — only the conversation tree and its metadata are snapshotted.
    pub fn snapshot_threads(&self) -> Vec<ThreadState> {
        self.threads.clone()
    }

    pub fn next_ids(&self) -> (ThreadId, AgentId) {
        (self.next_thread_id, self.next_agent_id)
    }

    pub fn always_allowed_tools(&self) -> Vec<String> {
        self.always_allowed_tools.iter().cloned().collect()
    }

    /// Whether any thread has a live run. Persistence skips snapshotting while a
    /// run is in flight so a half-finished turn is never written to disk.
    pub fn any_running(&self) -> bool {
        self.threads
            .iter()
            .any(|thread| thread.main_agent.status == AgentStatus::Running)
    }

    /// Bring a freshly loaded tree to rest: a run that was in flight at save
    /// time has no backing task after restart, so its non-terminal statuses are
    /// settled and each agent's retry baseline is re-anchored to its transcript.
    fn normalize_after_restore(&mut self) {
        for thread in &mut self.threads {
            normalize_restored_agent(&mut thread.main_agent, true);
            for agent in &mut thread.subagents {
                normalize_restored_agent(agent, false);
            }
        }
    }

    pub fn active_agent_running(&self) -> bool {
        self.agent(self.selected.thread_id, MAIN_AGENT_ID)
            .is_some_and(|agent| agent.status == AgentStatus::Running)
    }

    pub fn selected_subagent_accepts_messages(&self) -> bool {
        if self.selected.agent_id == MAIN_AGENT_ID {
            return false;
        }
        let Some(agent) = self.selected_agent() else {
            return false;
        };
        agent.status == AgentStatus::Running
            && agent.runtime_key.is_some_and(|key| {
                self.agent_controls
                    .contains_key(&(self.selected.thread_id, AgentAddr::Runtime(key)))
            })
    }

    /// Whether the input box should accept typing. The main agent always accepts
    /// it (idle starts a run, running queues a message); a subagent only while it
    /// is running and reachable.
    pub fn input_accepts_text(&self) -> bool {
        self.selected.agent_id == MAIN_AGENT_ID || self.selected_subagent_accepts_messages()
    }

    pub fn selected_agent(&self) -> Option<&AgentNode> {
        self.agent(self.selected.thread_id, self.selected.agent_id)
    }

    pub fn selected_thread(&self) -> Option<&ThreadState> {
        self.threads
            .iter()
            .find(|thread| thread.id == self.selected.thread_id)
    }

    pub fn pending_tool_permission(&self) -> Option<&PendingToolPermission> {
        self.pending_tool_permissions.front()
    }

    pub fn sidebar_items(&self) -> Vec<SidebarItem> {
        let mut items = Vec::new();
        for thread in &self.threads {
            items.push(SidebarItem {
                thread_id: thread.id,
                agent_id: Some(MAIN_AGENT_ID),
                label: thread.title.clone(),
                depth: 0,
                expanded: thread.expanded,
                selected: self.selected.thread_id == thread.id
                    && self.selected.agent_id == MAIN_AGENT_ID,
                status: Some(thread.main_agent.status),
                has_children: thread
                    .subagents
                    .iter()
                    .any(|agent| agent.parent_id == Some(MAIN_AGENT_ID)),
            });

            if thread.expanded {
                append_child_sidebar_items(&mut items, thread, MAIN_AGENT_ID, 1, self.selected);
            }
        }
        items
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SubmitResult {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return SubmitResult::None;
        }

        if self.pending_tool_permission().is_some() {
            match key.code {
                KeyCode::Char('a') => {
                    self.resolve_next_tool_permission(ToolPermissionResponse::Allow);
                    return SubmitResult::None;
                }
                KeyCode::Char('A') => {
                    self.resolve_next_tool_permission(ToolPermissionResponse::AllowAlways);
                    return SubmitResult::None;
                }
                KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Esc => {
                    let Some(request) = self.pending_tool_permission() else {
                        return SubmitResult::None;
                    };
                    self.resolve_next_tool_permission(ToolPermissionResponse::Reject {
                        reason: rejected_tool_permission_reason(&request.name),
                    });
                    return SubmitResult::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.create_thread();
                SubmitResult::None
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cancel_selected_agent();
                SubmitResult::None
            }
            KeyCode::Char('[') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.collapse_all_sidebar();
                SubmitResult::None
            }
            KeyCode::Char(']') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.expand_all_sidebar();
                SubmitResult::None
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Input => Focus::Sidebar,
                    Focus::Sidebar => Focus::Conversation,
                    Focus::Conversation => Focus::Input,
                };
                if self.focus == Focus::Conversation {
                    self.clamp_conversation_cursor();
                }
                SubmitResult::None
            }
            KeyCode::Esc => {
                self.focus = Focus::Input;
                SubmitResult::None
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_last_reasoning();
                SubmitResult::None
            }
            _ => match self.focus {
                Focus::Sidebar => self.handle_sidebar_key(key),
                Focus::Conversation => self.handle_conversation_key(key),
                Focus::Input => self.handle_input_key(key),
            },
        }
    }

    pub fn apply_tool_permission_request(&mut self, request: PendingToolPermission) {
        if self.always_allowed_tools.contains(&request.name) {
            self.allow_tool_permission(request, ToolPermissionResponse::AllowAlways);
            return;
        }

        if let Some(agent_id) = self.resolve_addr(request.thread_id, request.addr) {
            self.selected = Selection {
                thread_id: request.thread_id,
                agent_id,
            };
            self.focus = Focus::Conversation;
            self.collapse_last_reasoning(request.thread_id, agent_id);
            self.set_tool_status(
                request.thread_id,
                agent_id,
                &request.id,
                ToolStatus::AwaitingPermission,
                None,
            );
        }
        self.pending_tool_permissions.push_back(request);
    }

    pub fn apply_thread_event(&mut self, thread_id: ThreadId, event: ThreadEvent) {
        match event {
            ThreadEvent::MainStarted => {
                if let Some(agent) = self.agent_mut(thread_id, MAIN_AGENT_ID) {
                    agent.status = AgentStatus::Running;
                    // The user prompt is already appended; mark this as the
                    // rewind point so a retry keeps the prompt but drops output.
                    agent.message_baseline = agent.messages.len();
                }
            }
            ThreadEvent::SpawnedSubagent {
                key,
                parent,
                depth,
                task,
                context,
                control,
            } => self.spawn_nested(
                thread_id,
                SpawnNested {
                    key,
                    parent,
                    depth,
                    task,
                    context,
                    control,
                },
            ),
            ThreadEvent::Agent { addr, event } => {
                self.apply_agent_node_event(thread_id, addr, event);
            }
            ThreadEvent::Cancelled => {
                // Cancellation is thread-wide: the dropped run leaves running
                // subagents with no terminal event of their own, so sweep them
                // all here and drop the run's controls and pending prompts.
                self.discard_pending_permissions_for_thread(thread_id);
                self.cancel_thread_agents(thread_id);
                self.agent_controls
                    .retain(|(control_thread_id, _), _| *control_thread_id != thread_id);
            }
        }
    }

    fn apply_agent_node_event(
        &mut self,
        thread_id: ThreadId,
        addr: AgentAddr,
        event: AgentNodeEvent,
    ) {
        let Some(agent_id) = self.resolve_addr(thread_id, addr) else {
            return;
        };

        match event {
            AgentNodeEvent::AssistantDelta { delta } => {
                self.collapse_last_reasoning(thread_id, agent_id);
                self.append_delta(thread_id, agent_id, TextDeltaKind::Assistant, &delta);
            }
            AgentNodeEvent::ReasoningDelta { delta } => {
                self.append_delta(thread_id, agent_id, TextDeltaKind::Reasoning, &delta);
            }
            AgentNodeEvent::UserMessage { text } => {
                // Lands at a turn boundary, so collapse any in-progress reasoning
                // and append the user bubble after it — never mid-stream.
                self.collapse_last_reasoning(thread_id, agent_id);
                if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                    agent.messages.push(Message::user(text));
                }
            }
            AgentNodeEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.collapse_last_reasoning(thread_id, agent_id);
                if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                    agent
                        .messages
                        .push(Message::tool_call(id, name, &arguments));
                }
            }
            AgentNodeEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                self.collapse_last_reasoning(thread_id, agent_id);
                self.apply_tool_result_to_agent(thread_id, agent_id, &id, &content, is_error);
            }
            AgentNodeEvent::Usage(usage) => {
                self.collapse_last_reasoning(thread_id, agent_id);
                if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                    agent.token_usage += usage;
                }
            }
            AgentNodeEvent::Finished(completion) => {
                if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                    agent.status = AgentStatus::Complete;
                    collapse_reasoning(agent);
                    if let AgentCompletion::Returned(result) = completion {
                        agent.messages.push(Message::tool_result(result));
                    }
                }
                self.remove_agent_control(thread_id, addr);
            }
            AgentNodeEvent::Cancelled(reason) => {
                self.cancel_agent_subtree(thread_id, agent_id, &reason);
                self.remove_agent_controls_for_subtree(thread_id, agent_id);
            }
            AgentNodeEvent::Error(error) => {
                if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                    agent.status = AgentStatus::Error;
                    collapse_reasoning(agent);
                    agent.messages.push(Message::error(error));
                }
                self.remove_agent_control(thread_id, addr);
            }
            AgentNodeEvent::Reset => {
                if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                    agent.messages.truncate(agent.message_baseline);
                    agent.status = AgentStatus::Running;
                }
            }
        }
    }

    fn resolve_next_tool_permission(&mut self, response: ToolPermissionResponse) {
        let Some(request) = self.pending_tool_permissions.pop_front() else {
            return;
        };

        if matches!(
            response,
            ToolPermissionResponse::Allow | ToolPermissionResponse::AllowAlways
        ) {
            self.allow_tool_permission(request, response);
            self.resolve_always_allowed_tool_permissions();
            return;
        }

        self.finish_tool_permission(request, response);
    }

    fn allow_tool_permission(
        &mut self,
        request: PendingToolPermission,
        response: ToolPermissionResponse,
    ) {
        if matches!(response, ToolPermissionResponse::AllowAlways) {
            self.always_allowed_tools.insert(request.name.clone());
        }
        self.finish_tool_permission(request, response);
    }

    fn resolve_always_allowed_tool_permissions(&mut self) {
        let mut remaining = VecDeque::new();
        while let Some(request) = self.pending_tool_permissions.pop_front() {
            if self.always_allowed_tools.contains(&request.name) {
                self.allow_tool_permission(request, ToolPermissionResponse::AllowAlways);
            } else {
                remaining.push_back(request);
            }
        }
        self.pending_tool_permissions = remaining;
    }

    fn finish_tool_permission(
        &mut self,
        request: PendingToolPermission,
        response: ToolPermissionResponse,
    ) {
        if let Some(agent_id) = self.resolve_addr(request.thread_id, request.addr) {
            let (status, result) = match &response {
                ToolPermissionResponse::Allow | ToolPermissionResponse::AllowAlways => {
                    (ToolStatus::Running, None)
                }
                ToolPermissionResponse::Reject { reason } => {
                    (ToolStatus::Failed, Some(reason.as_str()))
                }
            };
            self.set_tool_status(request.thread_id, agent_id, &request.id, status, result);
        }

        request.respond(response);
    }

    /// Resolves an event address to a concrete agent id within the thread.
    /// Returns `None` for a runtime key that no longer maps to a live node.
    fn resolve_addr(&self, thread_id: ThreadId, addr: AgentAddr) -> Option<AgentId> {
        match addr {
            AgentAddr::Main => Some(MAIN_AGENT_ID),
            AgentAddr::Runtime(key) => self.agent_id_by_runtime_key(thread_id, key),
        }
    }

    fn handle_sidebar_key(&mut self, key: KeyEvent) -> SubmitResult {
        match key.code {
            KeyCode::Up => self.move_sidebar_selection(-1),
            KeyCode::Down => self.move_sidebar_selection(1),
            KeyCode::PageUp => self.move_sidebar_selection(-8),
            KeyCode::PageDown => self.move_sidebar_selection(8),
            KeyCode::Enter | KeyCode::Right => self.expand_selected_sidebar_item(),
            KeyCode::Left => self.collapse_selected_sidebar_item(),
            _ => {}
        }
        SubmitResult::None
    }

    fn handle_conversation_key(&mut self, key: KeyEvent) -> SubmitResult {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.conversation_cursor = self.conversation_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let count = self.collapsible_message_indices().len();
                if count > 0 {
                    self.conversation_cursor = (self.conversation_cursor + 1).min(count - 1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected_collapsible(),
            _ => {}
        }
        SubmitResult::None
    }

    /// Indices into the selected agent's messages that can be collapsed/expanded.
    pub fn collapsible_message_indices(&self) -> Vec<usize> {
        self.selected_agent()
            .map(|agent| {
                agent
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| message.is_collapsible())
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Message index of the collapsible currently selected in conversation focus.
    pub fn selected_collapsible_message_index(&self) -> Option<usize> {
        self.collapsible_message_indices()
            .get(self.conversation_cursor)
            .copied()
    }

    fn clamp_conversation_cursor(&mut self) {
        let count = self.collapsible_message_indices().len();
        self.conversation_cursor = self.conversation_cursor.min(count.saturating_sub(1));
    }

    fn toggle_selected_collapsible(&mut self) {
        let Some(index) = self.selected_collapsible_message_index() else {
            return;
        };
        if let Some(agent) = self.agent_mut(self.selected.thread_id, self.selected.agent_id)
            && let Some(message) = agent.messages.get_mut(index)
        {
            message.set_collapsed(!message.collapsed());
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> SubmitResult {
        match key.code {
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Char(ch) => {
                self.input.value.insert(self.input.cursor, ch);
                self.input.cursor += ch.len_utf8();
                SubmitResult::None
            }
            KeyCode::Backspace => {
                if self.input.cursor > 0 {
                    let previous = self.input.value[..self.input.cursor]
                        .char_indices()
                        .last()
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                    self.input
                        .value
                        .replace_range(previous..self.input.cursor, "");
                    self.input.cursor = previous;
                }
                SubmitResult::None
            }
            KeyCode::Delete => {
                if self.input.cursor < self.input.value.len()
                    && let Some(ch) = self.input.value[self.input.cursor..].chars().next()
                {
                    let end = self.input.cursor + ch.len_utf8();
                    self.input.value.replace_range(self.input.cursor..end, "");
                }
                SubmitResult::None
            }
            KeyCode::Left => {
                if self.input.cursor > 0 {
                    self.input.cursor = self.input.value[..self.input.cursor]
                        .char_indices()
                        .last()
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                }
                SubmitResult::None
            }
            KeyCode::Right => {
                if self.input.cursor < self.input.value.len()
                    && let Some(ch) = self.input.value[self.input.cursor..].chars().next()
                {
                    self.input.cursor += ch.len_utf8();
                }
                SubmitResult::None
            }
            KeyCode::Up => {
                self.conversation_scroll = self.conversation_scroll.saturating_add(1);
                SubmitResult::None
            }
            KeyCode::Down => {
                self.conversation_scroll = self.conversation_scroll.saturating_sub(1);
                SubmitResult::None
            }
            _ => SubmitResult::None,
        }
    }

    fn submit_prompt(&mut self) -> SubmitResult {
        let prompt = self.input.value.trim().to_string();
        if prompt.is_empty() {
            return SubmitResult::None;
        }

        if self.selected.agent_id != MAIN_AGENT_ID {
            self.enqueue_subagent_message(prompt);
            return SubmitResult::None;
        }

        // A message typed while the main agent is running is queued for delivery
        // at the next turn boundary, not started as a competing run.
        if self.active_agent_running() {
            self.enqueue_main_message(prompt);
            return SubmitResult::None;
        }

        let Some(thread) = self.thread_mut(self.selected.thread_id) else {
            return SubmitResult::None;
        };

        thread.title = title_from_prompt(&thread.title, &prompt);
        thread.main_agent.status = AgentStatus::Running;
        thread
            .main_agent
            .messages
            .push(Message::user(prompt.clone()));
        let conversation_id = thread.conversation_id.clone();

        self.selected.agent_id = MAIN_AGENT_ID;
        self.input.value.clear();
        self.input.cursor = 0;
        self.conversation_scroll = 0;
        self.conversation_cursor = 0;

        let cancel = CancellationToken::new();
        let (queue_tx, queue_rx) = mpsc::unbounded_channel();
        self.agent_controls.insert(
            (self.selected.thread_id, AgentAddr::Main),
            AgentControl::new(cancel.clone(), queue_tx),
        );

        SubmitResult::Submitted {
            thread_id: self.selected.thread_id,
            conversation_id,
            prompt,
            cancel,
            queued_messages: queue_rx,
        }
    }

    /// Queue a message for the running main agent. Best-effort: if the run has
    /// already ended the send fails and the message is dropped. No visible
    /// message is pushed here — the agent loop emits a [`AgentNodeEvent::UserMessage`]
    /// when it injects the message at a turn boundary, so a streaming block is
    /// never split.
    fn enqueue_main_message(&mut self, prompt: String) {
        let thread_id = self.selected.thread_id;
        let Some(control) = self
            .agent_controls
            .get(&(thread_id, AgentAddr::Main))
            .cloned()
        else {
            return;
        };
        if control.enqueue(prompt).is_ok() {
            self.input.value.clear();
            self.input.cursor = 0;
            self.conversation_scroll = 0;
            self.conversation_cursor = 0;
        }
    }

    fn enqueue_subagent_message(&mut self, prompt: String) {
        let thread_id = self.selected.thread_id;
        let agent_id = self.selected.agent_id;
        let Some(key) = self
            .agent(thread_id, agent_id)
            .filter(|agent| agent.status == AgentStatus::Running)
            .and_then(|agent| agent.runtime_key)
        else {
            return;
        };

        let Some(control) = self
            .agent_controls
            .get(&(thread_id, AgentAddr::Runtime(key)))
            .cloned()
        else {
            return;
        };

        // No visible message is pushed here — the subagent loop emits a
        // `UserMessage` event when it injects the message at a turn boundary, so
        // it lands between turns rather than splitting a streaming block.
        match control.enqueue(prompt) {
            Ok(()) => {
                self.input.value.clear();
                self.input.cursor = 0;
                self.conversation_scroll = 0;
                self.conversation_cursor = 0;
            }
            Err(_) => {
                if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                    agent
                        .messages
                        .push(Message::error("subagent is no longer accepting messages"));
                }
                self.remove_agent_control(thread_id, AgentAddr::Runtime(key));
            }
        }
    }

    fn move_sidebar_selection(&mut self, direction: isize) {
        let items = self.sidebar_items();
        if items.is_empty() {
            return;
        }

        let current = items
            .iter()
            .position(|item| item.selected)
            .unwrap_or_default();
        let next = (current as isize + direction).clamp(0, items.len() as isize - 1) as usize;
        let item = &items[next];
        self.selected.thread_id = item.thread_id;
        self.selected.agent_id = item.agent_id.unwrap_or(MAIN_AGENT_ID);
        self.conversation_scroll = 0;
        self.conversation_cursor = 0;
    }

    fn expand_selected_sidebar_item(&mut self) {
        if self.selected.agent_id == MAIN_AGENT_ID {
            if let Some(thread) = self.thread_mut(self.selected.thread_id) {
                thread.expanded = true;
            }
        } else if let Some(agent) = self.agent_mut(self.selected.thread_id, self.selected.agent_id)
        {
            agent.expanded = true;
        }
    }

    fn collapse_selected_sidebar_item(&mut self) {
        if self.selected.agent_id == MAIN_AGENT_ID {
            if let Some(thread) = self.thread_mut(self.selected.thread_id) {
                thread.expanded = false;
            }
        } else if let Some(agent) = self.agent_mut(self.selected.thread_id, self.selected.agent_id)
        {
            agent.expanded = false;
        }
    }

    /// Cancel the selected running agent. For a subagent this cancels only that
    /// child run so its parent receives a failed `subagent` tool result and can
    /// continue. For the main agent this cancels the whole thread.
    fn cancel_selected_agent(&mut self) {
        let thread_id = self.selected.thread_id;
        let addr = if self.selected.agent_id == MAIN_AGENT_ID {
            AgentAddr::Main
        } else {
            let Some(key) = self
                .agent(thread_id, self.selected.agent_id)
                .filter(|agent| agent.status == AgentStatus::Running)
                .and_then(|agent| agent.runtime_key)
            else {
                return;
            };
            AgentAddr::Runtime(key)
        };

        // A control is only present while the agent is running, so its presence
        // is the running check; cancelling the main agent tears down the whole
        // thread, a subagent only its own subtree (the token's wiring differs).
        if let Some(control) = self.agent_controls.get(&(thread_id, addr)) {
            control.cancel();
        }
    }

    /// Mark the main agent and any still-running subagents of a thread as
    /// cancelled. The dropped run never delivers their own terminal events.
    fn cancel_thread_agents(&mut self, thread_id: ThreadId) {
        let Some(thread) = self.thread_mut(thread_id) else {
            return;
        };
        for agent in std::iter::once(&mut thread.main_agent).chain(thread.subagents.iter_mut()) {
            if agent.status == AgentStatus::Running {
                settle_cancelled_agent(agent, "cancelled by user");
            }
        }
    }

    /// Drop any pending permission prompts belonging to a thread whose run was
    /// cancelled; their requesting tool calls will never resume.
    fn discard_pending_permissions_for_thread(&mut self, thread_id: ThreadId) {
        self.pending_tool_permissions
            .retain(|request| request.thread_id != thread_id);
    }

    fn create_thread(&mut self) {
        let id = self.next_thread_id.next();
        self.threads.push(ThreadState {
            id,
            title: format!("Thread {}", id.display_number()),
            conversation_id: format!("agent-thread-{id}"),
            expanded: true,
            main_agent: AgentNode::main(),
            subagents: Vec::new(),
        });
        self.selected = Selection {
            thread_id: id,
            agent_id: MAIN_AGENT_ID,
        };
        self.focus = Focus::Input;
        self.conversation_scroll = 0;
        self.conversation_cursor = 0;
    }

    fn collapse_all_sidebar(&mut self) {
        for thread in &mut self.threads {
            thread.expanded = false;
            thread.main_agent.expanded = false;
            for agent in &mut thread.subagents {
                agent.expanded = false;
            }
        }
    }

    fn expand_all_sidebar(&mut self) {
        for thread in &mut self.threads {
            thread.expanded = true;
            thread.main_agent.expanded = true;
            for agent in &mut thread.subagents {
                agent.expanded = true;
            }
        }
    }

    fn append_delta(
        &mut self,
        thread_id: ThreadId,
        agent_id: AgentId,
        kind: TextDeltaKind,
        delta: &str,
    ) {
        if delta.is_empty() {
            return;
        }

        let Some(agent) = self.agent_mut(thread_id, agent_id) else {
            return;
        };

        if let Some(last) = agent.messages.last_mut()
            && last.role() == kind.message_role()
            && let Some(content) = last.text_mut()
        {
            content.push_str(delta);
            return;
        }

        agent.messages.push(kind.message(delta));
    }

    fn spawn_nested(&mut self, thread_id: ThreadId, spawned: SpawnNested) {
        let SpawnNested {
            key,
            parent,
            depth,
            task,
            context,
            control,
        } = spawned;

        let parent_id = parent
            .and_then(|parent_key| self.agent_id_by_runtime_key(thread_id, parent_key))
            .unwrap_or(MAIN_AGENT_ID);

        let id = self.next_agent_id.next();
        let label = truncate_chars(&task, 36);

        let mut messages = vec![Message::user(task)];
        if let Some(context) = context.filter(|context| !context.trim().is_empty()) {
            messages.push(Message::system(format!("Context:\n{}", context.trim())));
        }

        self.agent_controls
            .insert((thread_id, AgentAddr::Runtime(key)), control);

        if let Some(thread) = self.thread_mut(thread_id) {
            thread.expanded = true;
            thread.subagents.push(AgentNode {
                id,
                parent_id: Some(parent_id),
                runtime_key: Some(key),
                label: format!("subagent: {label}"),
                depth,
                status: AgentStatus::Running,
                expanded: false,
                token_usage: TokenUsage::default(),
                // The task/context messages are seeded above; a retry rewinds to
                // them and discards only the model's output.
                message_baseline: messages.len(),
                messages,
            });
        }
    }

    fn apply_tool_result_to_agent(
        &mut self,
        thread_id: ThreadId,
        agent_id: AgentId,
        tool_call_id: &str,
        content: &str,
        is_error: bool,
    ) {
        let status = if is_error {
            ToolStatus::Failed
        } else {
            ToolStatus::Finished
        };
        if self.set_tool_status(thread_id, agent_id, tool_call_id, status, Some(content)) {
            return;
        }

        if let Some(agent) = self.agent_mut(thread_id, agent_id) {
            agent
                .messages
                .push(Message::tool_result(content.to_string()));
        }
    }

    fn set_tool_status(
        &mut self,
        thread_id: ThreadId,
        agent_id: AgentId,
        tool_call_id: &str,
        status: ToolStatus,
        result: Option<&str>,
    ) -> bool {
        let Some(agent) = self.agent_mut(thread_id, agent_id) else {
            return false;
        };

        let Some(tool) = agent
            .messages
            .iter_mut()
            .rev()
            .filter_map(|message| message.tool_mut())
            .find(|tool| tool.call_id == tool_call_id)
        else {
            return false;
        };

        if let Some(result) = result {
            tool.result = Some(result.to_string());
        }
        tool.status = status;
        true
    }

    fn collapse_last_reasoning(&mut self, thread_id: ThreadId, agent_id: AgentId) {
        if let Some(agent) = self.agent_mut(thread_id, agent_id)
            && let Some(message) = agent
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.role() == MessageRole::Reasoning)
        {
            message.set_collapsed(true);
        }
    }

    fn toggle_last_reasoning(&mut self) {
        if let Some(agent) = self.agent_mut(self.selected.thread_id, self.selected.agent_id)
            && let Some(message) = agent
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.role() == MessageRole::Reasoning)
        {
            message.set_collapsed(!message.collapsed());
        }
    }

    fn agent(&self, thread_id: ThreadId, agent_id: AgentId) -> Option<&AgentNode> {
        let thread = self.threads.iter().find(|thread| thread.id == thread_id)?;
        if agent_id == MAIN_AGENT_ID {
            Some(&thread.main_agent)
        } else {
            thread.subagents.iter().find(|agent| agent.id == agent_id)
        }
    }

    fn agent_mut(&mut self, thread_id: ThreadId, agent_id: AgentId) -> Option<&mut AgentNode> {
        let thread = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)?;
        if agent_id == MAIN_AGENT_ID {
            Some(&mut thread.main_agent)
        } else {
            thread
                .subagents
                .iter_mut()
                .find(|agent| agent.id == agent_id)
        }
    }

    fn agent_id_by_runtime_key(
        &self,
        thread_id: ThreadId,
        key: RuntimeAgentKey,
    ) -> Option<AgentId> {
        self.threads
            .iter()
            .find(|thread| thread.id == thread_id)?
            .subagents
            .iter()
            .find(|agent| agent.runtime_key == Some(key))
            .map(|agent| agent.id)
    }

    fn remove_agent_control(&mut self, thread_id: ThreadId, addr: AgentAddr) {
        self.agent_controls.remove(&(thread_id, addr));
    }

    fn remove_agent_controls_for_subtree(&mut self, thread_id: ThreadId, root: AgentId) {
        let keys: HashSet<_> = self
            .thread(thread_id)
            .map(|thread| descendant_agent_ids(thread, root))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|agent_id| self.agent(thread_id, agent_id)?.runtime_key)
            .collect();

        self.agent_controls.retain(|(control_thread_id, addr), _| {
            *control_thread_id != thread_id
                || !matches!(addr, AgentAddr::Runtime(key) if keys.contains(key))
        });
    }

    fn cancel_agent_subtree(&mut self, thread_id: ThreadId, root: AgentId, reason: &str) {
        let agent_ids = self
            .thread(thread_id)
            .map(|thread| descendant_agent_ids(thread, root))
            .unwrap_or_default();

        for agent_id in agent_ids {
            if let Some(agent) = self.agent_mut(thread_id, agent_id)
                && agent.status == AgentStatus::Running
            {
                settle_cancelled_agent(agent, reason);
            }
        }
    }

    fn thread(&self, thread_id: ThreadId) -> Option<&ThreadState> {
        self.threads.iter().find(|thread| thread.id == thread_id)
    }

    fn thread_mut(&mut self, thread_id: ThreadId) -> Option<&mut ThreadState> {
        self.threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
    }
}

fn append_agent_sidebar_item(
    items: &mut Vec<SidebarItem>,
    thread: &ThreadState,
    agent: &AgentNode,
    depth: usize,
    selected: Selection,
) {
    let has_children = thread
        .subagents
        .iter()
        .any(|child| child.parent_id == Some(agent.id));
    items.push(SidebarItem {
        thread_id: thread.id,
        agent_id: Some(agent.id),
        label: agent.label.clone(),
        depth,
        expanded: agent.expanded,
        selected: selected.thread_id == thread.id && selected.agent_id == agent.id,
        status: Some(agent.status),
        has_children,
    });
}

fn append_child_sidebar_items(
    items: &mut Vec<SidebarItem>,
    thread: &ThreadState,
    parent_id: AgentId,
    depth: usize,
    selected: Selection,
) {
    for child in thread
        .subagents
        .iter()
        .filter(|agent| agent.parent_id == Some(parent_id))
    {
        append_agent_sidebar_item(items, thread, child, depth, selected);
        if child.expanded {
            append_child_sidebar_items(items, thread, child.id, depth + 1, selected);
        }
    }
}

fn descendant_agent_ids(thread: &ThreadState, root: AgentId) -> Vec<AgentId> {
    let mut ids = vec![root];
    let mut index = 0;
    while index < ids.len() {
        let parent = ids[index];
        ids.extend(
            thread
                .subagents
                .iter()
                .filter(|agent| agent.parent_id == Some(parent))
                .map(|agent| agent.id),
        );
        index += 1;
    }
    ids
}

fn settle_cancelled_agent(agent: &mut AgentNode, reason: &str) {
    collapse_reasoning(agent);
    agent.status = AgentStatus::Cancelled;
    for message in &mut agent.messages {
        if let Some(tool) = message.tool_mut()
            && !tool.status.is_done()
        {
            tool.status = ToolStatus::Failed;
            tool.result = Some(reason.to_string());
        }
    }
    agent.messages.push(Message::error(reason));
}

fn initial_main_messages() -> Vec<Message> {
    vec![Message::system(crate::config::MAIN_AGENT_PREAMBLE)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextDeltaKind {
    Assistant,
    Reasoning,
}

impl TextDeltaKind {
    fn message_role(self) -> MessageRole {
        match self {
            TextDeltaKind::Assistant => MessageRole::Assistant,
            TextDeltaKind::Reasoning => MessageRole::Reasoning,
        }
    }

    fn message(self, content: impl Into<String>) -> Message {
        match self {
            TextDeltaKind::Assistant => Message::Assistant(content.into()),
            TextDeltaKind::Reasoning => Message::Reasoning {
                content: content.into(),
                collapsed: false,
            },
        }
    }
}

/// Settle a restored agent. A run interrupted by exit left it `Running` with no
/// task to resume: the main agent drops to `Idle` so its thread accepts a new
/// prompt, a subagent becomes `Cancelled`, and any tool call still awaiting
/// permission or mid-execution is marked failed. The retry baseline is
/// re-anchored to the loaded transcript, and reasoning is collapsed as at rest.
fn normalize_restored_agent(agent: &mut AgentNode, is_main: bool) {
    if agent.status == AgentStatus::Running {
        agent.status = if is_main {
            AgentStatus::Idle
        } else {
            AgentStatus::Cancelled
        };
    }
    for message in &mut agent.messages {
        if let Some(tool) = message.tool_mut()
            && !tool.status.is_done()
        {
            tool.status = ToolStatus::Failed;
        }
    }
    agent.message_baseline = agent.messages.len();
    collapse_reasoning(agent);
}

fn collapse_reasoning(agent: &mut AgentNode) {
    for message in &mut agent.messages {
        if message.role() == MessageRole::Reasoning {
            message.set_collapsed(true);
        }
    }
}

fn rejected_tool_permission_reason(tool_name: &str) -> String {
    format!("Tool call rejected by user: {tool_name} was not executed")
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn tool_call_summary(name: &str, arguments: &Value) -> String {
    match name {
        "read_file" | "read_pdf" | "list_directory" | "edit_file" | "write_file" => {
            path_tool_summary(name, arguments)
        }
        "subagent" => string_arg(arguments, "task")
            .map(|task| format!("{name} {}", truncate_chars(task, 80)))
            .unwrap_or_else(|| name.to_string()),
        _ => name.to_string(),
    }
}

fn path_tool_summary(name: &str, arguments: &Value) -> String {
    string_arg(arguments, "path")
        .map(|path| format!("{name} {path}"))
        .unwrap_or_else(|| name.to_string())
}

fn string_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn title_from_prompt(current: &str, prompt: &str) -> String {
    if current != "Thread 1" {
        return current.to_string();
    }

    truncate_chars(prompt, 28)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    //! Behavior tests pinning the current reducer/selector/input semantics so the
    //! planned refactors can be shown to be behavior-preserving.
    use super::*;
    use serde_json::json;

    const T: ThreadId = ThreadId::new(0);

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn main_messages(app: &AppState) -> &[Message] {
        &app.threads[0].main_agent.messages
    }

    fn last(app: &AppState) -> &Message {
        main_messages(app).last().expect("a message")
    }

    fn type_str(app: &mut AppState, text: &str) {
        for ch in text.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    fn subagent_control() -> AgentControl {
        let (queue, _receiver) = mpsc::unbounded_channel();
        AgentControl::new(CancellationToken::new(), queue)
    }

    fn subagent_control_with_receiver() -> (AgentControl, mpsc::UnboundedReceiver<String>) {
        let (queue, receiver) = mpsc::unbounded_channel();
        (AgentControl::new(CancellationToken::new(), queue), receiver)
    }

    // Event constructors for the top-level agent.
    fn main_event(event: AgentNodeEvent) -> ThreadEvent {
        ThreadEvent::Agent {
            addr: AgentAddr::Main,
            event,
        }
    }

    fn agent_event(addr: AgentAddr, event: AgentNodeEvent) -> ThreadEvent {
        ThreadEvent::Agent { addr, event }
    }

    fn assistant(delta: &str) -> ThreadEvent {
        main_event(AgentNodeEvent::AssistantDelta {
            delta: delta.into(),
        })
    }

    fn reasoning(delta: &str) -> ThreadEvent {
        main_event(AgentNodeEvent::ReasoningDelta {
            delta: delta.into(),
        })
    }

    fn tool_result(id: &str, content: &str) -> ThreadEvent {
        main_event(AgentNodeEvent::ToolResult {
            id: id.into(),
            content: content.into(),
            is_error: false,
        })
    }

    fn finished(result: Option<&str>) -> ThreadEvent {
        let completion = result
            .map(|result| AgentCompletion::Returned(result.into()))
            .unwrap_or(AgentCompletion::Done);
        main_event(AgentNodeEvent::Finished(completion))
    }

    fn error(msg: &str) -> ThreadEvent {
        main_event(AgentNodeEvent::Error(msg.into()))
    }

    // ----- reducer: main agent -----

    #[test]
    fn fresh_app_has_idle_main_agent_with_preamble() {
        let app = AppState::new();
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Idle);
        assert_eq!(main_messages(&app).len(), 1);
        assert_eq!(main_messages(&app)[0].role(), MessageRole::System);
    }

    #[test]
    fn started_sets_running_without_visible_message() {
        let mut app = AppState::new();
        let message_count = main_messages(&app).len();
        app.apply_thread_event(T, ThreadEvent::MainStarted);
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Running);
        assert_eq!(main_messages(&app).len(), message_count);
    }

    #[test]
    fn assistant_deltas_accumulate_into_one_message() {
        let mut app = AppState::new();
        app.apply_thread_event(T, assistant("Hel"));
        app.apply_thread_event(T, assistant("lo"));
        assert_eq!(last(&app).role(), MessageRole::Assistant);
        assert_eq!(last(&app).content(), "Hello");
    }

    #[test]
    fn assistant_after_reasoning_collapses_the_reasoning() {
        let mut app = AppState::new();
        app.apply_thread_event(T, reasoning("thinking"));
        assert!(!main_messages(&app)[1].collapsed());
        app.apply_thread_event(T, assistant("answer"));
        assert!(main_messages(&app)[1].collapsed(), "reasoning collapses");
        assert_eq!(last(&app).role(), MessageRole::Assistant);
    }

    #[test]
    fn finished_sets_complete_and_collapses_reasoning_without_visible_message() {
        let mut app = AppState::new();
        app.apply_thread_event(T, reasoning("x"));
        let message_count = main_messages(&app).len();
        app.apply_thread_event(T, finished(None));
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Complete);
        assert!(main_messages(&app)[1].collapsed());
        assert_eq!(main_messages(&app).len(), message_count);
    }

    #[test]
    fn error_sets_error_status_and_message() {
        let mut app = AppState::new();
        app.apply_thread_event(T, error("boom"));
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Error);
        assert_eq!(last(&app).role(), MessageRole::Error);
        assert_eq!(last(&app).content(), "boom");
    }

    #[test]
    fn tool_call_appends_running_tool_message() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "/tmp" }),
                },
            },
        );
        assert_eq!(last(&app).role(), MessageRole::ToolCall);
        assert_eq!(last(&app).tool_call_id(), Some("t1"));
        assert_eq!(last(&app).tool_status(), ToolStatus::Running);
        assert_eq!(last(&app).tool_summary(), Some("read_file /tmp"));
    }

    #[test]
    fn path_tool_call_summary_includes_path() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "list_directory".into(),
                    arguments: json!({ "path": "/workspace/src" }),
                },
            },
        );
        assert_eq!(
            last(&app).tool_summary(),
            Some("list_directory /workspace/src")
        );
    }

    #[test]
    fn write_file_tool_call_summary_includes_path() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent { addr: AgentAddr::Main, event: AgentNodeEvent::ToolCall {
                id: "t1".into(),
                name: "write_file".into(),
                arguments: json!({ "path": "/workspace/src/new.rs", "content": "", "overwrite": false }),
             } },
        );
        assert_eq!(
            last(&app).tool_summary(),
            Some("write_file /workspace/src/new.rs")
        );
    }

    #[test]
    fn subagent_tool_call_summary_includes_task() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent { addr: AgentAddr::Main, event: AgentNodeEvent::ToolCall {
                id: "t1".into(),
                name: "subagent".into(),
                arguments: json!({ "task": "review the parser module", "context": "src/parser.rs" }),
             } },
        );
        assert_eq!(
            last(&app).tool_summary(),
            Some("subagent review the parser module")
        );
    }

    #[test]
    fn tool_result_success_marks_finished() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    arguments: json!({}),
                },
            },
        );
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolResult {
                    id: "t1".into(),
                    content: "file contents".into(),
                    is_error: false,
                },
            },
        );
        assert_eq!(last(&app).tool_status(), ToolStatus::Finished);
        assert_eq!(last(&app).tool_result_text(), Some("file contents"));
    }

    #[tokio::test]
    async fn permission_request_marks_tool_call_and_accept_resumes_it() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/a" }),
                },
            },
        );
        let (respond_to, response) = oneshot::channel();

        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t1".into(),
            "edit_file".into(),
            json!({ "path": "/tmp/a" }),
            respond_to,
        ));
        assert_eq!(last(&app).tool_status(), ToolStatus::AwaitingPermission);
        assert_eq!(
            app.pending_tool_permission().unwrap().summary(),
            "edit_file /tmp/a"
        );

        app.handle_key(key(KeyCode::Char('a')));

        assert_eq!(response.await.unwrap(), ToolPermissionResponse::Allow);
        assert_eq!(last(&app).tool_status(), ToolStatus::Running);
        assert!(app.pending_tool_permission().is_none());
    }

    #[tokio::test]
    async fn accept_once_does_not_allow_future_calls_for_same_tool() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/a" }),
                },
            },
        );
        let (respond_to, response) = oneshot::channel();
        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t1".into(),
            "edit_file".into(),
            json!({ "path": "/tmp/a" }),
            respond_to,
        ));

        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(response.await.unwrap(), ToolPermissionResponse::Allow);

        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t2".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/b" }),
                },
            },
        );
        let (respond_to, mut response) = oneshot::channel();
        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t2".into(),
            "edit_file".into(),
            json!({ "path": "/tmp/b" }),
            respond_to,
        ));

        assert_eq!(app.pending_tool_permission().unwrap().id, "t2");
        assert!(response.try_recv().is_err(), "second call still prompts");
    }

    #[tokio::test]
    async fn accept_always_allows_same_tool_only() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/a" }),
                },
            },
        );
        let (respond_to, response) = oneshot::channel();
        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t1".into(),
            "edit_file".into(),
            json!({ "path": "/tmp/a" }),
            respond_to,
        ));

        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(response.await.unwrap(), ToolPermissionResponse::AllowAlways);
        assert!(app.pending_tool_permission().is_none());

        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t2".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/b" }),
                },
            },
        );
        let (respond_to, response) = oneshot::channel();
        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t2".into(),
            "edit_file".into(),
            json!({ "path": "/tmp/b" }),
            respond_to,
        ));
        assert_eq!(response.await.unwrap(), ToolPermissionResponse::AllowAlways);
        assert!(app.pending_tool_permission().is_none());

        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t3".into(),
                    name: "write_file".into(),
                    arguments: json!({ "path": "/tmp/c", "content": "", "overwrite": false }),
                },
            },
        );
        let (respond_to, mut response) = oneshot::channel();
        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t3".into(),
            "write_file".into(),
            json!({ "path": "/tmp/c", "content": "", "overwrite": false }),
            respond_to,
        ));

        assert_eq!(app.pending_tool_permission().unwrap().name, "write_file");
        assert!(
            response.try_recv().is_err(),
            "different write tool still prompts"
        );
    }

    #[tokio::test]
    async fn rejecting_permission_marks_tool_call_failed() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/a" }),
                },
            },
        );
        let (respond_to, response) = oneshot::channel();

        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t1".into(),
            "edit_file".into(),
            json!({ "path": "/tmp/a" }),
            respond_to,
        ));
        app.handle_key(key(KeyCode::Char('r')));

        match response.await.unwrap() {
            ToolPermissionResponse::Reject { reason } => {
                assert!(reason.contains("edit_file"));
                assert_eq!(last(&app).tool_result_text(), Some(reason.as_str()));
            }
            other => panic!("unexpected response: {other:?}"),
        }
        assert_eq!(last(&app).tool_status(), ToolStatus::Failed);
        assert!(app.pending_tool_permission().is_none());
    }

    #[test]
    fn tool_result_status_follows_is_error_flag_not_content() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    arguments: json!({}),
                },
            },
        );
        // The status comes from the `is_error` flag now, not by sniffing the
        // content: a successful result is finished even if its text happens to
        // look like an error string.
        app.apply_thread_event(T, tool_result("t1", "ToolCallError: no such file"));
        assert_eq!(last(&app).tool_status(), ToolStatus::Finished);
    }

    #[test]
    fn explicit_tool_result_error_marks_failed_without_legacy_prefix() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "edit_file".into(),
                    arguments: json!({}),
                },
            },
        );
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolResult {
                    id: "t1".into(),
                    content: "old_text was not found in the file".into(),
                    is_error: true,
                },
            },
        );
        assert_eq!(last(&app).tool_status(), ToolStatus::Failed);
    }

    #[test]
    fn parallel_tool_results_mark_each_matching_call_finished() {
        let mut app = AppState::new();
        for id in ["t1", "t2", "t3"] {
            app.apply_thread_event(
                T,
                ThreadEvent::Agent {
                    addr: AgentAddr::Main,
                    event: AgentNodeEvent::ToolCall {
                        id: id.into(),
                        name: "read_file".into(),
                        arguments: json!({ "path": id }),
                    },
                },
            );
        }

        // Parallel calls may complete in any order; each result must update its
        // own call rather than only the most recent visible tool call.
        app.apply_thread_event(T, tool_result("t2", "second"));
        app.apply_thread_event(T, tool_result("t1", "first"));
        app.apply_thread_event(T, tool_result("t3", "third"));

        let tool_calls: Vec<_> = main_messages(&app)
            .iter()
            .filter(|message| message.role() == MessageRole::ToolCall)
            .collect();
        assert_eq!(tool_calls.len(), 3);
        assert!(
            tool_calls
                .iter()
                .all(|message| message.tool_status() == ToolStatus::Finished)
        );
        assert_eq!(tool_calls[0].tool_result_text(), Some("first"));
        assert_eq!(tool_calls[1].tool_result_text(), Some("second"));
        assert_eq!(tool_calls[2].tool_result_text(), Some("third"));
    }

    #[test]
    fn tool_result_without_matching_call_is_appended() {
        let mut app = AppState::new();
        app.apply_thread_event(T, tool_result("missing", "orphan"));
        assert_eq!(last(&app).role(), MessageRole::ToolResult);
        assert_eq!(last(&app).content(), "orphan");
    }

    // ----- reducer: nested agents -----

    fn nested_started(
        key: RuntimeAgentKey,
        parent: Option<RuntimeAgentKey>,
        depth: AgentDepth,
        task: &str,
    ) -> ThreadEvent {
        ThreadEvent::SpawnedSubagent {
            key,
            parent,
            depth,
            task: task.into(),
            context: None,
            control: subagent_control(),
        }
    }

    fn nested_assistant(key: RuntimeAgentKey, delta: &str) -> ThreadEvent {
        ThreadEvent::Agent {
            addr: AgentAddr::Runtime(key),
            event: AgentNodeEvent::AssistantDelta {
                delta: delta.into(),
            },
        }
    }

    fn nested_finished(key: RuntimeAgentKey, result: &str) -> ThreadEvent {
        ThreadEvent::Agent {
            addr: AgentAddr::Runtime(key),
            event: AgentNodeEvent::Finished(AgentCompletion::Returned(result.into())),
        }
    }

    fn nested_error(key: RuntimeAgentKey, error: &str) -> ThreadEvent {
        ThreadEvent::Agent {
            addr: AgentAddr::Runtime(key),
            event: AgentNodeEvent::Error(error.into()),
        }
    }

    fn usage(
        addr: AgentAddr,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
    ) -> ThreadEvent {
        agent_event(
            addr,
            AgentNodeEvent::Usage(TokenUsage {
                input_tokens,
                output_tokens,
                total_tokens,
            }),
        )
    }

    #[test]
    fn nested_started_creates_subagent_under_main() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            nested_started(RuntimeAgentKey::new(1), None, 1, "do thing"),
        );

        let subs = &app.threads[0].subagents;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].parent_id, Some(MAIN_AGENT_ID));
        assert_eq!(subs[0].runtime_key, Some(RuntimeAgentKey::new(1)));
        assert_eq!(subs[0].status, AgentStatus::Running);
        assert_eq!(subs[0].depth, 1);
        assert_eq!(subs[0].label, "subagent: do thing");
        assert!(
            app.threads[0].expanded,
            "thread expands to reveal the subagent"
        );
        assert_eq!(subs[0].messages[0].role(), MessageRole::User);
        assert_eq!(subs[0].messages[0].content(), "do thing");
    }

    #[test]
    fn nested_started_with_context_adds_system_message() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            ThreadEvent::SpawnedSubagent {
                key: RuntimeAgentKey::new(1),
                parent: None,
                depth: 1,
                task: "task".into(),
                context: Some("important ctx".into()),
                control: subagent_control(),
            },
        );
        let messages = &app.threads[0].subagents[0].messages;
        assert!(
            messages
                .iter()
                .any(|m| m.role() == MessageRole::System && m.content().contains("important ctx"))
        );
    }

    #[test]
    fn worker_nests_under_its_parent_subagent() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            nested_started(RuntimeAgentKey::new(1), None, 1, "parent"),
        );
        let parent_id = app.threads[0].subagents[0].id;
        app.apply_thread_event(
            T,
            nested_started(
                RuntimeAgentKey::new(2),
                Some(RuntimeAgentKey::new(1)),
                2,
                "child",
            ),
        );

        let worker = app.threads[0]
            .subagents
            .iter()
            .find(|a| a.runtime_key == Some(RuntimeAgentKey::new(2)))
            .expect("worker node");
        assert_eq!(worker.parent_id, Some(parent_id));
        assert_eq!(worker.depth, 2);
    }

    #[test]
    fn nested_agents_recurse_arbitrarily_deep() {
        let mut app = AppState::new();
        // main(0) -> a(1) -> b(2) -> c(3): the tree nests past the old two-level
        // cap, each child one level deeper than its parent.
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "a"));
        app.apply_thread_event(
            T,
            nested_started(
                RuntimeAgentKey::new(2),
                Some(RuntimeAgentKey::new(1)),
                2,
                "b",
            ),
        );
        app.apply_thread_event(
            T,
            nested_started(
                RuntimeAgentKey::new(3),
                Some(RuntimeAgentKey::new(2)),
                3,
                "c",
            ),
        );

        let subs = &app.threads[0].subagents;
        let find = |key| {
            subs.iter()
                .find(|a| a.runtime_key == Some(key))
                .expect("node")
        };
        assert_eq!(find(RuntimeAgentKey::new(1)).depth, 1);
        assert_eq!(find(RuntimeAgentKey::new(2)).depth, 2);
        assert_eq!(find(RuntimeAgentKey::new(3)).depth, 3);
        assert_eq!(
            find(RuntimeAgentKey::new(2)).parent_id,
            Some(find(RuntimeAgentKey::new(1)).id)
        );
        assert_eq!(
            find(RuntimeAgentKey::new(3)).parent_id,
            Some(find(RuntimeAgentKey::new(2)).id)
        );
    }

    #[test]
    fn nested_deltas_and_completion_target_the_right_node() {
        let mut app = AppState::new();
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "task"));
        app.apply_thread_event(T, nested_assistant(RuntimeAgentKey::new(1), "hi"));
        app.apply_thread_event(T, nested_finished(RuntimeAgentKey::new(1), "done"));

        let node = &app.threads[0].subagents[0];
        assert_eq!(node.status, AgentStatus::Complete);
        assert!(
            node.messages
                .iter()
                .any(|m| m.role() == MessageRole::Assistant && m.content() == "hi")
        );
        assert_eq!(
            node.messages.last().unwrap().role(),
            MessageRole::ToolResult
        );
        assert_eq!(node.messages.last().unwrap().content(), "done");
    }

    #[test]
    fn nested_error_marks_node_error() {
        let mut app = AppState::new();
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "task"));
        app.apply_thread_event(T, nested_error(RuntimeAgentKey::new(1), "bad"));
        assert_eq!(app.threads[0].subagents[0].status, AgentStatus::Error);
    }

    #[test]
    fn cancelling_selected_subagent_cancels_only_its_control() {
        let mut app = AppState::new();
        let main_cancel = submit(&mut app, "go");
        let subagent_cancel = CancellationToken::new();
        let (queue, _receiver) = mpsc::unbounded_channel();
        app.apply_thread_event(
            T,
            ThreadEvent::SpawnedSubagent {
                key: RuntimeAgentKey::new(1),
                parent: None,
                depth: 1,
                task: "sub".into(),
                context: None,
                control: AgentControl::new(subagent_cancel.clone(), queue),
            },
        );
        app.selected.agent_id = app.threads[0].subagents[0].id;

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

        assert!(subagent_cancel.is_cancelled());
        assert!(
            !main_cancel.is_cancelled(),
            "targeted subagent cancel must let the parent run continue"
        );
    }

    #[test]
    fn cancelled_subagent_marks_subtree_and_running_tools_failed() {
        let mut app = AppState::new();
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "sub"));
        app.apply_thread_event(
            T,
            agent_event(
                AgentAddr::Runtime(RuntimeAgentKey::new(1)),
                AgentNodeEvent::ToolCall {
                    id: "child-tool".into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "/tmp/a" }),
                },
            ),
        );

        app.apply_thread_event(
            T,
            agent_event(
                AgentAddr::Runtime(RuntimeAgentKey::new(1)),
                AgentNodeEvent::Cancelled("subagent cancelled by user".into()),
            ),
        );

        let child = &app.threads[0].subagents[0];
        assert_eq!(child.status, AgentStatus::Cancelled);
        let tool = child
            .messages
            .iter()
            .find(|message| message.tool_call_id() == Some("child-tool"))
            .expect("running tool card");
        assert_eq!(tool.tool_status(), ToolStatus::Failed);
        assert_eq!(
            tool.tool_result_text(),
            Some("subagent cancelled by user"),
            "cancelled tools show a terminal result"
        );
    }

    #[test]
    fn input_to_selected_running_subagent_queues_a_message_without_a_visible_message() {
        let mut app = AppState::new();
        let (control, mut receiver) = subagent_control_with_receiver();
        app.apply_thread_event(
            T,
            ThreadEvent::SpawnedSubagent {
                key: RuntimeAgentKey::new(1),
                parent: None,
                depth: 1,
                task: "sub".into(),
                context: None,
                control,
            },
        );
        let subagent_id = app.threads[0].subagents[0].id;
        app.selected.agent_id = subagent_id;
        type_str(&mut app, "focus on tests");

        app.handle_key(key(KeyCode::Enter));

        // The message is queued for the run, the input clears, but no visible
        // bubble appears yet — it lands when the loop injects it at a boundary.
        assert_eq!(receiver.try_recv().unwrap(), "focus on tests");
        assert_eq!(app.input.value, "");
        assert!(
            app.threads[0].subagents[0]
                .messages
                .iter()
                .all(|message| message.text() != Some("focus on tests")),
            "no visible message until the boundary event arrives"
        );

        // The agent loop reports the injection at a turn boundary; only now does
        // the user bubble appear in the transcript.
        app.apply_thread_event(
            T,
            agent_event(
                AgentAddr::Runtime(RuntimeAgentKey::new(1)),
                AgentNodeEvent::UserMessage {
                    text: "focus on tests".into(),
                },
            ),
        );
        assert!(
            app.threads[0].subagents[0]
                .messages
                .iter()
                .any(|message| message.role() == MessageRole::User
                    && message.content() == "focus on tests")
        );
    }

    #[test]
    fn message_to_running_main_agent_is_queued_not_started_as_a_new_run() {
        let mut app = AppState::new();
        type_str(&mut app, "first");
        // Keep the queue receiver alive, as the runtime does, so the enqueue
        // succeeds (a dropped receiver would fail the send).
        let (cancel, mut queued_messages) = match app.handle_key(key(KeyCode::Enter)) {
            SubmitResult::Submitted {
                cancel,
                queued_messages,
                ..
            } => (cancel, queued_messages),
            other => panic!("expected Submitted, got {other:?}"),
        };
        assert!(app.active_agent_running());

        // Typing while the main agent runs queues a message instead of being
        // rejected; the run keeps going and no second run is started.
        type_str(&mut app, "second");
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)),
            SubmitResult::None
        ));
        assert_eq!(app.input.value, "");
        assert_eq!(queued_messages.try_recv().unwrap(), "second");
        assert!(!cancel.is_cancelled());
        assert!(app.active_agent_running());

        // No visible bubble for the queued message yet — it appears only when the
        // loop injects it at a turn boundary.
        assert!(
            main_messages(&app)
                .iter()
                .all(|message| message.text() != Some("second")),
            "queued message is not shown until injected"
        );

        app.apply_thread_event(
            T,
            main_event(AgentNodeEvent::UserMessage {
                text: "second".into(),
            }),
        );
        assert_eq!(last(&app).role(), MessageRole::User);
        assert_eq!(last(&app).content(), "second");
    }

    #[test]
    fn user_message_event_lands_after_in_progress_reasoning() {
        let mut app = AppState::new();
        let _ = submit(&mut app, "go");
        // messages: [System(0), User(1) "go", Reasoning(2)]
        app.apply_thread_event(T, reasoning("pondering"));
        assert!(!main_messages(&app)[2].collapsed());

        app.apply_thread_event(
            T,
            main_event(AgentNodeEvent::UserMessage {
                text: "actually, do this".into(),
            }),
        );

        // The streaming reasoning block is collapsed and the user bubble appended
        // after it as a single new message — the reasoning is not split in two.
        assert!(main_messages(&app)[2].collapsed());
        let reasoning_blocks = main_messages(&app)
            .iter()
            .filter(|message| message.role() == MessageRole::Reasoning)
            .count();
        assert_eq!(reasoning_blocks, 1, "reasoning is not split");
        assert_eq!(last(&app).role(), MessageRole::User);
        assert_eq!(last(&app).content(), "actually, do this");
    }

    #[test]
    fn nested_event_for_unknown_key_is_ignored() {
        let mut app = AppState::new();
        app.apply_thread_event(T, nested_assistant(RuntimeAgentKey::new(99), "x"));
        assert_eq!(app.threads[0].subagents.len(), 0);
    }

    #[test]
    fn thread_token_usage_includes_nested_agents_recursively() {
        let mut app = AppState::new();
        app.apply_thread_event(
            T,
            nested_started(RuntimeAgentKey::new(1), None, 1, "subagent"),
        );
        app.apply_thread_event(
            T,
            nested_started(
                RuntimeAgentKey::new(2),
                Some(RuntimeAgentKey::new(1)),
                2,
                "worker",
            ),
        );

        app.apply_thread_event(T, usage(AgentAddr::Main, 10, 5, 15));
        app.apply_thread_event(
            T,
            usage(AgentAddr::Runtime(RuntimeAgentKey::new(1)), 20, 7, 27),
        );
        app.apply_thread_event(
            T,
            usage(AgentAddr::Runtime(RuntimeAgentKey::new(2)), 3, 4, 7),
        );

        let total = app.threads[0].total_token_usage();
        assert_eq!(total.input_tokens, 33);
        assert_eq!(total.output_tokens, 16);
        assert_eq!(total.total_tokens, 49);
    }

    // ----- selectors -----

    #[test]
    fn sidebar_items_reflect_nesting_depth() {
        let mut app = AppState::new();
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "sub"));
        app.apply_thread_event(
            T,
            nested_started(
                RuntimeAgentKey::new(2),
                Some(RuntimeAgentKey::new(1)),
                2,
                "work",
            ),
        );
        // Expand the subagent so its worker child is rendered.
        app.threads[0]
            .subagents
            .iter_mut()
            .for_each(|a| a.expanded = true);

        let depths: Vec<usize> = app.sidebar_items().iter().map(|i| i.depth).collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn collapsible_indices_cover_reasoning_and_tool_calls_only() {
        let mut app = AppState::new();
        app.apply_thread_event(T, reasoning("r"));
        app.apply_thread_event(T, assistant("a"));
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t".into(),
                    name: "n".into(),
                    arguments: json!({}),
                },
            },
        );
        // messages: [System(0), Reasoning(1), Assistant(2), ToolCall(3)]
        assert_eq!(app.collapsible_message_indices(), vec![1, 3]);
        assert_eq!(app.selected_collapsible_message_index(), Some(1));
    }

    // ----- input & focus -----

    #[test]
    fn tab_cycles_focus_and_esc_returns_to_input() {
        let mut app = AppState::new();
        assert_eq!(app.focus, Focus::Input);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sidebar);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Conversation);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Input);

        app.focus = Focus::Sidebar;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn typing_and_backspace_edit_the_input() {
        let mut app = AppState::new();
        type_str(&mut app, "hi");
        assert_eq!(app.input.value, "hi");
        assert_eq!(app.input.cursor, 2);
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input.value, "h");
        assert_eq!(app.input.cursor, 1);
    }

    #[test]
    fn submitting_a_prompt_returns_submitted_and_resets_input() {
        let mut app = AppState::new();
        type_str(&mut app, "hello");
        let result = app.handle_key(key(KeyCode::Enter));
        match result {
            SubmitResult::Submitted {
                prompt, thread_id, ..
            } => {
                assert_eq!(prompt, "hello");
                assert_eq!(thread_id, T);
            }
            other => panic!("expected Submitted, got {other:?}"),
        }
        assert_eq!(app.input.value, "");
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Running);
        assert_eq!(last(&app).role(), MessageRole::User);
        assert_eq!(last(&app).content(), "hello");
        assert_eq!(app.threads[0].title, "hello");
    }

    #[test]
    fn submitting_empty_input_is_a_noop() {
        let mut app = AppState::new();
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)),
            SubmitResult::None
        ));
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Idle);
    }

    // ----- cancellation -----

    fn submit(app: &mut AppState, prompt: &str) -> CancellationToken {
        type_str(app, prompt);
        match app.handle_key(key(KeyCode::Enter)) {
            SubmitResult::Submitted { cancel, .. } => cancel,
            other => panic!("expected Submitted, got {other:?}"),
        }
    }

    #[test]
    fn cancel_keybind_cancels_the_running_thread_token() {
        let mut app = AppState::new();
        let cancel = submit(&mut app, "do work");
        assert!(!cancel.is_cancelled());
        assert!(app.active_agent_running());

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(cancel.is_cancelled(), "Ctrl+X cancels the run's token");
    }

    #[test]
    fn cancel_keybind_is_a_noop_without_a_running_agent() {
        let mut app = AppState::new();
        // No active run: cancelling must not panic or change state.
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Idle);
    }

    #[test]
    fn cancelled_event_marks_running_agents_and_reenables_submit() {
        let mut app = AppState::new();
        let _ = submit(&mut app, "go");
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "sub"));
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Running);
        assert_eq!(app.threads[0].subagents[0].status, AgentStatus::Running);

        app.apply_thread_event(T, ThreadEvent::Cancelled);

        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Cancelled);
        assert_eq!(
            app.threads[0].subagents[0].status,
            AgentStatus::Cancelled,
            "a running subagent is swept even though it sent no terminal event"
        );
        assert!(
            !app.active_agent_running(),
            "the thread accepts a new prompt after cancellation"
        );
    }

    #[test]
    fn cancelled_event_leaves_already_finished_agents_untouched() {
        let mut app = AppState::new();
        let _ = submit(&mut app, "go");
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "sub"));
        app.apply_thread_event(T, nested_finished(RuntimeAgentKey::new(1), "done"));
        assert_eq!(app.threads[0].subagents[0].status, AgentStatus::Complete);

        app.apply_thread_event(T, ThreadEvent::Cancelled);

        assert_eq!(
            app.threads[0].subagents[0].status,
            AgentStatus::Complete,
            "a finished subagent keeps its status"
        );
    }

    #[tokio::test]
    async fn cancelling_drops_pending_permissions_for_the_thread() {
        let mut app = AppState::new();
        let _ = submit(&mut app, "go");
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/a" }),
                },
            },
        );
        let (respond_to, _response) = oneshot::channel();
        app.apply_tool_permission_request(PendingToolPermission::new(
            T,
            AgentAddr::Main,
            "t1".into(),
            "edit_file".into(),
            json!({ "path": "/tmp/a" }),
            respond_to,
        ));
        assert!(app.pending_tool_permission().is_some());

        app.apply_thread_event(T, ThreadEvent::Cancelled);
        assert!(
            app.pending_tool_permission().is_none(),
            "a cancelled run's pending prompts are discarded"
        );
    }

    // ----- persistence restore -----

    #[test]
    fn restore_with_no_threads_falls_back_to_fresh_app() {
        let app = AppState::restored(
            Vec::new(),
            ThreadId::new(5),
            AgentId::new(9),
            vec!["edit_file".into()],
        );
        assert_eq!(app.threads.len(), 1);
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Idle);
    }

    #[test]
    fn restore_settles_in_flight_run_and_reanchors_baseline() {
        let mut app = AppState::new();
        let _ = submit(&mut app, "go");
        app.apply_thread_event(T, nested_started(RuntimeAgentKey::new(1), None, 1, "sub"));
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t1".into(),
                    name: "edit_file".into(),
                    arguments: json!({ "path": "/tmp/a" }),
                },
            },
        );
        assert!(app.active_agent_running());

        let (next_thread_id, next_agent_id) = app.next_ids();
        let restored = AppState::restored(
            app.snapshot_threads(),
            next_thread_id,
            next_agent_id,
            app.always_allowed_tools(),
        );

        let main = &restored.threads[0].main_agent;
        assert_eq!(
            main.status,
            AgentStatus::Idle,
            "main run no longer in flight"
        );
        assert_eq!(
            restored.threads[0].subagents[0].status,
            AgentStatus::Cancelled,
            "an orphaned subagent is cancelled"
        );
        let tool = main
            .messages
            .iter()
            .find(|message| message.role() == MessageRole::ToolCall)
            .expect("the tool call survives the round-trip");
        assert_eq!(
            tool.tool_status(),
            ToolStatus::Failed,
            "an interrupted tool call is marked failed"
        );
        assert_eq!(
            main.message_baseline,
            main.messages.len(),
            "the retry baseline is re-anchored to the loaded transcript"
        );
        assert!(
            !restored.active_agent_running(),
            "the restored session accepts a new prompt"
        );
    }

    #[test]
    fn restored_node_serialization_omits_the_transient_baseline() {
        // `message_baseline` is per-run state, not session state: it must not be
        // written, and a node loaded without it still deserializes.
        let node = AgentNode::main();
        let json = serde_json::to_string(&node).expect("serialize");
        assert!(
            !json.contains("message_baseline"),
            "baseline is not persisted"
        );
        let back: AgentNode = serde_json::from_str(&json).expect("deserialize without baseline");
        assert_eq!(back.message_baseline, 0, "absent baseline defaults to zero");
    }

    #[test]
    fn conversation_focus_navigates_and_toggles_collapsibles() {
        let mut app = AppState::new();
        app.apply_thread_event(T, reasoning("r"));
        app.apply_thread_event(
            T,
            ThreadEvent::Agent {
                addr: AgentAddr::Main,
                event: AgentNodeEvent::ToolCall {
                    id: "t".into(),
                    name: "n".into(),
                    arguments: json!({}),
                },
            },
        );
        app.focus = Focus::Conversation;

        // Two collapsibles: cursor moves within [0, 1] and clamps.
        assert_eq!(app.conversation_cursor, 0);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.conversation_cursor, 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.conversation_cursor, 1, "clamped at last");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.conversation_cursor, 0);

        let index = app.selected_collapsible_message_index().unwrap();
        let before = app.threads[0].main_agent.messages[index].collapsed();
        app.handle_key(key(KeyCode::Enter));
        let after = app.threads[0].main_agent.messages[index].collapsed();
        assert_ne!(before, after, "Enter toggles the selected collapsible");
    }
}
