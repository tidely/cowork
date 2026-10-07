//! The card asking whether a tool call may run: what the call would do, and
//! Allow and Deny for someone who may decide, or a note that it waits for
//! someone who may.
//!
//! The card is the shared shell. A tool that wants to show its calls in its
//! own terms passes its own [`ToolCallCard::body`]; every other tool gets its
//! arguments listed.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, FontWeight, IntoElement, Pixels, RenderOnce, SharedString,
    StyleRefinement, Window, div, prelude::*, px, relative,
};
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::Button,
    group_box::{GroupBox, GroupBoxVariants as _},
    h_flex,
    shimmer::ShimmerText,
};
use gpui_kit_assets::IconName;
use rig::message::ToolCall;

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// What the card offers under the call.
enum Decision {
    /// Allow and Deny, for someone who may approve tool calls.
    Offered {
        on_allow: ClickHandler,
        on_deny: ClickHandler,
    },
    /// A note that the call waits for someone who may.
    Waiting,
}

#[derive(IntoElement)]
pub(crate) struct ToolCallCard {
    /// Prefixes the ids and debug selectors of the card's parts, so it must
    /// be unique among the cards on screen.
    id: SharedString,
    call: ToolCall,
    body: Option<AnyElement>,
    decision: Decision,
}

impl ToolCallCard {
    /// A card for `call` that lists its arguments and waits for a decision,
    /// until [`Self::decide`] offers one.
    pub(crate) fn new(id: impl Into<SharedString>, call: ToolCall) -> Self {
        Self {
            id: id.into(),
            call,
            body: None,
            decision: Decision::Waiting,
        }
    }

    /// Shows `body` instead of the arguments: a tool's own presentation of
    /// what the call would do.
    pub(crate) fn body(mut self, body: impl IntoElement) -> Self {
        self.body = Some(body.into_any_element());
        self
    }

    /// Offers Allow and Deny in place of the waiting note. Only for someone
    /// who may approve tool calls; the card does not check.
    pub(crate) fn decide(
        mut self,
        on_allow: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_deny: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.decision = Decision::Offered {
            on_allow: Rc::new(on_allow),
            on_deny: Rc::new(on_deny),
        };
        self
    }
}

impl RenderOnce for ToolCallCard {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let muted = cx.theme().muted_foreground;
        let title = h_flex()
            .min_w_0()
            .gap_1p5()
            .child(Icon::new(IconName::Wrench).size_3().text_color(muted))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .child(self.call.function.name.to_string()),
            )
            .child(div().truncate().text_color(muted).child("wants to run"));
        let decision = match self.decision {
            Decision::Offered { on_allow, on_deny } => {
                let deny = format!("{id}-deny");
                let allow = format!("{id}-allow");
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(deny.clone()))
                            .debug_selector(move || deny.clone())
                            .outline()
                            .xsmall()
                            .icon(Icon::new(IconName::X).text_color(cx.theme().danger))
                            .label("Deny")
                            .on_click(move |event, window, cx| on_deny(event, window, cx)),
                    )
                    .child(
                        Button::new(SharedString::from(allow.clone()))
                            .debug_selector(move || allow.clone())
                            .outline()
                            .xsmall()
                            .icon(Icon::new(IconName::Check).text_color(cx.theme().success))
                            .label("Allow")
                            .on_click(move |event, window, cx| on_allow(event, window, cx)),
                    )
                    .into_any_element()
            }
            Decision::Waiting => {
                let waiting = format!("{id}-waiting");
                h_flex()
                    .debug_selector({
                        let waiting = waiting.clone();
                        move || waiting.clone()
                    })
                    .gap_1()
                    .text_color(muted)
                    .child(Icon::new(IconName::Hourglass).size_3())
                    // Shimmering like the "Working for" line, as both are
                    // something still under way.
                    .child(
                        ShimmerText::new("Waiting for the host or an admin to allow this call…")
                            .id(SharedString::from(waiting)),
                    )
                    .into_any_element()
            }
        };
        let body = self
            .body
            .unwrap_or_else(|| arguments_view(&id, &self.call, cx));
        div().debug_selector(move || id.to_string()).child(
            GroupBox::new()
                .outline()
                .content_style(
                    StyleRefinement::default()
                        .px_2p5()
                        .py_2()
                        .gap_1p5()
                        .text_xs(),
                )
                .child(title)
                .child(body)
                .child(decision),
        )
    }
}

/// How tall the arguments may grow before they scroll: about seven lines, so
/// a call with long arguments stays a card rather than a page.
const MAX_ARGUMENTS_HEIGHT: Pixels = px(112.);

/// The call's arguments as pretty-printed JSON, in a block of their own.
fn arguments_view(id: &SharedString, call: &ToolCall, cx: &App) -> AnyElement {
    // A `Value` always encodes.
    let json = serde_json::to_string_pretty(&call.function.arguments_value()).unwrap_or_default();
    div()
        .id(SharedString::from(format!("{id}-arguments")))
        .max_h(MAX_ARGUMENTS_HEIGHT)
        .overflow_y_scroll()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(cx.theme().secondary)
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs()
        .line_height(relative(1.35))
        .text_color(cx.theme().muted_foreground)
        .child(json)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{Modifiers, Render};
    use rig::message::{CallId, ToolFunction, ToolName};
    use serde_json::json;

    use super::*;

    fn call() -> ToolCall {
        ToolCall::new(
            CallId::from_wire("call_1"),
            ToolFunction::new(
                ToolName::new("calculate").expect("a valid name"),
                json!({"a": 1, "b": 2, "operation": "add"}),
            ),
        )
    }

    struct CardView {
        offer: bool,
        allowed: Rc<Cell<u32>>,
        denied: Rc<Cell<u32>>,
    }

    impl Render for CardView {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            let card = ToolCallCard::new("card", call());
            let card = if self.offer {
                let allowed = self.allowed.clone();
                let denied = self.denied.clone();
                card.decide(
                    move |_, _, _| allowed.set(allowed.get() + 1),
                    move |_, _, _| denied.set(denied.get() + 1),
                )
            } else {
                card
            };
            div().w(px(480.)).child(card)
        }
    }

    fn render_card(
        offer: bool,
        cx: &mut gpui::TestAppContext,
    ) -> (Rc<Cell<u32>>, Rc<Cell<u32>>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let allowed = Rc::new(Cell::new(0));
        let denied = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view(|_, _| CardView {
            offer,
            allowed: allowed.clone(),
            denied: denied.clone(),
        });
        cx.run_until_parked();
        (allowed, denied, cx)
    }

    #[gpui::test]
    fn deciders_get_allow_and_deny(cx: &mut gpui::TestAppContext) {
        let (allowed, denied, cx) = render_card(true, cx);
        assert!(cx.debug_bounds("card-waiting").is_none());

        let deny = cx.debug_bounds("card-deny").expect("Deny is offered");
        cx.simulate_click(deny.center(), Modifiers::default());
        assert_eq!((allowed.get(), denied.get()), (0, 1));

        let allow = cx.debug_bounds("card-allow").expect("Allow is offered");
        assert!(deny.right() <= allow.left(), "Allow comes last");
        cx.simulate_click(allow.center(), Modifiers::default());
        assert_eq!((allowed.get(), denied.get()), (1, 1));
    }

    #[gpui::test]
    fn everyone_else_sees_that_the_call_waits(cx: &mut gpui::TestAppContext) {
        let (_, _, cx) = render_card(false, cx);
        assert!(cx.debug_bounds("card-waiting").is_some());
        assert!(cx.debug_bounds("card-allow").is_none());
        assert!(cx.debug_bounds("card-deny").is_none());
    }
}
