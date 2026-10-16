//! The local context settings and the shared context details shown by the
//! composer and the component preview. Compaction starts at the configured
//! percentage of the context window.

use gpui::{
    App, Context, Div, Entity, IntoElement, Subscription, Window, div, prelude::*, px, relative,
};
use gpui_base::input::{InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, button::Button, input::Input,
    progress::ProgressCircle,
};

use crate::{
    Cowork,
    usage::{ContextUsage, format_token_count},
};

const MIN_CONTEXT_TOKENS: u64 = 1_024;

/// App-session policy; like provider selection, this is local to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContextSettings {
    /// Requested Ollama window for local threads; mirrored in model catalogs.
    pub(crate) window_tokens: u64,
    pub(crate) threshold_percent: u8,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            window_tokens: 131_072,
            threshold_percent: 80,
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

    /// The first eligible context size, rounded up to a whole token.
    pub(crate) fn threshold(self, max_tokens: u64) -> Option<u64> {
        if max_tokens == 0 {
            return None;
        }
        let percentage = (u128::from(max_tokens) * u128::from(self.threshold_percent.clamp(1, 95)))
            .div_ceil(100);
        Some(percentage as u64)
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

/// Context usage and its marker at the configured compaction percentage.
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
        (_, Some(settings), Some(tokens)) => format!(
            "Compact at {}% · {} tokens",
            settings.threshold_percent,
            format_token_count(tokens)
        ),
        (_, Some(_), None) => "Compaction awaits a known context size".to_owned(),
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

/// Move to an adjacent power of two, including from a custom token count.
fn step_tokens(value: u64, minimum: u64, up: bool) -> u64 {
    if up {
        value
            .checked_add(1)
            .and_then(u64::checked_next_power_of_two)
            .unwrap_or(value)
            .max(minimum)
    } else {
        let previous = value
            .checked_sub(1)
            .filter(|value| *value > 0)
            .map(|value| 1u64 << value.ilog2())
            .unwrap_or(0);
        previous.max(minimum)
    }
}

/// Keep incomplete edits local; only valid counts reach the host's settings.
struct TokenInput {
    settings: ContextSettings,
    input: Entity<InputState>,
    _subscription: Subscription,
}

fn token_input<T: 'static>(
    key: &'static str,
    settings: ContextSettings,
    window: &mut Window,
    cx: &mut Context<T>,
    change: fn(&mut T, ContextSettings, &mut Context<T>),
) -> gpui::Stateful<Div> {
    let owner = cx.entity().downgrade();
    let state = window.use_keyed_state(key, cx, |window, cx| {
        let input = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.window_tokens.to_string())
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |this: &mut TokenInput, input, event, window, cx| match event {
                InputEvent::Change => {
                    if let Ok(value) = input.read(cx).value().trim().parse::<u64>()
                        && value >= MIN_CONTEXT_TOKENS
                        && value != this.settings.window_tokens
                    {
                        this.settings.window_tokens = value;
                        let _ = owner.update(cx, |owner, cx| change(owner, this.settings, cx));
                    }
                }
                InputEvent::Blur | InputEvent::PressEnter { .. } => {
                    input.update(cx, |input, cx| {
                        input.set_value(this.settings.window_tokens.to_string(), window, cx)
                    });
                }
                _ => {}
            },
        );
        TokenInput {
            settings,
            input,
            _subscription: subscription,
        }
    });
    let input = state.update(cx, |state, cx| {
        if state.settings.window_tokens != settings.window_tokens {
            state.input.update(cx, |input, cx| {
                input.set_value(settings.window_tokens.to_string(), window, cx)
            });
        }
        state.settings = settings;
        state.input.clone()
    });
    div()
        .id(key)
        .debug_selector(move || key.to_owned())
        .child(Input::new(&input).small().w(px(112.)))
}

pub(crate) fn controls<T: 'static>(
    settings: ContextSettings,
    window: &mut Window,
    cx: &mut Context<T>,
    change: fn(&mut T, ContextSettings, &mut Context<T>),
) -> Div {
    let window_down = ContextSettings {
        window_tokens: step_tokens(settings.window_tokens, MIN_CONTEXT_TOKENS, false),
        ..settings
    };
    let window_up = ContextSettings {
        window_tokens: step_tokens(settings.window_tokens, MIN_CONTEXT_TOKENS, true),
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

    let window_input = token_input("context-window-input", settings, window, cx, change);

    let stepper =
        |key: &'static str, value: gpui::AnyElement, down, up, disabled_down, disabled_up| {
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
            "Enter a token count (at least 1,024), or step through powers of two.",
            stepper(
                "context-window",
                window_input.into_any_element(),
                window_down,
                window_up,
                settings.window_tokens <= MIN_CONTEXT_TOKENS,
                window_up.window_tokens == settings.window_tokens,
            ),
        ))
        .child(row(
            "Compact at",
            "Summarize before the next response when this limit is reached.",
            stepper(
                "compaction-percent",
                div()
                    .child(format!("{}%", settings.threshold_percent))
                    .into_any_element(),
                percentage_down,
                percentage_up,
                settings.threshold_percent <= 5,
                settings.threshold_percent >= 95,
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

    pub(crate) fn render_context_page(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = self.context_settings;
        div().id("context-page").debug_selector(|| "context-page".to_owned())
            .flex_1().min_h_0().min_w_0().rounded_tl(px(12.)).border_t_1().border_l_1()
            .border_color(cx.theme().border).bg(cx.theme().background).overflow_y_scroll()
            .child(div().mx_auto().max_w(px(720.)).p_8().flex().flex_col().gap_4()
                .child(div().text_lg().child("Context"))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child("Choose how much context the agent can use and when long conversations are summarized."))
                .child(controls(settings, window, cx, Self::set_context_settings))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child("Changes apply for this app session. The window applies to new and existing local chats from their next agent run. Shared chats use their host’s window. Compaction starts when context usage reaches the selected percentage."))
                .child(div().pt_4().text_sm().child(format!("Example · {}-token window", settings.window_tokens)))
                .child(context_details(settings.example_usage(60), Some(settings), false, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_steps_use_adjacent_powers_and_respect_bounds() {
        for (value, down, up) in [
            (0, 0, 1),
            (1, 0, 2),
            (65_536, 32_768, 131_072),
            (100_000, 65_536, 131_072),
            (131_072, 65_536, 262_144),
            (1u64 << 63, 1u64 << 62, 1u64 << 63),
            (u64::MAX, 1u64 << 63, u64::MAX),
        ] {
            assert_eq!(step_tokens(value, 0, false), down);
            assert_eq!(step_tokens(value, 0, true), up);
        }
        assert_eq!(step_tokens(1_024, MIN_CONTEXT_TOKENS, false), 1_024);
        assert_eq!(step_tokens(1_025, MIN_CONTEXT_TOKENS, false), 1_024);
        assert_eq!(step_tokens(1_024, MIN_CONTEXT_TOKENS, true), 2_048);
        assert_eq!(step_tokens(2_048, MIN_CONTEXT_TOKENS, false), 1_024);
    }

    #[test]
    fn threshold_uses_the_percentage_even_for_small_windows() {
        let settings = ContextSettings::default();
        assert_eq!(settings.threshold(131_072), Some(104_858));
        assert_eq!(settings.threshold(81_920), Some(65_536));
        assert_eq!(settings.threshold(65_536), Some(52_429));
        assert_eq!(settings.threshold(32_768), Some(26_215));
        assert_eq!(settings.threshold(1_024), Some(820));
        assert_eq!(settings.threshold(0), None);
        let earlier = ContextSettings {
            threshold_percent: 20,
            ..settings
        };
        assert_eq!(earlier.threshold(131_072), Some(26_215));
        assert_eq!(earlier.threshold(1_024), Some(205));
    }
}
