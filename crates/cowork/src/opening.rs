//! The main stage while a chat opened from its saved row loads (see
//! `chat_store.rs`): a placeholder shaped like a chat, its bottom bar
//! included so nothing moves in when the chat replaces it, or what to do
//! once the load failed. Only the lines of text shimmer; avatars and
//! controls are still shapes.
//!
//! Most loads take milliseconds, so the stage shown before stays for
//! [`PLACEHOLDER_DELAY`] and the chat simply replaces it; only a slower
//! load shows the placeholder, which would otherwise flash on every open.
//! Once shown it stays at least [`PLACEHOLDER_MIN_SHOWN`], so a load ending
//! just after it appeared doesn't flash it either (the timing Android's
//! `ContentLoadingProgressBar` uses). The sidebar selects the chat from the
//! click on, and the composer shown before loses focus then, so typing
//! can't reach the chat being left. A failed load shows at once, whatever
//! the stage, as a finished one does, and is announced as an alert with
//! keyboard focus on its retry.

use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, DefiniteLength, FocusHandle,
    FontWeight, IntoElement, Pixels, Role, Task, Window, div, linear_color_stop, linear_gradient,
    prelude::*, px, relative,
};
use gpui_component::{ActiveTheme as _, Icon, button::Button, v_flex};
use gpui_kit_assets::IconName as AssetIconName;

use crate::{Cowork, MainStage, chat_store::OpeningChat, top_bar::TOP_BAR_HEIGHT};

pub(crate) const PLACEHOLDER_DELAY: Duration = Duration::from_millis(200);
pub(crate) const PLACEHOLDER_MIN_SHOWN: Duration = Duration::from_millis(500);

/// How long the highlight takes to sweep across the placeholder once.
const SHIMMER_PERIOD: Duration = Duration::from_millis(1600);
/// The highlight's width, and how far it travels: past the widest line, so
/// every line's highlight is in the same place at once.
const SHIMMER_WIDTH: Pixels = px(240.);
const SHIMMER_TRAVEL: Pixels = px(1200.);

/// The placeholder's messages, alternating the user's and the agent's as
/// a chat does, each as the widths of its lines.
const PLACEHOLDER_MESSAGES: [&[f32]; 4] =
    [&[0.46], &[1., 0.92, 0.58], &[0.3], &[1., 0.86, 0.95, 0.36]];
/// As `render_avatar` draws both authors'.
const AVATAR_SIZE: Pixels = px(22.);

/// What the opening stage keeps between frames.
#[derive(Default)]
pub(crate) struct OpeningStage {
    /// Shows the placeholder once the load has taken [`PLACEHOLDER_DELAY`].
    reveal: Option<Task<()>>,
    /// When the placeholder appeared, by the executor's clock.
    shown_at: Option<Instant>,
    /// Shows the loaded chat once the placeholder has had
    /// [`PLACEHOLDER_MIN_SHOWN`]; until then it stands in for the chat,
    /// already active.
    hold: Option<Task<()>>,
    pub(crate) retry_focus: Option<FocusHandle>,
}

impl Cowork {
    /// After the store started loading the chat it is opening, shows the
    /// placeholder if the load is still running after the delay and
    /// nothing else was shown meanwhile.
    pub(crate) fn await_opening(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.blur(cx);
        cx.notify();
        self.opening_stage.hold = None;
        if self.main_stage == MainStage::Opening {
            // Another chat, or a retry: the placeholder shows again at once.
            self.opening_stage.shown_at = Some(cx.background_executor().now());
            return;
        }
        let Some(id) = self.thread_store.read(cx).opening_id(cx) else {
            return;
        };
        let from = self.main_stage;
        self.opening_stage.reveal = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PLACEHOLDER_DELAY).await;
            _ = this.update(cx, |this, cx| {
                this.opening_stage.reveal = None;
                if this.main_stage == from && this.thread_store.read(cx).opening_id(cx) == Some(id)
                {
                    this.main_stage = MainStage::Opening;
                    this.opening_stage.shown_at = Some(cx.background_executor().now());
                    cx.notify();
                }
            });
        }));
    }

    /// Shows the chat that was being opened, now active, once the
    /// placeholder has been up long enough.
    pub(crate) fn show_opened_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let remaining = self
            .opening_stage
            .shown_at
            .filter(|_| self.main_stage == MainStage::Opening)
            .and_then(|shown_at| (shown_at + PLACEHOLDER_MIN_SHOWN).checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero());
        let Some(remaining) = remaining else {
            self.show_active_thread(window, cx);
            return;
        };
        self.opening_stage.hold = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(remaining).await;
            _ = this.update_in(cx, |this, window, cx| this.show_active_thread(window, cx));
        }));
    }

    /// Ends any opening when a chat is shown, whichever way it was.
    pub(crate) fn end_opening(&mut self) {
        self.opening_stage.reveal = None;
        self.opening_stage.hold = None;
        self.opening_stage.shown_at = None;
    }

    pub(crate) fn show_failed_opening(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.opening_stage.reveal = None;
        self.opening_stage.shown_at = None;
        self.main_stage = MainStage::Opening;
        self.opening_stage
            .retry_focus
            .get_or_insert_with(|| cx.focus_handle())
            .focus(window, cx);
        cx.notify();
    }

    /// What the opening stage shows: the chat being opened, or while the
    /// placeholder is held, the chat that just loaded.
    pub(crate) fn shown_opening(&self, cx: &App) -> Option<OpeningChat> {
        let store = self.thread_store.read(cx);
        store.opening(cx).or_else(|| {
            self.opening_stage.hold.as_ref()?;
            let thread = store.active(cx)?;
            let thread = thread.read(cx);
            Some(OpeningChat {
                id: thread.instance_id,
                title: thread.summary.title.clone(),
                failed: false,
            })
        })
    }

    pub(crate) fn render_opening(
        &self,
        opening: OpeningChat,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let failed = opening.failed;
        let content = if failed {
            self.render_failed_opening(opening, cx)
        } else {
            render_placeholder(opening, window, cx)
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div()
                    .id("opening-chat")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .rounded_tl(px(12.))
                    .border_t_1()
                    .border_l_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(content),
            )
            .child(render_placeholder_bottom_bar(!failed, cx))
            .into_any_element()
    }

    fn render_failed_opening(&self, opening: OpeningChat, cx: &mut Context<Self>) -> AnyElement {
        let id = opening.id;
        let title = format!("Couldn't open “{}”", opening.title);
        let description = "Its saved messages couldn't be read.";
        v_flex()
            .id("opening-failed")
            .debug_selector(|| "opening-failed".to_owned())
            .role(Role::Alert)
            .aria_label(title.clone())
            .aria_description(description)
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .px_6()
            .child(
                Icon::new(AssetIconName::TriangleAlert)
                    .size_8()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .max_w(px(480.))
                    .truncate()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(description),
            )
            .child(
                Button::new("retry-opening")
                    .outline()
                    .mt_2()
                    .icon(Icon::new(AssetIconName::RotateCcw))
                    .label("Try again")
                    .debug_selector(|| "retry-opening".to_owned())
                    .when_some(self.opening_stage.retry_focus.as_ref(), |button, focus| {
                        button.track_focus(focus)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_thread(id, window, cx);
                    })),
            )
            .into_any_element()
    }
}

/// Rows shaped like the chat's messages, an avatar and lines of text each.
fn render_placeholder(opening: OpeningChat, window: &Window, cx: &App) -> AnyElement {
    let mut line = 0;
    let messages = PLACEHOLDER_MESSAGES.iter().map(|widths| {
        let lines = widths.iter().map(|width| {
            line += 1;
            skeleton_block(line, relative(*width), px(14.), cx)
        });
        div()
            .w_full()
            .flex()
            .items_start()
            .child(
                div().w(px(40.)).flex_none().flex().justify_center().child(
                    div()
                        .size(AVATAR_SIZE)
                        .rounded_full()
                        .bg(cx.theme().skeleton),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    // The first line centered on the avatar.
                    .pt(px(4.))
                    .gap(px(10.))
                    .children(lines.collect::<Vec<_>>()),
            )
            .child(div().w(px(40.)).flex_none())
    });
    v_flex()
        .id("opening-placeholder")
        .debug_selector(|| "opening-placeholder".to_owned())
        .role(Role::ProgressIndicator)
        .aria_label(format!("Opening “{}”", opening.title))
        .w_full()
        .max_w(crate::timeline_row_width(window, cx))
        .mx_auto()
        .pt_6()
        .gap_8()
        .children(messages.collect::<Vec<_>>())
        .into_any_element()
}

/// The bottom bar as `render_bottom_bar` lays it out, with a shape where
/// each of its controls goes: the project folders, then the context
/// indicator, project mode, model and send button. Only the chat's own
/// would be true, and they are part of what is loading, so there are none
/// to show or use; after a failed load the bar stays, empty.
fn render_placeholder_bottom_bar(loading: bool, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let shape = |width: Pixels, height: Pixels, round: bool| {
        div()
            .flex_none()
            .w(width)
            .h(height)
            .when(round, |this| this.rounded_full())
            .when(!round, |this| this.rounded(theme.radius))
            .bg(theme.skeleton)
    };
    div()
        .id("opening-bottom-bar")
        .debug_selector(|| "opening-bottom-bar".to_owned())
        .h(TOP_BAR_HEIGHT)
        .w_full()
        .flex_none()
        .border_l_1()
        .border_color(theme.border)
        .bg(theme.background)
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .when(loading, |this| {
            this.child(shape(px(18.), px(16.), false))
                .child(div().flex_1())
                .child(shape(px(16.), px(16.), true))
                .child(shape(px(52.), px(14.), false))
                .child(shape(px(120.), px(14.), false))
                .child(shape(px(24.), px(24.), false))
        })
}

/// A line of the placeholder, with a highlight sweeping across it in step
/// with every other line's; still when the system asks for reduced motion.
fn skeleton_block(index: usize, width: DefiniteLength, height: Pixels, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let block = div()
        .relative()
        .overflow_hidden()
        .w(width)
        .h(height)
        .rounded(theme.radius)
        .bg(theme.skeleton);
    if cx.reduce_motion() {
        return block.into_any_element();
    }
    let highlight = theme.foreground.opacity(0.07);
    let clear = highlight.opacity(0.);
    let half = SHIMMER_WIDTH / 2.;
    let band = div()
        .absolute()
        .top_0()
        .bottom_0()
        .w(SHIMMER_WIDTH)
        .flex()
        .child(div().w(half).h_full().bg(linear_gradient(
            90.,
            linear_color_stop(clear, 0.),
            linear_color_stop(highlight, 1.),
        )))
        .child(div().w(half).h_full().bg(linear_gradient(
            90.,
            linear_color_stop(highlight, 0.),
            linear_color_stop(clear, 1.),
        )))
        .with_animation(
            ("opening-shimmer", index),
            Animation::new(SHIMMER_PERIOD).repeat_synced(),
            |band, phase| band.left(-SHIMMER_WIDTH + (SHIMMER_TRAVEL + SHIMMER_WIDTH) * phase),
        );
    block.child(band).into_any_element()
}
