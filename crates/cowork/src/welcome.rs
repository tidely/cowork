//! Provider selection and provider-specific setup before the first chat.

use gpui::{Context, IntoElement, div, img, prelude::*, px, rgb};
use gpui_component::tooltip::Tooltip;

use crate::{
    Cowork, MainStage, ProviderSetupStage,
    models::{ModelProvider, ModelRef},
    usage::format_token_count,
};

impl Cowork {
    /// Shows the available models for one selected provider.
    pub(crate) fn render_provider_setup(
        &self,
        provider_setup: ProviderSetupStage,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let provider_id = match provider_setup {
            ProviderSetupStage::Ollama => ModelProvider::Ollama,
        };
        let catalog = self.active_catalog(cx);
        let selected = self.active_model(cx);
        let can_continue = self.active_model_is_runnable(cx);
        let models = catalog
            .providers()
            .filter(|(provider, _)| *provider == provider_id)
            .flat_map(|(provider, models)| {
                models.iter().map(move |(id, info)| {
                    (
                        ModelRef {
                            provider,
                            id: id.clone(),
                        },
                        info.name.clone(),
                        info.max_tokens,
                    )
                })
            })
            .collect::<Vec<_>>();
        let has_models = !models.is_empty();

        div()
            .id("provider-setup-stage")
            .debug_selector(|| "provider-setup-stage".to_owned())
            .flex_1()
            .min_h_0()
            .w_full()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(rgb(0x2d2d30))
            .bg(rgb(0x18181b))
            .px_6()
            .py_8()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(px(760.))
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_2xl()
                            .text_color(rgb(0xf4f4f5))
                            .child(format!("Set up {}", provider_id.label())),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_sm()
                            .text_color(rgb(0xa1a1aa))
                            .child("Choose a model to start your first chat."),
                    )
                    .child(
                        div()
                            .mt_8()
                            .mb_3()
                            .w_full()
                            .text_xs()
                            .text_color(rgb(0x71717a))
                            .child("AVAILABLE MODELS"),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_3()
                            .children(models.into_iter().map(|(model, name, max_tokens)| {
                                let is_selected = selected.as_ref() == Some(&model);
                                let provider = model.provider;
                                let card_selector =
                                    format!("setup-model-{}-{}", provider.label(), model.id);
                                let full_name = name.clone();
                                let context =
                                    format!("{} context tokens", format_token_count(max_tokens));
                                div()
                                    .id(card_selector.clone())
                                    .debug_selector(move || card_selector.clone())
                                    .w(px(238.))
                                    .h(px(92.))
                                    .p_4()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(if is_selected {
                                        rgb(0x8b8bf0)
                                    } else {
                                        rgb(0x3f3f46)
                                    })
                                    .bg(if is_selected {
                                        rgb(0x272738)
                                    } else {
                                        rgb(0x202023)
                                    })
                                    .cursor_pointer()
                                    .hover(|this| this.bg(rgb(0x303036)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_model(model.clone(), cx);
                                    }))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(full_name.clone()).build(window, cx)
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                img(provider.icon_path())
                                                    .size(px(24.))
                                                    .flex_none()
                                                    .rounded(px(5.)),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .truncate()
                                                    .text_color(rgb(0xe4e4e7))
                                                    .child(name),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .mt_3()
                                            .text_xs()
                                            .text_color(rgb(0x8b8b94))
                                            .child(context),
                                    )
                            }))
                            .when(!has_models, |this| {
                                this.child(
                                    div()
                                        .w_full()
                                        .p_5()
                                        .rounded_lg()
                                        .border_1()
                                        .border_color(rgb(0x3f3f46))
                                        .text_color(rgb(0xa1a1aa))
                                        .child(
                                            "No Ollama models found. Start Ollama, then try again.",
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .mt_6()
                            .flex()
                            .gap_4()
                            .child(
                                div()
                                    .id("setup-back")
                                    .debug_selector(|| "setup-back".to_owned())
                                    .cursor_pointer()
                                    .text_color(rgb(0xa1a1aa))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.main_stage = MainStage::Welcome;
                                        cx.notify();
                                    }))
                                    .child("Back"),
                            )
                            .when(!has_models, |this| {
                                this.child(
                                    div()
                                        .id("setup-retry")
                                        .debug_selector(|| "setup-retry".to_owned())
                                        .cursor_pointer()
                                        .text_color(rgb(0xe4e4e7))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.discover_models(window, cx);
                                        }))
                                        .child("Try again"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("setup-continue")
                            .debug_selector(|| "setup-continue".to_owned())
                            .mt_8()
                            .px_6()
                            .py_3()
                            .rounded_lg()
                            .bg(if can_continue {
                                rgb(0xe4e4e7)
                            } else {
                                rgb(0x3f3f46)
                            })
                            .text_color(if can_continue {
                                rgb(0x18181b)
                            } else {
                                rgb(0x8b8b94)
                            })
                            .when(can_continue, |this| {
                                this.cursor_pointer().hover(|this| this.bg(rgb(0xffffff)))
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.active_model_is_runnable(cx) {
                                    this.main_stage = MainStage::Thread;
                                    this.focus_composer(window, cx);
                                    cx.notify();
                                }
                            }))
                            .child("Continue"),
                    ),
            )
    }

    /// Selects a provider before opening its own setup page.
    pub(crate) fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let greeting = self
            .profile
            .name
            .as_ref()
            .map(|name| format!("Welcome back, {name}"))
            .unwrap_or_else(|| "Welcome to Cowork".into());
        let selected = self.selected_welcome_provider;

        div()
            .id("welcome-stage")
            .debug_selector(|| "welcome-stage".to_owned())
            .flex_1()
            .min_h_0()
            .w_full()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(rgb(0x2d2d30))
            .bg(rgb(0x18181b))
            .px_6()
            .py_8()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(px(760.))
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(div().text_2xl().text_color(rgb(0xf4f4f5)).child(greeting))
                    .child(
                        div()
                            .mt_2()
                            .text_sm()
                            .text_color(rgb(0xa1a1aa))
                            .child("Choose a provider to start your first chat."),
                    )
                    .child(
                        div()
                            .mt_8()
                            .mb_3()
                            .w_full()
                            .text_xs()
                            .text_color(rgb(0x71717a))
                            .child("AVAILABLE PROVIDERS"),
                    )
                    .child(div().w_full().flex().flex_wrap().gap_3().children(
                        ModelProvider::ALL.into_iter().map(|provider| {
                            let is_selected = selected == Some(provider);
                            let card_selector = format!("welcome-provider-{}", provider.label());
                            div()
                                .id(card_selector.clone())
                                .debug_selector(move || card_selector.clone())
                                .w(px(238.))
                                .h(px(92.))
                                .p_4()
                                .rounded_lg()
                                .border_1()
                                .border_color(if is_selected {
                                    rgb(0x8b8bf0)
                                } else {
                                    rgb(0x3f3f46)
                                })
                                .bg(if is_selected {
                                    rgb(0x272738)
                                } else {
                                    rgb(0x202023)
                                })
                                .cursor_pointer()
                                .hover(|this| this.bg(rgb(0x303036)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected_welcome_provider = Some(provider);
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            img(provider.icon_path())
                                                .size(px(24.))
                                                .flex_none()
                                                .rounded(px(5.)),
                                        )
                                        .child(
                                            div().text_color(rgb(0xe4e4e7)).child(provider.label()),
                                        ),
                                )
                                .child(
                                    div()
                                        .mt_3()
                                        .text_xs()
                                        .text_color(rgb(0x8b8b94))
                                        .child("Run models locally"),
                                )
                        }),
                    ))
                    .child(
                        div()
                            .id("welcome-continue")
                            .debug_selector(|| "welcome-continue".to_owned())
                            .mt_8()
                            .px_6()
                            .py_3()
                            .rounded_lg()
                            .bg(if selected.is_some() {
                                rgb(0xe4e4e7)
                            } else {
                                rgb(0x3f3f46)
                            })
                            .text_color(if selected.is_some() {
                                rgb(0x18181b)
                            } else {
                                rgb(0x8b8b94)
                            })
                            .when(selected.is_some(), |this| {
                                this.cursor_pointer().hover(|this| this.bg(rgb(0xffffff)))
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(ModelProvider::Ollama) = this.selected_welcome_provider
                                {
                                    this.main_stage =
                                        MainStage::ProviderSetup(ProviderSetupStage::Ollama);
                                    this.discover_models(window, cx);
                                    cx.notify();
                                }
                            }))
                            .child("Continue"),
                    ),
            )
    }
}
