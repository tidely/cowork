use std::{
    borrow::Cow,
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    ops::Range,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent::{Agent as StreamingAgent, AgentEvent};
use anyhow::Context as _;
use gpui::{
    Animation, AnimationExt, AnyWindowHandle, App, AppContext, AssetSource, AsyncApp, Bounds,
    ClipboardItem, Context, Entity, Focusable, FontStyle, FontWeight, FutureExt, HighlightStyle,
    IntoElement, KeyBinding, KeyDownEvent, LineFragment, MouseButton, MouseDownEvent, MouseUpEvent,
    PlatformInput, QuitMode, Render, ScrollHandle, ScrollWheelEvent, SharedString, SpringAnimation,
    SpringConfig, Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowControlArea, WindowOptions, actions, canvas, div, img, point, prelude::*, px, rems, rgb,
    rgba, size,
};
use gpui_base::{
    GlobalState, SelectableText, TextSelection, TextSelectionLayer, TextView, TextViewDefaults,
    TextViewState, TextViewStyle, Textarea,
    input::{Input, InputEditorStyle, InputEvent, InputState, TextareaState},
    text::{CodeBlock, SelectionFormat},
};
use gpui_component::{
    Icon, Sizable as _, Theme as ComponentTheme, ThemeMode,
    button::{Button, ButtonVariants as _},
    sidebar::SidebarToggleButton,
};
use gpui_kit_assets::IconName as AssetIconName;
use iroh::{
    Endpoint, EndpointId,
    endpoint::{Accepting, Connection, presets},
};
use itertools::Itertools;
use rig::{
    completion::Message as RigMessage,
    prelude::*,
    providers::ollama::wire::Ollama,
    streaming::{BlockClose, Delta, StreamEvent},
    tool::ToolSet,
};
use serde_json::json;
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle as SyntectFontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use tokio::{
    runtime::Runtime,
    sync::{broadcast, mpsc},
};
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

mod protocol;

const SIDEBAR_WIDTH: gpui::Pixels = px(275.);
const TOP_BAR_HEIGHT: gpui::Pixels = px(40.);
const BOTTOM_BAR_DIVIDER_THRESHOLD: gpui::Pixels = px(24.);
const MACOS_TRAFFIC_LIGHT_X_INSET: gpui::Pixels = px(12.);
const MACOS_TRAFFIC_LIGHT_SIZE: gpui::Pixels = px(14.);
const MACOS_TRAFFIC_LIGHT_SPACING: gpui::Pixels = px(6.);
const MACOS_TRAFFIC_LIGHT_TRAILING_GAP: gpui::Pixels = px(12.);
const OLLAMA_MODEL: &str = "qwen3.8:27b";
const OLLAMA_CONTEXT_TOKENS: u64 = 16 * 8_192;
const OLLAMA_AVATAR_PATH: &str = "providers/ollama.png";
const USER_ACCENT: u32 = 0xe26d5a;
const COWORK_ALPN: &[u8] = b"cowork/0";
/// How long any single step of the collaboration handshake may take.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);
/// How many thread events a collaborator may fall behind before the host
/// re-bases it on a fresh snapshot instead of a delta.
const THREAD_EVENT_CAPACITY: usize = 1024;

static TOKIO_RUNTIME: OnceLock<Runtime> = OnceLock::new();
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static SYNTAX_THEME: OnceLock<Option<Theme>> = OnceLock::new();

fn macos_traffic_light_position() -> gpui::Point<gpui::Pixels> {
    point(
        MACOS_TRAFFIC_LIGHT_X_INSET,
        (TOP_BAR_HEIGHT - MACOS_TRAFFIC_LIGHT_SIZE) / 2.,
    )
}

fn macos_sidebar_toggle_margin() -> gpui::Pixels {
    MACOS_TRAFFIC_LIGHT_X_INSET
        + MACOS_TRAFFIC_LIGHT_SIZE * 3.
        + MACOS_TRAFFIC_LIGHT_SPACING * 2.
        + MACOS_TRAFFIC_LIGHT_TRAILING_GAP
}

fn highlight_code_block(block: &CodeBlock) -> Vec<(Range<usize>, HighlightStyle)> {
    let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
    let theme = SYNTAX_THEME.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        themes
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .or_else(|| themes.themes.values().next().cloned())
    });
    let Some(theme) = theme else {
        return Vec::new();
    };

    let syntax = block
        .lang()
        .and_then(|language| {
            let language = language.split_whitespace().next()?;
            syntax_set
                .find_syntax_by_token(language)
                .or_else(|| syntax_set.find_syntax_by_extension(language))
                .or_else(|| {
                    syntax_set
                        .syntaxes()
                        .iter()
                        .find(|syntax| syntax.name.eq_ignore_ascii_case(language))
                })
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let code = block.code();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut offset = 0;
    let mut highlights = Vec::new();

    for line in LinesWithEndings::from(code.as_ref()) {
        let line_highlights = match highlighter.highlight_line(line, syntax_set) {
            Ok(line_highlights) => line_highlights,
            Err(error) => {
                eprintln!(
                    "failed to highlight {syntax_name} code block: {error}",
                    syntax_name = syntax.name
                );
                return Vec::new();
            }
        };

        for (style, text) in line_highlights {
            let end = offset + text.len();
            if offset < end {
                let foreground = style.foreground;
                let color = rgba(
                    (u32::from(foreground.r) << 24)
                        | (u32::from(foreground.g) << 16)
                        | (u32::from(foreground.b) << 8)
                        | u32::from(foreground.a),
                );
                highlights.push((
                    offset..end,
                    HighlightStyle {
                        color: Some(color.into()),
                        font_weight: style
                            .font_style
                            .contains(SyntectFontStyle::BOLD)
                            .then_some(FontWeight::BOLD),
                        font_style: style
                            .font_style
                            .contains(SyntectFontStyle::ITALIC)
                            .then_some(FontStyle::Italic),
                        ..Default::default()
                    },
                ));
            }
            offset = end;
        }
    }

    highlights
}

gpui_kit_assets::icon_assets!(
    AppIconAssets,
    [PanelLeftClose, PanelLeftOpen, SendHorizontal, Square]
);

struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match path {
            OLLAMA_AVATAR_PATH => Ok(Some(Cow::Borrowed(include_bytes!(
                "../../../assets/providers/ollama.png"
            )))),
            _ => AppIconAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut assets = AppIconAssets.list(path)?;
        if OLLAMA_AVATAR_PATH.starts_with(path) {
            assets.push(OLLAMA_AVATAR_PATH.into());
        }
        Ok(assets)
    }
}

actions!(cowork, [Quit, SubmitComposer]);

#[derive(Clone, Copy)]
enum MessageAuthor {
    User,
    Agent,
}

#[derive(Clone)]
enum TimelineMessage {
    User(UserMessageGroup),
    Agent(AgentMessage),
}

#[derive(Clone)]
struct AgentMessage {
    id: Uuid,
    comment_group_id: Option<Uuid>,
    comment_responses: Vec<AgentCommentResponse>,
    thinking: String,
    thinking_view: Entity<TextViewState>,
    thinking_complete: bool,
    thinking_expanded: bool,
    text: String,
    text_view: Entity<TextViewState>,
    complete: bool,
    failed: bool,
}

#[derive(Clone)]
struct AgentCommentResponse {
    id: Uuid,
    comment_id: Uuid,
    response: String,
    response_view: Entity<TextViewState>,
}

#[derive(Clone)]
struct UserMessageGroup {
    id: Uuid,
    comments: Vec<UserComment>,
    content: UserMessageContent,
    comments_folded: bool,
}

#[derive(Clone)]
enum UserMessageContent {
    Editing(Entity<TextareaState>),
    Submitted {
        text: String,
        history_text: Option<String>,
    },
}

#[derive(Clone)]
struct UserComment {
    id: Uuid,
    reference: CommentReference,
    body: UserCommentBody,
}

#[derive(Clone)]
struct CommentReference {
    message_id: Uuid,
    range: Range<usize>,
    quote: String,
}

#[derive(Clone)]
enum UserCommentBody {
    Editing {
        inline: Entity<TextareaState>,
        composer: Entity<TextareaState>,
    },
    Submitted(SharedString),
}

struct MarkdownTextLeaf {
    source_range: Range<usize>,
    annotation_range: Range<usize>,
    atomic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ThreadMessageId {
    thread_id: Uuid,
    message_id: Uuid,
}

struct SegmentTextView {
    state: Entity<TextViewState>,
    text: String,
    source_offsets: Option<Vec<usize>>,
    rendered_at: u64,
}

#[derive(Clone)]
struct ThreadSummary {
    id: Uuid,
    title: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SharingStatus {
    NotShared,
    Sharing,
    Shared,
    Connected,
    Failed,
}

/// The host's end of a collaborator connection.
type HostPeer = protocol::Peer<protocol::HostMessage, protocol::CollaboratorMessage>;

/// A collaborator's end of its connection to a thread's host.
type ThreadHost = protocol::Peer<protocol::CollaboratorMessage, protocol::HostMessage>;

enum ThreadSharing {
    NotShared,
    Sharing,
    /// Hosting the thread. `events` fans every local change out to all
    /// collaborators; dropping it tears their connections down.
    Shared {
        endpoint: Endpoint,
        events: broadcast::Sender<protocol::HostMessage>,
    },
    /// Mirroring someone else's thread.
    ///
    /// `connection` and `host` are held rather than read: they keep the QUIC
    /// connection and its protocol stream open, so replacing this state is what
    /// disconnects. `host` is also where peer specific requests, such as
    /// permission changes, will be sent from.
    #[allow(dead_code, reason = "fields are held open for their lifetime")]
    Connected {
        endpoint: Endpoint,
        connection: Connection,
        host: async_channel::Sender<protocol::CollaboratorMessage>,
    },
    Failed,
}

impl ThreadSharing {
    fn status(&self) -> SharingStatus {
        match self {
            Self::NotShared => SharingStatus::NotShared,
            Self::Sharing => SharingStatus::Sharing,
            Self::Shared { .. } => SharingStatus::Shared,
            Self::Connected { .. } => SharingStatus::Connected,
            Self::Failed => SharingStatus::Failed,
        }
    }

    fn is_collaborating(&self) -> bool {
        matches!(
            self,
            Self::Sharing | Self::Shared { .. } | Self::Connected { .. }
        )
    }
}

enum JoinStatus {
    Idle,
    Joining,
    Failed(String),
}

struct JoinDialog {
    endpoint_token: Entity<InputState>,
    status: JoinStatus,
    _input_subscription: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadOwnership {
    Local,
    Remote,
}

impl ThreadOwnership {
    fn can_write(self) -> bool {
        matches!(self, Self::Local)
    }

    fn remove_on_disconnect(self) -> bool {
        matches!(self, Self::Remote)
    }
}

struct Thread {
    /// Identifies this local view. Multiple views may mirror the same shared
    /// thread, so this must remain distinct from `summary.id`.
    instance_id: Uuid,
    summary: ThreadSummary,
    timeline: Vec<TimelineMessage>,
    draft: UserMessageGroup,
    generating: bool,
    sharing: ThreadSharing,
    ownership: ThreadOwnership,
}

impl UserComment {
    fn to_protocol(&self) -> Option<protocol::UserComment> {
        let UserCommentBody::Submitted(body) = &self.body else {
            return None;
        };
        Some(protocol::UserComment {
            id: self.id.into_bytes(),
            reference: protocol::CommentReference {
                message_id: self.reference.message_id.into_bytes(),
                range: self.reference.range.clone(),
                quote: self.reference.quote.clone(),
            },
            body: body.to_string(),
        })
    }
}

impl protocol::UserComment {
    fn into_native(self) -> UserComment {
        UserComment {
            id: Uuid::from_bytes(self.id),
            reference: CommentReference {
                message_id: Uuid::from_bytes(self.reference.message_id),
                range: self.reference.range,
                quote: self.reference.quote,
            },
            body: UserCommentBody::Submitted(self.body.into()),
        }
    }
}

impl UserMessageGroup {
    fn to_protocol(&self) -> Option<protocol::UserMessage> {
        let UserMessageContent::Submitted { text, .. } = &self.content else {
            return None;
        };
        Some(protocol::UserMessage {
            id: self.id.into_bytes(),
            text: text.clone(),
            comments: self
                .comments
                .iter()
                .filter_map(UserComment::to_protocol)
                .collect(),
        })
    }
}

impl protocol::UserMessage {
    fn into_native(self) -> UserMessageGroup {
        UserMessageGroup {
            id: Uuid::from_bytes(self.id),
            comments: self
                .comments
                .into_iter()
                .map(protocol::UserComment::into_native)
                .collect(),
            content: UserMessageContent::Submitted {
                text: self.text,
                history_text: None,
            },
            comments_folded: false,
        }
    }
}

impl AgentMessage {
    /// An empty message for an agent that has just started responding.
    fn new(id: Uuid, comment_group_id: Option<Uuid>, cx: &mut impl AppContext) -> Self {
        Self {
            id,
            comment_group_id,
            comment_responses: Vec::new(),
            thinking: String::new(),
            thinking_view: cx.new(|cx| TextViewState::markdown("", cx)),
            thinking_complete: false,
            thinking_expanded: true,
            text: String::new(),
            text_view: cx.new(|cx| TextViewState::markdown("", cx)),
            complete: false,
            failed: false,
        }
    }

    fn to_protocol(&self) -> protocol::AgentMessage {
        protocol::AgentMessage {
            id: self.id.into_bytes(),
            comment_group_id: self.comment_group_id.map(Uuid::into_bytes),
            comment_responses: self
                .comment_responses
                .iter()
                .map(|response| protocol::AgentCommentResponse {
                    id: response.id.into_bytes(),
                    comment_id: response.comment_id.into_bytes(),
                    response: response.response.clone(),
                })
                .collect(),
            thinking: self.thinking.clone(),
            thinking_complete: self.thinking_complete,
            text: self.text.clone(),
            complete: self.complete,
            failed: self.failed,
        }
    }
}

impl protocol::AgentMessage {
    fn into_native(self, cx: &mut impl AppContext) -> AgentMessage {
        let thinking_view = cx.new(|cx| TextViewState::markdown(&self.thinking, cx));
        let text_view = cx.new(|cx| TextViewState::markdown(&self.text, cx));
        AgentMessage {
            id: Uuid::from_bytes(self.id),
            comment_group_id: self.comment_group_id.map(Uuid::from_bytes),
            comment_responses: self
                .comment_responses
                .into_iter()
                .map(|response| AgentCommentResponse {
                    id: Uuid::from_bytes(response.id),
                    comment_id: Uuid::from_bytes(response.comment_id),
                    response_view: cx.new(|cx| TextViewState::markdown(&response.response, cx)),
                    response: response.response,
                })
                .collect(),
            thinking: self.thinking,
            thinking_view,
            thinking_complete: self.thinking_complete,
            thinking_expanded: !self.thinking_complete,
            text: self.text,
            text_view,
            complete: self.complete,
            failed: self.failed,
        }
    }
}

impl TimelineMessage {
    fn to_protocol(&self) -> Option<protocol::TimelineMessage> {
        match self {
            Self::User(message) => message.to_protocol().map(protocol::TimelineMessage::User),
            Self::Agent(message) => Some(protocol::TimelineMessage::Agent(message.to_protocol())),
        }
    }
}

impl protocol::TimelineMessage {
    fn into_native(self, cx: &mut impl AppContext) -> TimelineMessage {
        match self {
            Self::User(message) => TimelineMessage::User(message.into_native()),
            Self::Agent(message) => TimelineMessage::Agent(message.into_native(cx)),
        }
    }
}

impl Thread {
    /// Builds the local mirror of a thread hosted by someone else.
    fn from_snapshot(
        snapshot: protocol::ThreadSnapshot,
        draft: UserMessageGroup,
        sharing: ThreadSharing,
        cx: &mut impl AppContext,
    ) -> Self {
        let (summary, timeline) = snapshot.into_native(cx);
        let mut thread = Self {
            instance_id: Uuid::new_v4(),
            summary,
            timeline: Vec::new(),
            draft,
            generating: false,
            sharing,
            ownership: ThreadOwnership::Remote,
        };
        thread.set_timeline(timeline);
        thread
    }

    fn to_protocol(&self) -> protocol::ThreadSnapshot {
        protocol::ThreadSnapshot {
            id: self.summary.id.into_bytes(),
            title: self.summary.title.clone(),
            messages: self
                .timeline
                .iter()
                .filter_map(TimelineMessage::to_protocol)
                .collect(),
        }
    }

    /// Subscribes to this thread's events, returning `None` when it is not
    /// being hosted.
    ///
    /// Callers that also need a snapshot must take both in the same
    /// `Entity::update`: thread state only changes on the foreground thread, so
    /// pairing them there guarantees the subscription starts exactly where the
    /// snapshot ends, with no event missed or replayed.
    fn subscribe(&self) -> Option<broadcast::Receiver<protocol::HostMessage>> {
        match &self.sharing {
            ThreadSharing::Shared { events, .. } => Some(events.subscribe()),
            _ => None,
        }
    }

    /// Broadcasts an event to every collaborator without applying it locally.
    ///
    /// Needed for the changes whose local representation carries more than the
    /// wire form does, such as a user message that also remembers the prompt
    /// the agent was given and whether its comments are folded.
    fn publish(&self, event: protocol::HostMessage) {
        if let ThreadSharing::Shared { events, .. } = &self.sharing {
            // An error here only means nobody has joined yet.
            _ = events.send(event);
        }
    }

    /// Applies a thread event locally and broadcasts it verbatim.
    ///
    /// Host and collaborators then run the same [`Thread::apply`] over the same
    /// events, so their timelines stay identical by construction. Only use this
    /// for events that fully describe the change they make.
    fn emit(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        // Checked up front so that an unshared thread, which is the common
        // case, never pays to clone a streamed chunk.
        if matches!(self.sharing, ThreadSharing::Shared { .. }) {
            self.publish(event.clone());
        }
        self.apply(event, cx);
    }

    /// Folds a thread event into the timeline.
    fn apply(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        match event {
            protocol::HostMessage::Welcome(snapshot) => {
                let (summary, timeline) = snapshot.into_native(cx);
                self.summary = summary;
                self.set_timeline(timeline);
            }
            protocol::HostMessage::ThreadTitled(title) => self.summary.title = title,
            protocol::HostMessage::UserMessage(message) => self
                .timeline
                .push(TimelineMessage::User(message.into_native())),
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id,
            } => {
                self.timeline.push(TimelineMessage::Agent(AgentMessage::new(
                    Uuid::from_bytes(id),
                    comment_group_id.map(Uuid::from_bytes),
                    cx,
                )));
                self.generating = true;
            }
            protocol::HostMessage::AgentTextAppended { id, target, text } => {
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                let view = match target {
                    protocol::AgentText::Thinking => {
                        message.thinking.push_str(&text);
                        message.thinking_view.clone()
                    }
                    protocol::AgentText::Response => {
                        // Some models never close the reasoning block, so the
                        // first answer token ends it instead.
                        if !message.thinking.is_empty() && !message.thinking_complete {
                            message.thinking_complete = true;
                            message.thinking_expanded = false;
                        }
                        message.text.push_str(&text);
                        message.text_view.clone()
                    }
                };
                view.update(cx, |view, cx| view.push_str(&text, cx));
            }
            protocol::HostMessage::AgentThinkingEnded { id } => {
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                message.thinking_complete = true;
                message.thinking_expanded = false;
            }
            protocol::HostMessage::AgentCommentResponded {
                id,
                response_id,
                comment_id,
                response,
            } => {
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                message.comment_responses.push(AgentCommentResponse {
                    id: Uuid::from_bytes(response_id),
                    comment_id: Uuid::from_bytes(comment_id),
                    response_view: cx.new(|cx| TextViewState::markdown(&response, cx)),
                    response,
                });
            }
            protocol::HostMessage::AgentEnded { id, failure } => {
                self.generating = false;
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                message.complete = true;
                message.thinking_complete = true;
                message.thinking_expanded = false;
                message.failed = failure.is_some();
                // Only surface the failure when the agent said nothing itself.
                if let Some(failure) = failure
                    && message.text.is_empty()
                {
                    let view = message.text_view.clone();
                    view.update(cx, |view, cx| view.set_text(&failure, cx));
                    message.text = failure;
                }
            }
        }
    }

    fn set_timeline(&mut self, timeline: Vec<TimelineMessage>) {
        self.generating = timeline
            .iter()
            .any(|message| matches!(message, TimelineMessage::Agent(message) if !message.complete));
        self.timeline = timeline;
    }

    fn agent_message_mut(&mut self, id: uuid::Bytes) -> Option<&mut AgentMessage> {
        let id = Uuid::from_bytes(id);
        self.timeline.iter_mut().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if message.id == id => Some(message),
            _ => None,
        })
    }
}

impl protocol::ThreadSnapshot {
    fn into_native(self, cx: &mut impl AppContext) -> (ThreadSummary, Vec<TimelineMessage>) {
        let summary = ThreadSummary {
            id: Uuid::from_bytes(self.id),
            title: self.title,
        };
        let timeline = self
            .messages
            .into_iter()
            .map(|message| message.into_native(cx))
            .collect();
        (summary, timeline)
    }
}

#[derive(Default)]
struct ThreadStore {
    threads: VecDeque<Entity<Thread>>,
}

impl ThreadStore {
    fn thread(&self, thread_id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        self.threads
            .iter()
            .find(|thread| thread.read(cx).instance_id == thread_id)
            .cloned()
    }
}

struct ActiveGeneration {
    message_id: Uuid,
    abort_handle: tokio::task::AbortHandle,
    cancelled: Arc<AtomicBool>,
}

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    new_thread_draft: UserMessageGroup,
    timeline_scroll_handle: ScrollHandle,
    follow_generation: bool,
    thread_store: Entity<ThreadStore>,
    active_thread_id: Option<Uuid>,
    selection_message_id: Option<Uuid>,
    segment_text_views: HashMap<(ThreadMessageId, Range<usize>), SegmentTextView>,
    render_generation: u64,
    titlebar_click_armed: bool,
    join_dialog: Option<JoinDialog>,
    tokio_handle: tokio::runtime::Handle,
    active_generations: HashMap<Uuid, ActiveGeneration>,
    _window_activation_subscription: Subscription,
}

impl Cowork {
    fn new_user_message_draft(window: &mut Window, cx: &mut App) -> UserMessageGroup {
        let composer = cx.new(|cx| {
            let mut composer = TextareaState::new(window, cx).auto_grow(1, usize::MAX);
            composer.set_editor_style(InputEditorStyle {
                caret: rgb(0xffffff).into(),
                ..Default::default()
            });
            composer
        });
        UserMessageGroup {
            id: Uuid::new_v4(),
            comments: Vec::new(),
            content: UserMessageContent::Editing(composer),
            comments_folded: false,
        }
    }

    fn draft_composer(draft: &UserMessageGroup) -> Entity<TextareaState> {
        let UserMessageContent::Editing(composer) = &draft.content else {
            unreachable!("thread drafts are always editable");
        };
        composer.clone()
    }

    fn editable_composer(&self, cx: &App) -> Option<Entity<TextareaState>> {
        self.active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .map(|thread| {
                let thread = thread.read(cx);
                thread
                    .ownership
                    .can_write()
                    .then(|| Self::draft_composer(&thread.draft))
            })
            .unwrap_or_else(|| Some(Self::draft_composer(&self.new_thread_draft)))
    }

    fn new_comment_editor(
        initial_text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<TextareaState> {
        cx.new(|cx| {
            let mut body = TextareaState::new(window, cx).auto_grow(1, usize::MAX);
            body.set_editor_style(InputEditorStyle {
                caret: rgb(0xffffff).into(),
                ..Default::default()
            });
            body.insert(initial_text, window, cx);
            body
        })
    }

    fn new_empty_local_thread(draft: UserMessageGroup, cx: &mut App) -> Entity<Thread> {
        let thread_id = Uuid::new_v4();
        cx.new(|_| Thread {
            instance_id: thread_id,
            summary: ThreadSummary {
                id: thread_id,
                title: "New thread".into(),
            },
            timeline: Vec::new(),
            draft,
            generating: false,
            sharing: ThreadSharing::NotShared,
            ownership: ThreadOwnership::Local,
        })
    }

    fn prepare_thread_for_sharing(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Thread> {
        if let Some(thread) = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
        {
            return thread;
        }

        let next_draft = Self::new_user_message_draft(window, cx);
        let draft = std::mem::replace(&mut self.new_thread_draft, next_draft);
        let thread = Self::new_empty_local_thread(draft, cx);
        self.active_thread_id = Some(thread.read(cx).instance_id);
        self.thread_store.update(cx, |store, _| {
            store.threads.push_front(thread.clone());
        });
        thread
    }

    fn end_stale_mouse_drag(window: &mut Window, cx: &mut App) {
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: window.mouse_position(),
                modifiers: window.modifiers(),
                click_count: 1,
            }),
            cx,
        );
    }

    fn render_caption_button(
        id: &'static str,
        icon: &'static str,
        control_area: WindowControlArea,
        is_close: bool,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h_full()
            .w(px(46.))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .text_size(px(10.))
            .text_color(rgb(0xd4d4d8))
            .window_control_area(control_area)
            .when(is_close, |this| this.hover(|this| this.bg(rgb(0xe81123))))
            .when(!is_close, |this| this.hover(|this| this.bg(rgb(0x2d2d30))))
            .child(icon)
    }

    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .when(cfg!(target_os = "macos"), |this| {
                this.ml(macos_sidebar_toggle_margin())
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.titlebar_click_armed = false;
                    cx.stop_propagation();
                }),
            )
            .child(
                SidebarToggleButton::new()
                    .collapsed(!self.sidebar_open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    })),
            )
    }

    fn start_sharing(&mut self, thread: Entity<Thread>, cx: &mut Context<Self>) {
        thread.update(cx, |thread, _| {
            thread.sharing = ThreadSharing::Sharing;
        });
        cx.notify();

        let (peers, accepted_peers) = async_channel::bounded(protocol::PEER_CHANNEL_CAPACITY);
        let endpoint_task = Self::bind_shared_endpoint(&self.tokio_handle, peers);

        cx.spawn(async move |this, cx| {
            let endpoint = endpoint_task
                .await
                .context("Endpoint setup task failed.")
                .and_then(|result| result);
            let endpoint = match endpoint {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    eprintln!("failed to share thread: {error:#}");
                    thread.update(cx, |thread, _| thread.sharing = ThreadSharing::Failed);
                    _ = this.update(cx, |_, cx| cx.notify());
                    return;
                }
            };
            thread.update(cx, |thread, _| {
                thread.sharing = ThreadSharing::Shared {
                    endpoint,
                    events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
                };
            });
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                return;
            }

            // Peers are only served once the thread is hosting, so that every
            // one of them can subscribe to its events. Connections accepted
            // before that wait in the channel.
            let thread = thread.downgrade();
            while let Ok(peer) = accepted_peers.recv().await {
                let thread = thread.clone();
                cx.spawn(async move |cx| {
                    if let Err(error) = Self::serve_peer(thread, peer, cx).await {
                        eprintln!("stopped serving collaborator: {error:#}");
                    }
                })
                .detach();
            }
        })
        .detach();
    }

    /// Binds the endpoint collaborators dial into, forwarding every accepted
    /// connection to `peers` as a ready to use protocol channel.
    fn bind_shared_endpoint(
        tokio_handle: &tokio::runtime::Handle,
        peers: async_channel::Sender<HostPeer>,
    ) -> tokio::task::JoinHandle<anyhow::Result<Endpoint>> {
        tokio_handle.spawn(async move {
            let endpoint = Endpoint::builder(presets::N0)
                .alpns(vec![COWORK_ALPN.to_vec()])
                .bind()
                .await?;

            tokio::spawn({
                let endpoint = endpoint.clone();
                async move {
                    while let Some(incoming) = endpoint.accept().await {
                        let accepting = match incoming.accept() {
                            Ok(accepting) => accepting,
                            Err(error) => {
                                eprintln!("failed to accept connection: {error}");
                                continue;
                            }
                        };
                        let peers = peers.clone();
                        tokio::spawn(async move {
                            if let Err(error) = Self::accept_peer(accepting, peers).await {
                                eprintln!("failed to accept collaborator: {error:#}");
                            }
                        });
                    }
                }
            });

            Ok(endpoint)
        })
    }

    /// Finishes one incoming connection's handshake and hands its protocol
    /// channel over to the foreground.
    async fn accept_peer(
        accepting: Accepting,
        peers: async_channel::Sender<HostPeer>,
    ) -> anyhow::Result<()> {
        let connection = accepting.await?;
        let (send, recv) = tokio::time::timeout(PEER_TIMEOUT, connection.accept_bi())
            .await
            .context("Timed out waiting for a peer protocol stream.")??;
        // The streams keep the connection alive, so `connection` itself can go.
        peers
            .send(protocol::spawn_peer(tokio::io::join(recv, send)))
            .await
            .context("Shared thread is no longer available.")
    }

    /// Streams the shared thread to one collaborator for as long as it stays
    /// connected.
    ///
    /// The collaborator is first re-based onto a snapshot, then fed the thread's
    /// events verbatim. Peer specific requests flow the other way on the peer's
    /// own channel.
    async fn serve_peer(
        thread: WeakEntity<Thread>,
        peer: HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        let join = peer
            .receive()
            .with_timeout(PEER_TIMEOUT, cx.background_executor())
            .await?
            .context("Peer closed before joining.")?;
        anyhow::ensure!(
            join == protocol::CollaboratorMessage::Join,
            "Expected a join message, got {join:?}."
        );

        // Drain peer specific requests so that a peer which stops reading its
        // own replies can never stall the event stream it is subscribed to.
        cx.spawn({
            let requests = peer.incoming.clone();
            async move |_| {
                while let Ok(request) = requests.recv().await {
                    match request {
                        protocol::CollaboratorMessage::Join => {}
                    }
                }
            }
        })
        .detach();

        let mut events = Self::send_snapshot(&thread, &peer, cx).await?;
        loop {
            match events.recv().await {
                Ok(event) => peer
                    .send(event)
                    .await
                    .context("Peer stopped receiving thread events.")?,
                // A peer that fell further behind than the event buffer has
                // missed changes, so re-base it rather than applying deltas to
                // a timeline that no longer matches the host's.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    events = Self::send_snapshot(&thread, &peer, cx).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }

    /// Sends a peer a full snapshot and returns a subscription that resumes
    /// exactly where the snapshot left off.
    async fn send_snapshot(
        thread: &WeakEntity<Thread>,
        peer: &HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<broadcast::Receiver<protocol::HostMessage>> {
        let (snapshot, events) = thread
            .update(cx, |thread, _| {
                Some((thread.to_protocol(), thread.subscribe()?))
            })?
            .context("Thread is no longer shared.")?;
        peer.send(protocol::HostMessage::Welcome(snapshot))
            .await
            .context("Peer disconnected before receiving the thread snapshot.")?;
        Ok(events)
    }

    fn copy_endpoint_id(&self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let endpoint_id = {
            let thread = thread.read(cx);
            let ThreadSharing::Shared { endpoint, .. } = &thread.sharing else {
                return;
            };
            endpoint.id().to_string()
        };

        cx.write_to_clipboard(ClipboardItem::new_string(endpoint_id));
    }

    fn open_join_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let endpoint_token = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Paste endpoint token");
            input.set_editor_style(InputEditorStyle {
                caret: rgb(0xffffff).into(),
                ..Default::default()
            });
            input
        });
        let input_subscription =
            cx.subscribe(&endpoint_token, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(dialog) = &mut this.join_dialog
                        && matches!(dialog.status, JoinStatus::Failed(_))
                    {
                        dialog.status = JoinStatus::Idle;
                    }
                    cx.notify();
                }
            });
        endpoint_token.focus_handle(cx).focus(window, cx);
        self.join_dialog = Some(JoinDialog {
            endpoint_token,
            status: JoinStatus::Idle,
            _input_subscription: input_subscription,
        });
        cx.notify();
    }

    fn close_join_dialog(&mut self, cx: &mut Context<Self>) {
        if self
            .join_dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.status, JoinStatus::Joining))
        {
            return;
        }
        self.join_dialog = None;
        cx.notify();
    }

    fn join_dialog_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.close_join_dialog(cx);
            cx.stop_propagation();
        }
    }

    fn join_shared_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.join_dialog else {
            return;
        };
        if matches!(dialog.status, JoinStatus::Joining) {
            return;
        }

        let token = dialog.endpoint_token.read(cx).value().trim().to_string();
        let endpoint_id = match token.parse::<EndpointId>() {
            Ok(endpoint_id) => endpoint_id,
            Err(_) => {
                dialog.status = JoinStatus::Failed("Enter a valid endpoint token.".into());
                cx.notify();
                return;
            }
        };
        dialog.status = JoinStatus::Joining;
        let draft = Self::new_user_message_draft(window, cx);
        cx.notify();

        let join_task = self.tokio_handle.spawn(async move {
            let endpoint = Endpoint::builder(presets::N0).bind().await?;
            let connection =
                tokio::time::timeout(PEER_TIMEOUT, endpoint.connect(endpoint_id, COWORK_ALPN))
                    .await
                    .context("Connection timed out.")??;
            let (send, recv) = tokio::time::timeout(PEER_TIMEOUT, connection.open_bi())
                .await
                .context("Timed out opening the peer protocol stream.")??;
            let host: ThreadHost = protocol::spawn_peer(tokio::io::join(recv, send));
            host.send(protocol::CollaboratorMessage::Join)
                .await
                .context("Peer connection is no longer available.")?;
            let welcome = tokio::time::timeout(PEER_TIMEOUT, host.receive())
                .await
                .context("Timed out waiting for the thread snapshot.")?
                .context("Host closed the protocol stream before sending the thread snapshot.")?;
            let protocol::HostMessage::Welcome(snapshot) = welcome else {
                anyhow::bail!("Host sent a thread event before the thread snapshot.");
            };
            Ok::<_, anyhow::Error>((endpoint, connection, host, snapshot))
        });

        cx.spawn(async move |this, cx| {
            let result = join_task
                .await
                .context("Join task failed.")
                .and_then(|result| result);
            let (endpoint, connection, host, snapshot) = match result {
                Ok(joined) => joined,
                Err(error) => {
                    eprintln!("failed to join shared thread: {error:#}");
                    _ = this.update(cx, |this, cx| {
                        if let Some(dialog) = &mut this.join_dialog {
                            dialog.status = JoinStatus::Failed(error.to_string());
                        }
                        cx.notify();
                    });
                    return;
                }
            };

            let (requests, events) = host.split();
            let Ok((thread, thread_id)) = this.update(cx, move |this, cx| {
                let thread = cx.new(|cx| {
                    Thread::from_snapshot(
                        snapshot,
                        draft,
                        ThreadSharing::Connected {
                            endpoint,
                            connection,
                            host: requests,
                        },
                        cx,
                    )
                });
                let thread_id = thread.read(cx).instance_id;
                this.thread_store.update(cx, |store, _| {
                    store.threads.push_front(thread.clone());
                });
                this.active_thread_id = Some(thread_id);
                this.selection_message_id = None;
                this.join_dialog = None;
                cx.notify();
                (thread, thread_id)
            }) else {
                return;
            };

            // Replay the host's changes onto the local mirror of the thread.
            while let Ok(event) = events.recv().await {
                thread.update(cx, |thread, cx| thread.apply(event, cx));
                if this
                    .update(cx, |this, cx| this.thread_updated(thread_id, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    fn toggle_sharing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread = self.prepare_thread_for_sharing(window, cx);
        let thread_id = thread.read(cx).instance_id;

        match thread.read(cx).sharing.status() {
            SharingStatus::NotShared | SharingStatus::Failed => self.start_sharing(thread, cx),
            SharingStatus::Sharing => {}
            SharingStatus::Shared => {
                // Replacing the state drops the event channel, which ends every
                // peer's subscription and unwinds the tasks serving them.
                let endpoint = thread.update(cx, |thread, _| {
                    let ThreadSharing::Shared { endpoint, .. } =
                        std::mem::replace(&mut thread.sharing, ThreadSharing::NotShared)
                    else {
                        return None;
                    };
                    Some(endpoint)
                });
                if let Some(endpoint) = endpoint {
                    self.tokio_handle.spawn(async move {
                        endpoint.close().await;
                    });
                }
                cx.notify();
            }
            SharingStatus::Connected => {
                // Replacing the state drops the channel to the host, closing
                // the protocol stream that kept the connection alive.
                let endpoint = thread.update(cx, |thread, _| {
                    let ThreadSharing::Connected { endpoint, .. } =
                        std::mem::replace(&mut thread.sharing, ThreadSharing::NotShared)
                    else {
                        return None;
                    };
                    Some(endpoint)
                });
                if thread.read(cx).ownership.remove_on_disconnect() {
                    self.thread_store.update(cx, |store, cx| {
                        store
                            .threads
                            .retain(|thread| thread.read(cx).instance_id != thread_id);
                    });
                    self.active_thread_id = None;
                    self.selection_message_id = None;
                    self.new_thread_draft = Self::new_user_message_draft(window, cx);
                    Self::draft_composer(&self.new_thread_draft)
                        .focus_handle(cx)
                        .focus(window, cx);
                }
                if let Some(endpoint) = endpoint {
                    self.tokio_handle.spawn(async move {
                        endpoint.close().await;
                    });
                }
                cx.notify();
            }
        }
    }

    fn render_top_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_thread = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        let sharing_status = active_thread
            .as_ref()
            .map(|thread| thread.read(cx).sharing.status())
            .unwrap_or(SharingStatus::NotShared);
        let share_label = match sharing_status {
            SharingStatus::NotShared => "Share",
            SharingStatus::Sharing => "Sharing…",
            SharingStatus::Shared => "Unshare",
            SharingStatus::Connected => "Disconnect",
            SharingStatus::Failed => "Retry share",
        };
        let sharing_enabled = sharing_status != SharingStatus::Sharing;

        div()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(0x1c1c1f))
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    GlobalState::suppress_text_selection(cx);

                    if cfg!(target_os = "macos") {
                        cx.stop_propagation();
                        let is_titlebar_double_click =
                            event.click_count == 2 && this.titlebar_click_armed;
                        this.titlebar_click_armed = event.click_count == 1;

                        if is_titlebar_double_click {
                            window.titlebar_double_click();
                        } else {
                            window.start_window_move();
                        }
                    }
                }),
            )
            .child(self.render_sidebar_toggle(cx))
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .when(sharing_status == SharingStatus::Shared, |this| {
                        this.child(
                            div()
                                .id("copy-endpoint-id")
                                .size(px(28.))
                                .mr_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .occlude()
                                .cursor_pointer()
                                .text_sm()
                                .text_color(rgb(0xa1a1aa))
                                .hover(|this| this.bg(rgb(0x2d2d30)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.titlebar_click_armed = false;
                                        cx.stop_propagation();
                                    }),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.copy_endpoint_id(cx);
                                }))
                                .child("🔗"),
                        )
                    })
                    .child(
                        div()
                            .id("toggle-sharing")
                            .h(px(28.))
                            .px_3()
                            .mr_2()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .occlude()
                            .text_sm()
                            .text_color(rgb(0x71717a))
                            .when(sharing_enabled, |this| {
                                this.cursor_pointer()
                                    .text_color(rgb(0xd4d4d8))
                                    .hover(|this| this.bg(rgb(0x2d2d30)))
                            })
                            .when(sharing_status == SharingStatus::Failed, |this| {
                                this.text_color(rgb(0xf87171))
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.titlebar_click_armed = false;
                                    cx.stop_propagation();
                                }),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_sharing(window, cx);
                            }))
                            .child(share_label),
                    )
                    .when(!cfg!(target_os = "macos"), |this| {
                        this.child(
                            div()
                                .h_full()
                                .flex()
                                .font_family("Segoe Fluent Icons")
                                .child(Self::render_caption_button(
                                    "minimize-window",
                                    "\u{e921}",
                                    WindowControlArea::Min,
                                    false,
                                ))
                                .child(Self::render_caption_button(
                                    "maximize-window",
                                    if window.is_maximized() {
                                        "\u{e923}"
                                    } else {
                                        "\u{e922}"
                                    },
                                    WindowControlArea::Max,
                                    false,
                                ))
                                .child(Self::render_caption_button(
                                    "close-window",
                                    "\u{e8bb}",
                                    WindowControlArea::Close,
                                    true,
                                )),
                        )
                    }),
            )
    }

    fn open_thread(&mut self, thread_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread_store.read(cx).thread(thread_id, cx).is_none() {
            return;
        }

        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let thread = thread.read(cx);
        let composer = Self::draft_composer(&thread.draft);
        let can_write = thread.ownership.can_write();
        self.active_thread_id = Some(thread_id);
        self.selection_message_id = None;
        self.follow_generation = true;
        self.timeline_scroll_handle.scroll_to_bottom();
        if can_write {
            composer.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn render_sidebar_thread(
        &self,
        thread_id: Uuid,
        thread: &ThreadSummary,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(thread_id.to_string())
            .h(px(30.))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .when(self.active_thread_id == Some(thread_id), |this| {
                this.bg(rgb(0x2d2d30))
            })
            .hover(|this| this.bg(rgb(0x3a3a3e)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_thread(thread_id, window, cx);
            }))
            .text_sm()
            .text_color(rgb(0xd4d4d8))
            .truncate()
            .child(thread.title.clone())
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (collaborating_threads, recent_threads): (Vec<_>, Vec<_>) = self
            .thread_store
            .read(cx)
            .threads
            .iter()
            .map(|thread| {
                let thread = thread.read(cx);
                (
                    thread.instance_id,
                    thread.summary.clone(),
                    thread.sharing.is_collaborating(),
                )
            })
            .partition(|(_, _, collaborating)| *collaborating);
        let recents_arrow = div()
            .size(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .child(if self.recents_open { "⌄" } else { "›" });

        div()
            .h_full()
            .w(SIDEBAR_WIDTH)
            .flex_none()
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .w(SIDEBAR_WIDTH)
                    .flex_none()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x1c1c1f))
                    .child(
                        div()
                            .h(px(42.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_3()
                            .text_size(px(18.))
                            .text_color(rgb(0xe4e4e7))
                            .child("Cowork"),
                    )
                    .child(
                        div()
                            .id("new-chat")
                            .h(px(34.))
                            .mx_2()
                            .px_2()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(self.active_thread_id.is_none(), |this| {
                                this.bg(rgb(0x2d2d30))
                            })
                            .hover(|this| this.bg(rgb(0x3a3a3e)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_thread_draft = Self::new_user_message_draft(window, cx);
                                this.active_thread_id = None;
                                this.selection_message_id = None;
                                Self::draft_composer(&this.new_thread_draft)
                                    .focus_handle(cx)
                                    .focus(window, cx);
                                cx.notify();
                            }))
                            .text_sm()
                            .text_color(rgb(0xf4f4f5))
                            .child("✎")
                            .child("New chat"),
                    )
                    .child(
                        div()
                            .id("join-shared-thread")
                            .h(px(34.))
                            .mx_2()
                            .px_2()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(0x3a3a3e)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_join_dialog(window, cx);
                            }))
                            .text_sm()
                            .text_color(rgb(0xf4f4f5))
                            .child("🔗")
                            .child("Join shared thread"),
                    )
                    .when(!collaborating_threads.is_empty(), |this| {
                        this.child(
                            div()
                                .h(px(44.))
                                .flex_none()
                                .flex()
                                .items_end()
                                .px_3()
                                .pb_2()
                                .text_sm()
                                .text_color(rgb(0x71717a))
                                .child("Collaborating"),
                        )
                        .child(
                            div()
                                .flex_none()
                                .px_2()
                                .children(collaborating_threads.iter().map(
                                    |(thread_id, thread, _)| {
                                        self.render_sidebar_thread(*thread_id, thread, cx)
                                    },
                                )),
                        )
                    })
                    .child(
                        div()
                            .id("toggle-recents")
                            .h(px(44.))
                            .flex_none()
                            .flex()
                            .items_end()
                            .justify_between()
                            .px_3()
                            .pb_2()
                            .cursor_pointer()
                            .text_sm()
                            .text_color(rgb(0x71717a))
                            .hover(|this| this.text_color(rgb(0xa1a1aa)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.recents_open = !this.recents_open;
                                cx.notify();
                            }))
                            .child("Recents")
                            .child(recents_arrow),
                    )
                    .when(self.recents_open, |this| {
                        this.child(div().flex_1().min_h_0().overflow_hidden().px_2().children(
                            recent_threads.iter().map(|(thread_id, thread, _)| {
                                self.render_sidebar_thread(*thread_id, thread, cx)
                            }),
                        ))
                    }),
            )
    }

    fn render_join_dialog(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.join_dialog.as_ref()?;
        let joining = matches!(dialog.status, JoinStatus::Joining);
        let has_token = !dialog.endpoint_token.read(cx).value().trim().is_empty();
        let can_join = has_token && !joining;
        let error = match &dialog.status {
            JoinStatus::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let endpoint_token = dialog.endpoint_token.clone();

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .on_key_down(cx.listener(Self::join_dialog_key_down))
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("join-dialog-backdrop")
                        .absolute()
                        .inset_0()
                        .bg(rgba(0x00000099))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.close_join_dialog(cx);
                        })),
                )
                .child(
                    div()
                        .id("join-dialog")
                        .relative()
                        .w(px(440.))
                        .p_5()
                        .flex()
                        .flex_col()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0x3f3f46))
                        .bg(rgb(0x242427))
                        .shadow_lg()
                        .text_sm()
                        .text_color(rgb(0xd4d4d8))
                        .child(
                            div()
                                .text_size(px(17.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xf4f4f5))
                                .child("Join shared thread"),
                        )
                        .child(
                            div()
                                .mt_2()
                                .text_color(rgb(0xa1a1aa))
                                .child("Paste the endpoint token shared with you."),
                        )
                        .child(
                            div()
                                .id("endpoint-token-input")
                                .h(px(38.))
                                .mt_4()
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x52525b))
                                .bg(rgb(0x18181b))
                                .on_click({
                                    let endpoint_token = endpoint_token.clone();
                                    cx.listener(move |_, _, window, cx| {
                                        endpoint_token.focus_handle(cx).focus(window, cx);
                                    })
                                })
                                .child(Input::new(&endpoint_token)),
                        )
                        .children(
                            error.map(|error| div().mt_2().text_color(rgb(0xf87171)).child(error)),
                        )
                        .child(
                            div()
                                .mt_5()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("cancel-join")
                                        .h(px(32.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x52525b))
                                        .text_color(if joining {
                                            rgb(0x71717a)
                                        } else {
                                            rgb(0xd4d4d8)
                                        })
                                        .when(!joining, |this| {
                                            this.cursor_pointer()
                                                .hover(|this| this.bg(rgb(0x3a3a3e)))
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.close_join_dialog(cx);
                                        }))
                                        .child("Cancel"),
                                )
                                .child(
                                    div()
                                        .id("confirm-join")
                                        .h(px(32.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .bg(if can_join {
                                            rgb(USER_ACCENT)
                                        } else {
                                            rgb(0x3f3f46)
                                        })
                                        .text_color(if can_join {
                                            rgb(0xffffff)
                                        } else {
                                            rgb(0x71717a)
                                        })
                                        .when(can_join, |this| {
                                            this.cursor_pointer().hover(|this| this.opacity(0.9))
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.join_shared_thread(window, cx);
                                        }))
                                        .child(if joining { "Joining…" } else { "Join thread" }),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn markdown_text_leaves(markdown: &str) -> Vec<MarkdownTextLeaf> {
        fn collect(node: &markdown::mdast::Node, source: &str, leaves: &mut Vec<MarkdownTextLeaf>) {
            let atomic = matches!(node, markdown::mdast::Node::InlineCode(_));
            if (matches!(node, markdown::mdast::Node::Text(_)) || atomic)
                && let Some(position) = node.position()
                && position.start.offset < position.end.offset
                && source
                    .get(position.start.offset..position.end.offset)
                    .is_some()
            {
                let mut source_start = position.start.offset;
                if !atomic
                    && let Some(previous) = source_start.checked_sub(1)
                    && source.as_bytes().get(previous) == Some(&b'\\')
                {
                    source_start = previous;
                }
                let source_range = source_start..position.end.offset;
                leaves.push(MarkdownTextLeaf {
                    source_range: source_range.clone(),
                    annotation_range: position.start.offset..position.end.offset,
                    atomic,
                });
                return;
            }
            if let Some(children) = node.children() {
                for child in children {
                    collect(child, source, leaves);
                }
            }
        }

        let Ok(tree) = markdown::to_mdast(markdown, &markdown::ParseOptions::gfm()) else {
            return Vec::new();
        };
        let mut leaves = Vec::new();
        collect(&tree, markdown, &mut leaves);
        leaves
    }

    fn selected_message_source_range(
        &self,
        thread_message_id: ThreadMessageId,
        text_view: &Entity<TextViewState>,
        cx: &App,
    ) -> Option<Range<usize>> {
        let mut selected_ranges = self
            .segment_text_views
            .iter()
            .filter(|((segment_id, _), _)| *segment_id == thread_message_id)
            .filter_map(|((_, source_range), text_view)| {
                text_view
                    .state
                    .read(cx)
                    .selected_source_range()
                    .map(|range| {
                        let range = text_view
                            .source_offsets
                            .as_ref()
                            .and_then(|offsets| {
                                Some(*offsets.get(range.start)?..*offsets.get(range.end)?)
                            })
                            .unwrap_or(range);
                        (range.start + source_range.start)..(range.end + source_range.start)
                    })
            });
        let first = selected_ranges.next();
        let segmented = selected_ranges.fold(first, |combined, range| {
            Some(match combined {
                Some(combined) => combined.start.min(range.start)..combined.end.max(range.end),
                None => range,
            })
        });

        segmented.or_else(|| text_view.read(cx).selected_source_range())
    }

    fn annotation_ranges(markdown: &str, selection: Range<usize>) -> Vec<Range<usize>> {
        let mut ranges = Vec::<Range<usize>>::new();
        for leaf in Self::markdown_text_leaves(markdown) {
            let start = leaf.source_range.start.max(selection.start);
            let end = leaf.source_range.end.min(selection.end);
            if start >= end {
                continue;
            }
            let range = if leaf.atomic {
                leaf.annotation_range
            } else {
                start..end
            };
            if let Some(previous) = ranges.last_mut()
                && previous.end == range.start
            {
                previous.end = range.end;
            } else {
                ranges.push(range);
            }
        }
        ranges
    }

    fn annotate_markdown_with_source_offsets(
        markdown: &str,
        ranges: impl IntoIterator<Item = Range<usize>>,
    ) -> (String, Vec<usize>) {
        let mut ranges = ranges
            .into_iter()
            .filter(|range| {
                range.start < range.end
                    && range.end <= markdown.len()
                    && markdown.is_char_boundary(range.start)
                    && markdown.is_char_boundary(range.end)
            })
            .collect::<Vec<_>>();
        ranges.sort_by_key(|range| range.start);

        let mut merged_ranges = Vec::<Range<usize>>::new();
        for range in ranges {
            if let Some(previous) = merged_ranges.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
            } else {
                merged_ranges.push(range);
            }
        }

        let mut annotated = String::new();
        let mut source_offsets = vec![0];
        let mut cursor = 0;
        for range in merged_ranges {
            annotated.push_str(&markdown[cursor..range.start]);
            source_offsets.extend((cursor + 1)..=range.start);

            annotated.push('[');
            source_offsets.push(range.start);

            annotated.push_str(&markdown[range.clone()]);
            source_offsets.extend((range.start + 1)..=range.end);

            const LINK_SUFFIX: &str = "](#inline-comment)";
            annotated.push_str(LINK_SUFFIX);
            source_offsets.extend(std::iter::repeat_n(range.end, LINK_SUFFIX.len()));
            cursor = range.end;
        }
        annotated.push_str(&markdown[cursor..]);
        source_offsets.extend((cursor + 1)..=markdown.len());

        debug_assert_eq!(source_offsets.len(), annotated.len() + 1);
        (annotated, source_offsets)
    }

    fn begin_inline_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .is_some_and(|thread| !thread.read(cx).ownership.can_write())
        {
            return;
        }

        if event.keystroke.key == "escape" {
            let Some(thread) = self
                .active_thread_id
                .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            else {
                return;
            };
            let empty_comment_id = thread.read(cx).draft.comments.iter().find_map(|comment| {
                let UserCommentBody::Editing { inline, composer } = &comment.body else {
                    return None;
                };
                let focused_editor = if inline.focus_handle(cx).is_focused(window) {
                    inline
                } else if composer.focus_handle(cx).is_focused(window) {
                    composer
                } else {
                    return None;
                };
                focused_editor
                    .read(cx)
                    .value()
                    .trim()
                    .is_empty()
                    .then_some(comment.id)
            });
            let Some(comment_id) = empty_comment_id else {
                return;
            };

            thread.update(cx, |thread, _| {
                thread
                    .draft
                    .comments
                    .retain(|comment| comment.id != comment_id);
            });
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.platform
            || event.keystroke.modifiers.function
        {
            return;
        }
        let Some(initial_text) = event.keystroke.key_char.as_deref() else {
            return;
        };
        if initial_text.chars().all(char::is_control) {
            return;
        }
        let quote = TextSelection::selected_text(window, cx).trim().to_string();
        let (Some(thread_id), Some(preferred_message_id)) =
            (self.active_thread_id, self.selection_message_id)
        else {
            return;
        };
        if quote.is_empty() {
            return;
        }
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let (message_id, source_range) = {
            let thread = thread.read(cx);
            let mut targets = thread
                .timeline
                .iter()
                .filter_map(|entry| match entry {
                    TimelineMessage::Agent(message) => Some(
                        std::iter::once((message.id, message.text_view.clone())).chain(
                            message
                                .comment_responses
                                .iter()
                                .map(|response| (response.id, response.response_view.clone())),
                        ),
                    ),
                    TimelineMessage::User(_) => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            targets.sort_by_key(|(message_id, _)| *message_id != preferred_message_id);
            let Some(target) = targets.into_iter().find_map(|(message_id, text_view)| {
                let thread_message_id = ThreadMessageId {
                    thread_id,
                    message_id,
                };
                self.selected_message_source_range(thread_message_id, &text_view, cx)
                    .map(|source_range| (message_id, source_range))
            }) else {
                return;
            };
            target
        };

        let inline_body = Self::new_comment_editor(initial_text, window, cx);
        let composer_body = Self::new_comment_editor(initial_text, window, cx);
        let comment_id = Uuid::new_v4();
        thread.update(cx, |thread, _| {
            thread.draft.comments.push(UserComment {
                id: comment_id,
                reference: CommentReference {
                    message_id,
                    range: source_range,
                    quote,
                },
                body: UserCommentBody::Editing {
                    inline: inline_body.clone(),
                    composer: composer_body.clone(),
                },
            });
            thread.draft.comments_folded = false;
        });
        Self::synchronize_comment_editor(
            &inline_body,
            composer_body.clone(),
            thread.clone(),
            comment_id,
            window.window_handle(),
            cx,
        );
        Self::synchronize_comment_editor(
            &composer_body,
            inline_body.clone(),
            thread,
            comment_id,
            window.window_handle(),
            cx,
        );
        TextSelection::clear(window, cx);
        inline_body.focus_handle(cx).focus(window, cx);
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    fn synchronize_comment_editor(
        source: &Entity<TextareaState>,
        target: Entity<TextareaState>,
        thread: Entity<Thread>,
        comment_id: Uuid,
        window_handle: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let source = source.clone();
        cx.subscribe(
            &source.clone(),
            move |_, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let value = source.read(cx).value();
                    cx.defer({
                        let value = value.clone();
                        let target = target.clone();
                        move |cx| {
                            let _ = cx.update_window(window_handle, |_, window, cx| {
                                if target.read(cx).value() != value {
                                    target.update(cx, |target, cx| {
                                        target.set_value(value, window, cx);
                                    });
                                }
                            });
                        }
                    });
                    cx.notify();
                }
                InputEvent::Blur if source.read(cx).value().trim().is_empty() => {
                    thread.update(cx, |thread, _| {
                        thread
                            .draft
                            .comments
                            .retain(|comment| comment.id != comment_id);
                    });
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
    }

    fn toggle_comment_group(&mut self, group_id: Uuid, cx: &mut Context<Self>) {
        if self.new_thread_draft.id == group_id {
            self.new_thread_draft.comments_folded = !self.new_thread_draft.comments_folded;
            cx.notify();
            return;
        }

        let threads = self.thread_store.read(cx).threads.clone();
        for thread in threads {
            let toggled = thread.update(cx, |thread, _| {
                if thread.draft.id == group_id {
                    thread.draft.comments_folded = !thread.draft.comments_folded;
                    return true;
                }
                for entry in &mut thread.timeline {
                    if let TimelineMessage::User(group) = entry
                        && group.id == group_id
                    {
                        group.comments_folded = !group.comments_folded;
                        return true;
                    }
                }
                false
            });
            if toggled {
                break;
            }
        }
        cx.notify();
    }

    fn render_comment_group_toggle(
        group_id: Uuid,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = format!("{} comment{}", count, if count == 1 { "" } else { "s" });
        div()
            .id(format!("toggle-comments-{group_id}"))
            .h(px(24.))
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_color(rgb(0xa1a1aa))
            .hover(|this| this.text_color(rgb(0xe4e4e7)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_comment_group(group_id, cx);
            }))
            .child(label)
            .child(if collapsed { "›" } else { "⌄" })
    }

    fn render_inline_comment(comment: &UserComment) -> gpui::AnyElement {
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(rgb(0xe4e4e7))
                .child(body.clone())
                .into_any_element(),
            UserCommentBody::Editing { inline, .. } => div()
                .id(format!("comment-editor-inline-{}", comment.id))
                .flex_1()
                .min_w_0()
                .child(Textarea::new(inline))
                .into_any_element(),
        };

        div()
            .id(format!("inline-comment-{}", comment.id))
            .w_full()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x303036))
            .bg(rgb(0x1d1d20))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_l_2()
                    .border_color(rgb(USER_ACCENT))
                    .child(Self::render_avatar(MessageAuthor::User))
                    .child(body),
            )
            .into_any_element()
    }

    fn render_composer_comment(comment: &UserComment) -> gpui::AnyElement {
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(rgb(0xe4e4e7))
                .child(body.clone())
                .into_any_element(),
            UserCommentBody::Editing { composer, .. } => div()
                .id(format!("comment-editor-composer-{}", comment.id))
                .flex_1()
                .min_w_0()
                .child(Textarea::new(composer))
                .into_any_element(),
        };

        div()
            .id(format!("composer-comment-{}", comment.id))
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(rgb(0x303036))
            .bg(rgb(0x202023))
            .child(
                div()
                    .w_full()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .text_color(rgb(0xd4d4d8))
                    .line_clamp(2)
                    .child(comment.reference.quote.clone()),
            )
            .child(
                div().w_full().px_3().pb_3().child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .border_1()
                        .border_color(rgb(0x303036))
                        .rounded_md()
                        .bg(rgb(0x1d1d20))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .border_l_2()
                                .border_color(rgb(USER_ACCENT))
                                .child(Self::render_avatar(MessageAuthor::User))
                                .child(body),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_avatar(author: MessageAuthor) -> gpui::Div {
        div()
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .rounded_full()
            .when(matches!(author, MessageAuthor::User), |this| {
                this.bg(rgb(USER_ACCENT))
                    .text_xs()
                    .text_color(rgb(0xf4f4f5))
                    .child("U")
            })
            .when(matches!(author, MessageAuthor::Agent), |this| {
                this.child(img(OLLAMA_AVATAR_PATH).size_full())
            })
    }

    fn markdown_style() -> TextViewStyle {
        let code_background = rgb(0x27272a);

        TextViewStyle::default()
            .with_foreground(rgb(0xd4d4d8).into())
            .with_muted_foreground(rgb(0x8b8b95).into())
            .with_link(rgb(0x60a5fa).into())
            .with_selection(rgba(0xe26d5a40).into())
            .with_code_background(code_background.into())
            .with_border(rgb(0x3f3f46).into())
            .with_paragraph_gap(rems(0.75))
            .with_heading_base_font_size(px(14.))
            .with_code_block(
                gpui::StyleRefinement::default()
                    .bg(code_background)
                    .text_color(rgb(0xd4d4d8)),
            )
            .with_inline_code(HighlightStyle {
                color: Some(rgb(0xe4e4e7).into()),
                background_color: Some(code_background.into()),
                ..Default::default()
            })
            .with_table(
                gpui::StyleRefinement::default()
                    .bg(rgb(0x18181b))
                    .text_color(rgb(0xd4d4d8)),
            )
            .with_table_head(
                gpui::StyleRefinement::default()
                    .bg(code_background)
                    .text_color(rgb(0xe4e4e7)),
            )
            .with_table_cell(gpui::StyleRefinement::default().text_color(rgb(0xd4d4d8)))
            .with_dark(true)
    }

    fn annotated_markdown_style() -> TextViewStyle {
        Self::markdown_style().with_link(rgb(USER_ACCENT).into())
    }

    fn render_message_segment(
        &mut self,
        thread_message_id: ThreadMessageId,
        _segment_index: usize,
        source_range: Range<usize>,
        text: &str,
        annotated: bool,
        source_offsets: Option<Vec<usize>>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let text_view = self
            .segment_text_views
            .entry((thread_message_id, source_range))
            .or_insert_with(|| SegmentTextView {
                state: cx.new(|cx| TextViewState::markdown(text, cx)),
                text: text.to_owned(),
                source_offsets: source_offsets.clone(),
                rendered_at: self.render_generation,
            });
        text_view.rendered_at = self.render_generation;
        text_view.source_offsets = source_offsets;
        if text_view.text != text {
            text_view.text.clear();
            text_view.text.push_str(text);
            if annotated {
                // Reparse the whole annotated segment. Incrementally replacing
                // Markdown can retain stale link-render caches and drop an
                // existing highlight when a neighboring annotation is added.
                text_view.state = cx.new(|cx| TextViewState::markdown(text, cx));
            } else {
                text_view
                    .state
                    .update(cx, |view, cx| view.set_text(text, cx));
            }
        }
        TextView::new(&text_view.state)
            .selection_format(SelectionFormat::Plain)
            .style(if annotated {
                Self::annotated_markdown_style()
            } else {
                Self::markdown_style()
            })
            .w_full()
            .into_any_element()
    }

    fn hard_line_start(text: &str, offset: usize) -> usize {
        text[..offset].rfind('\n').map_or(0, |offset| offset + 1)
    }

    fn wrapped_line_end(
        text: &str,
        selection_end: usize,
        wrap_width: gpui::Pixels,
        window: &mut Window,
    ) -> usize {
        let hard_line_start = Self::hard_line_start(text, selection_end);
        let hard_line_end = text[selection_end..]
            .find('\n')
            .map_or(text.len(), |offset| selection_end + offset);
        let hard_line = &text[hard_line_start..hard_line_end];
        let selected_end_in_line = selection_end - hard_line_start;
        let font_size = px(14.);
        let mut wrapper = window
            .text_system()
            .line_wrapper(window.text_style().font(), font_size);
        let fragments = [LineFragment::text(hard_line)];
        let end = wrapper
            .wrap_line(&fragments, wrap_width)
            .map(|boundary| boundary.ix)
            .find(|boundary| *boundary >= selected_end_in_line)
            .unwrap_or(hard_line.len());
        let source_end = hard_line_start + end;
        if source_end == hard_line_end && hard_line_end < text.len() {
            hard_line_end + 1
        } else {
            source_end
        }
    }

    fn render_user_message_group(
        &self,
        index: usize,
        group: &UserMessageGroup,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut content = Vec::new();
        if !group.comments.is_empty() {
            content.push(
                Self::render_comment_group_toggle(
                    group.id,
                    group.comments.len(),
                    group.comments_folded,
                    cx,
                )
                .into_any_element(),
            );
            if !group.comments_folded {
                content.extend(group.comments.iter().map(Self::render_composer_comment));
            }
        }
        if let UserMessageContent::Submitted { text, .. } = &group.content
            && !text.trim().is_empty()
        {
            content.push(
                SelectableText::new(format!("timeline-user-text-{}", group.id), text.clone())
                    .document_order((index * 1_000 + content.len()) as u64)
                    .into_any_element(),
            );
        }

        div()
            .id(("timeline-message", index))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.selection_message_id = None;
                }),
            )
            .w_full()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(Self::render_avatar(MessageAuthor::User)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(content),
            )
            .child(div().w(px(40.)).flex_none())
            .into_any_element()
    }

    fn toggle_thinking(&mut self, thread_id: Uuid, message_id: Uuid, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        thread.update(cx, |thread, _| {
            for entry in &mut thread.timeline {
                if let TimelineMessage::Agent(message) = entry
                    && message.id == message_id
                    && message.thinking_complete
                    && !message.thinking.is_empty()
                {
                    message.thinking_expanded = !message.thinking_expanded;
                    break;
                }
            }
        });
        cx.notify();
    }

    fn render_agent_text(
        &mut self,
        thread_message_id: ThreadMessageId,
        text: &str,
        text_view: &Entity<TextViewState>,
        comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut content = Vec::new();
        let mut cursor = 0;
        let mut anchored_comments = comments
            .iter()
            .filter(|comment| comment.reference.message_id == thread_message_id.message_id)
            .filter(|comment| {
                comment.reference.range.start < comment.reference.range.end
                    && comment.reference.range.end <= text.len()
                    && text.is_char_boundary(comment.reference.range.start)
                    && text.is_char_boundary(comment.reference.range.end)
            })
            .collect::<Vec<_>>();
        anchored_comments.sort_by_key(|comment| comment.reference.range.start);

        if anchored_comments.is_empty() && !text.is_empty() {
            content.push(
                TextView::new(text_view)
                    .selection_format(SelectionFormat::Plain)
                    .style(Self::markdown_style())
                    .w_full()
                    .into_any_element(),
            );
            return content;
        }

        let mut comment_index = 0;
        while comment_index < anchored_comments.len() {
            let first = anchored_comments[comment_index];
            if first.reference.range.start < cursor {
                comment_index += 1;
                continue;
            }
            let annotated_start =
                Self::hard_line_start(text, first.reference.range.start).max(cursor);
            if cursor < annotated_start {
                content.push(self.render_message_segment(
                    thread_message_id,
                    content.len(),
                    cursor..annotated_start,
                    &text[cursor..annotated_start],
                    false,
                    None,
                    cx,
                ));
                cursor = annotated_start;
            }
            let line_end =
                Self::wrapped_line_end(text, first.reference.range.end, wrap_width, window);
            let group_start = comment_index;
            while comment_index < anchored_comments.len()
                && anchored_comments[comment_index].reference.range.start < line_end
            {
                comment_index += 1;
            }
            let group = &anchored_comments[group_start..comment_index];
            let annotation_ranges = group
                .iter()
                .flat_map(|comment| Self::annotation_ranges(text, comment.reference.range.clone()))
                .filter_map(|range| {
                    (range.start >= cursor && range.end <= line_end)
                        .then_some((range.start - cursor)..(range.end - cursor))
                });
            let (annotated, source_offsets) = Self::annotate_markdown_with_source_offsets(
                &text[cursor..line_end],
                annotation_ranges,
            );
            content.push(self.render_message_segment(
                thread_message_id,
                content.len(),
                cursor..line_end,
                &annotated,
                true,
                Some(source_offsets),
                cx,
            ));
            content.extend(
                group
                    .iter()
                    .map(|comment| Self::render_inline_comment(comment)),
            );
            cursor = line_end;
        }
        if cursor < text.len() {
            content.push(self.render_message_segment(
                thread_message_id,
                content.len(),
                cursor..text.len(),
                &text[cursor..],
                false,
                None,
                cx,
            ));
        }
        content
    }

    fn render_agent_message(
        &mut self,
        thread_id: Uuid,
        index: usize,
        message: &AgentMessage,
        comments: &[UserComment],
        submitted_comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = !message.complete && message.thinking.is_empty() && message.text.is_empty();
        let mut submitted_comment_content = Vec::new();
        for comment in submitted_comments {
            submitted_comment_content.push(Self::render_composer_comment(comment));
            if let Some(response) = message
                .comment_responses
                .iter()
                .find(|response| response.comment_id == comment.id)
            {
                let response_id = response.id;
                let response_content = self.render_agent_text(
                    ThreadMessageId {
                        thread_id,
                        message_id: response.id,
                    },
                    &response.response,
                    &response.response_view,
                    comments,
                    wrap_width,
                    window,
                    cx,
                );
                submitted_comment_content.push(
                    div()
                        .id(format!("comment-response-{response_id}"))
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x242428))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, _| {
                                this.selection_message_id = Some(response_id);
                            }),
                        )
                        .children(response_content)
                        .into_any_element(),
                );
            }
        }

        let message_content = self.render_agent_text(
            ThreadMessageId {
                thread_id,
                message_id: message.id,
            },
            &message.text,
            &message.text_view,
            comments,
            wrap_width,
            window,
            cx,
        );

        let message_id = message.id;
        let thinking_expanded = !message.thinking_complete || message.thinking_expanded;
        let thinking_content = (!message.thinking.is_empty() && thinking_expanded).then(|| {
            TextView::new(&message.thinking_view)
                .selection_format(SelectionFormat::Plain)
                .style(Self::markdown_style())
                .w_full()
                .into_any_element()
        });
        let thinking = (!message.thinking.is_empty()).then(|| {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .id(format!("toggle-thinking-{message_id}"))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(rgb(0x71717a))
                        .when(message.thinking_complete, |this| {
                            this.hover(|this| this.text_color(rgb(0xa1a1aa)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_thinking(thread_id, message_id, cx);
                                }))
                        })
                        .child(if message.thinking_complete {
                            "Thinking"
                        } else {
                            "Thinking…"
                        }),
                )
                .children(thinking_content.map(|content| {
                    div()
                        .pl_3()
                        .border_l_1()
                        .border_color(rgb(0x3f3f46))
                        .opacity(0.7)
                        .child(content)
                }))
        });
        div()
            .id(("timeline-message", index))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.selection_message_id = Some(message_id);
                }),
            )
            .w_full()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(Self::render_avatar(MessageAuthor::Agent)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(message.failed, |this| this.text_color(rgb(0xf87171)))
                    .children(submitted_comment_content)
                    .when(waiting, |this| {
                        this.child(
                            div()
                                .text_color(rgb(0x8b8b95))
                                .child("Thinking…")
                                .with_animation(
                                    ("agent-waiting", index),
                                    Animation::new(Duration::from_millis(900)).repeat(),
                                    |label, delta| {
                                        label.opacity(if delta < 0.5 { 1. } else { 0.45 })
                                    },
                                ),
                        )
                    })
                    .children(thinking)
                    .children(message_content),
            )
            .child(div().w(px(40.)).flex_none())
            .into_any_element()
    }

    fn render_timeline_message(
        &mut self,
        thread_id: Uuid,
        index: usize,
        message: &TimelineMessage,
        comments: &[UserComment],
        submitted_comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match message {
            TimelineMessage::User(group) => self.render_user_message_group(index, group, cx),
            TimelineMessage::Agent(message) => self.render_agent_message(
                thread_id,
                index,
                message,
                comments,
                submitted_comments,
                wrap_width,
                window,
                cx,
            ),
        }
    }

    fn rig_history(messages: &[TimelineMessage]) -> Vec<RigMessage> {
        messages
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => {
                    let UserMessageContent::Submitted { text, history_text } = &group.content
                    else {
                        return None;
                    };
                    Some(RigMessage::user(history_text.as_deref().unwrap_or(text)))
                }
                TimelineMessage::Agent(message) if message.complete && !message.failed => {
                    let submitted_comments = message
                        .comment_group_id
                        .and_then(|group_id| {
                            messages.iter().find_map(|entry| match entry {
                                TimelineMessage::User(group) if group.id == group_id => {
                                    Some(group.comments.as_slice())
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    let mut history_text = submitted_comments
                        .iter()
                        .enumerate()
                        .filter_map(|(index, comment)| {
                            message
                                .comment_responses
                                .iter()
                                .find(|response| response.comment_id == comment.id)
                                .map(|response| (index, response))
                        })
                        .map(|(index, response)| {
                            format!("Reply to comment {}: {}", index + 1, response.response)
                        })
                        .join("\n\n");
                    if !message.text.is_empty() {
                        if !history_text.is_empty() {
                            history_text.push_str("\n\n");
                        }
                        history_text.push_str(&message.text);
                    }
                    Some(RigMessage::assistant(history_text))
                }
                TimelineMessage::Agent(_) => None,
            })
            .collect()
    }

    fn start_generation(
        &mut self,
        thread_id: Uuid,
        prompt: String,
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
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::AgentStarted {
                    id: message_id.into_bytes(),
                    comment_group_id: comment_group_id.map(Uuid::into_bytes),
                },
                cx,
            );
        });

        let (sender, mut receiver) = mpsc::unbounded_channel();
        let tool_comments = turn_comments.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation_task = self.tokio_handle.spawn(async move {
            let client = Ollama::new().bound()?;
            let model = client.completion(OLLAMA_MODEL);
            let mut tools = ToolSet::default();
            tools.add_tool(RespondToComment::new(tool_comments));
            StreamingAgent::new(model, tools)
                .additional_params(json!({
                    "num_ctx": OLLAMA_CONTEXT_TOKENS,
                    "think": "medium"
                }))
                .run(RigMessage::user(prompt), &mut history, move |event| {
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
            while let Some(item) = receiver.recv().await {
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
                thread.update(cx, |thread, cx| {
                    thread.emit(
                        protocol::HostMessage::AgentEnded {
                            id: message_id.into_bytes(),
                            failure: error
                                .map(|error| format!("Unable to generate a response: {error}")),
                        },
                        cx,
                    );
                });
                _ = this.update(cx, |this, cx| {
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
            AgentEvent::Model(_) | AgentEvent::ToolCall(_) | AgentEvent::ToolResult { .. } => None,
        }
    }

    /// Redraws the timeline after `thread_id` changed, staying pinned to the
    /// newest output unless the user has scrolled away.
    fn thread_updated(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        if self.active_thread_id != Some(thread_id) {
            return;
        }
        if self.follow_generation {
            self.timeline_scroll_handle.scroll_to_bottom();
        }
        cx.notify();
    }

    fn thread_title(prompt: &str) -> String {
        const MAX_CHARACTERS: usize = 32;

        let mut title = Itertools::intersperse(prompt.split_whitespace(), " ")
            .flat_map(str::chars)
            .take(MAX_CHARACTERS + 1)
            .collect::<String>();
        if title.chars().count() > MAX_CHARACTERS {
            title.pop();
            title.push('…');
        }
        title
    }

    fn title_for_first_message(timeline: &[TimelineMessage], prompt: &str) -> Option<String> {
        timeline.is_empty().then(|| Self::thread_title(prompt))
    }

    fn editable_comment_body(comment: &UserComment, cx: &App) -> Option<SharedString> {
        let UserCommentBody::Editing { inline, .. } = &comment.body else {
            return None;
        };
        let value = inline.read(cx).value();
        (!value.trim().is_empty()).then_some(value)
    }

    fn prompt_with_comments(
        prompt: &str,
        comments: &[UserComment],
        comment_ids: &[tools::CommentId],
        timeline: &[TimelineMessage],
    ) -> String {
        if comments.is_empty() {
            return prompt.to_string();
        }

        let mut result = String::from(
            "The user attached the following inline comments to immutable excerpts from the conversation. You MUST call `respond_to_comment` exactly once for every comment_id before finishing your response. Put the direct reply to that comment in the tool's `response` argument; do not repeat these replies in your final prose.\n",
        );
        for (index, (comment, comment_id)) in comments.iter().zip(comment_ids.iter()).enumerate() {
            let UserCommentBody::Submitted(body) = &comment.body else {
                continue;
            };
            let message_number = timeline
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        TimelineMessage::Agent(message)
                            if message.id == comment.reference.message_id
                                || message.comment_responses.iter().any(|response| {
                                    response.id == comment.reference.message_id
                                })
                    )
                })
                .map(|index| index + 1)
                .unwrap_or_default();
            result.push_str(&format!(
                "\n{}. {} — Excerpt from assistant message {}:\n> {}\nComment: {}\n",
                index + 1,
                comment_id,
                message_number,
                comment.reference.quote.replace('\n', "\n> "),
                body.trim(),
            ));
        }
        if !prompt.trim().is_empty() {
            result.push_str("\nAdditional user message:\n");
            result.push_str(prompt);
        }
        result
    }

    fn stop_generation(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let Some(generation) = self.active_generations.get(&thread_id) else {
            return;
        };
        generation.cancelled.store(true, Ordering::Release);
        generation.abort_handle.abort();
        cx.notify();
    }

    fn composer_button_clicked(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generating = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .is_some_and(|thread| thread.read(cx).generating);
        if generating {
            self.stop_generation(cx);
        } else {
            self.submit_composer(window, cx);
        }
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_thread = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        if active_thread.as_ref().is_some_and(|thread| {
            let thread = thread.read(cx);
            !thread.ownership.can_write() || thread.generating
        }) {
            return;
        }

        let draft = active_thread
            .as_ref()
            .map(|thread| thread.read(cx).draft.clone())
            .unwrap_or_else(|| self.new_thread_draft.clone());
        let composer = Self::draft_composer(&draft);
        let prompt = composer.read(cx).value().to_string();
        let mut submitted_comments = Vec::new();
        let mut remaining_comments = Vec::new();
        for comment in &draft.comments {
            if let Some(body) = Self::editable_comment_body(comment, cx) {
                submitted_comments.push(UserComment {
                    id: comment.id,
                    reference: comment.reference.clone(),
                    body: UserCommentBody::Submitted(body),
                });
            } else {
                remaining_comments.push(comment);
            }
        }
        let has_comments = !submitted_comments.is_empty();
        if prompt.trim().is_empty() && !has_comments {
            return;
        }
        let remaining_comments = remaining_comments.into_iter().cloned().collect::<Vec<_>>();

        let timeline = active_thread
            .as_ref()
            .map(|thread| thread.read(cx).timeline.clone())
            .unwrap_or_default();
        let history = Self::rig_history(&timeline);
        let turn_comments = Arc::new(TurnComments::new(submitted_comments.len()));
        let agent_prompt = Self::prompt_with_comments(
            &prompt,
            &submitted_comments,
            turn_comments.comment_ids(),
            &timeline,
        );
        let submitted_comment_ids = submitted_comments
            .iter()
            .map(|comment| comment.id)
            .collect::<Vec<_>>();
        let submitted_group = UserMessageGroup {
            id: draft.id,
            comments: submitted_comments,
            content: UserMessageContent::Submitted {
                text: prompt.clone(),
                history_text: has_comments.then(|| agent_prompt.clone()),
            },
            comments_folded: has_comments || draft.comments_folded,
        };
        let mut next_draft = Self::new_user_message_draft(window, cx);
        next_draft.comments = remaining_comments;
        next_draft.comments_folded = !next_draft.comments.is_empty() && draft.comments_folded;
        let next_composer = Self::draft_composer(&next_draft);

        let thread_id = if let Some(thread) = active_thread {
            let thread_id = thread.read(cx).instance_id;
            thread.update(cx, |thread, cx| {
                if let Some(title) = Self::title_for_first_message(&thread.timeline, &prompt) {
                    thread.emit(protocol::HostMessage::ThreadTitled(title), cx);
                }
                if let Some(message) = submitted_group.to_protocol() {
                    thread.publish(protocol::HostMessage::UserMessage(message));
                }
                thread.timeline.push(TimelineMessage::User(submitted_group));
                thread.draft = next_draft;
            });
            thread_id
        } else {
            let thread_id = Uuid::new_v4();
            let thread = cx.new(|_| Thread {
                instance_id: thread_id,
                summary: ThreadSummary {
                    id: thread_id,
                    title: Self::thread_title(&prompt),
                },
                timeline: vec![TimelineMessage::User(submitted_group)],
                draft: next_draft,
                generating: false,
                sharing: ThreadSharing::NotShared,
                ownership: ThreadOwnership::Local,
            });
            self.new_thread_draft = Self::new_user_message_draft(window, cx);
            self.thread_store.update(cx, |store, _| {
                store.threads.push_front(thread.clone());
            });
            self.active_thread_id = Some(thread_id);
            thread_id
        };

        self.selection_message_id = None;
        self.follow_generation = true;
        next_composer.focus_handle(cx).focus(window, cx);
        self.start_generation(
            thread_id,
            agent_prompt,
            history,
            has_comments.then_some(draft.id),
            submitted_comment_ids,
            turn_comments,
            cx,
        );
        self.timeline_scroll_handle.scroll_to_bottom();
    }

    fn timeline_scrolled(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        let delta_y = event.delta.pixel_delta(window.line_height()).y;
        let max_offset = self.timeline_scroll_handle.max_offset().y;

        if delta_y > px(0.) && max_offset > px(0.) {
            self.follow_generation = false;
        } else if delta_y < px(0.) {
            let projected_offset = self.timeline_scroll_handle.offset().y + delta_y;
            if projected_offset <= -max_offset + px(1.) {
                self.follow_generation = true;
            }
        }
    }

    fn render_bottom_bar(
        &self,
        composer: Option<Entity<TextareaState>>,
        read_only_line_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_scroll_handle = self.timeline_scroll_handle.clone();
        let show_button = composer.is_some();
        let generating = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .is_some_and(|thread| thread.read(cx).generating);
        let button = if generating {
            Button::new("stop-generation")
                .icon(Icon::new(AssetIconName::Square))
                .danger()
                .small()
                .accessibility_label("Stop generating")
                .on_click(cx.listener(Self::composer_button_clicked))
        } else {
            Button::new("send-message")
                .icon(Icon::new(AssetIconName::SendHorizontal))
                .primary()
                .small()
                .accessibility_label("Send message")
                .on_click(cx.listener(Self::composer_button_clicked))
        };

        div()
            .id("bottom-bar")
            .debug_selector(|| "bottom-bar".to_owned())
            .relative()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .border_l_1()
            .border_color(rgb(0x2d2d30))
            .bg(rgb(0x18181b))
            .flex()
            .items_center()
            .justify_end()
            .px_3()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let content_bottom = if let Some(composer) = &composer {
                            let composer = composer.read(cx);
                            let text_end = composer.value().len();
                            composer
                                .range_to_bounds(&(text_end..text_end))
                                .map(|bounds| bounds.bottom())
                        } else {
                            read_only_line_bounds.get().map(|bounds| bounds.bottom())
                        };
                        let Some(content_bottom) = content_bottom else {
                            return;
                        };
                        let scroll_offset = timeline_scroll_handle.offset().y;
                        let max_scroll_offset = timeline_scroll_handle.max_offset().y;
                        let is_scrolled_to_bottom = max_scroll_offset > px(0.)
                            && scroll_offset <= -max_scroll_offset + px(1.);
                        let divider_visible = !is_scrolled_to_bottom
                            && content_bottom >= bounds.top() - BOTTOM_BAR_DIVIDER_THRESHOLD;

                        if divider_visible {
                            window.paint_quad(gpui::fill(bounds, rgb(0x2d2d30)));
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(1.)),
            )
            .when(show_button, |this| this.child(button))
    }

    fn submit_composer_action(
        &mut self,
        _: &SubmitComposer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_composer(window, cx);
    }

    fn render_composer_input(composer: &Entity<TextareaState>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("composer")
            .debug_selector(|| "composer".to_owned())
            .min_h(px(110.))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(Textarea::new(composer))
    }

    fn render_main_editor(
        &mut self,
        read_only_line_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_generation = self.render_generation.wrapping_add(1);
        let active_thread_id = self.active_thread_id;
        let (messages, draft, can_write) = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .map(|thread| {
                let thread = thread.read(cx);
                (
                    thread.timeline.clone(),
                    thread.draft.clone(),
                    thread.ownership.can_write(),
                )
            })
            .unwrap_or_else(|| (Vec::new(), self.new_thread_draft.clone(), true));
        let comments = messages
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => Some(group.comments.as_slice()),
                TimelineMessage::Agent(_) => None,
            })
            .flatten()
            .chain(draft.comments.iter())
            .cloned()
            .collect::<Vec<_>>();
        let sidebar_width = if self.sidebar_open {
            SIDEBAR_WIDTH
        } else {
            px(0.)
        };
        let available_width = window.viewport_size().width - sidebar_width - px(82.);
        let wrap_width = if available_width > px(120.) {
            available_width
        } else {
            px(120.)
        };
        let mut timeline_messages = Vec::new();
        if let Some(thread_id) = active_thread_id {
            for (index, message) in messages.iter().enumerate() {
                let submitted_comments = match message {
                    TimelineMessage::Agent(message) => message
                        .comment_group_id
                        .and_then(|group_id| {
                            messages.iter().find_map(|entry| match entry {
                                TimelineMessage::User(group) if group.id == group_id => {
                                    Some(group.comments.as_slice())
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default(),
                    TimelineMessage::User(_) => &[],
                };
                timeline_messages.push(self.render_timeline_message(
                    thread_id,
                    index,
                    message,
                    &comments,
                    submitted_comments,
                    wrap_width,
                    window,
                    cx,
                ));
            }
        }

        self.segment_text_views
            .retain(|_, text_view| text_view.rendered_at == self.render_generation);

        let composer_comments = draft
            .comments
            .iter()
            .map(Self::render_composer_comment)
            .collect::<Vec<_>>();
        let composer_comment_count = composer_comments.len();
        let composer_comment_group = (composer_comment_count > 0).then_some(draft.id);
        let composer_comments_collapsed = draft.comments_folded;
        let composer = Self::draft_composer(&draft);

        div()
            .id("main-editor")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(rgb(0x2d2d30))
            .bg(rgb(0x18181b))
            .child(
                div()
                    .id("timeline-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.timeline_scroll_handle)
                    .on_scroll_wheel(cx.listener(Self::timeline_scrolled))
                    .child(
                        div()
                            .w_full()
                            .py_6()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .text_sm()
                            .text_color(rgb(0xd4d4d8))
                            .children(timeline_messages)
                            .when(can_write, |this| {
                                this.child(
                                    div()
                                        .id("composer-row")
                                        .w_full()
                                        .flex()
                                        .items_start()
                                        .child(
                                            div()
                                                .w(px(40.))
                                                .flex_none()
                                                .flex()
                                                .justify_center()
                                                .child(Self::render_avatar(MessageAuthor::User)),
                                        )
                                        .child(
                                            div()
                                                .id("composer-column")
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .gap_3()
                                                .when_some(
                                                    composer_comment_group,
                                                    |this, group_id| {
                                                        this.child(
                                                            Self::render_comment_group_toggle(
                                                                group_id,
                                                                composer_comment_count,
                                                                composer_comments_collapsed,
                                                                cx,
                                                            ),
                                                        )
                                                    },
                                                )
                                                .when(!composer_comments_collapsed, |this| {
                                                    this.children(composer_comments)
                                                })
                                                .child(
                                                    Self::render_composer_input(&composer)
                                                        .on_click({
                                                            let composer = composer.clone();
                                                            cx.listener(move |_, _, window, cx| {
                                                                composer
                                                                    .focus_handle(cx)
                                                                    .focus(window, cx);
                                                            })
                                                        }),
                                                ),
                                        )
                                        .child(div().w(px(40.)).flex_none()),
                                )
                            })
                            .when(!can_write, |this| {
                                this.child(
                                    div()
                                        .id("read-only-thread")
                                        .relative()
                                        .w_full()
                                        .flex()
                                        .justify_center()
                                        .text_xs()
                                        .text_color(rgb(0x71717a))
                                        .child("Read-only thread")
                                        .child(
                                            canvas(
                                                move |bounds, _, _| {
                                                    read_only_line_bounds.set(Some(bounds));
                                                },
                                                |_, _, _, _| {},
                                            )
                                            .absolute()
                                            .size_full(),
                                        ),
                                )
                            }),
                    ),
            )
    }
}

impl Render for Cowork {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar_width = if self.sidebar_open {
            SIDEBAR_WIDTH
        } else {
            px(0.)
        };
        let composer = self.editable_composer(cx);
        let read_only_line_bounds = Rc::new(Cell::new(None));

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(0x1c1c1f))
            .on_action(cx.listener(Self::submit_composer_action))
            .on_key_down(cx.listener(Self::begin_inline_comment))
            .child(TextSelectionLayer)
            .child(self.render_top_bar(window, cx))
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .overflow_hidden()
                    .child(
                        self.render_sidebar(cx).with_spring(
                            "sidebar-width",
                            SpringAnimation::new(SpringConfig::new(250., 30., 1.))
                                .to(sidebar_width)
                                .with_epsilon(0.25),
                            |sidebar, width| sidebar.w(width),
                        ),
                    )
                    .child(
                        div()
                            .h_full()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(self.render_main_editor(
                                read_only_line_bounds.clone(),
                                window,
                                cx,
                            ))
                            .child(self.render_bottom_bar(composer, read_only_line_bounds, cx)),
                    ),
            )
            .children(self.render_join_dialog(cx))
    }
}

fn main() -> anyhow::Result<()> {
    let worker_threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(2, 8);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("cowork-agent")
        .enable_all()
        .build()?;
    let tokio_handle = runtime.handle().clone();
    anyhow::ensure!(
        TOKIO_RUNTIME.set(runtime).is_ok(),
        "the agent runtime was already initialized",
    );

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            ComponentTheme::change(ThemeMode::Dark, None, cx);
            TextViewDefaults::new()
                .with_code_block_highlighter(highlight_code_block)
                .install(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-enter", SubmitComposer, None),
                KeyBinding::new("cmd-enter", SubmitComposer, None),
            ]);
            #[cfg(target_os = "macos")]
            {
                cx.on_action(|_: &Quit, cx| cx.quit());
                cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            }
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(760.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Cowork".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(macos_traffic_light_position()),
                }),
                app_owns_titlebar_drag: cfg!(target_os = "macos"),
                ..Default::default()
            };

            if let Err(error) = cx.open_window(window_options, move |window, cx| {
                let tokio_handle = tokio_handle.clone();
                let thread_store = cx.new(|_| ThreadStore::default());
                let new_thread_draft = Cowork::new_user_message_draft(window, cx);
                Cowork::draft_composer(&new_thread_draft)
                    .focus_handle(cx)
                    .focus(window, cx);
                cx.new(|cx| {
                    let window_activation_subscription =
                        cx.observe_window_activation(window, |_, window, _cx| {
                            if window.is_window_active() {
                                window.on_next_frame(Cowork::end_stale_mouse_drag);
                            }
                        });
                    Cowork {
                        sidebar_open: true,
                        recents_open: true,
                        new_thread_draft,
                        timeline_scroll_handle: ScrollHandle::new(),
                        follow_generation: true,
                        thread_store,
                        active_thread_id: None,
                        selection_message_id: None,
                        segment_text_views: HashMap::new(),
                        render_generation: 0,
                        titlebar_click_armed: false,
                        join_dialog: None,
                        tokio_handle,
                        active_generations: HashMap::new(),
                        _window_activation_subscription: window_activation_subscription,
                    }
                })
            }) {
                eprintln!("failed to open Cowork window: {error}");
                cx.quit();
                return;
            }

            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annotate_markdown(markdown: &str, ranges: impl IntoIterator<Item = Range<usize>>) -> String {
        Cowork::annotate_markdown_with_source_offsets(markdown, ranges).0
    }

    #[test]
    fn annotated_markdown_uses_native_markdown_link() {
        let annotated = annotate_markdown("Before selected text after", [7..20]);

        assert_eq!(annotated, "Before [selected text](#inline-comment) after");
    }

    #[test]
    fn annotated_markdown_merges_intersecting_comments() {
        let annotated = annotate_markdown("overlapping", [0..7, 4..11]);

        assert_eq!(annotated, "[overlapping](#inline-comment)");
    }

    #[test]
    fn annotated_markdown_preserves_heading_around_partial_selection() {
        let annotated = annotate_markdown("### A Heading", [6..13]);

        assert_eq!(annotated, "### A [Heading](#inline-comment)");
    }

    #[test]
    fn annotated_markdown_preserves_bold_around_partial_selection() {
        let annotated = annotate_markdown("**Hi**", [3..4]);

        assert_eq!(annotated, "**H[i](#inline-comment)**");
    }

    #[test]
    fn comments_follow_gpui_source_ranges_across_markdown_boundaries() {
        fn through(markdown: &str, start: &str, end: &str) -> Range<usize> {
            let start_offset = markdown.find(start).expect("selection start");
            let end_offset = markdown[start_offset..]
                .find(end)
                .map(|offset| start_offset + offset + end.len())
                .expect("selection end");
            start_offset..end_offset
        }

        let cases = [
            (
                "inside bold",
                "Before **bold text** after",
                through("Before **bold text** after", "old", "old"),
                "Before **b[old](#inline-comment) text** after",
            ),
            (
                "across opening bold edge",
                "Before **bold text** after",
                through("Before **bold text** after", "re ", "bold"),
                "Befo[re ](#inline-comment)**[bold](#inline-comment) text** after",
            ),
            (
                "across closing bold edge",
                "Before **bold text** after",
                through("Before **bold text** after", "text", " af"),
                "Before **bold [text](#inline-comment)**[ af](#inline-comment)ter",
            ),
            (
                "whole bold section",
                "Before **bold text** after",
                through("Before **bold text** after", "bold", "text"),
                "Before **[bold text](#inline-comment)** after",
            ),
            (
                "across several styled sections",
                "A **bold** and *italic* tail",
                through("A **bold** and *italic* tail", "bold", " ta"),
                "A **[bold](#inline-comment)**[ and ](#inline-comment)*[italic](#inline-comment)*[ ta](#inline-comment)il",
            ),
            (
                "nested styles",
                "Start **bold and *italic*** end",
                through("Start **bold and *italic*** end", "and ", "italic"),
                "Start **bold [and ](#inline-comment)*[italic](#inline-comment)*** end",
            ),
            (
                "whole inline code",
                "Use `value` now",
                5..10,
                "Use [`value`](#inline-comment) now",
            ),
            (
                "partial inline code is atomic",
                "Use `value` now",
                6..9,
                "Use [`value`](#inline-comment) now",
            ),
            (
                "heading and emphasis",
                "### A **styled heading** here",
                through("### A **styled heading** here", "A ", "** h"),
                "### [A ](#inline-comment)**[styled heading](#inline-comment)**[ h](#inline-comment)ere",
            ),
        ];

        for (name, markdown, source_range, expected) in cases {
            let ranges = Cowork::annotation_ranges(markdown, source_range);
            let annotated = annotate_markdown(markdown, ranges);

            assert_eq!(annotated, expected, "{name}");
            let html = markdown::to_html_with_options(&annotated, &markdown::Options::gfm())
                .expect("annotated Markdown should compile");
            assert!(
                html.contains("href=\"#inline-comment\""),
                "{name}: annotation should survive Markdown rendering: {html}"
            );
        }
    }

    #[test]
    fn can_comment_on_selection_across_inline_code() {
        let markdown = "In Rust, we use `u128` to handle larger numbers";
        let ranges = Cowork::annotation_ranges(markdown, 0..markdown.len());
        let annotated = annotate_markdown(markdown, ranges);
        let html = markdown::to_html_with_options(&annotated, &markdown::Options::gfm())
            .expect("annotated Markdown should compile");

        assert_eq!(
            annotated,
            "[In Rust, we use `u128` to handle larger numbers](#inline-comment)"
        );
        assert!(html.contains(
            "<a href=\"#inline-comment\">In Rust, we use <code>u128</code> to handle larger numbers</a>"
        ));
    }

    #[test]
    fn comments_use_gpui_range_to_target_identical_styled_text() {
        let markdown = "**same** then **same**";
        let ranges = Cowork::annotation_ranges(markdown, 16..20);
        let annotated = annotate_markdown(markdown, ranges);

        assert_eq!(annotated, "**same** then **[same](#inline-comment)**");
    }

    fn assert_backslash_selection_creates_comment(
        cx: &mut gpui::TestAppContext,
        markdown: &'static str,
        expected_quote: &str,
        expected_range: Range<usize>,
        target_comment_reply: bool,
        existing_comment_range: Option<Range<usize>>,
        selection_start_x: f32,
        selection_end_x: f32,
        expected_highlights_after_comment: Option<usize>,
    ) {
        struct SelectionRoot {
            cowork: Entity<Cowork>,
            text_view: Entity<TextViewState>,
            composer: Entity<TextareaState>,
            thread_id: Uuid,
            message_id: Uuid,
            markdown: &'static str,
            comments: Vec<UserComment>,
        }

        impl Render for SelectionRoot {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let content = self.cowork.update(cx, |cowork, cx| {
                    cowork.render_agent_text(
                        ThreadMessageId {
                            thread_id: self.thread_id,
                            message_id: self.message_id,
                        },
                        self.markdown,
                        &self.text_view,
                        &self.comments,
                        px(160.),
                        window,
                        cx,
                    )
                });
                div()
                    .w(px(160.))
                    .flex()
                    .flex_col()
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        this.cowork.update(cx, |cowork, cx| {
                            cowork.begin_inline_comment(event, window, cx);
                        });
                    }))
                    .child(TextSelectionLayer)
                    .child(
                        div()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.cowork.update(cx, |cowork, _| {
                                        cowork.selection_message_id = Some(this.message_id);
                                    });
                                }),
                            )
                            .children(content),
                    )
                    .child(Textarea::new(&self.composer))
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let tokio_handle = runtime.handle().clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            let message_id = Uuid::new_v4();
            let text_view = cx.new(|cx| TextViewState::markdown(markdown, cx));
            let main_text = if target_comment_reply { "" } else { markdown };
            let main_text_view = if target_comment_reply {
                cx.new(|cx| TextViewState::markdown("", cx))
            } else {
                text_view.clone()
            };
            let thinking_view = cx.new(|cx| TextViewState::markdown("", cx));
            let thread_id = Uuid::new_v4();
            let mut draft = Cowork::new_user_message_draft(window, cx);
            if let Some(range) = existing_comment_range.clone() {
                draft.comments.push(UserComment {
                    id: Uuid::new_v4(),
                    reference: CommentReference {
                        message_id,
                        quote: markdown[range.clone()].into(),
                        range,
                    },
                    body: UserCommentBody::Submitted("Existing comment".into()),
                });
            }
            let comments = draft.comments.clone();
            let composer = Cowork::draft_composer(&draft);
            let thread = cx.new(|_| Thread {
                instance_id: thread_id,
                summary: ThreadSummary {
                    id: thread_id,
                    title: "Test".into(),
                },
                timeline: vec![TimelineMessage::Agent(AgentMessage {
                    id: if target_comment_reply {
                        Uuid::new_v4()
                    } else {
                        message_id
                    },
                    comment_group_id: None,
                    comment_responses: target_comment_reply
                        .then(|| AgentCommentResponse {
                            id: message_id,
                            comment_id: Uuid::new_v4(),
                            response: markdown.into(),
                            response_view: text_view.clone(),
                        })
                        .into_iter()
                        .collect(),
                    thinking: String::new(),
                    thinking_view,
                    thinking_complete: true,
                    thinking_expanded: false,
                    text: main_text.into(),
                    text_view: main_text_view,
                    complete: true,
                    failed: false,
                })],
                draft,
                generating: false,
                sharing: ThreadSharing::NotShared,
                ownership: ThreadOwnership::Local,
            });
            let thread_store = cx.new(|_| ThreadStore {
                threads: VecDeque::from([thread]),
            });
            let cowork = cx.new(|cx| Cowork {
                sidebar_open: true,
                recents_open: true,
                new_thread_draft: Cowork::new_user_message_draft(window, cx),
                timeline_scroll_handle: ScrollHandle::new(),
                follow_generation: true,
                thread_store,
                active_thread_id: Some(thread_id),
                selection_message_id: None,
                segment_text_views: HashMap::new(),
                render_generation: 0,
                titlebar_click_armed: false,
                join_dialog: None,
                tokio_handle,
                active_generations: HashMap::new(),
                _window_activation_subscription: cx.observe_window_activation(window, |_, _, _| {}),
            });
            composer.focus_handle(cx).focus(window, cx);
            SelectionRoot {
                cowork,
                text_view,
                composer,
                thread_id,
                message_id,
                markdown,
                comments,
            }
        });
        let cx: &mut gpui::VisualTestContext = cx;
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(selection_start_x), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(
            point(px(selection_end_x), px(8.)),
            Some(MouseButton::Left),
            gpui::Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(
            point(px(selection_end_x), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let annotated_state_before = expected_highlights_after_comment.map(|_| {
            view.read_with(cx, |view, cx| {
                view.cowork
                    .read(cx)
                    .segment_text_views
                    .iter()
                    .find(|((segment, _), _)| segment.message_id == view.message_id)
                    .expect("annotated segment")
                    .1
                    .state
                    .entity_id()
            })
        });
        cx.simulate_keystrokes("x");

        view.read_with(cx, |view, cx| {
            let cowork = view.cowork.read(cx);
            let thread = cowork
                .thread_store
                .read(cx)
                .thread(cowork.active_thread_id.expect("active thread"), cx)
                .expect("thread");
            let thread = thread.read(cx);
            let Some(comment) = thread
                .draft
                .comments
                .iter()
                .find(|comment| matches!(comment.body, UserCommentBody::Editing { .. }))
            else {
                panic!("typing with the selection should create an editable comment");
            };
            assert_eq!(comment.reference.quote, expected_quote);
            assert_eq!(comment.reference.range, expected_range);
            let UserCommentBody::Editing { inline, .. } = &comment.body else {
                unreachable!();
            };
            assert_eq!(inline.read(cx).value(), "x");
        });

        if let Some(expected_highlights) = expected_highlights_after_comment {
            let comments = view.read_with(cx, |view, cx| {
                let cowork = view.cowork.read(cx);
                cowork
                    .thread_store
                    .read(cx)
                    .thread(cowork.active_thread_id.expect("active thread"), cx)
                    .expect("thread")
                    .read(cx)
                    .draft
                    .comments
                    .clone()
            });
            view.update(cx, |view, cx| {
                view.comments = comments;
                cx.notify();
            });
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            view.read_with(cx, |view, cx| {
                let highlight_count = view
                    .cowork
                    .read(cx)
                    .segment_text_views
                    .iter()
                    .filter(|((segment, _), _)| segment.message_id == view.message_id)
                    .map(|(_, segment)| segment.text.matches("#inline-comment").count())
                    .sum::<usize>();
                assert_eq!(highlight_count, expected_highlights);
                let annotated_state_after = view
                    .cowork
                    .read(cx)
                    .segment_text_views
                    .iter()
                    .find(|((segment, _), _)| segment.message_id == view.message_id)
                    .expect("annotated segment")
                    .1
                    .state
                    .entity_id();
                assert_ne!(Some(annotated_state_after), annotated_state_before);
            });
        }
    }

    #[gpui::test]
    fn backslash_selections_create_comments_with_gpui_ranges(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            r"a\b",
            r"a\b",
            0..3,
            false,
            None,
            1.,
            155.,
            None,
        );
        assert_backslash_selection_creates_comment(
            cx,
            r"a\\b",
            r"a\b",
            0..4,
            false,
            None,
            1.,
            155.,
            None,
        );
    }

    #[gpui::test]
    fn comments_can_target_agent_comment_replies(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "Agent reply",
            "Agent reply",
            0..11,
            true,
            None,
            1.,
            155.,
            None,
        );
    }

    #[gpui::test]
    fn comments_can_target_text_before_an_existing_comment(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "alpha beta gamma",
            "alpha",
            0..5,
            false,
            Some(11..16),
            1.,
            48.,
            None,
        );
    }

    #[gpui::test]
    fn creating_comment_immediately_before_existing_preserves_both_highlights(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "alpha beta gamma",
            "beta",
            6..10,
            false,
            Some(11..16),
            54.,
            96.,
            Some(2),
        );
    }

    #[gpui::test]
    fn comments_after_an_existing_comment_keep_original_source_offsets(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "alpha beta gamma",
            "gamma",
            11..16,
            false,
            Some(0..5),
            104.,
            155.,
            Some(2),
        );
    }

    #[test]
    fn annotation_segment_excludes_preceding_code_block() {
        let markdown = "Before\n\n```rust\nfn main() {}\n```\n\nParagraph with selected text";
        let selection_start = markdown.find("selected").unwrap();
        let annotation_start = Cowork::hard_line_start(markdown, selection_start);

        assert_eq!(
            &markdown[..annotation_start],
            "Before\n\n```rust\nfn main() {}\n```\n\n"
        );
        assert_eq!(
            &markdown[annotation_start..],
            "Paragraph with selected text"
        );
    }

    #[test]
    fn highlights_fenced_rust_code() {
        let code = "fn main() { println!(\"hello\"); }\n";
        let block = CodeBlock::from_code(code, Some("rust"));
        let highlights = highlight_code_block(&block);

        assert!(!highlights.is_empty());
        assert!(
            highlights
                .iter()
                .all(|(range, _)| range.start < range.end && range.end <= code.len())
        );
    }

    #[test]
    fn collaborator_threads_are_read_only_and_removed_on_disconnect() {
        assert!(!ThreadOwnership::Remote.can_write());
        assert!(ThreadOwnership::Remote.remove_on_disconnect());
        assert!(ThreadOwnership::Local.can_write());
        assert!(!ThreadOwnership::Local.remove_on_disconnect());
    }

    #[test]
    fn thread_titles_normalize_whitespace_and_truncate_by_character() {
        assert_eq!(
            Cowork::thread_title("  Collaborate\non\tthis prompt  "),
            "Collaborate on this prompt"
        );
        assert_eq!(
            Cowork::thread_title("12345678901234567890123456789012"),
            "12345678901234567890123456789012"
        );
        assert_eq!(
            Cowork::thread_title("12345678901234567890123456789012 more"),
            "12345678901234567890123456789012…"
        );
        assert_eq!(
            Cowork::thread_title("🦀".repeat(33).as_str()),
            format!("{}…", "🦀".repeat(32))
        );
    }

    #[test]
    fn first_message_titles_an_empty_pre_shared_thread() {
        assert_eq!(
            Cowork::title_for_first_message(&[], "  Collaborate on this prompt  "),
            Some("Collaborate on this prompt".into())
        );

        let existing_timeline = vec![TimelineMessage::User(UserMessageGroup {
            id: Uuid::new_v4(),
            comments: Vec::new(),
            content: UserMessageContent::Submitted {
                text: "Existing message".into(),
                history_text: None,
            },
            comments_folded: false,
        })];
        assert_eq!(
            Cowork::title_for_first_message(&existing_timeline, "Later message"),
            None
        );
    }

    struct EmptyThreadTestView {
        thread: Entity<Thread>,
        draft_id: Uuid,
    }

    impl Render for EmptyThreadTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn sharing_before_first_message_materializes_an_empty_owned_thread(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let draft = Cowork::new_user_message_draft(window, cx);
            let draft_id = draft.id;
            let thread = Cowork::new_empty_local_thread(draft, cx);
            EmptyThreadTestView { thread, draft_id }
        });

        view.read_with(cx, |view, cx| {
            let thread = view.thread.read(cx);
            assert!(thread.timeline.is_empty());
            assert_eq!(thread.draft.id, view.draft_id);
            assert_eq!(thread.summary.title, "New thread");
            assert_eq!(thread.ownership, ThreadOwnership::Local);
            assert!(matches!(thread.sharing, ThreadSharing::NotShared));
        });
    }

    struct ThreadMirrorTestView {
        host: Entity<Thread>,
        collaborator: Option<Entity<Thread>>,
    }

    impl Render for ThreadMirrorTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The events a host broadcasts while answering one prompt.
    fn agent_stream_events(message_id: Uuid) -> Vec<protocol::HostMessage> {
        let id = message_id.into_bytes();
        let user_message_id = Uuid::new_v4().into_bytes();
        let comment_id = Uuid::new_v4().into_bytes();
        vec![
            protocol::HostMessage::ThreadTitled("Explain this".into()),
            protocol::HostMessage::UserMessage(protocol::UserMessage {
                id: user_message_id,
                text: "Explain this".into(),
                comments: vec![protocol::UserComment {
                    id: comment_id,
                    reference: protocol::CommentReference {
                        message_id: Uuid::new_v4().into_bytes(),
                        range: 0..10,
                        quote: "an excerpt".into(),
                    },
                    body: "why?".into(),
                }],
            }),
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id: Some(user_message_id),
            },
            protocol::HostMessage::AgentCommentResponded {
                id,
                response_id: Uuid::new_v4().into_bytes(),
                comment_id,
                response: "Because of this.".into(),
            },
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Thinking,
                text: "Weighing ".into(),
            },
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Thinking,
                text: "options.".into(),
            },
            // No explicit thinking end, so the first response token closes it.
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Response,
                text: "Here is ".into(),
            },
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Response,
                text: "the answer.".into(),
            },
            protocol::HostMessage::AgentEnded { id, failure: None },
        ]
    }

    /// A collaborator that joins midway through a generation has to end up with
    /// the host's timeline: its snapshot covers what it missed, and the events
    /// it replays afterwards cover the rest.
    #[gpui::test]
    fn collaborators_joining_mid_stream_converge_on_the_host_timeline(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let message_id = Uuid::new_v4();
        let events = agent_stream_events(message_id);
        // The collaborator joins once the agent has started reasoning.
        let joined_after = 4;

        let (view, cx) = cx.add_window_view(|window, cx| ThreadMirrorTestView {
            host: Cowork::new_empty_local_thread(Cowork::new_user_message_draft(window, cx), cx),
            collaborator: None,
        });

        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                for event in events.iter().take(joined_after) {
                    view.host
                        .update(cx, |thread, cx| thread.apply(event.clone(), cx));
                }

                let snapshot = view.host.read(cx).to_protocol();
                let draft = view.host.read(cx).draft.clone();
                let collaborator = cx
                    .new(|cx| Thread::from_snapshot(snapshot, draft, ThreadSharing::NotShared, cx));

                for event in events.iter().skip(joined_after) {
                    view.host
                        .update(cx, |thread, cx| thread.apply(event.clone(), cx));
                    collaborator.update(cx, |thread, cx| thread.apply(event.clone(), cx));
                }
                view.collaborator = Some(collaborator);
            });
        });

        view.read_with(cx, |view, cx| {
            let host = view.host.read(cx);
            let collaborator = view
                .collaborator
                .as_ref()
                .expect("collaborator should have joined")
                .read(cx);

            assert_eq!(collaborator.to_protocol(), host.to_protocol());
            assert_eq!(collaborator.summary.id, host.summary.id);
            assert_ne!(collaborator.instance_id, host.instance_id);
            assert!(!host.generating);
            assert!(!collaborator.generating);

            let TimelineMessage::Agent(message) = &collaborator.timeline[1] else {
                panic!("expected the agent's reply");
            };
            assert_eq!(message.thinking, "Weighing options.");
            assert_eq!(message.text, "Here is the answer.");
            assert!(message.thinking_complete);
            assert!(message.complete);
            assert!(!message.failed);
        });
    }

    struct ComposerTestView {
        composer: Entity<TextareaState>,
    }

    impl Render for ComposerTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .items_start()
                .child(Cowork::render_composer_input(&self.composer))
        }
    }

    struct MouseDragTestView {
        editor: Entity<TextareaState>,
    }

    impl Render for MouseDragTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(Textarea::new(&self.editor))
        }
    }

    #[gpui::test]
    fn composer_grows_beyond_four_lines(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let composer = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, usize::MAX));
            ComposerTestView { composer }
        });
        let composer = view.read_with(cx, |view, _| view.composer.clone());

        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_value("1\n2\n3\n4\n5\n6\n7\n8\n9\n10", window, cx);
            });
        });
        cx.run_until_parked();

        let composer_bounds = cx
            .debug_bounds("composer")
            .expect("composer should be rendered");
        assert!(
            composer_bounds.size.height >= px(200.),
            "ten text lines should expand the composer, got {composer_bounds:?}"
        );
    }

    #[gpui::test]
    fn synthetic_mouse_up_ends_a_stale_text_drag(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = TextareaState::new(window, cx);
                editor.set_value("selectable text", window, cx);
                editor
            });
            editor.focus_handle(cx).focus(window, cx);
            MouseDragTestView { editor }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        cx.simulate_mouse_down(
            gpui::point(px(8.), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(Cowork::end_stale_mouse_drag);
        cx.simulate_mouse_move(
            gpui::point(px(120.), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );

        assert!(view.read_with(cx, |view, cx| {
            view.editor.read(cx).selected_range().is_empty()
        }));
    }
}
