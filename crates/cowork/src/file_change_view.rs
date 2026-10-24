//! A `write_file` or `edit_file` call shown as the change it makes: what it
//! does to which file ("Edit zed/a.rs") and its line counts, a Diff / JSON
//! switch, and either a unified diff (gpui-component's [`Diff`]) or the
//! call's raw arguments. That label is also the approval card's title.
//!
//! Every copy of a thread computes the diff from the call's arguments
//! alone, never from the file: an edit's `old_string` becomes its
//! `new_string`, and a write shows its whole content as added, whatever the
//! file held. An edit shows only the text it replaces, without the file's
//! line numbers, which its arguments don't give. Each call keeps one view
//! (see `AgentMessage::show_file_changes`), both the approval card's body
//! and the timeline row's details, so the switch stays where it was left.

use std::time::Duration;

use agent::model::ToolCall;
use gpui::{
    Context, Entity, FontWeight, IntoElement, Pixels, Render, SharedString, Window, div,
    prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Toggle, ToggleGroup, ToggleVariants as _},
    diff::{Diff, DiffFile, DiffHunkSeparator, DiffState},
    h_flex,
};
use gpui_kit_assets::IconName;
use sandbox::{EditFile, WriteFile};

use crate::tool_call_card::code_block;

/// How long finding a change's lines may take before settling for a
/// coarser diff, as it runs on the UI thread.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);
/// Rows the diff grows to before it scrolls, so a card stays a card.
const MAX_ROWS: usize = 14;

/// What a file call changes, from its arguments.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileChange {
    pub(crate) path: String,
    pub(crate) kind: ChangeKind,
    /// A unified patch [`DiffFile::parse`] reads.
    pub(crate) patch: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    /// `write_file`: the whole content, as added lines.
    Write,
    /// `edit_file`: the replaced text and its replacement.
    Edit { replace_all: bool },
}

impl FileChange {
    /// The change `call` makes, or `None` when it isn't a file call with
    /// the arguments one needs.
    pub(crate) fn of(call: &ToolCall) -> Option<Self> {
        let arguments = crate::timeline::arguments_value(call);
        let path = arguments["path"].as_str()?.to_owned();
        let (kind, before, after) = match call.name.as_str() {
            WriteFile::NAME => (ChangeKind::Write, "", arguments["content"].as_str()?),
            EditFile::NAME => (
                ChangeKind::Edit {
                    replace_all: arguments["replace_all"].as_bool().unwrap_or(false),
                },
                arguments["old_string"].as_str()?,
                arguments["new_string"].as_str()?,
            ),
            _ => return None,
        };
        let original = match kind {
            ChangeKind::Write => "/dev/null".to_owned(),
            ChangeKind::Edit { .. } => format!("a/{path}"),
        };
        // Whether an edit's fragments end with a newline says nothing about
        // the file, so both sides end with one; otherwise the patch marks
        // "No newline at end of file", which `Diff` shows as rows of its own.
        // A write's content is the whole file, whose missing final newline
        // is worth that row, the diff's last.
        let terminated = |text: &str| {
            let mut text = text.to_owned();
            if matches!(kind, ChangeKind::Edit { .. }) && !text.is_empty() && !text.ends_with('\n')
            {
                text.push('\n');
            }
            text
        };
        let (before, after) = (terminated(before), terminated(after));
        let patch = similar::TextDiff::configure()
            .timeout(DIFF_TIMEOUT)
            .diff_lines(&before, &after)
            .unified_diff()
            .context_radius(3)
            .header(&original, &format!("b/{path}"))
            .to_string();
        Some(Self { path, kind, patch })
    }
}

pub(crate) struct FileChangeView {
    /// Prefixes the debug selectors of the view's parts.
    id: SharedString,
    path: String,
    kind: ChangeKind,
    /// The call's arguments, pretty-printed.
    json: String,
    /// `None` when nothing changes.
    file: Option<DiffFile>,
    /// Lines the diff shows: its hunks' lines and a divider for each.
    rows: usize,
    diff: Entity<DiffState>,
    /// Whether the raw JSON is shown instead of the diff.
    raw: bool,
}

impl FileChangeView {
    /// The view of `call`, or `None` when it isn't a file change.
    pub(crate) fn of(
        id: impl Into<SharedString>,
        call: &ToolCall,
        cx: &mut impl gpui::AppContext,
    ) -> Option<Entity<Self>> {
        let change = FileChange::of(call)?;
        let file = DiffFile::parse(&change.patch)
            .ok()
            .and_then(|files| files.into_iter().next());
        let rows = change
            .patch
            .lines()
            // A "No newline" marker is a row too.
            .filter(|line| !line.starts_with("--- ") && !line.starts_with("+++ "))
            .count();
        let id = id.into();
        // A `Value` always encodes.
        let json = serde_json::to_string_pretty(&crate::timeline::arguments_value(call))
            .unwrap_or_default();
        Some(cx.new(|cx| Self {
            id,
            path: change.path,
            kind: change.kind,
            json,
            diff: cx.new(|cx| DiffState::new(file.clone(), cx)),
            file,
            rows,
            raw: false,
        }))
    }

    fn file_label(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (verb, note) = match self.kind {
            ChangeKind::Write => ("Write", Some("whole file")),
            ChangeKind::Edit { replace_all: true } => ("Edit", Some("every occurrence")),
            ChangeKind::Edit { replace_all: false } => ("Edit", None),
        };
        h_flex()
            .min_w_0()
            .gap_1p5()
            .child(
                Icon::new(IconName::FilePen)
                    .size_3()
                    .flex_none()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .child(verb),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .child(self.path.clone()),
            )
            .children(self.file.as_ref().map(|file| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme.success)
                            .child(format!("+{}", file.additions())),
                    )
                    .when(file.deletions() > 0, |this| {
                        this.child(
                            div()
                                .text_color(theme.danger)
                                .child(format!("−{}", file.deletions())),
                        )
                    })
            }))
            .children(note.map(|note| {
                div()
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(note)
            }))
    }
}

impl Render for FileChangeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id.clone();
        let raw = self.raw;
        let switch = ToggleGroup::new(SharedString::from(format!("{id}-switch")))
            .segmented()
            .outline()
            .xsmall()
            .child(Toggle::new("diff").label("Diff").checked(!raw))
            .child(Toggle::new("json").label("JSON").checked(raw))
            .on_click(cx.listener(move |this, checked: &Vec<bool>, _, cx| {
                // Clicking the side shown unchecks it and changes nothing;
                // clicking the other checks both.
                if checked.iter().all(|checked| *checked) {
                    this.raw = !raw;
                    cx.notify();
                }
            }));
        let content = if raw {
            code_block(format!("{id}-json"), self.json.clone(), cx)
        } else if self.file.is_some() {
            // `Diff` owns its scrolling and needs a bounded height: its rows'
            // own, as it computes them, up to `MAX_ROWS`.
            let font_size = window.rem_size() * (f32::from(cx.theme().mono_font_size) / 16.);
            let height: Pixels = font_size * 1.6 * self.rows.min(MAX_ROWS) as f32;
            let selector = format!("{id}-diff");
            div()
                .debug_selector(move || selector.clone())
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .overflow_hidden()
                .child(
                    Diff::new(&self.diff)
                        .header_visible(false)
                        // A write's lines are the file's; an edit's aren't.
                        .line_number(self.kind == ChangeKind::Write)
                        .hunk_separator(DiffHunkSeparator::Simple)
                        .h(height + px(1.)),
                )
                .into_any_element()
        } else {
            div()
                .text_color(cx.theme().muted_foreground)
                .child("No changes to the file’s lines.")
                .into_any_element()
        };
        div()
            .debug_selector(move || id.to_string())
            .flex()
            .flex_col()
            .gap_1p5()
            .text_xs()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .child(self.file_label(cx))
                    .child(div().flex_1())
                    .child(switch),
            )
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Modifiers, TestAppContext};
    use serde_json::json;

    use super::*;
    use crate::timeline::tool_call;

    /// Writes show their whole content as added and edits their replaced
    /// text, as patches `Diff` reads; other calls, and calls missing an
    /// argument, have no change.
    #[test]
    fn changes_come_from_the_arguments() {
        for (call, additions, deletions) in [
            (
                tool_call(
                    "w",
                    WriteFile::NAME,
                    json!({"path": "scratch://my notes.md", "content": "a\nb\n"}),
                ),
                2,
                0,
            ),
            (
                tool_call(
                    "e",
                    EditFile::NAME,
                    json!({"path": "zed/a.rs", "old_string": "x\ny", "new_string": "x\nz"}),
                ),
                1,
                1,
            ),
        ] {
            let change = FileChange::of(&call).unwrap();
            let files = DiffFile::parse(&change.patch).unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path().as_str(), change.path);
            assert_eq!(
                (files[0].additions(), files[0].deletions()),
                (additions, deletions)
            );
        }
        // Only a whole file's missing final newline is marked, after its
        // last line; an edit's fragments are never.
        for (call, marked) in [
            (
                tool_call(
                    "e",
                    EditFile::NAME,
                    json!({"path": "a", "old_string": "x", "new_string": "x\n\ny"}),
                ),
                false,
            ),
            (
                tool_call(
                    "w",
                    WriteFile::NAME,
                    json!({"path": "a", "content": "x\ny"}),
                ),
                true,
            ),
            (
                tool_call("w", WriteFile::NAME, json!({"path": "a", "content": "x\n"})),
                false,
            ),
        ] {
            let patch = FileChange::of(&call).unwrap().patch;
            assert_eq!(
                patch.trim_end().ends_with("\\ No newline at end of file"),
                marked,
                "{patch}"
            );
            assert_eq!(patch.matches("\\ No newline").count(), usize::from(marked));
        }
        for call in [
            tool_call("c", "calculate", json!({"path": "a", "content": "b"})),
            tool_call("e", EditFile::NAME, json!({"path": "a", "old_string": "b"})),
        ] {
            assert_eq!(FileChange::of(&call), None);
        }
    }

    #[gpui::test]
    fn the_switch_shows_the_raw_arguments(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let call = tool_call(
            "call_1",
            EditFile::NAME,
            json!({"path": "zed/a.txt", "old_string": "a", "new_string": "b"}),
        );
        struct Host(Entity<FileChangeView>);
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(480.)).child(self.0.clone())
            }
        }
        let (_, cx) =
            cx.add_window_view(|_, cx| Host(FileChangeView::of("change", &call, cx).unwrap()));
        cx.run_until_parked();
        assert!(cx.debug_bounds("change-diff").is_some());
        assert!(cx.debug_bounds("change-json").is_none());

        let row = cx.debug_bounds("change").unwrap();
        // The JSON toggle is the switch's right half, at the row's end.
        let json = gpui::point(row.right() - px(8.), row.top() + px(8.));
        cx.simulate_click(json, Modifiers::default());
        assert!(cx.debug_bounds("change-json").is_some());
        assert!(cx.debug_bounds("change-diff").is_none());
    }
}
