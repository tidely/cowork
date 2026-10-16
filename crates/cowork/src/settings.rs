//! Settings: the sidebar's settings view, with a tab for every
//! [`SettingsPage`], and the page the selected tab shows on the center stage.
//!
//! The sidebar shows the settings view exactly while a settings page is on
//! the center stage, so going anywhere else (a thread from the search
//! palette, say) brings the app's sidebar back without extra bookkeeping.

use gpui::{AnyElement, Context, IntoElement, Window, div, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, Icon, h_flex,
    sidebar::{SidebarMenu, SidebarMenuItem},
};
use gpui_kit_assets::IconName as AssetIconName;

use crate::{
    Cowork, MainStage,
    provider_setup::{ProviderSetupPlace, ProviderStep, provider_setup_intro},
    sidebar::CoworkSidebarSection,
};

/// A settings page: its tab in the settings sidebar and the page it shows on
/// the center stage. Tabs are listed in [`SettingsPage::ALL`]'s order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum SettingsPage {
    /// The local user's name and picture, and their usage statistics; see
    /// `profile_page.rs`.
    #[default]
    Profile,
    /// The LLM providers and their models, to pick the model new chats start
    /// with and discover models again; see `provider_setup.rs`.
    Providers,
    /// The default context window and when conversations are summarized.
    Context,
}

impl SettingsPage {
    /// Every page, in the order the sidebar lists their tabs.
    pub(crate) const ALL: [SettingsPage; 3] = [
        SettingsPage::Profile,
        SettingsPage::Providers,
        SettingsPage::Context,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsPage::Profile => "Profile",
            SettingsPage::Providers => "Providers",
            SettingsPage::Context => "Context",
        }
    }

    pub(crate) fn icon(self) -> AssetIconName {
        match self {
            SettingsPage::Profile => AssetIconName::CircleUser,
            SettingsPage::Providers => AssetIconName::Bot,
            SettingsPage::Context => AssetIconName::FileText,
        }
    }
}

impl Cowork {
    /// Shows `page` on the center stage, switching the sidebar to settings.
    /// "Back to app" returns to the stage that was showing before settings
    /// opened.
    pub(crate) fn open_settings(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.main_stage, MainStage::Settings(_)) {
            self.stage_before_settings = self.main_stage;
        }
        self.main_stage = MainStage::Settings(page);
        match page {
            SettingsPage::Profile => self.profile_error = None,
            SettingsPage::Providers => self.settings_provider_step = ProviderStep::Providers,
            SettingsPage::Context => {}
        }
        // The composer is hidden, so it must not keep taking keystrokes.
        window.blur(cx);
        cx.notify();
    }

    /// Returns to the stage settings were opened from. A thread removed in
    /// the meantime has already cleared `active_thread_id`, which then shows
    /// a new chat.
    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.main_stage = self.stage_before_settings;
        if self.main_stage == MainStage::Thread {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn render_settings_page(
        &self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match page {
            SettingsPage::Profile => self.render_profile_page(cx).into_any_element(),
            SettingsPage::Providers => self.render_providers_page(cx).into_any_element(),
            SettingsPage::Context => self.render_context_page(window, cx).into_any_element(),
        }
    }

    /// The welcome screen's provider steps without its welcome text; the
    /// models step is titled with its provider's name.
    fn render_providers_page(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let step = self.settings_provider_step;
        let intro = match step {
            ProviderStep::Providers => None,
            ProviderStep::Models(provider) => {
                Some(provider_setup_intro(provider.label(), None, cx))
            }
        };
        self.render_provider_setup(ProviderSetupPlace::Settings, step, intro, None, cx)
    }

    /// The settings sidebar's header: the way back to the app, spaced further
    /// from the tabs than the tabs are from each other.
    pub(crate) fn render_settings_sidebar_header(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        // `Sidebar` puts its header in a row, so fill it.
        div().flex_1().pb_5().child(
            h_flex()
                .id("settings-back")
                .debug_selector(|| "settings-back".to_owned())
                .h(px(42.))
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm()
                .text_color(theme.sidebar_foreground)
                .hover(|this| {
                    this.bg(theme.sidebar_accent.opacity(0.8))
                        .text_color(theme.sidebar_accent_foreground)
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_settings(window, cx);
                }))
                .child(Icon::new(AssetIconName::ArrowLeft).size_4())
                .child("Back to app"),
        )
    }

    /// The settings sidebar's tabs, one per [`SettingsPage`].
    pub(crate) fn settings_sidebar_tabs(&self, cx: &mut Context<Self>) -> CoworkSidebarSection {
        let menu = SettingsPage::ALL
            .into_iter()
            .fold(SidebarMenu::new(), |menu, page| {
                menu.child(
                    SidebarMenuItem::new(page.label())
                        .min_h(px(34.))
                        .icon(
                            Icon::new(page.icon())
                                .size_4()
                                .text_color(cx.theme().secondary_foreground),
                        )
                        .active(self.main_stage == MainStage::Settings(page))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_settings(page, window, cx);
                        })),
                )
            });
        CoworkSidebarSection::new(None::<&str>, menu)
    }
}
