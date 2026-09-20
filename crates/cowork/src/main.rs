use std::{borrow::Cow, collections::HashMap, ops::Range, sync::OnceLock, time::Duration};

use futures::StreamExt;
use gpui::{
    Animation, AnimationExt, App, AppContext, AssetSource, Bounds, ClipboardItem, Context, Entity,
    Focusable, FontStyle, FontWeight, HighlightStyle, IntoElement, KeyBinding, KeyDownEvent,
    LineFragment, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, QuitMode, Render,
    ScrollHandle, ScrollWheelEvent, SharedString, SpringAnimation, SpringConfig, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowControlArea, WindowOptions, actions, div, img,
    point, prelude::*, px, rems, rgb, rgba, size,
};
use gpui_base::{
    SelectableText, TextSelection, TextSelectionLayer, TextView, TextViewDefaults, TextViewState,
    TextViewStyle, Textarea,
    input::{InputEditorStyle, InputEvent, TextareaState},
    text::{CodeBlock, SelectionFormat},
};
use iroh::{Endpoint, endpoint::presets};
use rig::{
    agent::MultiTurnStreamItem,
    completion::Message as RigMessage,
    prelude::*,
    providers::ollama::wire::Ollama,
    streaming::{BlockClose, Delta, StreamEvent},
};
use serde_json::json;
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle as SyntectFontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use tokio::{runtime::Runtime, sync::mpsc};
use uuid::Uuid;

const SIDEBAR_WIDTH: gpui::Pixels = px(275.);
const TOP_BAR_HEIGHT: gpui::Pixels = px(40.);
const MACOS_TRAFFIC_LIGHT_X_INSET: gpui::Pixels = px(12.);
const MACOS_TRAFFIC_LIGHT_SIZE: gpui::Pixels = px(14.);
const MACOS_TRAFFIC_LIGHT_SPACING: gpui::Pixels = px(6.);
const MACOS_TRAFFIC_LIGHT_TRAILING_GAP: gpui::Pixels = px(12.);
const OLLAMA_MODEL: &str = "lfm2.5";
const OLLAMA_CONTEXT_TOKENS: u64 = 8_192;
const OLLAMA_AVATAR_PATH: &str = "providers/ollama.png";
const USER_ACCENT: u32 = 0xe26d5a;

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

struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match path {
            OLLAMA_AVATAR_PATH => Ok(Some(Cow::Borrowed(include_bytes!(
                "../../../assets/providers/ollama.png"
            )))),
            _ => Ok(None),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(OLLAMA_AVATAR_PATH
            .starts_with(path)
            .then(|| OLLAMA_AVATAR_PATH.into())
            .into_iter()
            .collect())
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
    source_message_id: Uuid,
    quote: String,
    source_range: Range<usize>,
    body: UserCommentBody,
}

#[derive(Clone)]
enum UserCommentBody {
    Editing(Entity<TextareaState>),
    Submitted(String),
}

struct MarkdownTextLeaf {
    rendered: String,
    source_range: Range<usize>,
    annotation_range: Range<usize>,
}

struct SegmentTextView {
    state: Entity<TextViewState>,
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
    Failed,
}

enum ThreadSharing {
    NotShared,
    Sharing,
    Shared(Endpoint),
    Failed,
}

impl ThreadSharing {
    fn status(&self) -> SharingStatus {
        match self {
            Self::NotShared => SharingStatus::NotShared,
            Self::Sharing => SharingStatus::Sharing,
            Self::Shared(_) => SharingStatus::Shared,
            Self::Failed => SharingStatus::Failed,
        }
    }

    fn is_collaborating(&self) -> bool {
        matches!(self, Self::Sharing | Self::Shared(_))
    }
}

struct Thread {
    summary: ThreadSummary,
    timeline: Vec<TimelineMessage>,
    draft: UserMessageGroup,
    generating: bool,
    sharing: ThreadSharing,
}

struct ThreadStore {
    threads: Vec<Entity<Thread>>,
}

impl ThreadStore {
    fn thread(&self, thread_id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        self.threads
            .iter()
            .find(|thread| thread.read(cx).summary.id == thread_id)
            .cloned()
    }
}

enum AgentEvent {
    Thinking(String),
    ThinkingFinished,
    Text(String),
    Finished,
    Failed(String),
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
    segment_text_views: HashMap<(Uuid, Uuid, usize, usize), SegmentTextView>,
    render_generation: u64,
    titlebar_click_armed: bool,
    tokio_handle: tokio::runtime::Handle,
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
            .id("toggle-sidebar")
            .size(px(28.))
            .when(cfg!(target_os = "macos"), |this| {
                this.ml(macos_sidebar_toggle_margin())
            })
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
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .child("▥")
    }

    fn start_sharing(&mut self, thread: Entity<Thread>, cx: &mut Context<Self>) {
        thread.update(cx, |thread, _| {
            thread.sharing = ThreadSharing::Sharing;
        });
        cx.notify();

        let (sender, mut receiver) = mpsc::channel(1);
        self.tokio_handle.spawn(async move {
            let result = Endpoint::builder(presets::N0)
                .bind()
                .await
                .map_err(|error| error.to_string());
            if sender.send(result).await.is_err() {
                return;
            }
        });

        cx.spawn(async move |this, cx| {
            let Some(result) = receiver.recv().await else {
                return;
            };

            match result {
                Ok(endpoint) => {
                    let endpoint_id = endpoint.id().to_string();
                    thread.update(cx, |thread, _| {
                        thread.sharing = ThreadSharing::Shared(endpoint);
                    });
                    if this
                        .update(cx, |_, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(endpoint_id));
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    eprintln!("failed to share thread: {error}");
                    thread.update(cx, |thread, _| {
                        thread.sharing = ThreadSharing::Failed;
                    });
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            }
        })
        .detach();
    }

    fn toggle_sharing(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };

        match thread.read(cx).sharing.status() {
            SharingStatus::NotShared | SharingStatus::Failed => self.start_sharing(thread, cx),
            SharingStatus::Sharing => {}
            SharingStatus::Shared => {
                let endpoint = thread.update(cx, |thread, _| {
                    let ThreadSharing::Shared(endpoint) =
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
            SharingStatus::Failed => "Retry share",
        };
        let sharing_enabled = active_thread.is_some() && sharing_status != SharingStatus::Sharing;

        div()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(0x1c1c1f))
            .window_control_area(WindowControlArea::Drag)
            .when(cfg!(target_os = "macos"), |this| {
                this.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        let is_titlebar_double_click =
                            event.click_count == 2 && this.titlebar_click_armed;
                        this.titlebar_click_armed = event.click_count == 1;
                        cx.stop_propagation();

                        if is_titlebar_double_click {
                            window.titlebar_double_click();
                        } else {
                            window.start_window_move();
                        }
                    }),
                )
            })
            .child(self.render_sidebar_toggle(cx))
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
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
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_sharing(cx);
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
        let composer = Self::draft_composer(&thread.read(cx).draft);
        self.active_thread_id = Some(thread_id);
        self.selection_message_id = None;
        self.follow_generation = true;
        self.timeline_scroll_handle.scroll_to_bottom();
        composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn render_sidebar_thread(
        &self,
        thread: &ThreadSummary,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let thread_id = thread.id;
        div()
            .id(thread_id.to_string())
            .h(px(30.))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .when(self.active_thread_id == Some(thread.id), |this| {
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
                (thread.summary.clone(), thread.sharing.is_collaborating())
            })
            .partition(|(_, collaborating)| *collaborating);
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
                        div().flex_none().px_2().children(
                            collaborating_threads
                                .iter()
                                .map(|(thread, _)| self.render_sidebar_thread(thread, cx)),
                        ),
                    )
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
                        this.child(
                            div().flex_1().min_h_0().overflow_hidden().px_2().children(
                                recent_threads
                                    .iter()
                                    .map(|(thread, _)| self.render_sidebar_thread(thread, cx)),
                            ),
                        )
                    }),
            )
    }

    fn markdown_text_leaves(markdown: &str) -> Vec<MarkdownTextLeaf> {
        fn collect(node: &markdown::mdast::Node, source: &str, leaves: &mut Vec<MarkdownTextLeaf>) {
            let value_and_atomic = match node {
                markdown::mdast::Node::Text(text) => Some((&text.value, false)),
                markdown::mdast::Node::InlineCode(code) => Some((&code.value, true)),
                _ => None,
            };
            if let Some((value, atomic)) = value_and_atomic
                && !value.is_empty()
                && let Some(position) = node.position()
                && let Some(node_source) = source.get(position.start.offset..position.end.offset)
                && let Some(relative_start) = node_source.find(value)
            {
                let start = position.start.offset + relative_start;
                let source_range = start..start + value.len();
                leaves.push(MarkdownTextLeaf {
                    rendered: value.clone(),
                    source_range,
                    annotation_range: if atomic {
                        position.start.offset..position.end.offset
                    } else {
                        start..start + value.len()
                    },
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
        thread_id: Uuid,
        message_id: Uuid,
        message: &AgentMessage,
        cx: &App,
    ) -> Option<Range<usize>> {
        let mut selected_ranges = self
            .segment_text_views
            .iter()
            .filter(|((segment_thread_id, segment_message_id, _, _), _)| {
                *segment_thread_id == thread_id && *segment_message_id == message_id
            })
            .filter_map(|((_, _, segment_start, _), text_view)| {
                text_view
                    .state
                    .read(cx)
                    .selected_source_range()
                    .map(|range| (range.start + segment_start)..(range.end + segment_start))
            });
        let first = selected_ranges.next();
        let segmented = selected_ranges.fold(first, |combined, range| {
            Some(match combined {
                Some(combined) => combined.start.min(range.start)..combined.end.max(range.end),
                None => range,
            })
        });

        segmented.or_else(|| message.text_view.read(cx).selected_source_range())
    }

    fn unique_source_range_for_quote(markdown: &str, quote: &str) -> Option<Range<usize>> {
        let leaves = Self::markdown_text_leaves(markdown);
        let rendered = leaves
            .iter()
            .map(|leaf| leaf.rendered.as_str())
            .collect::<String>();
        let mut matches = rendered.match_indices(quote);
        let (rendered_start, _) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let rendered_end = rendered_start + quote.len();
        let mut rendered_cursor = 0;
        let mut source_start = None;
        let mut source_end = None;

        for leaf in leaves {
            let leaf_start = rendered_cursor;
            let leaf_end = leaf_start + leaf.rendered.len();
            if source_start.is_none() && rendered_start >= leaf_start && rendered_start < leaf_end {
                source_start = Some(leaf.source_range.start + rendered_start - leaf_start);
            }
            if rendered_end > leaf_start && rendered_end <= leaf_end {
                source_end = Some(leaf.source_range.start + rendered_end - leaf_start);
                break;
            }
            rendered_cursor = leaf_end;
        }

        Some(source_start?..source_end?)
    }

    fn annotation_ranges(markdown: &str, selection: Range<usize>) -> Vec<Range<usize>> {
        let mut ranges = Vec::<Range<usize>>::new();
        for leaf in Self::markdown_text_leaves(markdown) {
            let start = leaf.source_range.start.max(selection.start);
            let end = leaf.source_range.end.min(selection.end);
            if start >= end {
                continue;
            }
            let range = if leaf.annotation_range != leaf.source_range {
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

    fn annotate_markdown(markdown: &str, ranges: impl IntoIterator<Item = Range<usize>>) -> String {
        let mut ranges = ranges.into_iter().collect::<Vec<_>>();
        ranges.sort_by_key(|range| range.start);
        let mut annotated = markdown.to_string();
        for range in ranges.into_iter().rev() {
            if range.start < range.end
                && range.end <= annotated.len()
                && annotated.is_char_boundary(range.start)
                && annotated.is_char_boundary(range.end)
            {
                annotated.insert_str(range.end, "</a>");
                annotated.insert_str(range.start, "<a href=\"#inline-comment\">");
            }
        }
        annotated
    }

    fn begin_inline_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let (Some(thread_id), Some(message_id)) =
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
        let source_range = {
            let thread = thread.read(cx);
            let Some(message) = thread.timeline.iter().find_map(|entry| match entry {
                TimelineMessage::Agent(message) if message.id == message_id => Some(message),
                _ => None,
            }) else {
                return;
            };
            let Some(source_range) = self
                .selected_message_source_range(thread_id, message_id, message, cx)
                .or_else(|| Self::unique_source_range_for_quote(&message.text, &quote))
            else {
                return;
            };
            source_range
        };

        let body = cx.new(|cx| {
            let mut body = TextareaState::new(window, cx).auto_grow(1, usize::MAX);
            body.set_editor_style(InputEditorStyle {
                caret: rgb(0xffffff).into(),
                ..Default::default()
            });
            body
        });
        body.update(cx, |body, cx| {
            body.insert(initial_text, window, cx);
        });
        let comment_id = Uuid::new_v4();
        thread.update(cx, |thread, _| {
            thread.draft.comments.push(UserComment {
                id: comment_id,
                source_message_id: message_id,
                quote,
                source_range,
                body: UserCommentBody::Editing(body.clone()),
            });
            thread.draft.comments_folded = false;
        });
        let observed_body = body.clone();
        let observed_thread = thread.clone();
        cx.subscribe(&body, move |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && observed_body.read(cx).value().is_empty() {
                observed_thread.update(cx, |thread, _| {
                    thread
                        .draft
                        .comments
                        .retain(|comment| comment.id != comment_id);
                });
                cx.notify();
            }
        })
        .detach();
        TextSelection::clear(window, cx);
        body.focus_handle(cx).focus(window, cx);
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
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

    fn render_comment_card(comment: &UserComment, location: &'static str) -> gpui::AnyElement {
        let accent = rgb(USER_ACCENT);
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(rgb(0xe4e4e7))
                .child(body.clone())
                .into_any_element(),
            UserCommentBody::Editing(body) => div()
                .id(format!("comment-editor-{location}-{}", comment.id))
                .flex_1()
                .min_w_0()
                .child(Textarea::new(body))
                .into_any_element(),
        };

        div()
            .id(format!("inline-comment-{location}-{}", comment.id))
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
                    .child(comment.quote.clone()),
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
                                .border_color(accent)
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

    fn annotated_markdown_html(markdown: &str) -> String {
        let mut options = markdown::Options::gfm();
        options.compile.allow_dangerous_html = true;
        markdown::to_html_with_options(markdown, &options).unwrap_or_else(|_| markdown.to_string())
    }

    fn render_message_segment(
        &mut self,
        thread_id: Uuid,
        message_id: Uuid,
        segment_index: usize,
        source_range: Range<usize>,
        text: &str,
        annotated: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if annotated {
            return TextView::html(
                format!("timeline-annotated-{message_id}-{segment_index}"),
                Self::annotated_markdown_html(text),
            )
            .style(Self::annotated_markdown_style())
            .w_full()
            .into_any_element();
        }

        let text_view = self
            .segment_text_views
            .entry((thread_id, message_id, source_range.start, source_range.end))
            .or_insert_with(|| SegmentTextView {
                state: cx.new(|cx| TextViewState::markdown(text, cx)),
                rendered_at: self.render_generation,
            });
        text_view.rendered_at = self.render_generation;
        TextView::new(&text_view.state)
            .selection_format(SelectionFormat::Plain)
            .style(Self::markdown_style())
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
                content.extend(
                    group
                        .comments
                        .iter()
                        .map(|comment| Self::render_comment_card(comment, "submission")),
                );
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

    fn render_agent_message(
        &mut self,
        thread_id: Uuid,
        index: usize,
        message: &AgentMessage,
        comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = !message.complete && message.thinking.is_empty() && message.text.is_empty();
        let mut message_content = Vec::new();
        let mut cursor = 0;
        let mut anchored_comments = comments
            .iter()
            .filter(|comment| comment.source_message_id == message.id)
            .filter(|comment| {
                comment.source_range.start < comment.source_range.end
                    && comment.source_range.end <= message.text.len()
                    && message.text.is_char_boundary(comment.source_range.start)
                    && message.text.is_char_boundary(comment.source_range.end)
            })
            .collect::<Vec<_>>();
        anchored_comments.sort_by_key(|comment| comment.source_range.start);

        if anchored_comments.is_empty() && !message.text.is_empty() {
            message_content.push(
                TextView::new(&message.text_view)
                    .selection_format(SelectionFormat::Plain)
                    .style(Self::markdown_style())
                    .w_full()
                    .into_any_element(),
            );
            cursor = message.text.len();
        }

        let mut comment_index = 0;
        while comment_index < anchored_comments.len() {
            let first = anchored_comments[comment_index];
            if first.source_range.start < cursor {
                comment_index += 1;
                continue;
            }
            let annotated_start =
                Self::hard_line_start(&message.text, first.source_range.start).max(cursor);
            if cursor < annotated_start {
                message_content.push(self.render_message_segment(
                    thread_id,
                    message.id,
                    message_content.len(),
                    cursor..annotated_start,
                    &message.text[cursor..annotated_start],
                    false,
                    cx,
                ));
                cursor = annotated_start;
            }
            let line_end =
                Self::wrapped_line_end(&message.text, first.source_range.end, wrap_width, window);
            let group_start = comment_index;
            while comment_index < anchored_comments.len()
                && anchored_comments[comment_index].source_range.start < line_end
            {
                comment_index += 1;
            }
            let group = &anchored_comments[group_start..comment_index];
            let annotation_ranges = group
                .iter()
                .flat_map(|comment| {
                    Self::annotation_ranges(&message.text, comment.source_range.clone())
                })
                .filter_map(|range| {
                    (range.start >= cursor && range.end <= line_end)
                        .then_some((range.start - cursor)..(range.end - cursor))
                });
            let annotated =
                Self::annotate_markdown(&message.text[cursor..line_end], annotation_ranges);
            message_content.push(self.render_message_segment(
                thread_id,
                message.id,
                message_content.len(),
                cursor..line_end,
                &annotated,
                true,
                cx,
            ));
            message_content.extend(
                group
                    .iter()
                    .map(|comment| Self::render_comment_card(comment, "inline")),
            );
            cursor = line_end;
        }
        if cursor < message.text.len() {
            message_content.push(self.render_message_segment(
                thread_id,
                message.id,
                message_content.len(),
                cursor..message.text.len(),
                &message.text[cursor..],
                false,
                cx,
            ));
        }

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
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match message {
            TimelineMessage::User(group) => self.render_user_message_group(index, group, cx),
            TimelineMessage::Agent(message) => self
                .render_agent_message(thread_id, index, message, comments, wrap_width, window, cx),
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
                    Some(RigMessage::assistant(&message.text))
                }
                TimelineMessage::Agent(_) => None,
            })
            .collect()
    }

    fn start_generation(
        &mut self,
        thread_id: Uuid,
        prompt: String,
        history: Vec<RigMessage>,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let thinking_view = cx.new(|cx| TextViewState::markdown("", cx));
        let text_view = cx.new(|cx| TextViewState::markdown("", cx));
        let message_id = Uuid::new_v4();
        thread.update(cx, |thread, _| {
            thread.timeline.push(TimelineMessage::Agent(AgentMessage {
                id: message_id,
                thinking: String::new(),
                thinking_view,
                thinking_complete: false,
                thinking_expanded: true,
                text: String::new(),
                text_view,
                complete: false,
                failed: false,
            }));
            thread.generating = true;
        });

        let thread = thread.clone();
        let (sender, mut receiver) = mpsc::channel(128);
        self.tokio_handle.spawn(async move {
            let client = match Ollama::new().bound() {
                Ok(client) => client,
                Err(error) => {
                    if sender
                        .send(AgentEvent::Failed(error.to_string()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    return;
                }
            };
            let agent = client
                .agent(OLLAMA_MODEL)
                .additional_params(json!({
                    "num_ctx": OLLAMA_CONTEXT_TOKENS,
                    "think": "medium"
                }))
                .build();
            let mut stream = agent.prompt(prompt).history(&history).stream();

            while let Some(item) = stream.next().await {
                match item {
                    Ok(MultiTurnStreamItem::StreamAssistantItem(StreamEvent::BlockDelta {
                        delta: Delta::Reasoning { text },
                        ..
                    })) => {
                        if sender.send(AgentEvent::Thinking(text)).await.is_err() {
                            return;
                        }
                    }
                    Ok(MultiTurnStreamItem::StreamAssistantItem(StreamEvent::BlockEnd {
                        end: BlockClose::Reasoning { .. },
                        ..
                    })) => {
                        if sender.send(AgentEvent::ThinkingFinished).await.is_err() {
                            return;
                        }
                    }
                    Ok(MultiTurnStreamItem::StreamAssistantItem(StreamEvent::BlockDelta {
                        delta: Delta::Text { text },
                        ..
                    })) => {
                        if sender.send(AgentEvent::Text(text)).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        if sender
                            .send(AgentEvent::Failed(error.to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        return;
                    }
                    _ => {}
                }
            }

            if sender.send(AgentEvent::Finished).await.is_err() {
                return;
            }
        });

        cx.spawn(async move |this, cx| {
            while let Some(event) = receiver.recv().await {
                let (finished, text_view_update) = thread.update(cx, |thread, _| {
                    let Some(message) = thread.timeline.iter_mut().find_map(|entry| match entry {
                        TimelineMessage::Agent(message) if message.id == message_id => {
                            Some(message)
                        }
                        _ => None,
                    }) else {
                        return (true, None);
                    };
                    match event {
                        AgentEvent::Thinking(text) => {
                            message.thinking.push_str(&text);
                            (false, Some((message.thinking_view.clone(), text, true)))
                        }
                        AgentEvent::ThinkingFinished => {
                            message.thinking_complete = true;
                            message.thinking_expanded = false;
                            (false, None)
                        }
                        AgentEvent::Text(text) => {
                            if !message.thinking.is_empty() && !message.thinking_complete {
                                message.thinking_complete = true;
                                message.thinking_expanded = false;
                            }
                            message.text.push_str(&text);
                            (false, Some((message.text_view.clone(), text, true)))
                        }
                        AgentEvent::Finished => {
                            message.complete = true;
                            message.thinking_complete = true;
                            message.thinking_expanded = false;
                            thread.generating = false;
                            (true, None)
                        }
                        AgentEvent::Failed(error) => {
                            message.complete = true;
                            message.thinking_complete = true;
                            message.thinking_expanded = false;
                            message.failed = true;
                            let update = if message.text.is_empty() {
                                message.text = format!("Unable to generate a response: {error}");
                                Some((message.text_view.clone(), message.text.clone(), false))
                            } else {
                                None
                            };
                            thread.generating = false;
                            (true, update)
                        }
                    }
                });
                if let Some((text_view, text, append)) = text_view_update {
                    text_view.update(cx, |text_view, cx| {
                        if append {
                            text_view.push_str(&text, cx);
                        } else {
                            text_view.set_text(&text, cx);
                        }
                    });
                }

                let result = this.update(cx, |this, cx| {
                    if this.active_thread_id == Some(thread_id) {
                        if this.follow_generation {
                            this.timeline_scroll_handle.scroll_to_bottom();
                        }
                        cx.notify();
                    }
                });
                if result.is_err() || finished {
                    break;
                }
            }
        })
        .detach();
    }

    fn thread_title(prompt: &str) -> String {
        let normalized_prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut characters = normalized_prompt.chars();
        let mut title = characters.by_ref().take(32).collect::<String>();
        if characters.next().is_some() {
            title.push('…');
        }
        title
    }

    fn editable_comment_body(comment: &UserComment, cx: &App) -> Option<String> {
        let UserCommentBody::Editing(body) = &comment.body else {
            return None;
        };
        let value = body.read(cx).value().to_string();
        (!value.trim().is_empty()).then_some(value)
    }

    fn prompt_with_comments(
        prompt: &str,
        draft: &UserMessageGroup,
        timeline: &[TimelineMessage],
        cx: &App,
    ) -> String {
        let pending = draft
            .comments
            .iter()
            .filter_map(|comment| {
                Self::editable_comment_body(comment, cx).map(|body| (comment, body))
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return prompt.to_string();
        }

        let mut result = String::from(
            "The user attached the following inline comments to immutable excerpts from the conversation:\n",
        );
        for (index, (comment, body)) in pending.iter().enumerate() {
            let message_number = timeline
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        TimelineMessage::Agent(message)
                            if message.id == comment.source_message_id
                    )
                })
                .map(|index| index + 1)
                .unwrap_or_default();
            result.push_str(&format!(
                "\n{}. Excerpt from assistant message {}:\n> {}\nComment: {}\n",
                index + 1,
                message_number,
                comment.quote.replace('\n', "\n> "),
                body.trim(),
            ));
        }
        if !prompt.trim().is_empty() {
            result.push_str("\nAdditional user message:\n");
            result.push_str(prompt);
        }
        result
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_thread = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        if active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).generating)
        {
            return;
        }

        let draft = active_thread
            .as_ref()
            .map(|thread| thread.read(cx).draft.clone())
            .unwrap_or_else(|| self.new_thread_draft.clone());
        let composer = Self::draft_composer(&draft);
        let prompt = composer.read(cx).value().to_string();
        let has_comments = draft
            .comments
            .iter()
            .any(|comment| Self::editable_comment_body(comment, cx).is_some());
        if prompt.trim().is_empty() && !has_comments {
            return;
        }

        let timeline = active_thread
            .as_ref()
            .map(|thread| thread.read(cx).timeline.clone())
            .unwrap_or_default();
        let history = Self::rig_history(&timeline);
        let agent_prompt = Self::prompt_with_comments(&prompt, &draft, &timeline, cx);
        let submitted_comments = draft
            .comments
            .iter()
            .filter_map(|comment| {
                Self::editable_comment_body(comment, cx).map(|body| UserComment {
                    id: comment.id,
                    source_message_id: comment.source_message_id,
                    quote: comment.quote.clone(),
                    source_range: comment.source_range.clone(),
                    body: UserCommentBody::Submitted(body),
                })
            })
            .collect::<Vec<_>>();
        let remaining_comments = draft
            .comments
            .iter()
            .filter(|comment| Self::editable_comment_body(comment, cx).is_none())
            .cloned()
            .collect::<Vec<_>>();
        let submitted_group = UserMessageGroup {
            id: draft.id,
            comments: submitted_comments,
            content: UserMessageContent::Submitted {
                text: prompt.clone(),
                history_text: has_comments.then(|| agent_prompt.clone()),
            },
            comments_folded: draft.comments_folded,
        };
        let mut next_draft = Self::new_user_message_draft(window, cx);
        next_draft.comments = remaining_comments;
        next_draft.comments_folded = !next_draft.comments.is_empty() && draft.comments_folded;
        let next_composer = Self::draft_composer(&next_draft);

        let thread_id = if let Some(thread) = active_thread {
            let thread_id = thread.read(cx).summary.id;
            thread.update(cx, |thread, _| {
                thread.timeline.push(TimelineMessage::User(submitted_group));
                thread.draft = next_draft;
            });
            thread_id
        } else {
            let thread_id = Uuid::new_v4();
            let thread = cx.new(|_| Thread {
                summary: ThreadSummary {
                    id: thread_id,
                    title: Self::thread_title(&prompt),
                },
                timeline: vec![TimelineMessage::User(submitted_group)],
                draft: next_draft,
                generating: false,
                sharing: ThreadSharing::NotShared,
            });
            self.new_thread_draft = Self::new_user_message_draft(window, cx);
            self.thread_store.update(cx, |store, _| {
                store.threads.insert(0, thread.clone());
            });
            self.active_thread_id = Some(thread_id);
            thread_id
        };

        self.selection_message_id = None;
        self.follow_generation = true;
        next_composer.focus_handle(cx).focus(window, cx);
        self.start_generation(thread_id, agent_prompt, history, cx);
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_generation = self.render_generation.wrapping_add(1);
        let active_thread_id = self.active_thread_id;
        let (messages, draft) = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .map(|thread| {
                let thread = thread.read(cx);
                (thread.timeline.clone(), thread.draft.clone())
            })
            .unwrap_or_else(|| (Vec::new(), self.new_thread_draft.clone()));
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
                timeline_messages.push(self.render_timeline_message(
                    thread_id, index, message, &comments, wrap_width, window, cx,
                ));
            }
        }

        self.segment_text_views
            .retain(|_, text_view| text_view.rendered_at == self.render_generation);

        // Draft comments are already rendered beside their quoted agent text. A TextareaState
        // keeps one set of layout bounds for mouse hit-testing, so rendering the same comment
        // editor here as well would make clicks in the inline copy resolve against this copy.
        let composer = Self::draft_composer(&draft);

        div()
            .id("main-editor")
            .h_full()
            .flex_1()
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
                            .child(
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
                                            .child(
                                                Self::render_composer_input(&composer).on_click({
                                                    let composer = composer.clone();
                                                    cx.listener(move |_, _, window, cx| {
                                                        composer.focus_handle(cx).focus(window, cx);
                                                    })
                                                }),
                                            ),
                                    )
                                    .child(div().w(px(40.)).flex_none()),
                            ),
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

        div()
            .size_full()
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
                    .child(self.render_main_editor(window, cx)),
            )
    }
}

fn main() {
    let worker_threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(2, 8);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("cowork-agent")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start the agent runtime: {error}");
            return;
        }
    };
    let tokio_handle = runtime.handle().clone();
    if TOKIO_RUNTIME.set(runtime).is_err() {
        eprintln!("the agent runtime was already initialized");
        return;
    }

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_base::init(cx);
            TextViewDefaults::new()
                .with_code_block_highlighter(highlight_code_block)
                .install(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-enter", SubmitComposer, Some("Input")),
                KeyBinding::new("cmd-enter", SubmitComposer, Some("Input")),
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
                let thread_store = cx.new(|_| ThreadStore {
                    threads: Vec::new(),
                });
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
                        tokio_handle,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotated_markdown_preserves_inline_underline() {
        let html = Cowork::annotated_markdown_html(
            "Before <a href=\"#inline-comment\">selected text</a> after",
        );

        assert!(html.contains("<a href=\"#inline-comment\">selected text</a>"));
        assert!(html.contains("Before "));
        assert!(html.contains(" after"));
    }

    #[test]
    fn annotated_markdown_preserves_heading_around_partial_selection() {
        let html = Cowork::annotated_markdown_html("### A <a href=\"#inline-comment\">Heading</a>");

        assert!(html.contains("<h3>A <a href=\"#inline-comment\">Heading</a></h3>"));
    }

    #[test]
    fn annotated_markdown_preserves_bold_around_partial_selection() {
        let html = Cowork::annotated_markdown_html("**H<a href=\"#inline-comment\">i</a>**");

        assert!(html.contains("<strong>H<a href=\"#inline-comment\">i</a></strong>"));
    }

    #[test]
    fn comments_follow_plain_text_selections_across_markdown_boundaries() {
        let cases = [
            (
                "inside bold",
                "Before **bold text** after",
                "old",
                "Before **b<a href=\"#inline-comment\">old</a> text** after",
            ),
            (
                "across opening bold edge",
                "Before **bold text** after",
                "re bold",
                "Befo<a href=\"#inline-comment\">re </a>**<a href=\"#inline-comment\">bold</a> text** after",
            ),
            (
                "across closing bold edge",
                "Before **bold text** after",
                "text af",
                "Before **bold <a href=\"#inline-comment\">text</a>**<a href=\"#inline-comment\"> af</a>ter",
            ),
            (
                "whole bold section",
                "Before **bold text** after",
                "bold text",
                "Before **<a href=\"#inline-comment\">bold text</a>** after",
            ),
            (
                "across several styled sections",
                "A **bold** and *italic* tail",
                "bold and italic ta",
                "A **<a href=\"#inline-comment\">bold</a>**<a href=\"#inline-comment\"> and </a>*<a href=\"#inline-comment\">italic</a>*<a href=\"#inline-comment\"> ta</a>il",
            ),
            (
                "nested styles",
                "Start **bold and *italic*** end",
                "and italic",
                "Start **bold <a href=\"#inline-comment\">and </a>*<a href=\"#inline-comment\">italic</a>*** end",
            ),
            (
                "whole inline code",
                "Use `value` now",
                "value",
                "Use <a href=\"#inline-comment\">`value`</a> now",
            ),
            (
                "partial inline code is atomic",
                "Use `value` now",
                "alu",
                "Use <a href=\"#inline-comment\">`value`</a> now",
            ),
            (
                "heading and emphasis",
                "### A **styled heading** here",
                "A styled heading h",
                "### <a href=\"#inline-comment\">A </a>**<a href=\"#inline-comment\">styled heading</a>**<a href=\"#inline-comment\"> h</a>ere",
            ),
        ];

        for (name, markdown, quote, expected) in cases {
            let source_range = Cowork::unique_source_range_for_quote(markdown, quote)
                .unwrap_or_else(|| panic!("{name}: selection should map to source"));
            let ranges = Cowork::annotation_ranges(markdown, source_range);
            let annotated = Cowork::annotate_markdown(markdown, ranges);

            assert_eq!(annotated, expected, "{name}");
            let html = Cowork::annotated_markdown_html(&annotated);
            assert!(
                html.contains("<a href=\"#inline-comment\">"),
                "{name}: annotation should survive Markdown rendering: {html}"
            );
        }
    }

    #[test]
    fn can_comment_on_selection_across_inline_code() {
        let markdown = "In Rust, we use `u128` to handle larger numbers";
        let quote = "In Rust, we use u128 to handle larger numbers";
        let source_range = Cowork::unique_source_range_for_quote(markdown, quote)
            .expect("selection should map to the Markdown source");
        let ranges = Cowork::annotation_ranges(markdown, source_range);
        let annotated = Cowork::annotate_markdown(markdown, ranges);
        let html = Cowork::annotated_markdown_html(&annotated);

        assert_eq!(
            annotated,
            "<a href=\"#inline-comment\">In Rust, we use `u128` to handle larger numbers</a>"
        );
        assert!(html.contains(
            "<a href=\"#inline-comment\">In Rust, we use <code>u128</code> to handle larger numbers</a>"
        ));
    }

    #[test]
    fn quote_fallback_does_not_guess_between_repeated_text() {
        let markdown = "Repeat **this phrase** once, then repeat **this phrase** again.";

        assert_eq!(
            Cowork::unique_source_range_for_quote(markdown, "this phrase"),
            None
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
        cx.update(gpui_base::init);
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
        cx.update(gpui_base::init);
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
