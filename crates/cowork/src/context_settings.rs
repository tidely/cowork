//! The local context settings and the shared context details shown by the
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
pub(crate) struct ContextSettings {
    /// Requested Ollama window for local threads; mirrored in model catalogs.
    pub(crate) window_tokens: u64,
    pub(crate) threshold_percent: u8,
    pub(crate) min_tokens: u64,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            window_tokens: 131_072,
            threshold_percent: 80,
            min_tokens: 65_536,
        }
    }
}

impl ContextSettings {
    /// Preview the selected window without a provider or running conversation.
    pub(crate) fn example_usage(self, percent: u8) -> ContextUsage {
        ContextUsage {
            tokens: (u128::from(self.window_tokens) * u128::from(percent) / 100) as u64,
            max_tokens: self.window_tokens,
        }
    }

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
    settings: Option<ContextSettings>,
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
    settings: ContextSettings,
    cx: &Context<T>,
    change: fn(&mut T, ContextSettings, &mut Context<T>),
) -> Div {
    let window_down = ContextSettings {
        window_tokens: settings.window_tokens.saturating_sub(8_192).max(8_192),
        ..settings
    };
    let window_up = ContextSettings {
        window_tokens: settings.window_tokens.saturating_add(8_192),
        ..settings
    };
    let percentage_down = ContextSettings {
        threshold_percent: settings.threshold_percent.saturating_sub(5).max(5),
        ..settings
    };
    let percentage_up = ContextSettings {
        threshold_percent: settings.threshold_percent.saturating_add(5).min(95),
        ..settings
    };
    let minimum_down = ContextSettings {
        min_tokens: settings.min_tokens.saturating_sub(8_192),
        ..settings
    };
    let minimum_up = ContextSettings {
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
            "Default context window",
            "Tokens available to the agent. Adjust in steps of 8,192.",
            stepper(
                "context-window",
                settings.window_tokens.to_string(),
                window_down,
                window_up,
                settings.window_tokens <= 8_192,
                settings.window_tokens == u64::MAX,
            ),
        ))
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
    /// Update the host's catalogs through their existing sharing path. Runs
    /// already in progress keep the window and threshold captured at startup.
    pub(crate) fn set_context_settings(
        &mut self,
        settings: ContextSettings,
        cx: &mut Context<Self>,
    ) {
        let window_changed = settings.window_tokens != self.context_settings.window_tokens;
        self.context_settings = settings;
        if window_changed {
            let mut catalog = (*self.models).clone();
            catalog.set_context_window(settings.window_tokens);
            self.set_models(catalog, cx);
        }
        cx.notify();
    }

    pub(crate) fn render_context_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.context_settings;
        div().id("context-page").debug_selector(|| "context-page".to_owned())
            .flex_1().min_h_0().min_w_0().rounded_tl(px(12.)).border_t_1().border_l_1()
            .border_color(cx.theme().border).bg(cx.theme().background).overflow_y_scroll()
            .child(div().mx_auto().max_w(px(720.)).p_8().flex().flex_col().gap_4()
                .child(div().text_lg().child("Context"))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child("Choose how much context the agent can use and when long conversations are summarized."))
                .child(controls(settings, cx, Self::set_context_settings))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child("Changes apply for this app session. The window applies to new and existing local chats from their next agent run. Shared chats use their host’s window. Both compaction limits must be reached; windows at or below the minimum cannot compact."))
                .child(div().pt_4().text_sm().child(format!("Example · {}-token window", settings.window_tokens)))
                .child(context_details(settings.example_usage(60), Some(settings), false, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examples_and_compaction_follow_the_selected_window() {
        let mut settings = ContextSettings {
            window_tokens: 32_768,
            min_tokens: 8_192,
            ..Default::default()
        };
        let usage = settings.example_usage(60);
        assert_eq!(usage.max_tokens, 32_768);
        assert_eq!(usage.tokens, 19_660);
        assert_eq!(settings.threshold(usage.max_tokens), Some(26_215));

        settings.window_tokens = 8_192;
        assert_eq!(settings.example_usage(60).max_tokens, 8_192);
        assert_eq!(settings.threshold(settings.window_tokens), None);
    }

    #[test]
    fn threshold_obeys_the_minimum_and_leaves_headroom() {
        let settings = ContextSettings::default();
        assert_eq!(settings.threshold(131_072), Some(104_858));
        assert_eq!(settings.threshold(81_920), Some(65_536));
        assert_eq!(settings.threshold(65_536), None);
        assert_eq!(settings.threshold(32_768), None);
        assert_eq!(settings.threshold(0), None);
        let earlier = ContextSettings {
            threshold_percent: 20,
            ..settings
        };
        assert_eq!(earlier.threshold(131_072), Some(65_536));
    }
}
