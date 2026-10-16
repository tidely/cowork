//! Cowork's palette lives in the toolkit's theme, so built-in controls and
//! application-owned layouts share the same roles and theme-switching path.

use std::rc::Rc;

use gpui::App;
use gpui_base::TextViewDefaults;
use gpui_component::{ActiveTheme as _, Theme, ThemeConfig};

use crate::highlight::highlight_code_block_in_mode;

/// Installs the current appearance and keeps syntax highlighting in step with
/// later theme changes. Applying another `ThemeConfig` through `Theme::update`
/// refreshes the toolkit, Base editors, and custom views together.
pub(crate) fn init(cx: &mut App) {
    Theme::update(cx, |theme| theme.apply_config(&Rc::new(dark_theme())));
    install_highlighter(cx);
    cx.observe_global::<Theme>(install_highlighter).detach();
}

fn install_highlighter(cx: &mut App) {
    let is_dark = cx.theme().is_dark();
    // Base caches highlights by callback identity. Replacing the callback on
    // theme changes also invalidates code blocks already on screen.
    TextViewDefaults::new()
        .with_code_block_highlighter(move |block| highlight_code_block_in_mode(block, is_dark))
        .install(cx);
}

/// Unspecified component roles inherit the toolkit's dark defaults; its config
/// resolver supplies matching interactive-state fallbacks. The file uses the
/// toolkit's standard format, ready for additional named palettes later.
fn dark_theme() -> ThemeConfig {
    serde_json::from_str(include_str!("../themes/cowork-dark.json"))
        .expect("the bundled Cowork Dark theme must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_palette_uses_known_roles_and_valid_colors() {
        let source: serde_json::Value =
            serde_json::from_str(include_str!("../themes/cowork-dark.json")).unwrap();
        let parsed = serde_json::to_value(dark_theme()).unwrap();
        for (role, color) in source["colors"].as_object().unwrap() {
            assert_eq!(&parsed["colors"][role], color, "unknown theme role: {role}");
            gpui_component::try_parse_color(color.as_str().unwrap())
                .unwrap_or_else(|error| panic!("invalid {role}: {error}"));
        }
    }
}
