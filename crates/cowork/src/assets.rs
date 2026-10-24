//! Assets bundled into the binary: icons and provider logos.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub(crate) const OLLAMA_LOGO_PATH: &str = "providers/ollama.svg";
pub(crate) const ANTHROPIC_LOGO_PATH: &str = "providers/anthropic.svg";
pub(crate) const OPENAI_LOGO_PATH: &str = "providers/openai.svg";

/// The providers' logos, by path; how each is drawn is
/// `ModelProvider::icon`'s.
const PROVIDER_LOGOS: [(&str, &[u8]); 3] = [
    (
        OLLAMA_LOGO_PATH,
        include_bytes!("../../../assets/providers/ollama.svg"),
    ),
    (
        ANTHROPIC_LOGO_PATH,
        include_bytes!("../../../assets/providers/anthropic.svg"),
    ),
    (
        OPENAI_LOGO_PATH,
        include_bytes!("../../../assets/providers/openai.svg"),
    ),
];

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
        match PROVIDER_LOGOS.iter().find(|(logo, _)| *logo == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => AppIconAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut assets = AppIconAssets.list(path)?;
        assets.extend(
            PROVIDER_LOGOS
                .iter()
                .filter(|(logo, _)| logo.starts_with(path))
                .map(|(logo, _)| SharedString::from(*logo)),
        );
        Ok(assets)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit_assets::IconName;

    use super::*;

    #[test]
    fn assets_serve_icons_and_the_provider_logos() {
        let logos = PROVIDER_LOGOS
            .iter()
            .map(|(path, _)| SharedString::from(*path));
        for path in std::iter::once(IconName::SendHorizontal.path()).chain(logos) {
            let bytes = Assets.load(&path).unwrap().expect("a bundled asset");
            assert!(!bytes.is_empty(), "empty asset: {path}");
        }
    }
}
