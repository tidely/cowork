//! The search palette: a command palette, opened from the sidebar, that
//! finds things and opens them.
//!
//! Its rows come in categories, each shown as a titled group. Only chats
//! exist so far; another category is another [`PaletteTarget`] variant and
//! another section built in [`Cowork::search_palette_sections`].
//!
//! The palette does its own matching instead of `Command`'s substring
//! filter; see [`PaletteQuery`].

use std::{cell::Cell, cmp::Reverse, iter, ops::Range, rc::Rc};

use gpui::{
    App, AppContext as _, Context, Focusable as _, FontWeight, HighlightStyle, Hsla, SharedString,
    StyledText, Window, div, prelude::*, px, rgb,
};
use gpui_component::{
    IndexPath, WindowExt as _,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex, v_flex,
};
use itertools::Either;
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern},
};
use unicode_segmentation::UnicodeSegmentation as _;
use uuid::Uuid;

use crate::{
    Cowork,
    thread::ThreadSharing,
    timeline::{AgentMessage, TimelineMessage, UserMessageGroup},
};

/// How many chars of a message an excerpt shows before the match. Enough to
/// read the match in context; the row cuts off whatever follows.
const EXCERPT_LEAD_CHARS: usize = 32;

/// How many chars an excerpt keeps after the match: more than a row ever
/// shows, so that the row's own truncation, not this, cuts it off.
const EXCERPT_TAIL_CHARS: usize = 200;

/// What confirming a palette row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteTarget {
    /// Open the chat with this instance id.
    Chat(Uuid),
}

pub(crate) struct PaletteRow {
    pub(crate) target: PaletteTarget,
    /// Shown, and matched against the query.
    pub(crate) label: SharedString,
    /// Muted text at the right end of the row.
    pub(crate) detail: Option<SharedString>,
    /// The byte ranges of `label` the query matched, sorted, to be drawn
    /// highlighted.
    pub(crate) matched: Vec<Range<usize>>,
    /// Where the query appears in the row's text beyond its label, when it
    /// does.
    pub(crate) excerpt: Option<Excerpt>,
}

/// A stretch of text around a match, on one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Excerpt {
    pub(crate) text: SharedString,
    /// The byte range of `text` the query matched.
    pub(crate) matched: Range<usize>,
}

/// A category of rows, shown as one titled group.
pub(crate) struct PaletteSection {
    pub(crate) label: SharedString,
    pub(crate) rows: Vec<PaletteRow>,
}

/// A row that matched, and how well its label did.
pub(crate) struct RankedRow {
    /// `None` when only the row's other text matched.
    label_score: Option<u32>,
    row: PaletteRow,
}

/// The query, compiled for matching rows against.
///
/// A row's label is matched fuzzily, fzf-style: each whitespace-separated
/// word of the query must match on its own, in any order, its letters in
/// order but not necessarily together. The row's other text, such as a
/// chat's messages, is searched for the whole query as it was typed, which
/// in long text is what finds what someone remembers.
///
/// Both ignore case unless the query has an uppercase letter, and
/// diacritics unless the query has one.
pub(crate) struct PaletteQuery {
    words: Pattern,
    /// `None` for a blank query, which matches every row.
    phrase: Option<Atom>,
    matcher: Matcher,
    chars: Vec<char>,
    indices: Vec<u32>,
}

impl PaletteQuery {
    pub(crate) fn new(query: &str) -> Self {
        let phrase = query.trim();
        Self {
            words: Pattern::new(
                query,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
            ),
            phrase: (!phrase.is_empty()).then(|| {
                Atom::new(
                    phrase,
                    CaseMatching::Smart,
                    Normalization::Smart,
                    AtomKind::Substring,
                    false,
                )
            }),
            matcher: Matcher::new(Config::DEFAULT),
            chars: Vec::new(),
            indices: Vec::new(),
        }
    }

    /// The row, if its label or any of `texts` matches, with the matches
    /// marked. An excerpt comes from the first of `texts` that matches.
    pub(crate) fn row<'a>(
        &mut self,
        target: PaletteTarget,
        label: SharedString,
        detail: Option<SharedString>,
        texts: impl IntoIterator<Item = &'a str>,
    ) -> Option<RankedRow> {
        let Some(phrase) = &self.phrase else {
            return Some(RankedRow {
                label_score: Some(0),
                row: PaletteRow {
                    target,
                    label,
                    detail,
                    matched: Vec::new(),
                    excerpt: None,
                },
            });
        };

        self.indices.clear();
        let label_score = self.words.indices(
            Utf32Str::new(&label, &mut self.chars),
            &mut self.matcher,
            &mut self.indices,
        );
        let matched = if label_score.is_some() {
            grapheme_ranges(&label, &mut self.indices)
        } else {
            Vec::new()
        };

        let excerpt = texts.into_iter().find_map(|text| {
            self.indices.clear();
            phrase.indices(
                Utf32Str::new(text, &mut self.chars),
                &mut self.matcher,
                &mut self.indices,
            )?;
            // A substring match is contiguous: its first and last graphemes
            // bound it.
            let ranges = grapheme_ranges(text, &mut self.indices);
            let matched = ranges.first()?.start..ranges.last()?.end;
            Some(excerpt(text, matched))
        });

        (label_score.is_some() || excerpt.is_some()).then_some(RankedRow {
            label_score,
            row: PaletteRow {
                target,
                label,
                detail,
                matched,
                excerpt,
            },
        })
    }
}

/// A category of the matched `rows`, or `None` when none matched.
///
/// Rows whose label matched come first, best match first, then the rows
/// only their other text matched. Rows that rank the same keep their order,
/// and so does everything when the query is blank.
pub(crate) fn section(
    label: impl Into<SharedString>,
    mut rows: Vec<RankedRow>,
) -> Option<PaletteSection> {
    rows.sort_by_key(|row| Reverse(row.label_score));
    (!rows.is_empty()).then(|| PaletteSection {
        label: label.into(),
        rows: rows.into_iter().map(|row| row.row).collect(),
    })
}

/// Turns the matcher's indices into byte ranges of `text`.
///
/// The matcher counts in graphemes (each collapsed to its first char), so an
/// index picks out a whole grapheme, which also keeps the ranges on char
/// boundaries.
fn grapheme_ranges(text: &str, indices: &mut Vec<u32>) -> Vec<Range<usize>> {
    indices.sort_unstable();
    indices.dedup();
    let Some(&last) = indices.last() else {
        return Vec::new();
    };
    let mut indices = indices.iter().map(|&index| index as usize).peekable();
    text.grapheme_indices(true)
        .take(last as usize + 1)
        .enumerate()
        .filter_map(|(position, (start, grapheme))| {
            indices
                .next_if_eq(&position)
                .map(|_| start..start + grapheme.len())
        })
        .collect()
}

/// The part of `text` around `matched`, on one line: runs of whitespace,
/// line breaks included, become single spaces, and an ellipsis marks where
/// the text was cut.
fn excerpt(text: &str, matched: Range<usize>) -> Excerpt {
    let start = text[..matched.start]
        .char_indices()
        .rev()
        .nth(EXCERPT_LEAD_CHARS - 1)
        .map_or(0, |(index, _)| index);
    let end = text[matched.end..]
        .char_indices()
        .nth(EXCERPT_TAIL_CHARS)
        .map_or(text.len(), |(index, _)| matched.end + index);

    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push('…');
    }
    push_on_one_line(&mut excerpt, &text[start..matched.start]);
    let matched_start = excerpt.len();
    push_on_one_line(&mut excerpt, &text[matched.clone()]);
    let matched_end = excerpt.len();
    push_on_one_line(&mut excerpt, &text[matched.end..end]);
    let excerpt = excerpt.trim_end();
    Excerpt {
        text: if end < text.len() {
            format!("{excerpt}…").into()
        } else {
            excerpt.to_owned().into()
        },
        matched: matched_start..matched_end,
    }
}

fn push_on_one_line(line: &mut String, text: &str) {
    for char in text.chars() {
        if !char.is_whitespace() {
            line.push(char);
        } else if !(line.is_empty() || line.ends_with([' ', '…'])) {
            line.push(' ');
        }
    }
}

/// What the palette searches in a chat besides its title: what people asked,
/// and what the agent finally replied, without its work along the way.
fn searchable_texts(timeline: &[TimelineMessage]) -> impl Iterator<Item = &str> {
    timeline.iter().flat_map(|message| match message {
        TimelineMessage::User(UserMessageGroup { blocks, .. }) => {
            Either::Left(blocks.iter().map(|block| block.text.as_str()))
        }
        TimelineMessage::Agent(AgentMessage { output, .. }) => {
            Either::Right(iter::once(output.text.as_str()))
        }
    })
}

/// `text` with `ranges` drawn in `highlight`.
fn highlighted(
    text: SharedString,
    ranges: &[Range<usize>],
    highlight: impl Into<Hsla>,
) -> StyledText {
    let highlight = HighlightStyle {
        color: Some(highlight.into()),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    StyledText::new(text).with_highlights(ranges.iter().map(|range| (range.clone(), highlight)))
}

impl PaletteSection {
    fn group(&self) -> CommandGroup {
        CommandGroup::new()
            .label(self.label.clone())
            .items(self.rows.iter().map(|row| {
                let label = row.label.clone();
                let matched = row.matched.clone();
                let detail = row.detail.clone();
                let excerpt = row.excerpt.clone();
                CommandItem::new()
                    .label(row.label.clone())
                    .child(move |_, _| {
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_3()
                                    .items_center()
                                    .child(div().flex_1().min_w_0().truncate().child(highlighted(
                                        label.clone(),
                                        &matched,
                                        rgb(0xfafafa),
                                    )))
                                    .when_some(detail.clone(), |this, detail| {
                                        this.child(
                                            div()
                                                .flex_none()
                                                .text_color(rgb(0x71717a))
                                                .child(detail),
                                        )
                                    }),
                            )
                            .when_some(excerpt.clone(), |this, excerpt| {
                                this.child(
                                    div()
                                        .w_full()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(rgb(0x71717a))
                                        .child(highlighted(
                                            excerpt.text,
                                            &[excerpt.matched],
                                            rgb(0xd4d4d8),
                                        )),
                                )
                            })
                    })
            }))
    }
}

/// The target of the row at `index`.
///
/// The palette holds only groups, never ungrouped items, so an index path's
/// section is the position in `sections`.
fn target_at(sections: &[PaletteSection], index: IndexPath) -> Option<PaletteTarget> {
    sections
        .get(index.section)?
        .rows
        .get(index.row)
        .map(|row| row.target)
}

impl Cowork {
    /// The palette's categories, with the rows matching `query` in the order
    /// to show them. Categories with no matches are left out, so that
    /// section indices line up with the groups shown.
    pub(crate) fn search_palette_sections(&self, query: &str, cx: &App) -> Vec<PaletteSection> {
        let mut query = PaletteQuery::new(query);
        let chats = self
            .thread_store
            .read(cx)
            .threads
            .iter()
            .filter_map(|thread| {
                let thread = thread.read(cx);
                query.row(
                    PaletteTarget::Chat(thread.instance_id),
                    thread.summary.title.clone().into(),
                    match thread.sharing {
                        ThreadSharing::Sharing | ThreadSharing::Shared { .. } => {
                            Some("Shared by me".into())
                        }
                        ThreadSharing::Connected { .. } => Some("Collaborating".into()),
                        ThreadSharing::NotShared | ThreadSharing::Failed => None,
                    },
                    searchable_texts(&thread.timeline),
                )
            })
            .collect();

        section("Chats", chats).into_iter().collect()
    }

    pub(crate) fn open_search_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }

        // A fresh state per opening, so it starts with an empty query.
        let state = cx.new(|cx| CommandState::new(window, cx));
        let cowork = cx.entity().downgrade();
        let focus_on_mount = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let state = state.clone();
            let cowork = cowork.clone();
            let focus_on_mount = focus_on_mount.clone();
            dialog
                .w(px(560.))
                .margin_top(px(120.))
                .p_0()
                .bg(rgb(0x1c1c1f))
                .close_button(false)
                .content(move |content, window, cx| {
                    // The dialog takes focus as it opens; hand it to the
                    // query field once it has.
                    if focus_on_mount.replace(false) {
                        let state = state.clone();
                        window.defer(cx, move |window, cx| {
                            state.read(cx).focus_handle(cx).focus(window, cx);
                        });
                    }

                    let query = state.read(cx).query(cx);
                    let sections = Rc::new(
                        cowork
                            .read_with(cx, |cowork, cx| cowork.search_palette_sections(&query, cx))
                            .unwrap_or_default(),
                    );
                    let confirm_cowork = cowork.clone();
                    let confirm_sections = sections.clone();
                    let command = Command::new(&state)
                        .bordered(false)
                        .filterable(false)
                        .placeholder("Search chats")
                        .max_h(px(360.))
                        .empty(|_, _, _| {
                            div()
                                .py_6()
                                .w_full()
                                .text_center()
                                .text_sm()
                                .text_color(rgb(0x71717a))
                                .child("No chats found")
                        })
                        // The rows are ranked while the dialog renders, so a
                        // new query needs a new render to take effect.
                        .on_query(|_, window, _| window.refresh())
                        .on_confirm(move |index, window, cx| {
                            let Some(target) = target_at(&confirm_sections, index) else {
                                return;
                            };
                            window.close_dialog(cx);
                            _ = confirm_cowork.update(cx, |cowork, cx| {
                                cowork.open_palette_target(target, window, cx);
                            });
                        });
                    content.child(
                        sections
                            .iter()
                            .fold(command, |command, section| command.group(section.group())),
                    )
                })
        });
    }

    fn open_palette_target(
        &mut self,
        target: PaletteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            PaletteTarget::Chat(thread_id) => self.open_thread(thread_id, window, cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chats `query` finds among `chats` of (title, messages), in order.
    fn search(chats: &[(&str, &[&str])], query: &str) -> Vec<PaletteRow> {
        let mut query = PaletteQuery::new(query);
        let rows = chats
            .iter()
            .filter_map(|(title, texts)| {
                query.row(
                    PaletteTarget::Chat(Uuid::new_v4()),
                    SharedString::from(title.to_string()),
                    None,
                    texts.iter().copied(),
                )
            })
            .collect();
        section("Chats", rows).map_or_else(Vec::new, |section| section.rows)
    }

    fn found_titles(chats: &[(&str, &[&str])], query: &str) -> Vec<String> {
        search(chats, query)
            .into_iter()
            .map(|row| row.label.to_string())
            .collect()
    }

    fn titled(titles: &[&'static str]) -> Vec<(&'static str, &'static [&'static str])> {
        titles.iter().map(|title| (*title, &[][..])).collect()
    }

    fn excerpt_parts(excerpt: &Excerpt) -> (&str, &str) {
        (&excerpt.text, &excerpt.text[excerpt.matched.clone()])
    }

    #[test]
    fn blank_query_keeps_every_row_in_order() {
        let chats = titled(&["Beta review", "Alpha plans", "Gamma"]);
        let titles = ["Beta review", "Alpha plans", "Gamma"];
        assert_eq!(found_titles(&chats, ""), titles);
        assert_eq!(found_titles(&chats, "   "), titles);
    }

    #[test]
    fn titles_match_fuzzily_and_the_best_match_ranks_first() {
        let chats = titled(&[
            "Protocol decode failures",
            "Review protocol decoding edge cases",
            "Find Rust UI frameworks",
        ]);
        // Not a substring of any title, but its letters appear in order in
        // the first two.
        assert_eq!(
            found_titles(&chats, "prtdec"),
            [
                "Protocol decode failures",
                "Review protocol decoding edge cases"
            ]
        );
        // A match at word starts outranks one scattered through the title.
        assert_eq!(found_titles(&chats, "rui")[0], "Find Rust UI frameworks");
    }

    #[test]
    fn each_title_word_must_match_in_any_order() {
        let chats = titled(&["Set upstream Git remote", "Remove untracked file"]);
        assert_eq!(
            found_titles(&chats, "remote git"),
            ["Set upstream Git remote"]
        );
        assert!(found_titles(&chats, "remote zzz").is_empty());
    }

    #[test]
    fn no_match_leaves_no_category() {
        let mut query = PaletteQuery::new("zzz");
        let rows = query
            .row(
                PaletteTarget::Chat(Uuid::new_v4()),
                "Alpha".into(),
                None,
                ["nothing here"],
            )
            .into_iter()
            .collect();
        assert!(section("Chats", rows).is_none());
    }

    #[test]
    fn title_matches_mark_whole_graphemes() {
        let row = search(&titled(&["Café ☕ notes"]), "cfn").remove(0);
        let matched = row
            .matched
            .iter()
            .map(|range| &row.label[range.clone()])
            .collect::<Vec<_>>();
        assert_eq!(matched, ["C", "f", "n"]);

        let row = search(&titled(&["Café ☕ notes"]), "é").remove(0);
        assert_eq!(&row.label[row.matched[0].clone()], "é");
    }

    #[test]
    fn messages_are_searched_for_the_whole_phrase() {
        let chats: [(&str, &[&str]); 3] = [
            ("Lunch", &["Where should we eat?", "Try the noodle bar."]),
            ("Rust questions", &["How do lifetimes work?"]),
            ("Scattered", &["noodles and a bar"]),
        ];
        // The first chat's reply has the phrase; the third has its words
        // apart, which only fuzzy title matching would accept.
        let rows = search(&chats, "noodle bar");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Lunch");
        assert!(rows[0].matched.is_empty(), "the title did not match");
        assert_eq!(
            excerpt_parts(rows[0].excerpt.as_ref().expect("an excerpt")),
            ("Try the noodle bar.", "noodle bar")
        );

        // Case is ignored unless the query has a capital.
        assert_eq!(found_titles(&chats, "LIFETIMES"), Vec::<String>::new());
        assert_eq!(found_titles(&chats, "lifetimes"), ["Rust questions"]);
    }

    #[test]
    fn title_matches_rank_above_message_matches() {
        let chats: [(&str, &[&str]); 2] = [
            ("Weekend plans", &["Let's talk about the deploy"]),
            ("Deploy checklist", &[]),
        ];
        assert_eq!(
            found_titles(&chats, "deploy"),
            ["Deploy checklist", "Weekend plans"]
        );
    }

    #[test]
    fn a_title_match_still_shows_where_messages_match() {
        let chats: [(&str, &[&str]); 1] = [("Deploy checklist", &["Run the deploy script"])];
        let row = search(&chats, "deploy").remove(0);
        assert_eq!(
            &row.label[row.matched[0].start..row.matched.last().unwrap().end],
            "Deploy"
        );
        assert_eq!(
            excerpt_parts(row.excerpt.as_ref().expect("an excerpt")),
            ("Run the deploy script", "deploy")
        );
    }

    #[test]
    fn excerpts_sit_on_one_line_and_mark_where_they_were_cut() {
        let lead = "word ".repeat(20);
        let tail = "more ".repeat(60);
        let text = format!("{lead}the\n\n  needle\tin {tail}");
        let excerpt = excerpt(
            &text,
            text.find("needle").unwrap()..text.find("needle").unwrap() + 6,
        );
        let (line, matched) = excerpt_parts(&excerpt);
        assert_eq!(matched, "needle");
        assert!(line.starts_with('…'), "{line:?}");
        assert!(line.ends_with('…'), "{line:?}");
        assert!(line.contains("the needle in more"), "{line:?}");
        assert!(
            !line.contains(['\n', '\t']) && !line.contains("  "),
            "{line:?}"
        );
        // The lead is the last `EXCERPT_LEAD_CHARS` chars before the match:
        // 25 of the words, then "the" and its four whitespace chars.
        assert_eq!(
            &excerpt.text[..excerpt.matched.start],
            "…word word word word word the "
        );
    }

    #[test]
    fn short_messages_are_shown_whole() {
        let text = "Café is\nhere";
        let excerpt = excerpt(text, 0.."Café".len());
        assert_eq!(excerpt_parts(&excerpt), ("Café is here", "Café"));
    }
}
