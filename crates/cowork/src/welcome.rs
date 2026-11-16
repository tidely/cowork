//! The welcome screen before the first chat: setting up a provider (see
//! `provider_setup.rs`) with a greeting, then starting an onboarding chat.

use gpui::{Context, IntoElement, Window, div, prelude::*};
use gpui_component::ActiveTheme as _;

use crate::{
    Cowork,
    models::ModelProvider,
    provider_setup::{ProviderSetupPlace, ProviderStep, provider_setup_intro},
};

impl Cowork {
    pub(crate) fn render_welcome(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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
        // Someone invited to a chat needs no provider: the host runs the
        // agent. So joining is offered beside the providers, not after them.
        let join = div()
            .mt_8()
            .flex()
            .items_center()
            .gap_1()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("Got a link from someone?")
            .child(
                div()
                    .id("welcome-join")
                    .debug_selector(|| "welcome-join".to_owned())
                    .cursor_pointer()
                    .text_color(cx.theme().link)
                    .hover(|this| this.underline())
                    .child("Join their chat")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_join_dialog(window, cx);
                    })),
            )
            .child("— no provider needed.");
        self.render_provider_setup(
            ProviderSetupPlace::Welcome,
            ProviderStep::Providers,
            Some(intro),
            Some(join.into_any_element()),
            window,
            cx,
        )
    }

    /// Shows the selected provider's models; clicking one starts the
    /// onboarding chat with it (see `Cowork::pick_setup_model`).
    pub(crate) fn render_welcome_models(
        &mut self,
        provider: ModelProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let intro = provider_setup_intro(
            format!("Set up {}", provider.label()),
            Some("Choose a model to start your first chat."),
            cx,
        );
        self.render_provider_setup(
            ProviderSetupPlace::Welcome,
            ProviderStep::Models(provider),
            Some(intro),
            None,
            window,
            cx,
        )
    }
}
