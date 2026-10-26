//! Shared syntax highlighting for fenced code blocks, tool-call commands, and diffs.

use std::{ops::Range, sync::OnceLock};

use gpui::{FontStyle, FontWeight, HighlightStyle, rgba};
use gpui_base::text::CodeBlock;
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle as SyntectFontStyle, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static SYNTAX_THEMES: OnceLock<ThemeSet> = OnceLock::new();

/// GPUI Base's callback receives no block style or app context. The caller
/// supplies the mode and replaces the registered callback when it changes,
/// which also invalidates Base's cache keyed by callback identity.
pub(crate) fn highlight_code_block_in_mode(
    block: &CodeBlock,
    is_dark: bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    highlight_code_in_mode(block.lang().as_deref(), block.code().as_ref(), is_dark)
}

/// Accepts either a Markdown language tag or a diff's detected language.
/// Ranges are UTF-8 byte offsets into `code`; the same palette is used on
/// every surface. Callers must refresh cached highlights when the mode changes.
pub(crate) fn highlight_code_in_mode(
    language: Option<&str>,
    code: &str,
    is_dark: bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
    let themes = SYNTAX_THEMES.get_or_init(ThemeSet::load_defaults);
    let name = if is_dark {
        "base16-ocean.dark"
    } else {
        "base16-ocean.light"
    };
    let Some(theme) = themes.themes.get(name) else {
        return Vec::new();
    };

    let syntax = language
        .and_then(|language| {
            let language = language.split_whitespace().next()?;
            syntax_set
                .find_syntax_by_token(language)
                .or_else(|| syntax_set.find_syntax_by_extension(language))
                .or_else(|| {
                    syntax_set
                        .syntaxes()
                        .iter()
                        .find(|syntax| syntax.name.eq_ignore_ascii_case(language))
                })
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut offset = 0;
    let mut highlights = Vec::new();

    for line in LinesWithEndings::from(code) {
        let line_highlights = match highlighter.highlight_line(line, syntax_set) {
            Ok(line_highlights) => line_highlights,
            Err(error) => {
                eprintln!(
                    "failed to highlight {syntax_name} code block: {error}",
                    syntax_name = syntax.name
                );
                return Vec::new();
            }
        };

        for (style, text) in line_highlights {
            let end = offset + text.len();
            if offset < end {
                let foreground = style.foreground;
                let color = rgba(
                    (u32::from(foreground.r) << 24)
                        | (u32::from(foreground.g) << 16)
                        | (u32::from(foreground.b) << 8)
                        | u32::from(foreground.a),
                );
                highlights.push((
                    offset..end,
                    HighlightStyle {
                        color: Some(color.into()),
                        font_weight: style
                            .font_style
                            .contains(SyntectFontStyle::BOLD)
                            .then_some(FontWeight::BOLD),
                        font_style: style
                            .font_style
                            .contains(SyntectFontStyle::ITALIC)
                            .then_some(FontStyle::Italic),
                        ..Default::default()
                    },
                ));
            }
            offset = end;
        }
    }

    highlights
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_multiline_shell_commands() {
        let code = "# café\nfor file in *.rs; do\n    printf '%s\\n' \"$file\"\ndone";
        let block = CodeBlock::from_code(code, Some("sh"));
        for is_dark in [false, true] {
            let highlights = highlight_code_block_in_mode(&block, is_dark);
            assert!(!highlights.is_empty());
            assert_eq!(highlights.first().unwrap().0.start, 0);
            assert_eq!(highlights.last().unwrap().0.end, code.len());
            assert!(highlights.iter().all(|(range, _)| {
                range.start < range.end
                    && code.is_char_boundary(range.start)
                    && code.is_char_boundary(range.end)
            }));
            assert!(
                highlights.windows(2).any(|pair| pair[0].1 != pair[1].1),
                "shell tokens get different styles, not plain-text highlighting"
            );
        }
    }

    #[test]
    fn highlighting_tracks_theme_mode() {
        let block = CodeBlock::from_code("fn main() {}\n", Some("rust"));
        let dark = highlight_code_block_in_mode(&block, true);
        let light = highlight_code_block_in_mode(&block, false);
        assert_ne!(dark, light);
        assert_eq!(highlight_code_block_in_mode(&block, true), dark);
    }
}
