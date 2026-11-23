//! Host settings for Firecrawl. Only the saved endpoint drives tool calls;
//! the editor and its connection test operate on a separate draft.

use std::time::Duration;

use gpui::{Context, Entity, IntoElement, Subscription, Window, div, prelude::*, px};
use gpui_base::input::{InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    switch::Switch,
    v_flex,
};
use uuid::Uuid;

use crate::Cowork;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WebSettings {
    pub(crate) enabled: bool,
    pub(crate) endpoint: Option<String>,
}

pub(crate) struct WebEditor {
    input: Entity<InputState>,
    check: ConnectionCheck,
    _subscription: Subscription,
}

enum ConnectionCheck {
    Idle,
    Testing(Uuid),
    Connected(String),
    Failed(String),
}

fn endpoint(text: &str) -> Result<Option<String>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let invalid = "Enter an absolute http:// or https:// base URL.";
    if !text.to_ascii_lowercase().starts_with("https://")
        && !text.to_ascii_lowercase().starts_with("http://")
    {
        return Err(invalid);
    }
    let url = reqwest::Url::parse(text).map_err(|_| invalid)?;
    if url.host().is_none() {
        return Err(invalid);
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Use a base URL without credentials, a query, or a fragment.");
    }
    if url.path().trim_end_matches('/').ends_with("/v2") {
        return Err("Remove /v2 from the end of the base URL.");
    }
    Ok(Some(url.as_str().trim_end_matches('/').to_owned()))
}

impl Cowork {
    pub(crate) fn apply_web_settings(&mut self, settings: WebSettings) {
        *self.web_access.write() = match (settings.enabled, settings.endpoint.as_deref()) {
            (false, _) => tools::WebAccess::Disabled,
            (true, Some(root)) => {
                tools::WebAccess::Configured(firecrawl::Client::new(reqwest::Client::new(), root))
            }
            (true, None) => tools::WebAccess::MissingEndpoint,
        };
        self.web_settings = settings;
    }

    fn set_web_settings(&mut self, settings: WebSettings, cx: &mut Context<Self>) {
        self.apply_web_settings(settings);
        self.save_settings();
        cx.notify();
    }

    pub(crate) fn open_web_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("http://127.0.0.1:3002")
                .default_value(self.web_settings.endpoint.clone().unwrap_or_default())
        });
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                if let Some(editor) = &mut this.web_editor {
                    editor.check = ConnectionCheck::Idle;
                }
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.save_endpoint(cx),
            _ => {}
        });
        self.web_editor = Some(WebEditor {
            input,
            check: ConnectionCheck::Idle,
            _subscription: subscription,
        });
    }

    fn save_endpoint(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.web_editor else {
            return;
        };
        if let Ok(value) = endpoint(&editor.input.read(cx).value())
            && value != self.web_settings.endpoint
        {
            self.set_web_settings(
                WebSettings {
                    enabled: self.web_settings.enabled,
                    endpoint: value,
                },
                cx,
            );
        }
    }

    fn test_endpoint(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.web_editor else {
            return;
        };
        let Ok(Some(root)) = endpoint(&editor.input.read(cx).value()) else {
            return;
        };
        let id = Uuid::new_v4();
        editor.check = ConnectionCheck::Testing(id);
        cx.notify();
        let test = self.tokio_handle.spawn(async move {
            let client = firecrawl::Client::new(reqwest::Client::new(), &root);
            match tokio::time::timeout(Duration::from_secs(8), client.health()).await {
                Ok(Ok(health)) if health.status == "ok" => ConnectionCheck::Connected(format!("Connected · {} {}", health.engine, health.version)),
                Ok(Ok(health)) => ConnectionCheck::Failed(format!("Endpoint reported: {}", health.status)),
                Ok(Err(_)) => ConnectionCheck::Failed("Couldn't verify this Firecrawl endpoint. Check the address and that the server is running.".into()),
                Err(_) => ConnectionCheck::Failed("Connection timed out. Check the address and try again.".into()),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = test.await.unwrap_or_else(|_| {
                ConnectionCheck::Failed("Connection test was interrupted.".into())
            });
            _ = this.update(cx, |this, cx| {
                // A later edit, test, or new editor invalidates this result.
                if let Some(editor) = &mut this.web_editor
                    && matches!(editor.check, ConnectionCheck::Testing(current) if current == id)
                {
                    editor.check = result;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn render_web_page(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let editor = self.web_editor.as_ref().expect("opened web settings");
        let theme = cx.theme();
        let hint = |s: &'static str| div().text_sm().text_color(theme.muted_foreground).child(s);
        let draft = endpoint(&editor.input.read(cx).value());
        let can_test =
            matches!(&draft, Ok(Some(_))) && !matches!(editor.check, ConnectionCheck::Testing(_));
        let can_save = draft
            .as_ref()
            .is_ok_and(|value| value != &self.web_settings.endpoint);
        let status = match (&draft, &editor.check) {
            (Err(error), _) => Some((theme.danger, (*error).to_owned())),
            (_, ConnectionCheck::Testing(_)) => {
                Some((theme.muted_foreground, "Testing connection…".into()))
            }
            (_, ConnectionCheck::Connected(message)) => Some((theme.success, message.clone())),
            (_, ConnectionCheck::Failed(message)) => Some((theme.danger, message.clone())),
            _ => None,
        };
        div().id("web-access-page").flex_1().min_w_0().min_h_0().rounded_tl(px(12.)).border_t_1().border_l_1().border_color(theme.border).bg(theme.background).overflow_y_scroll()
            .child(v_flex().mx_auto().max_w(px(720.)).p_8().gap_6()
                .child(v_flex().gap_2().child(div().text_lg().child("Web access"))
                    .child(hint("Manage how agents read websites in chats hosted on this computer.")))
                .child(v_flex().gap_2().p_4().rounded_lg().border_1().border_color(theme.border)
                    .child(h_flex().justify_between().child("Allow web access")
                        .child(Switch::new("allow-web").checked(self.web_settings.enabled).accessibility_label("Allow web access")
                            .on_click(cx.listener(|this, enabled: &bool, _, cx| this.set_web_settings(WebSettings { enabled: *enabled, endpoint: this.web_settings.endpoint.clone() }, cx)))))
                    .child(hint("Agents can read a page or list URLs on a site. Each request needs your approval."))
                    .when(self.web_settings.enabled && self.web_settings.endpoint.is_none(), |el| el.child(hint("Save an endpoint before agents can use web access."))))
                .child(v_flex().gap_3()
                    .child(div().text_base().child("Firecrawl connection"))
                    .child(v_flex().gap_1().child(div().text_sm().child("Endpoint"))
                        .child(Input::new(&editor.input).id("web-endpoint").h(px(38.)))
                        .child(h_flex().flex_wrap().gap_1().child(hint("Enter the server’s base URL without /v2."))
                            .child(
                                div().id("firecrawl-install-guide").text_sm().cursor_pointer()
                                    .text_color(theme.link).hover(|el| el.underline()).child("Install Firecrawl Local")
                                    .on_click(|_, _, cx| cx.open_url("https://docs.firecrawl.dev/quickstarts/firecrawl-local")))))
                    .child(h_flex().gap_2()
                        .child(Button::new("test-endpoint").outline().label("Test connection").disabled(!can_test)
                            .on_click(cx.listener(|this, _, _, cx| this.test_endpoint(cx))))
                        .child(Button::new("save-endpoint").primary().label("Save endpoint").disabled(!can_save)
                            .on_click(cx.listener(|this, _, _, cx| this.save_endpoint(cx)))))
                    .children(status.map(|(color, text)| h_flex().gap_2()
                        .child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(color))
                        .child(div().text_sm().text_color(theme.muted_foreground).child(text))))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chat_db::TestDb, settings::SettingsPage, tests::composer_test_cowork};

    #[gpui::test]
    fn saving_a_draft_does_not_enable_web_access(cx: &mut gpui::TestAppContext) {
        let (view, _runtime, cx) = composer_test_cowork(cx);
        let (db_file, db) = TestDb::open();
        view.update_in(cx, |app, window, cx| {
            app.chat_db = Some(db);
            app.open_settings(SettingsPage::WebAccess, window, cx);
            let input = app.web_editor.as_ref().unwrap().input.clone();
            input.update(cx, |input, cx| {
                input.set_value("https://example.org/private/v2", window, cx)
            });
            app.save_endpoint(cx);
            assert!(app.web_settings.endpoint.is_none());
            input.update(cx, |input, cx| {
                input.set_value("https://example.org/private/", window, cx)
            });
            assert!(app.web_settings.endpoint.is_none());
            app.save_endpoint(cx);
            assert_eq!(
                app.web_settings.endpoint.as_deref(),
                Some("https://example.org/private")
            );
            assert!(matches!(*app.web_access.read(), tools::WebAccess::Disabled));
            app.set_web_settings(
                WebSettings {
                    enabled: true,
                    ..app.web_settings.clone()
                },
                cx,
            );
            futures::executor::block_on(app.chat_db.take().unwrap().quit());
            app.apply_web_settings(Default::default());
            let (db, saved) = db_file.reopen();
            app.start_settings(saved.state.settings, cx);
            assert_eq!(
                app.web_settings.endpoint.as_deref(),
                Some("https://example.org/private")
            );
            assert!(app.web_access.read().client().is_ok());
            input.update(cx, |input, cx| input.set_value("", window, cx));
            app.save_endpoint(cx);
            assert!(matches!(
                *app.web_access.read(),
                tools::WebAccess::MissingEndpoint
            ));
            futures::executor::block_on(db.quit());
        });
    }
}
