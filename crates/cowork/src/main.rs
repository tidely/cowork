use std::{sync::OnceLock, time::Duration};

use futures::StreamExt;
use gpui::{
    Animation, AnimationExt, App, AppContext, Bounds, Context, Entity, Focusable, IntoElement,
    KeyBinding, Render, ScrollHandle, SpringAnimation, SpringConfig, TitlebarOptions, Window,
    WindowBounds, WindowControlArea, WindowOptions, actions, div, prelude::*, px, rgb, size,
};
use gpui_base::{
    SelectableText, TextSelectionLayer, Textarea,
    input::{InputEditorStyle, TextareaState},
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
use uuid::Uuid;

const SIDEBAR_WIDTH: gpui::Pixels = px(275.);
const TOP_BAR_HEIGHT: gpui::Pixels = px(40.);
const OLLAMA_MODEL: &str = "qwen3.8:27b";
const OLLAMA_CONTEXT_TOKENS: u64 = 131_072;

static TOKIO_RUNTIME: OnceLock<Runtime> = OnceLock::new();

actions!(cowork, [SubmitComposer]);

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

#[derive(Clone)]
struct ThreadSummary {
    id: Uuid,
    title: String,
}

struct Thread {
    summary: ThreadSummary,
    messages: Vec<TimelineMessage>,
    generating: bool,
    collaborating: bool,
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
    thread_store: Entity<ThreadStore>,
    active_thread_id: Option<Uuid>,
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
        let active_thread = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        let collaborating = active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).collaborating);

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
                            .when(active_thread.is_some(), |this| {
                                this.cursor_pointer()
                                    .text_color(rgb(0xd4d4d8))
                                    .hover(|this| this.bg(rgb(0x2d2d30)))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(thread_id) = this.active_thread_id else {
                                    return;
                                };
                                let Some(thread) = this.thread_store.read(cx).thread(thread_id, cx)
                                else {
                                    return;
                                };
                                thread.update(cx, |thread, _| {
                                    thread.collaborating = !thread.collaborating;
                                });
                                cx.notify();
                            }))
                            .child(if collaborating { "Unshare" } else { "Share" }),
                    )
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
                    ),
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
                (thread.summary.clone(), thread.collaborating)
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

    fn render_timeline_message(
        thread_id: Uuid,
        index: usize,
        message: &TimelineMessage,
    ) -> impl IntoElement {
        let avatar = match message.author {
            MessageAuthor::User => "U",
            MessageAuthor::Agent => "A",
        };
        let waiting = !message.complete && message.text.is_empty();

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
                    .when(!message.text.is_empty(), |this| {
                        this.child(
                            SelectableText::new(
                                format!("timeline-text-{thread_id}-{index}"),
                                message.text.clone(),
                            )
                            .document_order(index as u64),
                        )
                    }),
            )
            .child(div().w(px(40.)).flex_none())
    }

    fn rig_history(messages: &[TimelineMessage]) -> Vec<RigMessage> {
        messages
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
        thread_id: Uuid,
        prompt: String,
        history: Vec<RigMessage>,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let message_index = thread.update(cx, |thread, _| {
            let message_index = thread.messages.len();
            thread.messages.push(TimelineMessage {
                author: MessageAuthor::Agent,
                text: String::new(),
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
                let finished = thread.update(cx, |thread, _| {
                    let Some(message) = thread.messages.get_mut(message_index) else {
                        return true;
                    };
                    match event {
                        AgentEvent::Text(text) => {
                            message.text.push_str(&text);
                            false
                        }
                        AgentEvent::Finished => {
                            message.complete = true;
                            thread.generating = false;
                            true
                        }
                        AgentEvent::Failed(error) => {
                            message.complete = true;
                            message.failed = true;
                            if message.text.is_empty() {
                                message.text = format!("Unable to generate a response: {error}");
                            }
                            thread.generating = false;
                            true
                        }
                    }
                });

                let result = this.update(cx, |this, cx| {
                    if this.active_thread_id == Some(thread_id) {
                        this.timeline_scroll_handle.scroll_to_bottom();
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

    fn submit_composer(
        &mut self,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = composer.read(cx).value().to_string();
        if prompt.trim().is_empty() {
            return;
        }

        let active_thread = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        if active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).generating)
        {
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
                generating: false,
                collaborating: false,
            });
            self.thread_store.update(cx, |store, _| {
                store.threads.insert(0, thread.clone());
            });
            self.active_thread_id = Some(thread_id);
            (thread_id, thread)
        };

        thread.update(cx, |thread, _| {
            thread.messages.push(TimelineMessage {
                author: MessageAuthor::User,
                text: prompt.clone(),
                complete: true,
                failed: false,
            });
        });
        self.start_generation(thread_id, prompt, history, cx);
        self.timeline_scroll_handle.scroll_to_bottom();
    }

    fn submit_composer_action(
        &mut self,
        _: &SubmitComposer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_composer(&self.composer.clone(), window, cx);
    }

    fn render_main_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_thread_id = self.active_thread_id;
        let messages = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .map(|thread| thread.read(cx).messages.clone())
            .unwrap_or_default();
        let timeline_messages = active_thread_id
            .into_iter()
            .flat_map(|thread_id| {
                messages.iter().enumerate().map(move |(index, message)| {
                    Self::render_timeline_message(thread_id, index, message)
                })
            })
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
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.composer.focus_handle(cx).focus(window, cx);
                                            }))
                                            .on_action(cx.listener(Self::submit_composer_action))
                                            .child(Textarea::new(&self.composer)),
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
        gpui_base::init(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", SubmitComposer, Some("Input")),
            KeyBinding::new("cmd-enter", SubmitComposer, Some("Input")),
        ]);
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
            let thread_store = cx.new(|_| ThreadStore {
                threads: Vec::new(),
            });
            let composer = cx.new(|cx| {
                let mut composer = TextareaState::new(window, cx).auto_grow(1, 8);
                composer.set_editor_style(InputEditorStyle {
                    caret: rgb(0xffffff).into(),
                    ..Default::default()
                });
                composer
            });
            composer.focus_handle(cx).focus(window, cx);
            cx.new(|_| Cowork {
                sidebar_open: true,
                recents_open: true,
                composer,
                timeline_scroll_handle: ScrollHandle::new(),
                thread_store,
                active_thread_id: None,
                tokio_handle,
            })
        }) {
            eprintln!("failed to open Cowork window: {error}");
            cx.quit();
            return;
        }

        cx.activate(true);
    });
}
