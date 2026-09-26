//! Commenting on agent messages: creating comments from selections, and
//! how messages are split and highlighted around them.

use super::*;

/// A segment of a rendered agent message: its Markdown, and each
/// highlight as the range and text of the rendered text it paints.
type HighlightedSegment = (String, Vec<(Range<usize>, String)>);

/// Renders `markdown` as an agent message `wrap_width` wide, with a comment
/// on each of `ranges`, and returns its segments.
fn highlighted_segments(
    cx: &mut gpui::TestAppContext,
    markdown: &'static str,
    ranges: &[Range<usize>],
    wrap_width: f32,
) -> Vec<HighlightedSegment> {
    struct HighlightRoot {
        cowork: Entity<Cowork>,
        text_view: Entity<TextViewState>,
        id: ThreadMessageId,
        markdown: &'static str,
        comments: Vec<UserComment>,
        wrap_width: gpui::Pixels,
    }

    impl Render for HighlightRoot {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let content = self.cowork.update(cx, |cowork, cx| {
                cowork.render_agent_text(
                    self.id,
                    self.markdown,
                    &self.text_view,
                    &self.comments,
                    self.wrap_width,
                    window,
                    cx,
                )
            });
            div().w(self.wrap_width).flex().flex_col().children(content)
        }
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let tokio_handle = runtime.handle().clone();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let id = ThreadMessageId {
            thread_id: Uuid::new_v4(),
            message_id: Uuid::new_v4(),
        };
        let draft = ThreadDraft::new(ParticipantId::new());
        for range in ranges {
            draft.doc.create_comment(
                draft.author.as_uuid(),
                CommentTarget {
                    message_id: id.message_id,
                    quote: markdown[range.clone()].into(),
                    range: range.clone(),
                },
                "comment",
            );
        }
        let thread_store = cx.new(|_| ThreadStore::default());
        HighlightRoot {
            cowork: cx.new(|cx| test_cowork(thread_store, None, tokio_handle, window, cx)),
            text_view: cx.new(|cx| TextViewState::markdown(markdown, cx)),
            id,
            markdown,
            comments: draft.comment_views(&[]),
            wrap_width: px(wrap_width),
        }
    });
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    root.read_with(cx, |root, cx| {
        let cowork = root.cowork.read(cx);
        let segments = cowork
            .shown_segments
            .get(&root.id)
            .and_then(|shown| shown.segments.as_ref())
            .expect("a commented message is split");
        segments
            .iter()
            .map(|segment| {
                let view = &cowork.segment_text_views[&(root.id, segment.source_range.start)];
                assert_eq!(view.text, root.markdown[segment.source_range.clone()]);
                let (text, highlights) = view.highlights.as_ref().expect("highlights are set");
                let highlights = highlights
                    .iter()
                    .map(|highlight| {
                        let range = highlight.range();
                        (range.clone(), text.as_str()[range].to_string())
                    })
                    .collect();
                (view.text.clone(), highlights)
            })
            .collect()
    })
}

/// The text each highlight of a one-segment message paints.
fn highlighted_texts(
    cx: &mut gpui::TestAppContext,
    markdown: &'static str,
    ranges: &[Range<usize>],
) -> Vec<String> {
    let segments = highlighted_segments(cx, markdown, ranges, 600.);
    let [(_, highlights)] = segments.as_slice() else {
        panic!("{markdown:?} is one segment, got {segments:?}");
    };
    highlights.iter().map(|(_, text)| text.clone()).collect()
}

/// The range of `markdown` from `start` through the next `end`.
fn through(markdown: &str, start: &str, end: &str) -> Range<usize> {
    let start_offset = markdown.find(start).expect("selection start");
    let end_offset = markdown[start_offset..]
        .find(end)
        .map(|offset| start_offset + offset + end.len())
        .expect("selection end");
    start_offset..end_offset
}

#[gpui::test]
fn comments_highlight_the_text_rendered_from_their_source(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let cases: [(&str, &'static str, Range<usize>, &str); 12] = [
        (
            "plain",
            "Before selected text after",
            7..20,
            "selected text",
        ),
        ("heading", "### A Heading", 6..13, "Heading"),
        ("inside bold", "**Hi**", 3..4, "i"),
        (
            "across opening bold edge",
            "Before **bold text** after",
            through("Before **bold text** after", "re ", "bold"),
            "re bold",
        ),
        (
            "across closing bold edge",
            "Before **bold text** after",
            through("Before **bold text** after", "text", " af"),
            "text af",
        ),
        (
            "whole bold section",
            "Before **bold text** after",
            through("Before **bold text** after", "**bold", "text**"),
            "bold text",
        ),
        (
            "across several styled sections",
            "A **bold** and *italic* tail",
            through("A **bold** and *italic* tail", "bold", " ta"),
            "bold and italic ta",
        ),
        (
            "nested styles",
            "Start **bold and *italic*** end",
            through("Start **bold and *italic*** end", "and ", "italic"),
            "and italic",
        ),
        ("whole inline code", "Use `value` now", 4..11, "value"),
        ("part of inline code", "Use `value` now", 6..9, "alu"),
        (
            "heading and emphasis",
            "### A **styled heading** here",
            through("### A **styled heading** here", "A ", "** h"),
            "A styled heading h",
        ),
        (
            "across inline code",
            "In Rust, we use `u128` to handle larger numbers",
            0.."In Rust, we use `u128` to handle larger numbers".len(),
            "In Rust, we use u128 to handle larger numbers",
        ),
    ];
    for (name, markdown, range, expected) in cases {
        assert_eq!(
            highlighted_texts(cx, markdown, std::slice::from_ref(&range)),
            [expected],
            "{name}"
        );
    }
}

#[gpui::test]
fn intersecting_comments_each_highlight_their_text(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    assert_eq!(
        highlighted_texts(cx, "overlapping", &[0..7, 4..11]),
        ["overlap", "lapping"]
    );
}

#[gpui::test]
fn comments_highlight_the_occurrence_they_are_on(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let second = 16..20;
    let segments = highlighted_segments(
        cx,
        "**same** then **same**",
        std::slice::from_ref(&second),
        600.,
    );
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].1, [(10..14, "same".to_string())]);
}

#[gpui::test]
fn messages_split_after_the_line_a_comment_ends_on(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let markdown = "First paragraph.\n\nSecond has a comment here.\n\nThird paragraph.";
    let comment = through(markdown, "comment", "comment");
    assert_eq!(
        highlighted_segments(cx, markdown, &[comment], 600.),
        [
            (
                "First paragraph.\n\nSecond has a comment here.\n".to_string(),
                vec![(30..37, "comment".to_string())],
            ),
            ("\nThird paragraph.".to_string(), Vec::new()),
        ]
    );
}

#[gpui::test]
fn a_comment_past_its_line_highlights_into_the_next_segment(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let markdown = "one two\n\nthree four\n\nfive";
    // The first comment ends the line its group is placed after; the
    // second starts on it and runs into the next paragraph.
    let segments = highlighted_segments(
        cx,
        markdown,
        &[4..7, through(markdown, "two", "three")],
        600.,
    );
    let texts = segments
        .iter()
        .map(|(text, highlights)| {
            let highlights = highlights
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>();
            (text.as_str(), highlights)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        [
            ("one two\n", vec!["two", "two"]),
            ("\nthree four\n\nfive", vec!["three"]),
        ]
    );
}

fn assert_backslash_selection_creates_comment(
    cx: &mut gpui::TestAppContext,
    markdown: &'static str,
    expected_quote: &str,
    expected_range: Range<usize>,
    target_comment_reply: bool,
    existing_comment_range: Option<Range<usize>>,
    selection_start_x: f32,
    selection_end_x: f32,
    expected_highlights_after_comment: Option<usize>,
) {
    struct SelectionRoot {
        cowork: Entity<Cowork>,
        text_view: Entity<TextViewState>,
        composer: Entity<TextareaState>,
        thread_id: Uuid,
        message_id: Uuid,
        markdown: &'static str,
        comments: Vec<UserComment>,
    }

    impl Render for SelectionRoot {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let content = self.cowork.update(cx, |cowork, cx| {
                cowork.render_agent_text(
                    ThreadMessageId {
                        thread_id: self.thread_id,
                        message_id: self.message_id,
                    },
                    self.markdown,
                    &self.text_view,
                    &self.comments,
                    px(160.),
                    window,
                    cx,
                )
            });
            div()
                .w(px(160.))
                .flex()
                .flex_col()
                .on_key_down(cx.listener(|this, event, window, cx| {
                    this.cowork.update(cx, |cowork, cx| {
                        cowork.begin_inline_comment(event, window, cx);
                    });
                }))
                .child(TextSelectionLayer)
                .child(
                    div()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.cowork.update(cx, |cowork, _| {
                                    cowork.selection_message_id = Some(this.message_id);
                                });
                            }),
                        )
                        .children(content),
                )
                .child(Textarea::new(&self.composer))
        }
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let tokio_handle = runtime.handle().clone();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let message_id = Uuid::new_v4();
        let text_view = cx.new(|cx| TextViewState::markdown(markdown, cx));
        let main_text = if target_comment_reply { "" } else { markdown };
        let main_text_view = if target_comment_reply {
            cx.new(|cx| TextViewState::markdown("", cx))
        } else {
            text_view.clone()
        };
        let thinking_view = cx.new(|cx| TextViewState::markdown("", cx));
        let thread_id = Uuid::new_v4();
        let draft = ThreadDraft::new(ParticipantId::new());
        if let Some(range) = existing_comment_range.clone() {
            draft.doc.create_comment(
                draft.author.as_uuid(),
                CommentTarget {
                    message_id,
                    quote: markdown[range.clone()].into(),
                    range,
                },
                "Existing comment",
            );
        }
        let comments = draft.comment_views(&[]);
        // Stands in for the composer, which has focus before commenting.
        let composer = Cowork::new_draft_editor("", window, cx);
        let timeline = vec![TimelineMessage::Agent(AgentMessage {
            id: if target_comment_reply {
                Uuid::new_v4()
            } else {
                message_id
            },
            comment_group_id: None,
            started_at: SystemTime::UNIX_EPOCH,
            comment_responses: target_comment_reply
                .then(|| AgentCommentResponse {
                    id: message_id,
                    comment_id: Uuid::new_v4(),
                    response: markdown.into(),
                    response_view: text_view.clone(),
                })
                .into_iter()
                .collect(),
            prompt: 0,
            pending_events: Vec::new(),
            run: crate::protocol::AgentRun::Ended {
                failure: None,
                duration: std::time::Duration::ZERO,
            },
            output: crate::timeline::AgentOutput {
                thinking_complete: true,
                text: main_text.into(),
                ..Default::default()
            },
            committed: Default::default(),
            comment_calls_checked: 0,
            thinking_view,
            text_view: main_text_view,
            thinking_expanded: false,
            tool_calls_expanded: false,
        })];
        let thread = cx.new(|_| test_thread(thread_id, timeline, draft));
        let thread_store = cx.new(|_| ThreadStore {
            threads: VecDeque::from([thread]),
        });
        let cowork =
            cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
        composer.focus_handle(cx).focus(window, cx);
        SelectionRoot {
            cowork,
            text_view,
            composer,
            thread_id,
            message_id,
            markdown,
            comments,
        }
    });
    let cx: &mut gpui::VisualTestContext = cx;
    cx.run_until_parked();
    // An existing comment splits the message, whose segments replace it
    // once they have been laid out.
    for _ in 0..2 {
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }
    cx.simulate_mouse_down(
        point(px(selection_start_x), px(8.)),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_mouse_move(
        point(px(selection_end_x), px(8.)),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_mouse_up(
        point(px(selection_end_x), px(8.)),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });

    // The view of the segment holding the existing comment's line.
    let segment_view = |view: &SelectionRoot, cx: &App| {
        view.cowork
            .read(cx)
            .segment_text_views
            .iter()
            .find(|((segment, start), _)| segment.message_id == view.message_id && *start == 0)
            .map(|(_, segment)| segment.state.entity_id())
            .expect("commented segment")
    };
    let segment_view_before = expected_highlights_after_comment
        .map(|_| view.read_with(cx, |view, cx| segment_view(view, cx)));
    cx.simulate_keystrokes("x");

    view.read_with(cx, |view, cx| {
        let cowork = view.cowork.read(cx);
        let thread = cowork
            .thread_store
            .read(cx)
            .thread(cowork.active_thread_id.expect("active thread"), cx)
            .expect("thread");
        let thread = thread.read(cx);
        // New comments are appended after any existing one.
        let Some(comment) = thread
            .draft
            .comment_views(&[])
            .into_iter()
            .last()
            .filter(|comment| matches!(comment.body, UserCommentBody::Editing { .. }))
        else {
            panic!("typing with the selection should create an editable comment");
        };
        assert_eq!(comment.reference.quote, expected_quote);
        assert_eq!(comment.reference.range, expected_range);
        assert_eq!(comment.author, thread.draft.author);
        let UserCommentBody::Editing { inline, composer } = &comment.body else {
            unreachable!();
        };
        assert_eq!(inline.read(cx).value(), "x");
        assert_eq!(composer.read(cx).value(), "x");
    });
    cx.update(|window, cx| {
        let cowork = view.read(cx).cowork.read(cx);
        let (_, slot, _) = cowork
            .focused_draft_editor(window, cx)
            .expect("a comment editor should have focus");
        assert!(matches!(slot, EditorSlot::CommentInline(_)));
    });

    if let Some(expected_highlights) = expected_highlights_after_comment {
        let comments = view.read_with(cx, |view, cx| {
            let cowork = view.cowork.read(cx);
            cowork
                .thread_store
                .read(cx)
                .thread(cowork.active_thread_id.expect("active thread"), cx)
                .expect("thread")
                .read(cx)
                .draft
                .comment_views(&[])
        });
        view.update(cx, |view, cx| {
            view.comments = comments;
            cx.notify();
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        view.read_with(cx, |view, cx| {
            let highlight_count = view
                .cowork
                .read(cx)
                .segment_text_views
                .iter()
                .filter(|((segment, _), _)| segment.message_id == view.message_id)
                .filter_map(|(_, segment)| segment.highlights.as_ref())
                .map(|(_, highlights)| highlights.len())
                .sum::<usize>();
            assert_eq!(highlight_count, expected_highlights);
            // Highlighting another comment on the same line keeps its view.
            assert_eq!(Some(segment_view(view, cx)), segment_view_before);
        });
    }
}

#[gpui::test]
fn backslash_selections_create_comments_with_gpui_ranges(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    assert_backslash_selection_creates_comment(
        cx,
        r"a\b",
        r"a\b",
        0..3,
        false,
        None,
        1.,
        155.,
        None,
    );
    assert_backslash_selection_creates_comment(
        cx,
        r"a\\b",
        r"a\b",
        0..4,
        false,
        None,
        1.,
        155.,
        None,
    );
}

#[gpui::test]
fn comments_can_target_agent_comment_replies(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    assert_backslash_selection_creates_comment(
        cx,
        "Agent reply",
        "Agent reply",
        0..11,
        true,
        None,
        1.,
        155.,
        None,
    );
}

#[gpui::test]
fn comments_can_target_text_before_an_existing_comment(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    assert_backslash_selection_creates_comment(
        cx,
        "alpha beta gamma",
        "alpha",
        0..5,
        false,
        Some(11..16),
        1.,
        48.,
        None,
    );
}

#[gpui::test]
fn creating_comment_immediately_before_existing_preserves_both_highlights(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(gpui_component::init);
    assert_backslash_selection_creates_comment(
        cx,
        "alpha beta gamma",
        "beta",
        6..10,
        false,
        Some(11..16),
        54.,
        96.,
        Some(2),
    );
}

#[gpui::test]
fn comments_after_an_existing_comment_keep_original_source_offsets(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    assert_backslash_selection_creates_comment(
        cx,
        "alpha beta gamma",
        "gamma",
        11..16,
        false,
        Some(0..5),
        104.,
        155.,
        Some(2),
    );
}

#[test]
fn comment_instructions_name_each_comment_author() {
    let author = ParticipantId::from_bytes([7; 16]);
    let turn_comments = TurnComments::new(1);
    let preface = Cowork::comments_preface(
        &[UserComment {
            id: Uuid::new_v4(),
            author,
            presence: ItemPresence::default(),
            reference: CommentReference {
                message_id: Uuid::new_v4(),
                range: 0..5,
                quote: "quote".into(),
            },
            body: UserCommentBody::Submitted(" why? ".into()),
        }],
        turn_comments.comment_ids(),
        &[],
        &HashMap::new(),
    )
    .expect("comments need instructions");

    assert!(preface.contains("comment_1 — Mossy Crane, on an excerpt"));
    assert!(preface.contains("> quote\nComment: why?"));
    assert_eq!(
        Cowork::comments_preface(&[], &[], &[], &HashMap::new()),
        None
    );
}

/// Commenting on a long response splits it into freshly parsed segments.
/// gpui-kit parses large Markdown in the background, so until then those
/// segments are empty; the timeline must not collapse (and clamp its
/// scroll offset) while they are.
#[gpui::test]
fn commenting_on_a_long_response_keeps_the_timeline_scroll_position(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let tokio_handle = runtime.handle().clone();
    let markdown = (1..=80)
        .map(|index| {
            format!(
                "Paragraph {index}. The quick brown fox jumps over the lazy dog, \
                 then circles back to see whether the dog noticed anything.\n\n"
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let quote = "Paragraph 79.";
    let quote_start = markdown.find(quote).expect("quoted paragraph");
    assert!(
        quote_start > 4 * 1024,
        "the text before the comment must be parsed in the background"
    );

    let thread_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let timeline = vec![TimelineMessage::Agent(AgentMessage {
            id: message_id,
            comment_group_id: None,
            started_at: SystemTime::UNIX_EPOCH,
            comment_responses: Vec::new(),
            prompt: 0,
            pending_events: Vec::new(),
            run: crate::protocol::AgentRun::Ended {
                failure: None,
                duration: std::time::Duration::ZERO,
            },
            output: crate::timeline::AgentOutput {
                thinking_complete: true,
                text: markdown.clone(),
                ..Default::default()
            },
            committed: Default::default(),
            comment_calls_checked: 0,
            thinking_view: cx.new(|cx| TextViewState::markdown("", cx)),
            text_view: cx.new(|cx| TextViewState::markdown(&markdown, cx)),
            thinking_expanded: false,
            tool_calls_expanded: false,
        })];
        let draft = ThreadDraft::new(ParticipantId::new());
        let thread = cx.new(|_| test_thread(thread_id, timeline, draft));
        let thread_store = cx.new(|_| ThreadStore {
            threads: VecDeque::from([thread]),
        });
        let cowork =
            cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
        Root::new(cowork, window, cx)
    });
    let cowork = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<Cowork>()
            .expect("cowork root")
    });
    let settle = |cx: &mut gpui::VisualTestContext| {
        for _ in 0..4 {
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
    };
    let scroll = |cx: &mut gpui::VisualTestContext| {
        cowork.read_with(cx, |cowork, _| {
            (
                cowork.timeline_scroll_handle.offset().y,
                cowork.timeline_scroll_handle.max_offset().y,
            )
        })
    };

    settle(cx);
    cowork.update(cx, |cowork, cx| {
        cowork.timeline_scroll_handle.scroll_to_bottom();
        cx.notify();
    });
    settle(cx);
    let (offset_before, max_before) = scroll(cx);
    assert!(
        max_before > px(500.),
        "the response must overflow the window, max offset {max_before:?}"
    );
    assert_eq!(offset_before, -max_before);

    let comment_id = cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            let thread = cowork
                .thread_store
                .read(cx)
                .thread(thread_id, cx)
                .expect("thread");
            let comment_id = thread.update(cx, |thread, _| {
                let draft = &mut thread.draft;
                draft.doc.create_comment(
                    draft.author.as_uuid(),
                    CommentTarget {
                        message_id,
                        range: quote_start..quote_start + quote.len(),
                        quote: quote.into(),
                    },
                    "x",
                )
            });
            cowork.focus_draft_editor(
                thread.read(cx).draft.id,
                EditorSlot::CommentInline(comment_id),
                None,
                window,
                cx,
            );
            cx.notify();
            comment_id
        })
    });
    // The frame right after the comment appears, before any background
    // parse has had a chance to finish.
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let (offset_first_frame, max_first_frame) = scroll(cx);
    assert!(
        max_first_frame >= max_before,
        "the timeline collapsed from {max_before:?} to {max_first_frame:?} \
         while the new segments were parsed"
    );
    assert_eq!(offset_first_frame, offset_before);
    let inline_selector: &'static str =
        Box::leak(format!("inline-comment-{comment_id}").into_boxed_str());
    assert!(
        cx.debug_bounds(inline_selector).is_none(),
        "the new editor must not appear at the end of the unsplit response"
    );

    settle(cx);
    assert!(
        cx.debug_bounds(inline_selector).is_some(),
        "the editor should appear at its anchor after parsing"
    );
    let (offset_after, _) = scroll(cx);
    assert!(
        (offset_after - offset_before).abs() < px(1.),
        "the timeline scrolled from {offset_before:?} to {offset_after:?}"
    );
    cowork.read_with(cx, |cowork, _| {
        let id = ThreadMessageId {
            thread_id,
            message_id,
        };
        let shown = cowork
            .shown_segments
            .get(&id)
            .expect("the commented response is split");
        assert!(shown.pending_since.is_none());
        let segments = shown.segments.as_ref().expect("segments are shown");
        // The quote is highlighted once the segment holding it has parsed.
        let highlighted = segments
            .iter()
            .filter_map(|segment| {
                cowork.segment_text_views[&(id, segment.source_range.start)]
                    .highlights
                    .as_ref()
            })
            .flat_map(|(text, highlights)| {
                highlights
                    .iter()
                    .map(|highlight| text.as_str()[highlight.range()].to_string())
            })
            .collect::<Vec<_>>();
        assert_eq!(highlighted, [quote]);
    });
}
