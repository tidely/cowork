//! Assets bundled into the binary: icons and provider logos.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub(crate) const OLLAMA_AVATAR_PATH: &str = "providers/ollama.png";

gpui_kit_assets::icon_assets!(
    AppIconAssets,
    [
        Check,
        ChevronDown,
        ChevronRight,
        ChevronUp,
        Link,
        PanelLeftClose,
        PanelLeftOpen,
        Search,
        SendHorizontal,
        Square,
        SquarePen,
        UsersRound,
        Users,
        Share2,
        Unlink,
        LogOut,
        Eye,
        Pencil,
        Shield,
        RotateCcw,
        Paperclip,
        FileText,
        Image,
        Pen,
        X,
        WindowMinimize,
        WindowMaximize,
        WindowRestore,
        WindowClose,
    ]
);

pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match path {
            OLLAMA_AVATAR_PATH => Ok(Some(Cow::Borrowed(include_bytes!(
                "../../../assets/providers/ollama.png"
            )))),
            _ => AppIconAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut assets = AppIconAssets.list(path)?;
        if OLLAMA_AVATAR_PATH.starts_with(path) {
            assets.push(OLLAMA_AVATAR_PATH.into());
        }
        Ok(assets)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit_assets::IconName;

    use super::*;

    #[test]
    fn sharing_and_permission_icons_are_bundled() {
        for icon in [
            IconName::Users,
            IconName::Share2,
            IconName::Unlink,
            IconName::LogOut,
            IconName::Eye,
            IconName::Pencil,
            IconName::Shield,
            IconName::RotateCcw,
            IconName::Check,
            IconName::Link,
        ] {
            let path = icon.path();
            let bytes = Assets.load(&path).unwrap().expect("bundled sharing icon");
            assert!(!bytes.is_empty(), "empty sharing icon: {path}");
        }
    }

    #[test]
    fn window_control_icons_are_bundled() {
        for icon in [
            IconName::WindowMinimize,
            IconName::WindowMaximize,
            IconName::WindowRestore,
            IconName::WindowClose,
        ] {
            let path = icon.path();
            let bytes = Assets.load(&path).unwrap().expect("bundled window icon");
            assert!(!bytes.is_empty(), "empty window icon: {path}");
        }
    }
}
