//! The new-thread welcome stage and its direct model choices.

use gpui::{Context, IntoElement, div, img, prelude::*, px, rgb};
use gpui_component::tooltip::Tooltip;

use crate::{Cowork, MainStage, models::ModelRef, usage::format_token_count};

impl Cowork {
    /// Shows the available models before opening the first new chat.
    pub(crate) fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let catalog = self.active_catalog(cx);
        let selected = self.active_model(cx);
        let can_continue = self.active_model_is_runnable(cx);
        let models = catalog
            .providers()
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
        let greeting = self
            .profile
            .name
            .as_ref()
            .map(|name| format!("Welcome back, {name}"))
            .unwrap_or_else(|| "Welcome to Cowork".into());

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
                    .child(
                        div()
                            .text_2xl()
                            .text_color(rgb(0xf4f4f5))
                            .child(greeting),
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
                                    format!("welcome-model-{}-{}", provider.label(), model.id);
                                let full_name = name.clone();
                                let context = format!(
                                    "{} context tokens",
                                    format_token_count(max_tokens)
                                );
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
                            .when(catalog.providers().next().is_none(), |this| {
                                this.child(
                                    div()
                                        .w_full()
                                        .p_5()
                                        .rounded_lg()
                                        .border_1()
                                        .border_color(rgb(0x3f3f46))
                                        .text_color(rgb(0xa1a1aa))
                                        .child("No models found. Start Ollama and reopen Cowork to discover your models."),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("welcome-continue")
                            .debug_selector(|| "welcome-continue".to_owned())
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
}
