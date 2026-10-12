//! The welcome screen before the first chat: setting up a provider (see
//! `provider_setup.rs`) with a greeting, then starting the chat.

use gpui::{Context, IntoElement, prelude::*};
use gpui_component::{
    Disableable as _,
    button::{Button, ButtonVariants as _},
};

use crate::{
    Cowork, MainStage,
    models::ModelProvider,
    provider_setup::{ProviderSetupPlace, ProviderStep, provider_setup_intro},
};

impl Cowork {
    /// Greets the user over the providers.
    pub(crate) fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let greeting = self
            .profile
            .name
            .as_ref()
            .map(|name| format!("Welcome back, {name}"))
            .unwrap_or_else(|| "Welcome to Cowork".into());
        let intro = provider_setup_intro(
            greeting,
            Some("Choose a provider to start your first chat."),
            cx,
        );
        self.render_provider_setup(
            ProviderSetupPlace::Welcome,
            ProviderStep::Providers,
            Some(intro),
            None,
            cx,
        )
    }

    /// Shows the selected provider's models, and starts the first chat once
    /// one is picked.
    pub(crate) fn render_welcome_models(
        &self,
        provider: ModelProvider,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let intro = provider_setup_intro(
            format!("Set up {}", provider.label()),
            Some("Choose a model to start your first chat."),
            cx,
        );
        let start = Button::new("setup-continue")
            .primary()
            .label("Continue")
            .disabled(!self.active_model_is_runnable(cx))
            .debug_selector(|| "setup-continue".to_owned())
            .mt_8()
            .px_6()
            .h_11()
            .rounded_lg()
            .on_click(cx.listener(|this, _, window, cx| {
                if this.active_model_is_runnable(cx) {
                    this.main_stage = MainStage::Thread;
                    this.focus_composer(window, cx);
                    cx.notify();
                }
            }));
        self.render_provider_setup(
            ProviderSetupPlace::Welcome,
            ProviderStep::Models(provider),
            Some(intro),
            Some(start.into_any_element()),
            cx,
        )
    }
}
