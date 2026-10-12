//! Disposable queue/steering UI study; no agent, sharing, or saved state.
//! Run with `cargo run -p cowork --example queue_preview`.
//! Add `-- --icons` to open directly in the numbered icon picker.
//! Delete this file to remove the whole study.

use std::rc::Rc;

use gpui::{
    App, Bounds, Context, Entity, IntoElement, KeyBinding, QuitMode, Render, Window, WindowBounds,
    WindowOptions, actions, canvas, div, prelude::*, px, rgb, size,
};
use gpui_base::{Textarea, input::TextareaState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Root, Sizable as _, Theme, ThemeConfig,
    button::{Button, ButtonVariants as _},
    kbd::Kbd,
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName;

const DRAFT: &str = "Also show what happens when someone queues three messages.";

gpui_kit_assets::icon_assets!(
    PreviewAssets,
    [
        SendHorizontal,
        Square,
        CornerDownLeft,
        Undo2,
        CornerUpLeft,
        Reply,
        ReplyAll,
        Undo,
        UndoDot,
        CornerLeftDown,
        CornerLeftUp,
        ArrowLeft,
        MoveLeft,
        ArrowLeftToLine,
        ArrowLeftFromLine,
        ArrowDownLeft,
        ArrowUpLeft,
        ArrowBigLeft,
        ArrowBigLeftDash,
        MessageCircleReply,
        MessageSquareReply,
        RotateCcw,
    ]
);

/// The toolkit's actual bundled assets, in preference order. The first
/// descends from the top and turns left, matching the proposed steering cue.
const STEERING_ICONS: [(IconName, &str); 20] = [
    (IconName::CornerDownLeft, "CornerDownLeft"),
    (IconName::Undo2, "Undo2"),
    (IconName::CornerUpLeft, "CornerUpLeft"),
    (IconName::Reply, "Reply (previous)"),
    (IconName::ReplyAll, "ReplyAll"),
    (IconName::Undo, "Undo"),
    (IconName::UndoDot, "UndoDot"),
    (IconName::CornerLeftDown, "CornerLeftDown"),
    (IconName::CornerLeftUp, "CornerLeftUp"),
    (IconName::ArrowLeft, "ArrowLeft"),
    (IconName::MoveLeft, "MoveLeft"),
    (IconName::ArrowLeftToLine, "ArrowLeftToLine"),
    (IconName::ArrowLeftFromLine, "ArrowLeftFromLine"),
    (IconName::ArrowDownLeft, "ArrowDownLeft"),
    (IconName::ArrowUpLeft, "ArrowUpLeft"),
    (IconName::ArrowBigLeft, "ArrowBigLeft"),
    (IconName::ArrowBigLeftDash, "ArrowBigLeftDash"),
    (IconName::MessageCircleReply, "MessageCircleReply"),
    (IconName::MessageSquareReply, "MessageSquareReply"),
    (IconName::RotateCcw, "RotateCcw"),
];

actions!(
    queue_preview,
    [Quit, SteerDraft, QueueDraft, StopGeneration]
);

fn main() {
    gpui_platform::application()
        .with_assets(PreviewAssets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            let mut palette: serde_json::Value =
                serde_json::from_str(include_str!("../themes/cowork-dark.json")).unwrap();
            // Try blue within this disposable study without changing the app's palette.
            for (role, color) in [
                ("primary.background", "#60a5fa"),
                ("primary.hover.background", "#93c5fd"),
                ("primary.active.background", "#3b82f6"),
                ("accent.background", "#202b3c"),
                ("ring", "#60a5fa"),
            ] {
                palette["colors"][role] = color.into();
            }
            let palette: ThemeConfig = serde_json::from_value(palette).unwrap();
            Theme::update(cx, |theme| theme.apply_config(&Rc::new(palette)));
            cx.on_action(|_: &Quit, cx| cx.quit());
            let modifier = if cfg!(target_os = "macos") {
                "cmd"
            } else {
                "ctrl"
            };
            cx.bind_keys([
                KeyBinding::new(&format!("{modifier}-q"), Quit, None),
                KeyBinding::new(
                    &format!("{modifier}-enter"),
                    SteerDraft,
                    Some("QueuePreview"),
                ),
                KeyBinding::new(
                    &format!("{modifier}-shift-enter"),
                    QueueDraft,
                    Some("QueuePreview"),
                ),
                KeyBinding::new(
                    &format!("{modifier}-."),
                    StopGeneration,
                    Some("QueuePreview"),
                ),
            ]);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1120.), px(960.)),
                        cx,
                    ))),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Cowork · Queue and steering study".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let editor = cx.new(|cx| {
                        let mut editor = TextareaState::new(window, cx).auto_grow(2, usize::MAX);
                        editor.set_value(DRAFT, window, cx);
                        editor
                    });
                    let preview = cx.new(|cx| Preview::new(editor, cx));
                    cx.new(|cx| Root::new(preview, window, cx))
                },
            )
            .expect("open queue preview");
            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Steering,
    Queue,
}

struct Pending {
    /// A submitted draft stays together as one delivery unit.
    blocks: Vec<String>,
    delivery: Delivery,
}

struct Preview {
    pending: Vec<Pending>,
    editor: Entity<TextareaState>,
    label_draft: bool,
    show_cancel: bool,
    generating: bool,
    sent: Vec<String>,
    icon_picker: bool,
    steering_icon: IconName,
    _editor_subscription: gpui::Subscription,
}

impl Preview {
    fn new(editor: Entity<TextareaState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&editor, |_, _, cx| cx.notify());
        let mut this = Self {
            pending: Vec::new(),
            editor,
            label_draft: true,
            show_cancel: true,
            generating: true,
            sent: Vec::new(),
            icon_picker: std::env::args().any(|arg| arg == "--icons"),
            steering_icon: IconName::CornerDownLeft,
            _editor_subscription: subscription,
        };
        this.seed();
        this
    }

    fn seed(&mut self) {
        self.pending.clear();
        for (delivery, text) in [
            (
                Delivery::Steering,
                "Keep this as a standalone example, please.",
            ),
            (
                Delivery::Steering,
                "Use the same gray line as the work summary.",
            ),
            (
                Delivery::Queue,
                "Keep the labels centered in the dashed lines.",
            ),
            (
                Delivery::Queue,
                "Use the same message styling before and after sending.",
            ),
            (
                Delivery::Queue,
                "Finally, check how it looks with an empty queue.",
            ),
        ] {
            self.push(delivery, text.to_owned());
        }
    }

    fn push(&mut self, delivery: Delivery, text: String) {
        // Steering is one batch at the next boundary; queued submissions
        // remain separate because they are delivered one at a time.
        if delivery == Delivery::Steering
            && let Some(batch) = self.pending.iter_mut().find(|p| p.delivery == delivery)
        {
            batch.blocks.push(text);
            return;
        }
        self.pending.push(Pending {
            blocks: vec![text],
            delivery,
        });
    }

    fn send_draft(&mut self, delivery: Delivery, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        if self.generating {
            self.push(delivery, text);
        } else {
            self.sent.push(text);
            self.generating = true;
        }
        self.editor
            .update(cx, |editor, cx| editor.set_value("", window, cx));
        cx.notify();
    }

    /// Match the work summary's theme border. Paint dashes at the actual
    /// column width, so resizing never leaves a partial row of fake text.
    fn rule(dashed: bool, cx: &App) -> impl IntoElement {
        let color = cx.theme().border;
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                if dashed {
                    let mut x = bounds.left();
                    while x < bounds.right() {
                        let width = px(6.).min(bounds.right() - x);
                        window.paint_quad(gpui::fill(
                            Bounds::new(gpui::point(x, bounds.top()), size(width, px(1.))),
                            color,
                        ));
                        x += px(11.);
                    }
                } else {
                    window.paint_quad(gpui::fill(bounds, color));
                }
            },
        )
        .w_full()
        .h(px(1.))
        .flex_none()
    }

    fn avatar() -> impl IntoElement {
        div()
            .size(px(22.))
            .flex_none()
            .rounded_full()
            .bg(rgb(0xe26d5a))
            .text_color(rgb(0xffffff))
            .text_size(px(9.))
            .flex()
            .items_center()
            .justify_center()
            .child("RA")
    }

    /// Center the muted label in the break, keeping equal line lengths on
    /// either side as the conversation column resizes.
    fn labeled_rule(label: String, dashed: bool, cx: &App) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .child(div().flex_1().child(Self::rule(dashed, cx)))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(div().flex_1().child(Self::rule(dashed, cx)))
    }

    fn render_icon_picker(&self, cx: &Context<Self>) -> gpui::AnyElement {
        div().size_full().flex().flex_col()
            .bg(cx.theme().background).text_color(cx.theme().foreground)
            .child(div().px_6().py_4().flex().items_center().justify_between()
                .child(div().flex().flex_col().gap_1()
                    .child("Steering icon picker")
                    .child(div().text_sm().text_color(cx.theme().muted_foreground)
                        .child("Click a numbered tile, then return to the thread to try it. #1 bends left from the top.")))
                .child(Button::new("back-to-thread").small().label("Back to thread")
                    .on_click(cx.listener(|this, _, _, cx| { this.icon_picker = false; cx.notify(); }))))
            .child(div().id("icon-grid").flex_1().min_h_0().overflow_y_scroll()
                .child(div().p_6().grid().grid_cols(4).gap_4()
                    .children(STEERING_ICONS.iter().enumerate().map(|(index, &(icon, name))| {
                        div().id(("icon-choice", index)).p_4().rounded_lg()
                            .border_1().border_color(if self.steering_icon == icon { cx.theme().primary } else { cx.theme().border })
                            .bg(if self.steering_icon == icon { cx.theme().accent } else { cx.theme().background })
                            .cursor_pointer().flex().flex_col().gap_3()
                            .on_click(cx.listener(move |this, _, _, cx| { this.steering_icon = icon; cx.notify(); }))
                            .child(div().text_sm().text_color(cx.theme().muted_foreground)
                                .child(format!("{}. {name}", index + 1)))
                            .child(div().flex().items_center().justify_between()
                                .child(Icon::new(icon).size(px(32.)).text_color(cx.theme().primary))
                                .child(Button::new(("icon-size-preview", index)).small()
                                    .icon(Icon::new(icon)).bg(cx.theme().accent).text_color(cx.theme().primary)
                                    .on_click(cx.listener(move |this, _, _, cx| { this.steering_icon = icon; cx.notify(); }))))
                    }))))
            .child(div().px_6().py_3().text_sm().text_color(cx.theme().muted_foreground)
                .child("Large icon at left; actual button size at right. Selection changes only the steering icon."))
            .into_any_element()
    }

    fn render_send_button(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let empty = self.editor.read(cx).value().trim().is_empty();
        if self.generating && empty {
            return div()
                .id("stop-tooltip")
                .tooltip(|window, cx| {
                    Tooltip::new("Stop generating")
                        .action(&StopGeneration, Some("QueuePreview"))
                        .build(window, cx)
                })
                .child(
                    Button::new("stop-generation")
                        .icon(Icon::new(IconName::Square))
                        .danger()
                        .small()
                        .accessibility_label("Stop generating")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.generating = false;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        let generating = self.generating;
        div()
            .id("send-tooltip")
            .tooltip(move |window, cx| {
                Tooltip::element(move |window, cx| {
                    let mut rows = div().py_1().flex().flex_col().gap_2().child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_6()
                            .child(if generating { "Steer" } else { "Send message" })
                            .children(Kbd::binding_for_action(
                                &SteerDraft,
                                Some("QueuePreview"),
                                window,
                            )),
                    );
                    if generating {
                        rows = rows
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_6()
                                    .child("Queue")
                                    .children(Kbd::binding_for_action(
                                        &QueueDraft,
                                        Some("QueuePreview"),
                                        window,
                                    )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Click to steer · Shift-click to queue"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_6()
                                    .child("Stop generating")
                                    .children(Kbd::binding_for_action(
                                        &StopGeneration,
                                        Some("QueuePreview"),
                                        window,
                                    )),
                            );
                    }
                    rows
                })
                .build(window, cx)
            })
            .child(
                Button::new("send-message")
                    .icon(Icon::new(if generating {
                        self.steering_icon
                    } else {
                        IconName::SendHorizontal
                    }))
                    .small()
                    .disabled(empty)
                    .when(!empty, |button| {
                        button.bg(cx.theme().accent).text_color(cx.theme().primary)
                    })
                    .accessibility_label(if generating {
                        "Steer message; Shift-click to queue"
                    } else {
                        "Send message"
                    })
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                        let delivery = if event.modifiers().shift {
                            Delivery::Queue
                        } else {
                            Delivery::Steering
                        };
                        this.send_draft(delivery, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn pending_messages(&self, cx: &Context<Self>) -> impl IntoElement {
        // Steering is delivered first. Each batch's separator names only
        // the delivery mode of the content immediately below it.
        let steering = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, p)| p.delivery == Delivery::Steering);
        let queued = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, p)| p.delivery == Delivery::Queue);
        div()
            .flex()
            .flex_col()
            .gap_6()
            .children(steering.chain(queued).map(|(index, p)| {
                let label = match p.delivery {
                    Delivery::Steering => "Steering",
                    Delivery::Queue => "Queued",
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .pl(px(40.))
                            .relative()
                            .child(Self::labeled_rule(label.to_owned(), true, cx))
                            .when(self.show_cancel, |el| {
                                // Overlay the control at the end of the line so the
                                // label stays centered when cancellation is shown.
                                el.child(
                                    div()
                                        .absolute()
                                        .right_0()
                                        .top(px(-4.))
                                        .bg(cx.theme().background)
                                        .child(
                                            Button::new(("cancel-pending", index))
                                                .ghost()
                                                .small()
                                                .text_color(cx.theme().muted_foreground)
                                                .label("×")
                                                .tooltip(if p.delivery == Delivery::Steering {
                                                    "Cancel all steering messages"
                                                } else {
                                                    "Cancel this queued message"
                                                })
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.pending.remove(index);
                                                    cx.notify();
                                                })),
                                        ),
                                )
                            }),
                    )
                    .children(p.blocks.iter().map(|text| {
                        div()
                            .flex()
                            .gap(px(18.))
                            .child(Self::avatar())
                            .child(div().flex_1().min_w_0().child(text.clone()))
                    }))
            }))
            .when(!self.pending.is_empty(), |el| {
                el.child(
                    div()
                        .pl(px(40.))
                        .when(self.label_draft, |el| {
                            el.child(Self::labeled_rule("Draft".to_owned(), false, cx))
                        })
                        .when(!self.label_draft, |el| el.child(Self::rule(false, cx))),
                )
            })
    }
}

impl Render for Preview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.icon_picker {
            return self.render_icon_picker(cx);
        }
        let muted = cx.theme().muted_foreground;
        div()
            .key_context("QueuePreview")
            .on_action(cx.listener(|this, _: &SteerDraft, window, cx| {
                this.send_draft(Delivery::Steering, window, cx);
            }))
            .on_action(cx.listener(|this, _: &StopGeneration, _, cx| {
                this.generating = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &QueueDraft, window, cx| {
                this.send_draft(Delivery::Queue, window, cx);
            }))
            .size_full().flex().flex_col()
            .bg(cx.theme().background).text_color(cx.theme().foreground).text_size(px(14.))
            .child(
                div().flex_none().px_6().py_3().flex().items_center().gap_2()
                    .border_b_1().border_color(cx.theme().border)
                    .child(div().flex_1().text_sm().child("Queue & steering · UI study"))
                    .child(Button::new("pick-icon").small().ghost().label("Pick icon")
                        .on_click(cx.listener(|this, _, _, cx| { this.icon_picker = true; cx.notify(); })))
                    .child(Button::new("cancel-controls").small().ghost()
                        .label(if self.show_cancel { "Cancel: on" } else { "Cancel: off" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_cancel = !this.show_cancel;
                            cx.notify();
                        })))
                    .child(Button::new("draft-label").small().ghost()
                        .label(if self.label_draft { "Draft label: on" } else { "Draft label: off" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.label_draft = !this.label_draft;
                            cx.notify();
                        })))
                    .children([
                        ("Mixed", None),
                        ("Steering only", Some(Delivery::Steering)),
                        ("Queued only", Some(Delivery::Queue)),
                    ].map(|(label, delivery)| {
                        Button::new(label).small().ghost().label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.seed();
                                if let Some(delivery) = delivery {
                                    this.pending.retain(|p| p.delivery == delivery);
                                }
                                cx.notify();
                            }))
                    }))
                    .child(Button::new("empty").small().ghost().label("Empty")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pending.clear();
                            cx.notify();
                        })))
                    .child(Button::new("reset").small().ghost().label("Reset")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.seed();
                            this.sent.clear();
                            this.generating = true;
                            this.editor.update(cx, |editor, cx| editor.set_value(DRAFT, window, cx));
                            cx.notify();
                        }))),
            )
            .child(div().flex_none().px_6().py_2().flex().items_center().gap_2()
                .border_b_1().border_color(cx.theme().border)
                .child(Button::new("agent-state").small().ghost()
                    .label(if self.generating { "Agent: generating" } else { "Agent: idle" })
                    .on_click(cx.listener(|this, _, _, cx| { this.generating = !this.generating; cx.notify(); })))
                .child(Button::new("empty-draft").small().ghost().label("Empty draft")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.editor.update(cx, |editor, cx| editor.set_value("", window, cx));
                    })))
                .child(Button::new("fill-draft").small().ghost().label("Fill draft")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.editor.update(cx, |editor, cx| editor.set_value(DRAFT, window, cx));
                    }))))
            .child(
                div().id("stage").flex_1().min_h_0().overflow_y_scroll()
                    .child(div().w_full().max_w(px(960.)).mx_auto().px_10().py_6()
                        .flex().flex_col().gap_6()
                        .child(div().flex().gap(px(18.)).child(Self::avatar())
                            .child("Explore a queue and steering interface for Cowork."))
                        .child(div().pl(px(40.)).flex().flex_col().gap_4()
                            .child(div().text_sm().text_color(muted).pb_2().border_b_1()
                                .border_color(cx.theme().border).child("Worked for 10 seconds"))
                            .child("I’ll put together a standalone example using the existing conversation layout."))
                        .child(div().pl(px(40.)).flex().flex_col().gap_3()
                            .child(div().text_sm().text_color(muted).pb_2().border_b_1()
                                .border_color(cx.theme().border).child(if self.generating { "Working for 8 seconds" } else { "Worked for 8 seconds" }))
                            .child(div().text_color(muted).child(if self.generating {
                                "›  Running command · cargo check -p cowork"
                            } else { "The preview agent has stopped." })))
                        .children(self.sent.iter().map(|text| div().flex().gap(px(18.))
                            .child(Self::avatar()).child(text.clone())))
                        .when(!self.pending.is_empty(), |el| el.child(self.pending_messages(cx)))
                        .child(div().flex().gap(px(18.)).min_h(px(110.))
                            .child(Self::avatar())
                            .child(div().flex_1().min_w_0().child(Textarea::new(&self.editor))))
                    ),
            )
            .child(div().flex_none().px_6().py_2().flex().items_center().gap_2()
                .border_t_1().border_color(cx.theme().border)
                .child(div().text_sm().text_color(muted).child("cowork  │  Read"))
                .child(div().flex_1())
                .child(div().text_xs().text_color(muted).child("Local preview · no messages sent"))
                .child(Button::new("clear").small().ghost().label("Clear pending")
                    .on_click(cx.listener(|this, _, _, cx| { this.pending.clear(); cx.notify(); })))
                .child(div().text_sm().text_color(muted).child("qwen3.5"))
                .child(self.render_send_button(cx)))
            .into_any_element()
    }
}
