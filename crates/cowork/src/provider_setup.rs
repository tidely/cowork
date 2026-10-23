//! Choosing an LLM provider, then one of its models: shown by the welcome
//! screen before the first chat (`welcome.rs`), and by the Providers settings
//! page without the welcome text (`settings.rs`). Each passes its own intro
//! and closing button; the steps, cards, and buttons between them are here.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, IntoElement, SharedString, Stateful, div, img, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
    tooltip::Tooltip,
};

use crate::{
    Cowork, MainStage,
    models::{ModelCatalog, ModelProvider, ModelRef},
    usage::format_token_count,
};

/// Which step of setting up a provider is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProviderStep {
    /// The providers, one to pick before continuing to its models.
    #[default]
    Providers,
    /// The models `ModelProvider` offers, discovered when the step opens.
    Models(ModelProvider),
}

/// Where the steps are shown, which decides what the user is choosing a
/// model for and where moving between the steps goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderSetupPlace {
    /// The center stage before the first chat, with the steps as
    /// [`MainStage::Welcome`] and [`MainStage::ProviderSetup`]. Models are
    /// picked for the active thread, or the one about to be created.
    Welcome,
    /// The Providers settings page, with the step in
    /// `Cowork::settings_provider_step`. Models are picked for new chats,
    /// leaving the thread open behind settings as it is.
    Settings,
}

/// A page title over the cards, with an optional line below it.
pub(crate) fn provider_setup_intro(
    title: impl Into<SharedString>,
    subtitle: Option<&'static str>,
    cx: &App,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .text_2xl()
                .text_color(cx.theme().secondary_foreground)
                .child(title.into()),
        )
        .when_some(subtitle, |this, subtitle| {
            this.child(
                div()
                    .mt_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(subtitle),
            )
        })
        .into_any_element()
}

/// A provider or model card, outlined while selected.
fn setup_card(id: String, selected: bool, cx: &App) -> Stateful<gpui::Div> {
    let selector = id.clone();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .w(px(238.))
        .h(px(92.))
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(if selected {
            cx.theme().primary
        } else {
            cx.theme().border
        })
        .bg(if selected {
            cx.theme().accent
        } else {
            cx.theme().secondary
        })
        .cursor_pointer()
        .hover(|this| this.bg(cx.theme().secondary_hover))
}

/// A card's icon and name, with a smaller line below.
fn setup_card_content(
    provider: ModelProvider,
    name: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    cx: &App,
) -> [AnyElement; 2] {
    [
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
                    .text_color(cx.theme().secondary_foreground)
                    .child(name.into()),
            )
            .into_any_element(),
        div()
            .mt_3()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(detail.into())
            .into_any_element(),
    ]
}

impl Cowork {
    /// Shows `step` with `intro` above the cards and `footer` below the
    /// buttons.
    pub(crate) fn render_provider_setup(
        &self,
        place: ProviderSetupPlace,
        step: ProviderStep,
        intro: Option<AnyElement>,
        footer: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let stage_id = match (place, step) {
            (ProviderSetupPlace::Welcome, ProviderStep::Providers) => "welcome-stage",
            (ProviderSetupPlace::Welcome, ProviderStep::Models(_)) => "provider-setup-stage",
            (ProviderSetupPlace::Settings, ProviderStep::Providers) => "providers-page",
            (ProviderSetupPlace::Settings, ProviderStep::Models(_)) => "provider-models-page",
        };
        let has_intro = intro.is_some();
        let (label, body) = match step {
            ProviderStep::Providers => {
                ("AVAILABLE PROVIDERS", self.render_provider_step(place, cx))
            }
            ProviderStep::Models(provider) => (
                "AVAILABLE MODELS",
                self.render_model_step(place, provider, cx),
            ),
        };

        div()
            .id(stage_id)
            .debug_selector(move || stage_id.to_owned())
            .flex_1()
            .min_h_0()
            .w_full()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
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
                    .children(intro)
                    .child(
                        div()
                            .when(has_intro, |this| this.mt_8())
                            .mb_3()
                            .w_full()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(label),
                    )
                    .children(body)
                    .children(footer),
            )
    }

    /// The provider cards, and continuing to the selected one's models.
    fn render_provider_step(
        &self,
        place: ProviderSetupPlace,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let selected = self.selected_provider;
        let cards =
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .gap_3()
                .children(ModelProvider::ALL.into_iter().map(|provider| {
                    setup_card(
                        format!("setup-provider-{}", provider.label()),
                        selected == Some(provider),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_provider = Some(provider);
                        cx.notify();
                    }))
                    .children(setup_card_content(
                        provider,
                        provider.label(),
                        provider.description(),
                        cx,
                    ))
                }));
        let continue_button = Button::new("provider-continue")
            .primary()
            .label("Continue")
            .disabled(selected.is_none())
            .debug_selector(|| "provider-continue".to_owned())
            .mt_8()
            .px_6()
            .h_11()
            .rounded_lg()
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(provider) = this.selected_provider {
                    this.show_provider_step(place, ProviderStep::Models(provider), cx);
                }
            }));
        vec![cards.into_any_element(), continue_button.into_any_element()]
    }

    /// `provider`'s model cards, going back to the providers, and
    /// discovering the models again.
    fn render_model_step(
        &self,
        place: ProviderSetupPlace,
        provider: ModelProvider,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (catalog, selected) = self.setup_models(place, cx);
        let models = catalog
            .providers()
            .filter(|(listed, _)| *listed == provider)
            .flat_map(|(provider, models)| {
                models.iter().map(move |(id, info)| {
                    let model = ModelRef {
                        provider,
                        id: id.clone(),
                    };
                    (model, info.name.clone(), info.max_tokens)
                })
            })
            .collect::<Vec<_>>();
        let has_models = !models.is_empty();

        let cards = div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_3()
            .children(models.into_iter().map(|(model, name, max_tokens)| {
                let full_name = name.clone();
                let context = format!("{} context tokens", format_token_count(max_tokens));
                setup_card(
                    format!("setup-model-{}-{}", model.provider.label(), model.id),
                    selected.as_ref() == Some(&model),
                    cx,
                )
                .children(setup_card_content(model.provider, name, context, cx))
                .tooltip(move |window, cx| Tooltip::new(full_name.clone()).build(window, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pick_setup_model(place, model.clone(), cx);
                }))
            }))
            .when(!has_models, |this| {
                this.child(
                    div()
                        .w_full()
                        .p_5()
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "No {0} models found. Start {0}, then try again.",
                            provider.label()
                        )),
                )
            });
        // The welcome screen offers discovering again only when it found
        // nothing; settings always do, for models pulled since.
        let show_refresh = !has_models || place == ProviderSetupPlace::Settings;
        let buttons = div()
            .mt_6()
            .flex()
            .gap_4()
            .child(
                Button::new("setup-back")
                    .ghost()
                    .label("Back")
                    .debug_selector(|| "setup-back".to_owned())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show_provider_step(place, ProviderStep::Providers, cx);
                    })),
            )
            .when(show_refresh, |this| {
                this.child(
                    Button::new("setup-refresh")
                        .secondary()
                        .label(if has_models {
                            "Refresh models"
                        } else {
                            "Try again"
                        })
                        .debug_selector(|| "setup-refresh".to_owned())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_models(cx);
                        })),
                )
            });
        vec![cards.into_any_element(), buttons.into_any_element()]
    }

    /// Moves to `step`, discovering the provider's models when it shows them.
    pub(crate) fn show_provider_step(
        &mut self,
        place: ProviderSetupPlace,
        step: ProviderStep,
        cx: &mut Context<Self>,
    ) {
        match place {
            ProviderSetupPlace::Welcome => {
                self.main_stage = match step {
                    ProviderStep::Providers => MainStage::Welcome,
                    ProviderStep::Models(provider) => MainStage::ProviderSetup(provider),
                }
            }
            ProviderSetupPlace::Settings => self.settings_provider_step = step,
        }
        if let ProviderStep::Models(_) = step {
            self.refresh_models(cx);
        }
        cx.notify();
    }

    /// The catalog the model cards list, and the model they show selected.
    fn setup_models(
        &self,
        place: ProviderSetupPlace,
        cx: &App,
    ) -> (Arc<ModelCatalog>, Option<ModelRef>) {
        match place {
            ProviderSetupPlace::Welcome => (self.active_catalog(cx), self.active_model(cx)),
            ProviderSetupPlace::Settings => (self.models.clone(), self.new_thread_model.clone()),
        }
    }

    fn pick_setup_model(
        &mut self,
        place: ProviderSetupPlace,
        model: ModelRef,
        cx: &mut Context<Self>,
    ) {
        match place {
            ProviderSetupPlace::Welcome => self.select_model(model, cx),
            ProviderSetupPlace::Settings => {
                if self.models.contains(&model) {
                    self.set_new_thread_model(model);
                    cx.notify();
                }
            }
        }
    }
}
