//! Pending submissions use the same user-message rows as the timeline. Only
//! separators distinguish delivery timing; steering shares one batch, while
//! each queued submission has its own boundary and cancellation.

use gpui::{AnyElement, App, Context, IntoElement, canvas, div, prelude::*, px, size};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use uuid::Uuid;

use crate::{Cowork, composer_attachments::AttachmentCard, submission::SubmissionMode};

/// Paint at the measured width so resizing keeps the dash rhythm independent
/// of font metrics. The solid draft boundary shares the work summary's color.
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
                        gpui::Bounds::new(gpui::point(x, bounds.top()), size(width, px(1.))),
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

impl Cowork {
    fn pending_separator(
        &self,
        label: &'static str,
        dashed: bool,
        cancellation: Option<(Uuid, Uuid)>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let separator = div()
            .w_full()
            .relative()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().child(rule(dashed, cx)))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(label),
                    )
                    .child(div().flex_1().child(rule(dashed, cx))),
            )
            .when_some(cancellation, |el, (thread_id, pending_id)| {
                el.child(
                    div()
                        .absolute()
                        .right_0()
                        .top(px(-4.))
                        .bg(cx.theme().background)
                        .child(
                            Button::new(format!("cancel-pending-{pending_id}"))
                                .ghost()
                                .small()
                                .label("×")
                                .debug_selector(move || format!("cancel-pending-{pending_id}"))
                                .text_color(cx.theme().muted_foreground)
                                .accessibility_label(if label == "Steering" {
                                    "Cancel all steering messages"
                                } else {
                                    "Cancel queued message"
                                })
                                .tooltip(if label == "Steering" {
                                    "Cancel all steering messages"
                                } else {
                                    "Cancel this queued message"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.cancel_pending_submission(thread_id, pending_id, cx);
                                })),
                        ),
                )
            });
        self.render_message_row(None, [separator.into_any_element()])
            .into_any_element()
    }

    pub(crate) fn render_pending_messages(
        &self,
        thread_id: Uuid,
        document_order_base: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return Vec::new();
        };
        let state = thread.read(cx);
        let pending = state.pending_submissions().to_vec();
        if pending.is_empty() {
            return Vec::new();
        }
        let can_cancel = state.can_control_generation();
        let ordered = pending
            .iter()
            .filter(|pending| pending.mode == SubmissionMode::Steering)
            .chain(
                pending
                    .iter()
                    .filter(|pending| pending.mode == SubmissionMode::Queue),
            );
        let draft_row = div()
            .mt_6()
            .child(self.pending_separator("Draft", false, None, cx))
            .into_any_element();
        ordered
            .enumerate()
            .map(|(position, pending)| {
                let cards = {
                    let state = thread.read(cx);
                    pending
                        .group
                        .blocks
                        .iter()
                        .map(|block| {
                            block
                                .attachments
                                .iter()
                                .map(|record| AttachmentCard::new(state.draft(), record))
                                .collect()
                        })
                        .collect()
                };
                let label = match pending.mode {
                    SubmissionMode::Steering => "Steering",
                    SubmissionMode::Queue => "Queued",
                };
                div()
                    .mt_6()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(self.pending_separator(
                        label,
                        true,
                        can_cancel.then_some((thread_id, pending.id)),
                        cx,
                    ))
                    .child(self.render_user_message_group(
                        document_order_base + position,
                        &pending.group.clone().into(),
                        cards,
                        cx,
                    ))
                    .into_any_element()
            })
            .chain(std::iter::once(draft_row))
            .collect()
    }
}
