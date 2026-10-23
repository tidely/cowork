//! Icons in Markdown text: `![Collaborate](icon:collaborate)` draws the icon the app
//! shows for that feature, inline at the surrounding text's size and color,
//! so a message (such as the onboarding greeting) can point at features the
//! way the UI does. The alt text is the icon's accessible name; copied text
//! leaves the icon out. Other images render as usual.

use gpui::{App, Styled as _, Window};
use gpui_base::{
    InlineElement, InlineRenderContext, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
    text::markdown_ast::Node,
};
use gpui_component::Icon;
use gpui_kit_assets::IconName;

const NODE: &str = "cowork-icon";

/// The icons a message may name, matching where each feature appears.
fn icon_named(name: &str) -> Option<IconName> {
    Some(match name {
        // The titlebar's collaborators popover.
        "collaborate" => IconName::Users,
        // The bottom bar's project folders.
        "folder" => IconName::Folder,
        // The sidebar's Scheduled page.
        "schedule" => IconName::CalendarClock,
        "comment" => IconName::MessageSquare,
        _ => return None,
    })
}

pub(crate) struct InlineIcons;

impl MarkdownPlugin for InlineIcons {
    fn name(&self) -> &str {
        NODE
    }

    fn parse(&self, node: &Node, _: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Image(image) = node else {
            return None;
        };
        let icon = icon_named(image.url.strip_prefix("icon:")?)?;
        Some(MarkdownNode::new(NODE, icon).accessibility_label(image.alt.clone()))
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        _: &mut Window,
        _: &mut App,
    ) -> Option<InlineElement> {
        let icon = *node.data::<IconName>()?;
        let size = context.font_size();
        let element = Icon::new(icon)
            .size(size)
            .text_color(context.text_style().color);
        // Sits on the baseline like a capital letter, rather than on the
        // descent below it.
        Some(InlineElement::new(element).with_baseline(size * 0.85))
    }
}
