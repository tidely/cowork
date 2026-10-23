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
        CornerDownLeft,
        Reply,
        Square,
        SquarePen,
        UsersRound,
        Users,
        Share2,
        Unlink,
        LogOut,
        Eye,
        Pencil,
        Globe,
        Shield,
        RotateCcw,
        Folder,
        FolderOpen,
        FolderPlus,
        Trash,
        Archive,
        ArchiveRestore,
        CalendarClock,
        Clock,
        Repeat,
        Plus,
        Play,
        MessageSquare,
        TriangleAlert,
        FileText,
        Image,
        Pen,
        X,
        Wrench,
        SquareTerminal,
        Hourglass,
        WindowMinimize,
        WindowMaximize,
        WindowRestore,
        WindowClose,
        ArrowLeft,
        CircleUser,
        Bot,
        // `write_file`'s diff: its raw-arguments toggle, and `Diff`'s own
        // controls for unfolding context.
        Braces,
        FileDiff,
        FilePen,
        ChevronsUpDown,
        ArrowUp,
        ArrowDown,
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
    fn assets_serve_icons_and_the_provider_logo() {
        for path in [IconName::SendHorizontal.path(), OLLAMA_AVATAR_PATH.into()] {
            let bytes = Assets.load(&path).unwrap().expect("a bundled asset");
            assert!(!bytes.is_empty(), "empty asset: {path}");
        }
    }
}
