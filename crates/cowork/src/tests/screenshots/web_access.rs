use gpui::AppContext as _;

use super::SceneContext;
use crate::{settings::SettingsPage, thread::ThreadStore, web_settings::WebSettings};

pub(super) fn scene(context: &SceneContext) -> anyhow::Result<()> {
    let mut stage = context.open(|_, cx| (cx.new(|_| ThreadStore::default()), None))?;
    stage.update(|app, window, cx| app.open_settings(SettingsPage::WebAccess, window, cx))?;
    stage.capture("1-empty")?;
    stage.update(|app, window, cx| {
        app.apply_web_settings(WebSettings {
            enabled: true,
            endpoint: Some("http://127.0.0.1:3002".into()),
        });
        app.open_settings(SettingsPage::WebAccess, window, cx);
    })?;
    stage.capture("2-configured")?;
    Ok(())
}
