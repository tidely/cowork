//! Syntax highlighting for fenced code blocks in agent messages.

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

    let syntax = block
        .lang()
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
    let code = block.code();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut offset = 0;
    let mut highlights = Vec::new();

    for line in LinesWithEndings::from(code.as_ref()) {
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
    fn highlights_fenced_rust_code() {
        let code = "fn main() { println!(\"hello\"); }\n";
        let block = CodeBlock::from_code(code, Some("rust"));
        for is_dark in [false, true] {
            let highlights = highlight_code_block_in_mode(&block, is_dark);
            assert!(!highlights.is_empty());
            assert!(
                highlights
                    .iter()
                    .all(|(range, _)| range.start < range.end && range.end <= code.len())
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
