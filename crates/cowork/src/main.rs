use std::time::Duration;

use gpui::{
    Animation, AnimationExt, App, AppContext, Bounds, Context, CursorStyle, FocusHandle,
    IntoElement, KeyDownEvent, Render, SpringAnimation, SpringConfig, TitlebarOptions, Window,
    WindowBounds, WindowControlArea, WindowOptions, div, prelude::*, px, rgb, size,
};

const SIDEBAR_WIDTH: gpui::Pixels = px(275.);
const TOP_BAR_HEIGHT: gpui::Pixels = px(40.);

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

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    composer_text: String,
    composer_focus_handle: FocusHandle,
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

    fn render_timeline_row(
        id: &'static str,
        avatar: &'static str,
        content: gpui::Div,
    ) -> impl IntoElement {
        div()
            .id(id)
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
            .child(content.flex_1().min_w_0())
            .child(div().w(px(40.)).flex_none())
    }

    fn on_composer_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let handled = match keystroke.key.as_str() {
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
                    .child(
                        div()
                            .w_full()
                            .py_6()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .text_sm()
                            .text_color(rgb(0xd4d4d8))
                            .child(Self::render_timeline_row(
                                "user-message",
                                "U",
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child("Could you take a look at the current implementation?")
                                    .child(
                                        div()
                                            .text_color(rgb(0xa1a1aa))
                                            .child("The center should support rich, mixed content while keeping only the final composer editable."),
                                    ),
                            ))
                            .child(Self::render_timeline_row(
                                "agent-summary",
                                "A",
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_color(rgb(0x8b8b95))
                                            .child("Thought, read 3 files, and ran 2 commands  ›"),
                                    )
                                    .child("I reviewed the layout and established a scrollable timeline with dedicated gutters for message authors and future annotations."),
                            ))
                            .child(Self::render_timeline_row(
                                "tool-call",
                                "A",
                                div()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(0x343438))
                                    .bg(rgb(0x1f1f22))
                                    .p_3()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_color(rgb(0xa1a1aa))
                                            .child("Read file · crates/cowork/src/main.rs"),
                                    )
                                    .child(
                                        div()
                                            .rounded_md()
                                            .bg(rgb(0x18181b))
                                            .p_3()
                                            .text_color(rgb(0x71717a))
                                            .child("Tool output and other rich elements can live inline with messages."),
                                    ),
                            ))
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
                                                div()
                                                    .min_h(px(20.))
                                                    .w_full()
                                                    .child(if line.is_empty() {
                                                        " ".to_string()
                                                    } else {
                                                        line
                                                    })
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
    gpui_platform::application().run(|cx: &mut App| {
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

        if let Err(error) = cx.open_window(window_options, |window, cx| {
            cx.new(|cx| {
                let composer_focus_handle = cx.focus_handle();
                composer_focus_handle.focus(window, cx);
                Cowork {
                    sidebar_open: true,
                    recents_open: true,
                    composer_text: String::new(),
                    composer_focus_handle,
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
