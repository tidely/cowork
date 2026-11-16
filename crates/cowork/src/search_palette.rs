//! The search palette: a command palette, opened from the sidebar, that
//! finds things and opens them.
//!
//! Rows come in categories shown as titled groups; another category is
//! another [`PaletteTarget`] variant and another section built in
//! [`Cowork::search_palette_sections`]. Matching is done here, not by
//! `Command`'s substring filter; see [`PaletteQuery`].
//!
//! Titles and the text of loaded chats are matched in memory as the query is
//! typed. The text of unloaded chats is searched in the database on Tokio
//! ([`SavedChatSearch`]); its rows join the group after the title matches,
//! so nothing under the pointer moves. The database only narrows: every text
//! it returns is matched by [`PaletteQuery`] like any other. Its index needs
//! three characters, so a shorter query matches unloaded chats by title only.

use std::{cell::Cell, cmp::Reverse, iter, ops::Range, rc::Rc};

use gpui::{
    App, AppContext as _, Context, Focusable as _, FontWeight, HighlightStyle, Hsla, SharedString,
    StyledText, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme, IndexPath, WindowExt as _,
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

use crate::chat_store::ListedChat;
use crate::{
    Cowork,
    chat_db::SearchHit,
    thread::ThreadSharing,
    timeline::{AgentMessage, TimelineMessage},
};

#[derive(Default)]
pub(crate) struct SavedChatSearch {
    phrase: String,
    found: std::collections::HashMap<Uuid, Vec<String>>,
    /// The search under way. Replacing it drops it, so an older query's
    /// answer is never applied.
    searching: Option<gpui::Task<()>>,
}

/// How many chars of a message an excerpt shows before the match. Enough to
/// read the match in context; the row cuts off whatever follows.
const EXCERPT_LEAD_CHARS: usize = 32;

/// How many chars an excerpt keeps after the match: more than a row ever
/// shows, so that the row's own truncation, not this, cuts it off.
const EXCERPT_TAIL_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteTarget {
    Chat(Uuid),
}

pub(crate) struct PaletteRow {
    pub(crate) target: PaletteTarget,
    pub(crate) label: SharedString,
    pub(crate) detail: Option<SharedString>,
    /// The byte ranges of `label` the query matched, sorted, to be drawn
    /// highlighted.
    pub(crate) matched: Vec<Range<usize>>,
    /// Where the query appears in the row's text beyond its label, when it
    /// does.
    pub(crate) excerpt: Option<Excerpt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Excerpt {
    pub(crate) text: SharedString,
    pub(crate) matched: Range<usize>,
}

pub(crate) struct PaletteSection {
    pub(crate) label: SharedString,
    pub(crate) rows: Vec<PaletteRow>,
}

pub(crate) struct RankedRow {
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
            SharedString::from(excerpt)
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
    timeline.iter().flat_map(message_texts)
}

/// [`searchable_texts`] of one message, as a saved chat's search index
/// keeps them (`chat_db.rs`).
pub(crate) fn message_search_texts(message: &TimelineMessage) -> Vec<&str> {
    message_texts(message).collect()
}

fn message_texts(message: &TimelineMessage) -> impl Iterator<Item = &str> {
    match message {
        TimelineMessage::User(group) => {
            Either::Left(group.record.blocks.iter().map(|block| block.text.as_str()))
        }
        TimelineMessage::Agent(AgentMessage { ui, .. }) => {
            Either::Right(iter::once(ui.output.text.as_str()))
        }
    }
}

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
                    .child(move |_, cx| {
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
                                        cx.theme().secondary_foreground,
                                    )))
                                    .when_some(detail.clone(), |this, detail| {
                                        this.child(
                                            div()
                                                .flex_none()
                                                .text_color(
                                                    cx.theme().muted_foreground.opacity(0.7),
                                                )
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
                                        .text_color(cx.theme().muted_foreground.opacity(0.7))
                                        .child(highlighted(
                                            excerpt.text,
                                            &[excerpt.matched],
                                            cx.theme().foreground,
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
        let phrase = query.trim();
        let mut query = PaletteQuery::new(query);
        let chats = self
            .thread_store
            .read(cx)
            .recents(cx)
            .filter_map(|chat| {
                let thread = match chat {
                    ListedChat::Loaded(thread) => thread.read(cx),
                    // Its text is searched in the database, asynchronously.
                    ListedChat::Unloaded(meta) => {
                        let found = (self.saved_chat_search.phrase == phrase)
                            .then(|| self.saved_chat_search.found.get(&meta.id))
                            .flatten();
                        return query.row(
                            PaletteTarget::Chat(meta.id),
                            meta.title.clone().into(),
                            None,
                            found.into_iter().flatten().map(String::as_str),
                        );
                    }
                };
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
        self.saved_chat_search = SavedChatSearch::default();
        let cowork = cx.entity().downgrade();
        let focus_on_mount = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, cx| {
            let state = state.clone();
            let cowork = cowork.clone();
            let focus_on_mount = focus_on_mount.clone();
            dialog
                .w(px(560.))
                .margin_top(px(120.))
                .p_0()
                .bg(cx.theme().popover)
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
                        .empty(|_, _, cx| {
                            div()
                                .py_6()
                                .w_full()
                                .text_center()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground.opacity(0.7))
                                .child("No chats found")
                        })
                        // The rows are ranked while the dialog renders, so a
                        // new query needs a new render to take effect.
                        .on_query({
                            let cowork = cowork.clone();
                            move |query, window, cx| {
                                _ = cowork.update(cx, |cowork, cx| {
                                    cowork.search_saved_chats(query, window, cx)
                                });
                                window.refresh();
                            }
                        })
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

    /// Starts searching unloaded chats' saved text for `query`, unless that
    /// search is already done or under way. Its answer refreshes the palette.
    pub(crate) fn search_saved_chats(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let phrase = query.trim();
        if self.saved_chat_search.phrase == phrase {
            return;
        }
        self.saved_chat_search = SavedChatSearch {
            phrase: phrase.to_owned(),
            ..SavedChatSearch::default()
        };
        let Some(db) = &self.chat_db else {
            return;
        };
        let search = db.search(phrase);
        let phrase = phrase.to_owned();
        self.saved_chat_search.searching = Some(cx.spawn_in(window, async move |this, cx| {
            let hits = match search.await {
                Ok(hits) => hits,
                Err(error) => {
                    eprintln!("could not search saved chats: {error:#}");
                    return;
                }
            };
            _ = this.update_in(cx, |this, window, _| {
                let search = &mut this.saved_chat_search;
                if search.phrase != phrase {
                    return;
                }
                for SearchHit { thread_id, text } in hits {
                    search.found.entry(thread_id).or_default().push(text);
                }
                window.refresh();
            });
        }));
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

    fn search(chats: &[(&str, &[&str])], query: &str) -> Vec<PaletteRow> {
        let mut query = PaletteQuery::new(query);
        let rows = chats
            .iter()
            .filter_map(|(title, texts)| {
                query.row(
                    PaletteTarget::Chat(Uuid::new_v4()),
                    SharedString::from(*title),
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
    fn titles_match_fuzzily_word_by_word() {
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

        // Each word must match on its own, in any order.
        let chats = titled(&["Set upstream Git remote", "Remove untracked file"]);
        assert_eq!(
            found_titles(&chats, "remote git"),
            ["Set upstream Git remote"]
        );
        assert!(found_titles(&chats, "remote zzz").is_empty());
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
    }

    #[test]
    fn title_matches_rank_above_message_matches_and_still_show_them() {
        let chats: [(&str, &[&str]); 2] = [
            ("Weekend plans", &["Let's talk about the deploy"]),
            ("Deploy checklist", &["Run the deploy script"]),
        ];
        let rows = search(&chats, "deploy");
        let titles: Vec<_> = rows.iter().map(|row| row.label.to_string()).collect();
        assert_eq!(titles, ["Deploy checklist", "Weekend plans"]);
        let row = &rows[0];
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
        let cut = excerpt(
            &text,
            text.find("needle").unwrap()..text.find("needle").unwrap() + 6,
        );
        let (line, matched) = excerpt_parts(&cut);
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
            &cut.text[..cut.matched.start],
            "…word word word word word the "
        );

        // Short messages are shown whole.
        let text = "Café is\nhere";
        let whole = excerpt(text, 0.."Café".len());
        assert_eq!(excerpt_parts(&whole), ("Café is here", "Café"));
    }
}
