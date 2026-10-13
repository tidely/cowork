//! The local compaction policy and the shared context details shown by the
//! composer and the component preview. The minimum is a floor on context
//! usage, so lowering the percentage cannot compact a small conversation.

use gpui::{App, Context, Div, IntoElement, div, prelude::*, px, relative};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, button::Button, progress::ProgressCircle,
};

use crate::{
    Cowork,
    usage::{ContextUsage, format_token_count},
};

/// App-session policy; like provider selection, this is local to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompactionSettings {
    pub(crate) threshold_percent: u8,
    pub(crate) min_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            threshold_percent: 80,
            min_tokens: 65_536,
        }
    }
}

impl CompactionSettings {
    /// The first eligible context size. A minimum filling the entire window
    /// disables compaction rather than leaving no room for its summary.
    pub(crate) fn threshold(self, max_tokens: u64) -> Option<u64> {
        if max_tokens == 0 || self.min_tokens >= max_tokens {
            return None;
        }
        let percentage = (u128::from(max_tokens) * u128::from(self.threshold_percent.clamp(1, 95)))
            .div_ceil(100);
        Some((percentage as u64).max(self.min_tokens))
    }
}

/// The same small context ring used beside the model picker.
pub(crate) fn context_ring(
    usage: ContextUsage,
    compacting: bool,
    cx: &App,
) -> impl IntoElement + use<> {
    ProgressCircle::new("context-ring")
        .value(usage.percent())
        .loading(compacting)
        .color(usage.color(cx.theme()))
        .accessibility_label(if compacting {
            "Compacting conversation".to_owned()
        } else {
            format!("Context window {:.0}% full", usage.percent())
        })
        .size(px(16.))
}

/// Context usage and its fixed compaction marker. The marker reflects both
/// settings, including cases where the minimum delays the chosen percentage.
pub(crate) fn context_details(
    usage: ContextUsage,
    settings: Option<CompactionSettings>,
    compacting: bool,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let threshold = settings.and_then(|settings| settings.threshold(usage.max_tokens));
    let trigger = threshold.map(|tokens| tokens as f32 / usage.max_tokens as f32);
    let status = match (compacting, settings, threshold) {
        (true, _, _) => "Compacting conversation…".to_owned(),
        (_, None, _) => "Host controls compaction".to_owned(),
        (_, Some(settings), Some(tokens)) => {
            let effective = trigger.unwrap() * 100.;
            if tokens == settings.min_tokens && effective > f32::from(settings.threshold_percent) {
                format!(
                    "Compact at {:.0}% · minimum {} (set to {}%)",
                    effective,
                    format_token_count(tokens),
                    settings.threshold_percent
                )
            } else {
                format!(
                    "Compact at {}% · {} tokens",
                    settings.threshold_percent,
                    format_token_count(tokens)
                )
            }
        }
        (_, _, None) if usage.max_tokens == 0 => {
            "Compaction awaits a known context size".to_owned()
        }
        (_, Some(settings), None) => format!(
            "Compaction off · minimum {} is at or above this window",
            format_token_count(settings.min_tokens)
        ),
    };
    div()
        .w(px(260.))
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_color(theme.muted_foreground).child("Context"))
        .child(
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .text_color(theme.secondary_foreground)
                .child(format!("{:.0}%", usage.percent()))
                .child(
                    div()
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child("·"),
                )
                .child(format_token_count(usage.tokens))
                .child(
                    div()
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(format!("/ {}", format_token_count(usage.max_tokens))),
                ),
        )
        .child(
            div()
                .relative()
                .h(px(14.))
                .w_full()
                .child(
                    div()
                        .absolute()
                        .top(px(4.))
                        .h(px(6.))
                        .w_full()
                        .rounded_full()
                        .bg(theme.border)
                        .child(
                            div()
                                .h_full()
                                .w(relative((usage.percent() / 100.).clamp(0., 1.)))
                                .rounded_full()
                                .bg(usage.color(theme)),
                        ),
                )
                .children(trigger.map(|fraction| {
                    div()
                        .absolute()
                        .left(relative(fraction))
                        .top_0()
                        .w(px(2.))
                        .h_full()
                        .bg(theme.primary)
                })),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(status),
        )
}

/// Small steppers keep the first UI iteration immediately editable without
/// giving the context tooltip an accidental drag interaction.
pub(crate) fn controls<T: 'static>(
    settings: CompactionSettings,
    cx: &Context<T>,
    change: fn(&mut T, CompactionSettings, &mut Context<T>),
) -> Div {
    let percentage_down = CompactionSettings {
        threshold_percent: settings.threshold_percent.saturating_sub(5).max(5),
        ..settings
    };
    let percentage_up = CompactionSettings {
        threshold_percent: settings.threshold_percent.saturating_add(5).min(95),
        ..settings
    };
    let minimum_down = CompactionSettings {
        min_tokens: settings.min_tokens.saturating_sub(8_192),
        ..settings
    };
    let minimum_up = CompactionSettings {
        min_tokens: settings.min_tokens.saturating_add(8_192),
        ..settings
    };
    let stepper = |key: &'static str, value: String, down, up, disabled_down, disabled_up| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new((key, 0usize))
                    .label("−")
                    .small()
                    .outline()
                    .disabled(disabled_down)
                    .on_click(cx.listener(move |this, _, _, cx| change(this, down, cx))),
            )
            .child(div().min_w(px(92.)).text_center().text_sm().child(value))
            .child(
                Button::new((key, 1usize))
                    .label("+")
                    .small()
                    .outline()
                    .disabled(disabled_up)
                    .on_click(cx.listener(move |this, _, _, cx| change(this, up, cx))),
            )
    };
    let row = |title: &'static str, description: &'static str, control: Div| {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_6()
            .py_5()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().secondary_foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(description),
                    ),
            )
            .child(control)
    };
    div()
        .flex()
        .flex_col()
        .child(row(
            "Compact at",
            "Summarize before the next response when this limit is reached.",
            stepper(
                "compaction-percent",
                format!("{}%", settings.threshold_percent),
                percentage_down,
                percentage_up,
                settings.threshold_percent <= 5,
                settings.threshold_percent >= 95,
            ),
        ))
        .child(row(
            "Minimum context",
            "Wait until this many tokens are in use. Adjust in steps of 8,192.",
            stepper(
                "compaction-minimum",
                settings.min_tokens.to_string(),
                minimum_down,
                minimum_up,
                settings.min_tokens == 0,
                settings.min_tokens == u64::MAX,
            ),
        ))
}

impl Cowork {
    pub(crate) fn render_compaction_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.compaction_settings;
        div().id("compaction-page").debug_selector(|| "compaction-page".to_owned())
            .flex_1().min_h_0().min_w_0().rounded_tl(px(12.)).border_t_1().border_l_1()
            .border_color(cx.theme().border).bg(cx.theme().background).overflow_y_scroll()
            .child(div().mx_auto().max_w(px(720.)).p_8().flex().flex_col().gap_4()
                .child(div().text_lg().child("Compaction"))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child("Summarize long conversations so the agent can keep working. The summary carries the details into a fresh context."))
                .child(controls(settings, cx, |this, settings, cx| { this.compaction_settings = settings; cx.notify(); }))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child("Both limits must be reached. Compaction stays off for context windows at or below the minimum. Changes apply for this app session."))
                .child(div().pt_4().text_sm().child("Example · 131,072-token window"))
                .child(context_details(ContextUsage { tokens: 78_643, max_tokens: 131_072 }, Some(settings), false, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_obeys_the_minimum_and_leaves_headroom() {
        let settings = CompactionSettings::default();
        assert_eq!(settings.threshold(131_072), Some(104_858));
        assert_eq!(settings.threshold(81_920), Some(65_536));
        assert_eq!(settings.threshold(65_536), None);
        assert_eq!(settings.threshold(32_768), None);
        assert_eq!(settings.threshold(0), None);
        let earlier = CompactionSettings {
            threshold_percent: 20,
            ..settings
        };
        assert_eq!(earlier.threshold(131_072), Some(65_536));
    }
}
