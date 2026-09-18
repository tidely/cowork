use std::{sync::OnceLock, time::Duration};

use futures::StreamExt;
use gpui::{
    Animation, AnimationExt, App, AppContext, Bounds, Context, CursorStyle, FocusHandle,
    IntoElement, KeyDownEvent, Render, ScrollHandle, SpringAnimation, SpringConfig,
    TitlebarOptions, Window, WindowBounds, WindowControlArea, WindowOptions, div, prelude::*, px,
    rgb, size,
};
use rig::{
    agent::MultiTurnStreamItem,
    completion::Message as RigMessage,
    prelude::*,
    providers::ollama::wire::Ollama,
    streaming::{Delta, StreamEvent},
};
use serde_json::json;
use tokio::{runtime::Runtime, sync::mpsc};

const SIDEBAR_WIDTH: gpui::Pixels = px(275.);
const TOP_BAR_HEIGHT: gpui::Pixels = px(40.);
const OLLAMA_MODEL: &str = "qwen3.8:27b";
const OLLAMA_CONTEXT_TOKENS: u64 = 131_072;

static TOKIO_RUNTIME: OnceLock<Runtime> = OnceLock::new();

const PLACEHOLDER_THREADS: &[&str] = &[
    "Assess media support",
    "Check browser support",
    "Complete assignment",
    "Fix smoke screen",
    "Check Rust availability",
    "Restrict main branch merges",
    "Schedule Rust checks",
    "Review pull request",
    "Implement indexing type",
    "Generalize animated blocks",
    "Design checkpoint blocks",
    "Review current changes",
];

#[derive(Clone, Copy)]
enum MessageAuthor {
    User,
    Agent,
}

#[derive(Clone)]
struct TimelineMessage {
    author: MessageAuthor,
    text: String,
    complete: bool,
    failed: bool,
}

enum AgentEvent {
    Text(String),
    Finished,
    Failed(String),
}

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    composer_text: String,
    composer_focus_handle: FocusHandle,
    timeline_scroll_handle: ScrollHandle,
    messages: Vec<TimelineMessage>,
    generating: bool,
    conversation_generation: u64,
    tokio_handle: tokio::runtime::Handle,
}

impl Cowork {
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
            .h_full()
            .w(px(40.))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .cursor_pointer()
            .text_sm()
            .text_color(rgb(0xa1a1aa))
            .hover(|this| this.bg(rgb(0x2d2d30)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .child("▥")
    }

    fn render_top_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(0x1c1c1f))
            .window_control_area(WindowControlArea::Drag)
            .child(self.render_sidebar_toggle(cx))
            .child(
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
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui::Div {
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
                            .bg(rgb(0x2d2d30))
                            .hover(|this| this.bg(rgb(0x3a3a3e)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.messages.clear();
                                this.composer_text.clear();
                                this.generating = false;
                                this.conversation_generation =
                                    this.conversation_generation.wrapping_add(1);
                                this.composer_focus_handle.focus(window, cx);
                                cx.notify();
                            }))
                            .text_sm()
                            .text_color(rgb(0xf4f4f5))
                            .child("✎")
                            .child("New chat"),
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
                                PLACEHOLDER_THREADS
                                    .iter()
                                    .enumerate()
                                    .map(|(index, title)| {
                                        div()
                                            .id(("placeholder-thread", index))
                                            .h(px(30.))
                                            .w_full()
                                            .px_2()
                                            .flex()
                                            .items_center()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .hover(|this| this.bg(rgb(0x2d2d30)))
                                            .text_sm()
                                            .text_color(rgb(0xd4d4d8))
                                            .truncate()
                                            .child(*title)
                                    }),
                            ),
                        )
                    }),
            )
    }

    fn render_avatar(label: &'static str) -> gpui::Div {
        div()
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(0x3f3f46))
            .text_xs()
            .text_color(rgb(0xf4f4f5))
            .child(label)
    }

    fn render_timeline_message(index: usize, message: &TimelineMessage) -> impl IntoElement {
        let avatar = match message.author {
            MessageAuthor::User => "U",
            MessageAuthor::Agent => "A",
        };
        let waiting = !message.complete && message.text.is_empty();
        let lines = message
            .text
            .split('\n')
            .map(|line| {
                div().min_h(px(20.)).w_full().child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                })
            })
            .collect::<Vec<_>>();

        div()
            .id(("timeline-message", index))
            .w_full()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(Self::render_avatar(avatar)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
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
                    .children(lines),
            )
            .child(div().w(px(40.)).flex_none())
    }

    fn rig_history(&self) -> Vec<RigMessage> {
        self.messages
            .iter()
            .filter(|message| message.complete && !message.failed)
            .map(|message| match message.author {
                MessageAuthor::User => RigMessage::user(&message.text),
                MessageAuthor::Agent => RigMessage::assistant(&message.text),
            })
            .collect()
    }

    fn start_generation(
        &mut self,
        prompt: String,
        history: Vec<RigMessage>,
        cx: &mut Context<Self>,
    ) {
        let message_index = self.messages.len();
        self.messages.push(TimelineMessage {
            author: MessageAuthor::Agent,
            text: String::new(),
            complete: false,
            failed: false,
        });
        self.generating = true;

        let conversation_generation = self.conversation_generation;
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

            // Reduce memory usage during streaming by dropping owned copy of history
            drop(history);

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
                let result = this.update(cx, |this, cx| {
                    if this.conversation_generation != conversation_generation {
                        return true;
                    }

                    let Some(message) = this.messages.get_mut(message_index) else {
                        return true;
                    };
                    let finished = match event {
                        AgentEvent::Text(text) => {
                            message.text.push_str(&text);
                            false
                        }
                        AgentEvent::Finished => {
                            message.complete = true;
                            this.generating = false;
                            true
                        }
                        AgentEvent::Failed(error) => {
                            message.complete = true;
                            message.failed = true;
                            if message.text.is_empty() {
                                message.text = format!("Unable to generate a response: {error}");
                            }
                            this.generating = false;
                            true
                        }
                    };
                    this.timeline_scroll_handle.scroll_to_bottom();
                    cx.notify();
                    finished
                });

                match result {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        })
        .detach();
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        if self.generating || self.composer_text.trim().is_empty() {
            return;
        }

        let history = self.rig_history();
        let prompt = std::mem::take(&mut self.composer_text);
        self.messages.push(TimelineMessage {
            author: MessageAuthor::User,
            text: prompt.clone(),
            complete: true,
            failed: false,
        });
        self.start_generation(prompt, history, cx);
        self.timeline_scroll_handle.scroll_to_bottom();
    }

    fn on_composer_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let submit = keystroke.key == "enter"
            && (keystroke.modifiers.control || keystroke.modifiers.platform);
        let handled = if submit {
            self.submit_composer(cx);
            true
        } else {
            match keystroke.key.as_str() {
                "backspace" => {
                    self.composer_text.pop();
                    true
                }
                "enter" => {
                    self.composer_text.push('\n');
                    true
                }
                _ if !keystroke.modifiers.control && !keystroke.modifiers.platform => {
                    if let Some(text) = keystroke.key_char.as_deref() {
                        self.composer_text.push_str(text);
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            }
        };

        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_main_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut composer_lines = self.composer_text.split('\n').collect::<Vec<_>>();
        let active_line = composer_lines.pop().unwrap_or_default().to_string();
        let completed_lines = composer_lines
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();

        let timeline_messages = self
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| Self::render_timeline_message(index, message))
            .collect::<Vec<_>>();

        let caret = div()
            .w(px(2.))
            .h(px(18.))
            .flex_none()
            .bg(rgb(0x60a5fa))
            .with_animation(
                "composer-caret",
                Animation::new(Duration::from_millis(900)).repeat(),
                |caret, delta| caret.opacity(if delta < 0.5 { 1. } else { 0. }),
            );

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
                                            .child(Self::render_avatar("U")),
                                    )
                                    .child(
                                        div()
                                            .id("composer")
                                            .min_h(px(110.))
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .track_focus(&self.composer_focus_handle)
                                            .cursor(CursorStyle::IBeam)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.composer_focus_handle.focus(window, cx);
                                            }))
                                            .on_key_down(cx.listener(Self::on_composer_key_down))
                                            .text_color(rgb(0xe4e4e7))
                                            .children(completed_lines.into_iter().map(|line| {
                                                div().min_h(px(20.)).w_full().child(
                                                    if line.is_empty() {
                                                        " ".to_string()
                                                    } else {
                                                        line
                                                    },
                                                )
                                            }))
                                            .child(
                                                div()
                                                    .min_h(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .child(active_line)
                                                    .child(caret),
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
                    .child(self.render_main_editor(cx)),
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

    gpui_platform::application().run(move |cx: &mut App| {
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1200.), px(760.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Cowork".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        if let Err(error) = cx.open_window(window_options, move |window, cx| {
            let tokio_handle = tokio_handle.clone();
            cx.new(|cx| {
                let composer_focus_handle = cx.focus_handle();
                composer_focus_handle.focus(window, cx);
                Cowork {
                    sidebar_open: true,
                    recents_open: true,
                    composer_text: String::new(),
                    composer_focus_handle,
                    timeline_scroll_handle: ScrollHandle::new(),
                    messages: Vec::new(),
                    generating: false,
                    conversation_generation: 0,
                    tokio_handle,
                }
            })
        }) {
            eprintln!("failed to open Cowork window: {error}");
            cx.quit();
            return;
        }

        cx.activate(true);
    });
}
