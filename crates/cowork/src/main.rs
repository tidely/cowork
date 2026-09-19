use std::{borrow::Cow, ops::Range, sync::OnceLock, time::Duration};

use futures::StreamExt;
use gpui::{
    Animation, AnimationExt, App, AppContext, AssetSource, Bounds, ClipboardItem, Context, Entity,
    Focusable, FontStyle, FontWeight, HighlightStyle, IntoElement, KeyBinding, KeyDownEvent,
    LineFragment, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, QuitMode, Render,
    ScrollHandle, ScrollWheelEvent, SharedString, SpringAnimation, SpringConfig, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowControlArea, WindowOptions, actions, div, img,
    point, prelude::*, px, rgb, rgba, size,
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
    streaming::{Delta, StreamEvent},
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
const OLLAMA_MODEL: &str = "gemma4:12b-it-qat";
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
struct TimelineMessage {
    author: MessageAuthor,
    text: String,
    history_text: Option<String>,
    text_view: Option<Entity<TextViewState>>,
    visible: bool,
    complete: bool,
    failed: bool,
}

#[derive(Clone)]
struct InlineComment {
    id: Uuid,
    message_index: usize,
    submitted_message_index: Option<usize>,
    quote: String,
    source_range: Range<usize>,
    body: Entity<TextareaState>,
    submitted: bool,
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
    messages: Vec<TimelineMessage>,
    comments: Vec<InlineComment>,
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
    Text(String),
    Finished,
    Failed(String),
}

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    composer: Entity<TextareaState>,
    timeline_scroll_handle: ScrollHandle,
    follow_generation: bool,
    thread_store: Entity<ThreadStore>,
    active_thread_id: Option<Uuid>,
    selection_message_index: Option<usize>,
    titlebar_click_armed: bool,
    tokio_handle: tokio::runtime::Handle,
    _window_activation_subscription: Subscription,
}

impl Cowork {
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

        self.composer.update(cx, |composer, cx| {
            composer.set_value("", window, cx);
        });
        self.active_thread_id = Some(thread_id);
        self.selection_message_index = None;
        self.follow_generation = true;
        self.timeline_scroll_handle.scroll_to_bottom();
        self.composer.focus_handle(cx).focus(window, cx);
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
                                this.composer.update(cx, |composer, cx| {
                                    composer.set_value("", window, cx);
                                });
                                this.active_thread_id = None;
                                this.selection_message_index = None;
                                this.composer.focus_handle(cx).focus(window, cx);
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
        let (Some(thread_id), Some(message_index)) =
            (self.active_thread_id, self.selection_message_index)
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
            let Some(message) = thread.messages.get(message_index) else {
                return;
            };
            if !matches!(message.author, MessageAuthor::Agent) {
                return;
            }
            let Some(start) = message.text.find(&quote) else {
                return;
            };
            start..start + quote.len()
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
            thread.comments.push(InlineComment {
                id: comment_id,
                message_index,
                submitted_message_index: None,
                quote,
                source_range,
                body: body.clone(),
                submitted: false,
            });
        });
        let observed_body = body.clone();
        let observed_thread = thread.clone();
        cx.subscribe(&body, move |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && observed_body.read(cx).value().is_empty() {
                observed_thread.update(cx, |thread, _| {
                    thread
                        .comments
                        .retain(|comment| comment.id != comment_id || comment.submitted);
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

    fn render_comment_card(
        comment: &InlineComment,
        location: &'static str,
        show_quote: bool,
        cx: &App,
    ) -> gpui::AnyElement {
        let accent = rgb(USER_ACCENT);
        let body = if comment.submitted {
            div()
                .w_full()
                .text_color(rgb(0xe4e4e7))
                .child(comment.body.read(cx).value())
                .into_any_element()
        } else {
            div()
                .id(format!("comment-editor-{location}-{}", comment.id))
                .flex_1()
                .min_w_0()
                .child(Textarea::new(&comment.body))
                .into_any_element()
        };

        div()
            .id(format!("inline-comment-{location}-{}", comment.id))
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .when(show_quote, |this| {
                this.child(
                    div()
                        .w_full()
                        .text_color(rgb(0xa1a1aa))
                        .line_clamp(2)
                        .child(comment.quote.clone()),
                )
            })
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
                    .rounded_md()
                    .bg(rgb(0x202023))
                    .child(Self::render_avatar(MessageAuthor::User))
                    .child(body),
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
        TextViewStyle::default()
            .with_foreground(rgb(0xd4d4d8).into())
            .with_muted_foreground(rgb(0x8b8b95).into())
            .with_link(rgb(0x60a5fa).into())
            .with_code_background(rgb(0x27272a).into())
            .with_border(rgb(0x3f3f46).into())
            .with_table(gpui::StyleRefinement::default().bg(rgb(0x18181b)))
            .with_heading_base_font_size(px(14.))
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
        thread_id: Uuid,
        message_index: usize,
        segment_index: usize,
        author: MessageAuthor,
        text: &str,
        annotated: bool,
    ) -> gpui::AnyElement {
        match author {
            _ if annotated => TextView::html(
                format!("timeline-annotated-{thread_id}-{message_index}-{segment_index}"),
                Self::annotated_markdown_html(text),
            )
            .style(Self::annotated_markdown_style())
            .w_full()
            .into_any_element(),
            MessageAuthor::User => SelectableText::new(
                format!("timeline-text-{thread_id}-{message_index}-{segment_index}"),
                text.to_string(),
            )
            .document_order((message_index * 1_000 + segment_index) as u64)
            .into_any_element(),
            MessageAuthor::Agent => TextView::markdown(
                format!("timeline-markdown-{thread_id}-{message_index}-{segment_index}"),
                text.to_string(),
            )
            .selection_format(SelectionFormat::Source)
            .style(Self::markdown_style())
            .w_full()
            .into_any_element(),
        }
    }

    fn wrapped_line_end(
        text: &str,
        selection_end: usize,
        wrap_width: gpui::Pixels,
        window: &mut Window,
    ) -> usize {
        let hard_line_start = text[..selection_end]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
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

    fn render_timeline_message(
        &self,
        thread_id: Uuid,
        index: usize,
        message: &TimelineMessage,
        comments: &[InlineComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = !message.complete && message.text.is_empty();
        let mut message_content = comments
            .iter()
            .filter(|comment| comment.submitted_message_index == Some(index))
            .map(|comment| Self::render_comment_card(comment, "submission", true, cx))
            .collect::<Vec<_>>();
        let mut cursor = 0;
        let mut anchored_comments = comments
            .iter()
            .filter(|comment| comment.message_index == index)
            .filter(|comment| {
                comment.source_range.start < comment.source_range.end
                    && comment.source_range.end <= message.text.len()
                    && message.text.is_char_boundary(comment.source_range.start)
                    && message.text.is_char_boundary(comment.source_range.end)
            })
            .collect::<Vec<_>>();
        anchored_comments.sort_by_key(|comment| comment.source_range.start);

        if anchored_comments.is_empty()
            && let Some(text_view) = &message.text_view
            && !message.text.is_empty()
        {
            message_content.push(
                TextView::new(text_view)
                    .selection_format(SelectionFormat::Source)
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
            let line_end =
                Self::wrapped_line_end(&message.text, first.source_range.end, wrap_width, window);
            let group_start = comment_index;
            while comment_index < anchored_comments.len()
                && anchored_comments[comment_index].source_range.start < line_end
            {
                comment_index += 1;
            }
            let group = &anchored_comments[group_start..comment_index];
            let mut annotated = message.text[cursor..line_end].to_string();
            for comment in group.iter().rev() {
                let start = comment.source_range.start - cursor;
                let end = comment.source_range.end - cursor;
                if end <= annotated.len()
                    && annotated.is_char_boundary(start)
                    && annotated.is_char_boundary(end)
                {
                    annotated.insert_str(end, "</a>");
                    annotated.insert_str(start, "<a href=\"#inline-comment\">");
                }
            }
            message_content.push(Self::render_message_segment(
                thread_id,
                index,
                message_content.len(),
                message.author,
                &annotated,
                true,
            ));
            for comment in group {
                message_content.push(Self::render_comment_card(comment, "timeline", false, cx));
            }
            cursor = line_end;
        }
        if cursor < message.text.len() {
            message_content.push(Self::render_message_segment(
                thread_id,
                index,
                message_content.len(),
                message.author,
                &message.text[cursor..],
                false,
            ));
        }

        div()
            .id(("timeline-message", index))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.selection_message_index = Some(index);
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
                    .child(Self::render_avatar(message.author)),
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
                    .when(!message_content.is_empty(), |this| {
                        this.children(message_content)
                    }),
            )
            .child(div().w(px(40.)).flex_none())
            .into_any_element()
    }

    fn rig_history(messages: &[TimelineMessage]) -> Vec<RigMessage> {
        messages
            .iter()
            .filter(|message| message.complete && !message.failed)
            .map(|message| {
                let text = message.history_text.as_deref().unwrap_or(&message.text);
                match message.author {
                    MessageAuthor::User => RigMessage::user(text),
                    MessageAuthor::Agent => RigMessage::assistant(text),
                }
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
        let text_view = cx.new(|cx| TextViewState::markdown("", cx));
        let message_index = thread.update(cx, |thread, _| {
            let message_index = thread.messages.len();
            thread.messages.push(TimelineMessage {
                author: MessageAuthor::Agent,
                text: String::new(),
                history_text: None,
                text_view: Some(text_view),
                visible: true,
                complete: false,
                failed: false,
            });
            thread.generating = true;
            message_index
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
                    let Some(message) = thread.messages.get_mut(message_index) else {
                        return (true, None);
                    };
                    match event {
                        AgentEvent::Text(text) => {
                            message.text.push_str(&text);
                            (
                                false,
                                message
                                    .text_view
                                    .clone()
                                    .map(|text_view| (text_view, text, true)),
                            )
                        }
                        AgentEvent::Finished => {
                            message.complete = true;
                            thread.generating = false;
                            (true, None)
                        }
                        AgentEvent::Failed(error) => {
                            message.complete = true;
                            message.failed = true;
                            let update = if message.text.is_empty() {
                                message.text = format!("Unable to generate a response: {error}");
                                message
                                    .text_view
                                    .clone()
                                    .map(|text_view| (text_view, message.text.clone(), false))
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

    fn prompt_with_comments(
        prompt: &str,
        comments: &[InlineComment],
        messages: &[TimelineMessage],
        cx: &App,
    ) -> String {
        let pending = comments
            .iter()
            .filter(|comment| {
                !comment.submitted && !comment.body.read(cx).value().trim().is_empty()
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return prompt.to_string();
        }

        let mut result = String::from(
            "The user attached the following inline comments to immutable excerpts from the conversation:\n",
        );
        for (index, comment) in pending.iter().enumerate() {
            let author = messages
                .get(comment.message_index)
                .map(|message| match message.author {
                    MessageAuthor::User => "user",
                    MessageAuthor::Agent => "assistant",
                })
                .unwrap_or("conversation");
            result.push_str(&format!(
                "\n{}. Excerpt from {} message {}:\n> {}\nComment: {}\n",
                index + 1,
                author,
                comment.message_index + 1,
                comment.quote.replace('\n', "\n> "),
                comment.body.read(cx).value().trim(),
            ));
        }
        if !prompt.trim().is_empty() {
            result.push_str("\nAdditional user message:\n");
            result.push_str(prompt);
        }
        result
    }

    fn submit_composer(
        &mut self,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = composer.read(cx).value().to_string();
        let active_thread = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        if active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).generating)
        {
            return;
        }
        let has_pending_comments = active_thread.as_ref().is_some_and(|thread| {
            thread.read(cx).comments.iter().any(|comment| {
                !comment.submitted && !comment.body.read(cx).value().trim().is_empty()
            })
        });
        if prompt.trim().is_empty() && !has_pending_comments {
            return;
        }
        let history = active_thread
            .as_ref()
            .map(|thread| Self::rig_history(&thread.read(cx).messages))
            .unwrap_or_default();
        composer.update(cx, |composer, cx| {
            composer.set_value("", window, cx);
        });

        let (thread_id, thread) = if let Some(thread) = active_thread {
            (thread.read(cx).summary.id, thread)
        } else {
            let thread_id = Uuid::new_v4();
            let thread = cx.new(|_| Thread {
                summary: ThreadSummary {
                    id: thread_id,
                    title: Self::thread_title(&prompt),
                },
                messages: Vec::new(),
                comments: Vec::new(),
                generating: false,
                sharing: ThreadSharing::NotShared,
            });
            self.thread_store.update(cx, |store, _| {
                store.threads.insert(0, thread.clone());
            });
            self.active_thread_id = Some(thread_id);
            (thread_id, thread)
        };

        let agent_prompt = {
            let thread = thread.read(cx);
            Self::prompt_with_comments(&prompt, &thread.comments, &thread.messages, cx)
        };
        let submitted_comment_ids = thread
            .read(cx)
            .comments
            .iter()
            .filter(|comment| {
                !comment.submitted && !comment.body.read(cx).value().trim().is_empty()
            })
            .map(|comment| comment.id)
            .collect::<Vec<_>>();
        thread.update(cx, |thread, _| {
            let submitted_message_index = thread.messages.len();
            for comment in &mut thread.comments {
                if submitted_comment_ids.contains(&comment.id) {
                    comment.submitted = true;
                    comment.submitted_message_index = Some(submitted_message_index);
                }
            }
            thread.messages.push(TimelineMessage {
                author: MessageAuthor::User,
                text: prompt.clone(),
                history_text: (!submitted_comment_ids.is_empty()).then(|| agent_prompt.clone()),
                text_view: None,
                visible: !prompt.trim().is_empty() || !submitted_comment_ids.is_empty(),
                complete: true,
                failed: false,
            });
        });
        self.selection_message_index = None;
        self.follow_generation = true;
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
        self.submit_composer(&self.composer.clone(), window, cx);
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

    fn render_main_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_thread_id = self.active_thread_id;
        let (messages, comments) = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .map(|thread| {
                let thread = thread.read(cx);
                (thread.messages.clone(), thread.comments.clone())
            })
            .unwrap_or_default();
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
                if message.visible {
                    timeline_messages.push(self.render_timeline_message(
                        thread_id, index, message, &comments, wrap_width, window, cx,
                    ));
                }
            }
        }

        let composer_comments = comments
            .iter()
            .filter(|comment| !comment.submitted)
            .map(|comment| Self::render_comment_card(comment, "composer", true, cx))
            .collect::<Vec<_>>();

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
                                            .children(composer_comments)
                                            .child(
                                                Self::render_composer_input(&self.composer)
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.composer
                                                            .focus_handle(cx)
                                                            .focus(window, cx);
                                                    }))
                                                    .on_action(
                                                        cx.listener(Self::submit_composer_action),
                                                    ),
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
                let composer = cx.new(|cx| {
                    let mut composer = TextareaState::new(window, cx).auto_grow(1, usize::MAX);
                    composer.set_editor_style(InputEditorStyle {
                        caret: rgb(0xffffff).into(),
                        ..Default::default()
                    });
                    composer
                });
                composer.focus_handle(cx).focus(window, cx);
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
                        composer,
                        timeline_scroll_handle: ScrollHandle::new(),
                        follow_generation: true,
                        thread_store,
                        active_thread_id: None,
                        selection_message_index: None,
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
