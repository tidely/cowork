use std::collections::{HashSet, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use tokio::sync::oneshot;

pub type ThreadId = usize;
pub type AgentId = usize;
pub type RuntimeAgentKey = u64;

pub const MAIN_AGENT_ID: AgentId = 0;

/// Nesting depth of an agent in the hierarchy. The top-level assistant is 0;
/// each `subagent` call spawns a child one level deeper. A plain count rather
/// than a fixed enum so the tree can recurse to an arbitrary (bounded) depth.
pub type AgentDepth = usize;

pub const MAIN_AGENT_DEPTH: AgentDepth = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
    Complete,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub collapsed: bool,
    pub tool_call_id: Option<String>,
    pub tool_result: Option<String>,
    pub tool_status: ToolStatus,
}

impl Message {
    fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            collapsed: false,
            tool_call_id: None,
            tool_result: None,
            tool_status: ToolStatus::Running,
        }
    }

    fn tool_call(id: String, name: String, arguments: &Value) -> Self {
        Self {
            role: MessageRole::ToolCall,
            content: format!(
                "{}\n{}",
                tool_call_summary(&name, arguments),
                pretty_json(arguments)
            ),
            collapsed: true,
            tool_call_id: Some(id),
            tool_result: None,
            tool_status: ToolStatus::Running,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    fn add(&mut self, other: TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
    }
}

#[derive(Debug, Clone)]
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
}

impl AgentNode {
    fn main() -> Self {
        Self {
            id: MAIN_AGENT_ID,
            parent_id: None,
            runtime_key: None,
            label: "Main Agent".to_string(),
            depth: MAIN_AGENT_DEPTH,
            status: AgentStatus::Idle,
            expanded: true,
            token_usage: TokenUsage::default(),
            messages: initial_main_messages(),
        }
    }
}

#[derive(Debug, Clone)]
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
            usage.add(agent.token_usage);
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
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPermissionResponse {
    Allow,
    AllowAlways,
    Reject { reason: String },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAddr {
    Main,
    Runtime(RuntimeAgentKey),
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The top-level prompt run has begun.
    Started,
    /// A subagent stream has begun; creates its node in the tree.
    Spawned {
        key: RuntimeAgentKey,
        parent: Option<RuntimeAgentKey>,
        depth: AgentDepth,
        task: String,
        context: Option<String>,
    },
    AssistantDelta {
        addr: AgentAddr,
        delta: String,
    },
    ReasoningDelta {
        addr: AgentAddr,
        delta: String,
    },
    ToolCall {
        addr: AgentAddr,
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        addr: AgentAddr,
        id: String,
        content: String,
    },
    Usage {
        addr: AgentAddr,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
    },
    /// An agent finished. `result` is the subagent's returned text (shown as a
    /// tool result); `None` marks the top-level agent done.
    Finished {
        addr: AgentAddr,
        result: Option<String>,
    },
    Error {
        addr: AgentAddr,
        error: String,
    },
}

impl AppState {
    pub fn new() -> Self {
        Self {
            threads: vec![ThreadState {
                id: 0,
                title: "Thread 1".to_string(),
                conversation_id: "agent-thread-0".to_string(),
                expanded: true,
                main_agent: AgentNode::main(),
                subagents: Vec::new(),
            }],
            selected: Selection {
                thread_id: 0,
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
            next_thread_id: 1,
            next_agent_id: 1,
        }
    }

    pub fn active_agent_running(&self) -> bool {
        self.agent(self.selected.thread_id, MAIN_AGENT_ID)
            .is_some_and(|agent| agent.status == AgentStatus::Running)
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

    pub fn apply_agent_event(&mut self, thread_id: ThreadId, event: AgentEvent) {
        match event {
            AgentEvent::Started => {
                if let Some(agent) = self.agent_mut(thread_id, MAIN_AGENT_ID) {
                    agent.status = AgentStatus::Running;
                }
            }
            AgentEvent::Spawned {
                key,
                parent,
                depth,
                task,
                context,
            } => self.spawn_nested(thread_id, key, parent, depth, task, context),
            AgentEvent::AssistantDelta { addr, delta } => {
                if let Some(agent_id) = self.resolve_addr(thread_id, addr) {
                    self.collapse_last_reasoning(thread_id, agent_id);
                    self.append_delta(thread_id, agent_id, MessageRole::Assistant, &delta);
                }
            }
            AgentEvent::ReasoningDelta { addr, delta } => {
                if let Some(agent_id) = self.resolve_addr(thread_id, addr) {
                    self.append_delta(thread_id, agent_id, MessageRole::Reasoning, &delta);
                }
            }
            AgentEvent::ToolCall {
                addr,
                id,
                name,
                arguments,
            } => {
                if let Some(agent_id) = self.resolve_addr(thread_id, addr) {
                    self.collapse_last_reasoning(thread_id, agent_id);
                    if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                        agent
                            .messages
                            .push(Message::tool_call(id, name, &arguments));
                    }
                }
            }
            AgentEvent::ToolResult { addr, id, content } => {
                if let Some(agent_id) = self.resolve_addr(thread_id, addr) {
                    self.collapse_last_reasoning(thread_id, agent_id);
                    self.apply_tool_result_to_agent(thread_id, agent_id, &id, &content);
                }
            }
            AgentEvent::Usage {
                addr,
                input_tokens,
                output_tokens,
                total_tokens,
            } => {
                if let Some(agent_id) = self.resolve_addr(thread_id, addr) {
                    self.collapse_last_reasoning(thread_id, agent_id);
                    if let Some(agent) = self.agent_mut(thread_id, agent_id) {
                        agent.token_usage.add(TokenUsage {
                            input_tokens,
                            output_tokens,
                            total_tokens,
                        });
                    }
                }
            }
            AgentEvent::Finished { addr, result } => {
                if let Some(agent_id) = self.resolve_addr(thread_id, addr)
                    && let Some(agent) = self.agent_mut(thread_id, agent_id)
                {
                    agent.status = AgentStatus::Complete;
                    collapse_reasoning(agent);
                    if let Some(result) = result {
                        agent
                            .messages
                            .push(Message::new(MessageRole::ToolResult, result));
                    }
                }
            }
            AgentEvent::Error { addr, error } => {
                if let Some(agent_id) = self.resolve_addr(thread_id, addr)
                    && let Some(agent) = self.agent_mut(thread_id, agent_id)
                {
                    agent.status = AgentStatus::Error;
                    collapse_reasoning(agent);
                    agent.messages.push(Message::new(MessageRole::Error, error));
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
                    .filter(|(_, message)| {
                        matches!(message.role, MessageRole::Reasoning | MessageRole::ToolCall)
                    })
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
            message.collapsed = !message.collapsed;
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
        if prompt.is_empty() || self.active_agent_running() {
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
            .push(Message::new(MessageRole::User, prompt.clone()));
        let conversation_id = thread.conversation_id.clone();

        self.selected.agent_id = MAIN_AGENT_ID;
        self.input.value.clear();
        self.input.cursor = 0;
        self.conversation_scroll = 0;
        self.conversation_cursor = 0;

        SubmitResult::Submitted {
            thread_id: self.selected.thread_id,
            conversation_id,
            prompt,
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

    fn create_thread(&mut self) {
        let id = self.next_thread_id;
        self.next_thread_id += 1;
        self.threads.push(ThreadState {
            id,
            title: format!("Thread {}", id + 1),
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
        role: MessageRole,
        delta: &str,
    ) {
        if delta.is_empty() {
            return;
        }

        let Some(agent) = self.agent_mut(thread_id, agent_id) else {
            return;
        };

        if let Some(last) = agent.messages.last_mut()
            && last.role == role
        {
            last.content.push_str(delta);
            return;
        }

        agent.messages.push(Message::new(role, delta));
    }

    fn spawn_nested(
        &mut self,
        thread_id: ThreadId,
        key: RuntimeAgentKey,
        parent: Option<RuntimeAgentKey>,
        depth: AgentDepth,
        task: String,
        context: Option<String>,
    ) {
        let parent_id = parent
            .and_then(|parent_key| self.agent_id_by_runtime_key(thread_id, parent_key))
            .unwrap_or(MAIN_AGENT_ID);

        let id = self.next_agent_id;
        self.next_agent_id += 1;
        let label = truncate_chars(&task, 36);

        let mut messages = vec![Message::new(MessageRole::User, task)];
        if let Some(context) = context.filter(|context| !context.trim().is_empty()) {
            messages.push(Message::new(
                MessageRole::System,
                format!("Context:\n{}", context.trim()),
            ));
        }

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
    ) {
        let status = if is_tool_error(content) {
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
                .push(Message::new(MessageRole::ToolResult, content.to_string()));
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

        let Some(message) = agent
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.tool_call_id.as_deref() == Some(tool_call_id))
        else {
            return false;
        };

        if let Some(result) = result {
            message.tool_result = Some(result.to_string());
        }
        message.tool_status = status;
        true
    }

    fn collapse_last_reasoning(&mut self, thread_id: ThreadId, agent_id: AgentId) {
        if let Some(agent) = self.agent_mut(thread_id, agent_id)
            && let Some(message) = agent
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.role == MessageRole::Reasoning)
        {
            message.collapsed = true;
        }
    }

    fn toggle_last_reasoning(&mut self) {
        if let Some(agent) = self.agent_mut(self.selected.thread_id, self.selected.agent_id)
            && let Some(message) = agent
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.role == MessageRole::Reasoning)
        {
            message.collapsed = !message.collapsed;
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

fn initial_main_messages() -> Vec<Message> {
    vec![Message::new(
        MessageRole::System,
        crate::agent::MAIN_AGENT_PREAMBLE,
    )]
}

fn collapse_reasoning(agent: &mut AgentNode) {
    for message in &mut agent.messages {
        if message.role == MessageRole::Reasoning {
            message.collapsed = true;
        }
    }
}

/// A failed tool call surfaces only as a string in rig's streamed result, prefixed
/// with the error variant's name (see `rig_core::tool::ToolError`/`ToolSetError`).
fn is_tool_error(content: &str) -> bool {
    let content = content.trim_start();
    content.starts_with("ToolCallError:")
        || content.starts_with("ToolNotFoundError:")
        || content.starts_with("JsonError:")
        || content.starts_with("Tool call rejected")
        || content == "Tool call interrupted"
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

    const T: ThreadId = 0;

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

    // Event constructors for the top-level agent.
    fn assistant(delta: &str) -> AgentEvent {
        AgentEvent::AssistantDelta {
            addr: AgentAddr::Main,
            delta: delta.into(),
        }
    }

    fn reasoning(delta: &str) -> AgentEvent {
        AgentEvent::ReasoningDelta {
            addr: AgentAddr::Main,
            delta: delta.into(),
        }
    }

    fn tool_result(id: &str, content: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            addr: AgentAddr::Main,
            id: id.into(),
            content: content.into(),
        }
    }

    fn finished(result: Option<&str>) -> AgentEvent {
        AgentEvent::Finished {
            addr: AgentAddr::Main,
            result: result.map(|s| s.into()),
        }
    }

    fn error(msg: &str) -> AgentEvent {
        AgentEvent::Error {
            addr: AgentAddr::Main,
            error: msg.into(),
        }
    }

    // ----- reducer: main agent -----

    #[test]
    fn fresh_app_has_idle_main_agent_with_preamble() {
        let app = AppState::new();
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Idle);
        assert_eq!(main_messages(&app).len(), 1);
        assert_eq!(main_messages(&app)[0].role, MessageRole::System);
    }

    #[test]
    fn started_sets_running_without_visible_message() {
        let mut app = AppState::new();
        let message_count = main_messages(&app).len();
        app.apply_agent_event(T, AgentEvent::Started);
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Running);
        assert_eq!(main_messages(&app).len(), message_count);
    }

    #[test]
    fn assistant_deltas_accumulate_into_one_message() {
        let mut app = AppState::new();
        app.apply_agent_event(T, assistant("Hel"));
        app.apply_agent_event(T, assistant("lo"));
        assert_eq!(last(&app).role, MessageRole::Assistant);
        assert_eq!(last(&app).content, "Hello");
    }

    #[test]
    fn assistant_after_reasoning_collapses_the_reasoning() {
        let mut app = AppState::new();
        app.apply_agent_event(T, reasoning("thinking"));
        assert!(!main_messages(&app)[1].collapsed);
        app.apply_agent_event(T, assistant("answer"));
        assert!(main_messages(&app)[1].collapsed, "reasoning collapses");
        assert_eq!(last(&app).role, MessageRole::Assistant);
    }

    #[test]
    fn finished_sets_complete_and_collapses_reasoning_without_visible_message() {
        let mut app = AppState::new();
        app.apply_agent_event(T, reasoning("x"));
        let message_count = main_messages(&app).len();
        app.apply_agent_event(T, finished(None));
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Complete);
        assert!(main_messages(&app)[1].collapsed);
        assert_eq!(main_messages(&app).len(), message_count);
    }

    #[test]
    fn error_sets_error_status_and_message() {
        let mut app = AppState::new();
        app.apply_agent_event(T, error("boom"));
        assert_eq!(app.threads[0].main_agent.status, AgentStatus::Error);
        assert_eq!(last(&app).role, MessageRole::Error);
        assert_eq!(last(&app).content, "boom");
    }

    #[test]
    fn tool_call_appends_running_tool_message() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "/tmp" }),
            },
        );
        assert_eq!(last(&app).role, MessageRole::ToolCall);
        assert_eq!(last(&app).tool_call_id.as_deref(), Some("t1"));
        assert_eq!(last(&app).tool_status, ToolStatus::Running);
        assert_eq!(last(&app).content.lines().next(), Some("read_file /tmp"));
    }

    #[test]
    fn path_tool_call_summary_includes_path() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "list_directory".into(),
                arguments: json!({ "path": "/workspace/src" }),
            },
        );
        assert_eq!(
            last(&app).content.lines().next(),
            Some("list_directory /workspace/src")
        );
    }

    #[test]
    fn write_file_tool_call_summary_includes_path() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "write_file".into(),
                arguments: json!({ "path": "/workspace/src/new.rs", "content": "", "overwrite": false }),
            },
        );
        assert_eq!(
            last(&app).content.lines().next(),
            Some("write_file /workspace/src/new.rs")
        );
    }

    #[test]
    fn subagent_tool_call_summary_includes_task() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "subagent".into(),
                arguments: json!({ "task": "review the parser module", "context": "src/parser.rs" }),
            },
        );
        assert_eq!(
            last(&app).content.lines().next(),
            Some("subagent review the parser module")
        );
    }

    #[test]
    fn tool_result_success_marks_finished() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "read_file".into(),
                arguments: json!({}),
            },
        );
        app.apply_agent_event(
            T,
            AgentEvent::ToolResult {
                addr: AgentAddr::Main,
                id: "t1".into(),
                content: "file contents".into(),
            },
        );
        assert_eq!(last(&app).tool_status, ToolStatus::Finished);
        assert_eq!(last(&app).tool_result.as_deref(), Some("file contents"));
    }

    #[tokio::test]
    async fn permission_request_marks_tool_call_and_accept_resumes_it() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "edit_file".into(),
                arguments: json!({ "path": "/tmp/a" }),
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
        assert_eq!(last(&app).tool_status, ToolStatus::AwaitingPermission);
        assert_eq!(
            app.pending_tool_permission().unwrap().summary(),
            "edit_file /tmp/a"
        );

        app.handle_key(key(KeyCode::Char('a')));

        assert_eq!(response.await.unwrap(), ToolPermissionResponse::Allow);
        assert_eq!(last(&app).tool_status, ToolStatus::Running);
        assert!(app.pending_tool_permission().is_none());
    }

    #[tokio::test]
    async fn accept_once_does_not_allow_future_calls_for_same_tool() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "edit_file".into(),
                arguments: json!({ "path": "/tmp/a" }),
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

        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t2".into(),
                name: "edit_file".into(),
                arguments: json!({ "path": "/tmp/b" }),
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
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "edit_file".into(),
                arguments: json!({ "path": "/tmp/a" }),
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

        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t2".into(),
                name: "edit_file".into(),
                arguments: json!({ "path": "/tmp/b" }),
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

        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t3".into(),
                name: "write_file".into(),
                arguments: json!({ "path": "/tmp/c", "content": "", "overwrite": false }),
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
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "edit_file".into(),
                arguments: json!({ "path": "/tmp/a" }),
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
                assert_eq!(last(&app).tool_result.as_deref(), Some(reason.as_str()));
            }
            other => panic!("unexpected response: {other:?}"),
        }
        assert_eq!(last(&app).tool_status, ToolStatus::Failed);
        assert!(app.pending_tool_permission().is_none());
    }

    #[test]
    fn tool_result_error_marks_failed() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t1".into(),
                name: "read_file".into(),
                arguments: json!({}),
            },
        );
        app.apply_agent_event(T, tool_result("t1", "ToolCallError: no such file"));
        assert_eq!(last(&app).tool_status, ToolStatus::Failed);
    }

    #[test]
    fn parallel_tool_results_mark_each_matching_call_finished() {
        let mut app = AppState::new();
        for id in ["t1", "t2", "t3"] {
            app.apply_agent_event(
                T,
                AgentEvent::ToolCall {
                    addr: AgentAddr::Main,
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": id }),
                },
            );
        }

        // Parallel calls may complete in any order; each result must update its
        // own call rather than only the most recent visible tool call.
        app.apply_agent_event(T, tool_result("t2", "second"));
        app.apply_agent_event(T, tool_result("t1", "first"));
        app.apply_agent_event(T, tool_result("t3", "third"));

        let tool_calls: Vec<_> = main_messages(&app)
            .iter()
            .filter(|message| message.role == MessageRole::ToolCall)
            .collect();
        assert_eq!(tool_calls.len(), 3);
        assert!(
            tool_calls
                .iter()
                .all(|message| message.tool_status == ToolStatus::Finished)
        );
        assert_eq!(tool_calls[0].tool_result.as_deref(), Some("first"));
        assert_eq!(tool_calls[1].tool_result.as_deref(), Some("second"));
        assert_eq!(tool_calls[2].tool_result.as_deref(), Some("third"));
    }

    #[test]
    fn tool_result_without_matching_call_is_appended() {
        let mut app = AppState::new();
        app.apply_agent_event(T, tool_result("missing", "orphan"));
        assert_eq!(last(&app).role, MessageRole::ToolResult);
        assert_eq!(last(&app).content, "orphan");
    }

    // ----- reducer: nested agents -----

    fn nested_started(
        key: RuntimeAgentKey,
        parent: Option<RuntimeAgentKey>,
        depth: AgentDepth,
        task: &str,
    ) -> AgentEvent {
        AgentEvent::Spawned {
            key,
            parent,
            depth,
            task: task.into(),
            context: None,
        }
    }

    fn nested_assistant(key: RuntimeAgentKey, delta: &str) -> AgentEvent {
        AgentEvent::AssistantDelta {
            addr: AgentAddr::Runtime(key),
            delta: delta.into(),
        }
    }

    fn nested_finished(key: RuntimeAgentKey, result: &str) -> AgentEvent {
        AgentEvent::Finished {
            addr: AgentAddr::Runtime(key),
            result: Some(result.into()),
        }
    }

    fn nested_error(key: RuntimeAgentKey, error: &str) -> AgentEvent {
        AgentEvent::Error {
            addr: AgentAddr::Runtime(key),
            error: error.into(),
        }
    }

    fn usage(
        addr: AgentAddr,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
    ) -> AgentEvent {
        AgentEvent::Usage {
            addr,
            input_tokens,
            output_tokens,
            total_tokens,
        }
    }

    #[test]
    fn nested_started_creates_subagent_under_main() {
        let mut app = AppState::new();
        app.apply_agent_event(T, nested_started(1, None, 1, "do thing"));

        let subs = &app.threads[0].subagents;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].parent_id, Some(MAIN_AGENT_ID));
        assert_eq!(subs[0].runtime_key, Some(1));
        assert_eq!(subs[0].status, AgentStatus::Running);
        assert_eq!(subs[0].depth, 1);
        assert_eq!(subs[0].label, "subagent: do thing");
        assert!(
            app.threads[0].expanded,
            "thread expands to reveal the subagent"
        );
        assert_eq!(subs[0].messages[0].role, MessageRole::User);
        assert_eq!(subs[0].messages[0].content, "do thing");
    }

    #[test]
    fn nested_started_with_context_adds_system_message() {
        let mut app = AppState::new();
        app.apply_agent_event(
            T,
            AgentEvent::Spawned {
                key: 1,
                parent: None,
                depth: 1,
                task: "task".into(),
                context: Some("important ctx".into()),
            },
        );
        let messages = &app.threads[0].subagents[0].messages;
        assert!(
            messages
                .iter()
                .any(|m| m.role == MessageRole::System && m.content.contains("important ctx"))
        );
    }

    #[test]
    fn worker_nests_under_its_parent_subagent() {
        let mut app = AppState::new();
        app.apply_agent_event(T, nested_started(1, None, 1, "parent"));
        let parent_id = app.threads[0].subagents[0].id;
        app.apply_agent_event(T, nested_started(2, Some(1), 2, "child"));

        let worker = app.threads[0]
            .subagents
            .iter()
            .find(|a| a.runtime_key == Some(2))
            .expect("worker node");
        assert_eq!(worker.parent_id, Some(parent_id));
        assert_eq!(worker.depth, 2);
    }

    #[test]
    fn nested_agents_recurse_arbitrarily_deep() {
        let mut app = AppState::new();
        // main(0) -> a(1) -> b(2) -> c(3): the tree nests past the old two-level
        // cap, each child one level deeper than its parent.
        app.apply_agent_event(T, nested_started(1, None, 1, "a"));
        app.apply_agent_event(T, nested_started(2, Some(1), 2, "b"));
        app.apply_agent_event(T, nested_started(3, Some(2), 3, "c"));

        let subs = &app.threads[0].subagents;
        let find = |key| {
            subs.iter()
                .find(|a| a.runtime_key == Some(key))
                .expect("node")
        };
        assert_eq!(find(1).depth, 1);
        assert_eq!(find(2).depth, 2);
        assert_eq!(find(3).depth, 3);
        assert_eq!(find(2).parent_id, Some(find(1).id));
        assert_eq!(find(3).parent_id, Some(find(2).id));
    }

    #[test]
    fn nested_deltas_and_completion_target_the_right_node() {
        let mut app = AppState::new();
        app.apply_agent_event(T, nested_started(1, None, 1, "task"));
        app.apply_agent_event(T, nested_assistant(1, "hi"));
        app.apply_agent_event(T, nested_finished(1, "done"));

        let node = &app.threads[0].subagents[0];
        assert_eq!(node.status, AgentStatus::Complete);
        assert!(
            node.messages
                .iter()
                .any(|m| m.role == MessageRole::Assistant && m.content == "hi")
        );
        assert_eq!(node.messages.last().unwrap().role, MessageRole::ToolResult);
        assert_eq!(node.messages.last().unwrap().content, "done");
    }

    #[test]
    fn nested_error_marks_node_error() {
        let mut app = AppState::new();
        app.apply_agent_event(T, nested_started(1, None, 1, "task"));
        app.apply_agent_event(T, nested_error(1, "bad"));
        assert_eq!(app.threads[0].subagents[0].status, AgentStatus::Error);
    }

    #[test]
    fn nested_event_for_unknown_key_is_ignored() {
        let mut app = AppState::new();
        app.apply_agent_event(T, nested_assistant(99, "x"));
        assert_eq!(app.threads[0].subagents.len(), 0);
    }

    #[test]
    fn thread_token_usage_includes_nested_agents_recursively() {
        let mut app = AppState::new();
        app.apply_agent_event(T, nested_started(1, None, 1, "subagent"));
        app.apply_agent_event(T, nested_started(2, Some(1), 2, "worker"));

        app.apply_agent_event(T, usage(AgentAddr::Main, 10, 5, 15));
        app.apply_agent_event(T, usage(AgentAddr::Runtime(1), 20, 7, 27));
        app.apply_agent_event(T, usage(AgentAddr::Runtime(2), 3, 4, 7));

        let total = app.threads[0].total_token_usage();
        assert_eq!(total.input_tokens, 33);
        assert_eq!(total.output_tokens, 16);
        assert_eq!(total.total_tokens, 49);
    }

    // ----- selectors -----

    #[test]
    fn sidebar_items_reflect_nesting_depth() {
        let mut app = AppState::new();
        app.apply_agent_event(T, nested_started(1, None, 1, "sub"));
        app.apply_agent_event(T, nested_started(2, Some(1), 2, "work"));
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
        app.apply_agent_event(T, reasoning("r"));
        app.apply_agent_event(T, assistant("a"));
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t".into(),
                name: "n".into(),
                arguments: json!({}),
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
        assert_eq!(last(&app).role, MessageRole::User);
        assert_eq!(last(&app).content, "hello");
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

    #[test]
    fn conversation_focus_navigates_and_toggles_collapsibles() {
        let mut app = AppState::new();
        app.apply_agent_event(T, reasoning("r"));
        app.apply_agent_event(
            T,
            AgentEvent::ToolCall {
                addr: AgentAddr::Main,
                id: "t".into(),
                name: "n".into(),
                arguments: json!({}),
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
        let before = app.threads[0].main_agent.messages[index].collapsed;
        app.handle_key(key(KeyCode::Enter));
        let after = app.threads[0].main_agent.messages[index].collapsed;
        assert_ne!(before, after, "Enter toggles the selected collapsible");
    }
}
