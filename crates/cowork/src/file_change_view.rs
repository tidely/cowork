//! A `write_file` or `edit_file` call shown as the change it makes: a
//! unified diff (gpui-component's [`Diff`]). The timeline row's summary
//! names the file, so its details are the diff alone; the approval card
//! puts [`card_body`]'s label, what the call does to which file ("Edit
//! zed/a.rs") and its line counts, above it in place of a title.
//!
//! Every copy of a thread computes the diff from the call's arguments
//! alone, never from the file: an edit's `old_string` becomes its
//! `new_string`, and a write shows its whole content as added, whatever the
//! file held. An edit shows only the text it replaces, without the file's
//! line numbers, which its arguments don't give. Each call keeps one view
//! (see `AgentMessage::show_file_changes`), both the approval card's body
//! and the timeline row's details, so its scrolling and selection carry
//! over once the call is decided.

use std::time::Duration;

use agent::model::ToolCall;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, Pixels, Render, SharedString,
    Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Theme,
    diff::{Diff, DiffFile, DiffHunkSeparator, DiffState},
    h_flex, v_flex,
};
use gpui_kit_assets::IconName;
use sandbox::{EditFile, WriteFile};

use crate::highlight::highlight_code_in_mode;

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
    /// `None` when nothing changes.
    file: Option<DiffFile>,
    /// Lines the diff shows: its hunks' lines and a divider for each.
    rows: usize,
    diff: Entity<DiffState>,
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
        Some(cx.new(|cx| Self {
            id,
            path: change.path,
            kind: change.kind,
            diff: cx.new(|cx| {
                // Diff prepares each hunk's two sides in the background. Only
                // colors depend on the app; no context crosses to that task.
                cx.observe_global::<Theme>(|diff: &mut DiffState, cx| {
                    let is_dark = cx.theme().is_dark();
                    // Replace only the highlighter, preserving selection and
                    // scrolling in this same state when the palette changes.
                    diff.set_syntax_highlighter(
                        move |language, code| highlight_code_in_mode(Some(language), code, is_dark),
                        cx,
                    );
                })
                .detach();
                let is_dark = cx.theme().is_dark();
                DiffState::new(file.clone(), cx).with_syntax_highlighter(move |language, code| {
                    highlight_code_in_mode(Some(language), code, is_dark)
                })
            }),
            file,
            rows,
        }))
    }

    fn file_label(&self, cx: &App) -> impl IntoElement {
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

/// The approval card's body for a file call: what it does to which file,
/// with its line counts, above its diff.
pub(crate) fn card_body(view: &Entity<FileChangeView>, cx: &App) -> AnyElement {
    v_flex()
        .gap_1p5()
        .text_xs()
        .child(view.read(cx).file_label(cx))
        .child(view.clone())
        .into_any_element()
}

impl Render for FileChangeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id.clone();
        let content = if self.file.is_some() {
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
            .text_xs()
            .child(content)
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn diff_languages_use_the_shared_palette() {
        use gpui_base::text::CodeBlock;

        use crate::highlight::highlight_code_block_in_mode;

        for (path, language, code) in [
            (
                "project/main.rs",
                "rust",
                "fn main() {\n    let café = 42;\n}\n",
            ),
            (
                "scratch://test.py",
                "python",
                "def café():\n    return 42\n",
            ),
        ] {
            let call = tool_call("w", WriteFile::NAME, json!({"path": path, "content": code}));
            let change = FileChange::of(&call).unwrap();
            let file = DiffFile::parse(&change.patch).unwrap().remove(0);
            let detected = file.language();
            let mut modes = Vec::new();
            for is_dark in [false, true] {
                let highlights = highlight_code_in_mode(Some(&detected), code, is_dark);
                assert_eq!(
                    highlights,
                    highlight_code_block_in_mode(
                        &CodeBlock::from_code(code, Some(language)),
                        is_dark,
                    ),
                );
                assert!(highlights.windows(2).any(|pair| pair[0].1 != pair[1].1));
                assert!(highlights.iter().all(|(range, _)| {
                    range.start < range.end
                        && code.is_char_boundary(range.start)
                        && code.is_char_boundary(range.end)
                }));
                modes.push(highlights);
            }
            assert_ne!(modes[0], modes[1]);
        }
        assert_eq!(
            highlight_code_in_mode(Some("unknown-language"), "plain café\n", true),
            highlight_code_in_mode(None, "plain café\n", true),
        );
    }
}
