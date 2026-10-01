//! Rendering a thread's timeline: messages split into segments around
//! their inline comments, the agent's reasoning, and comment groups.

use std::{
    collections::HashSet,
    ops::Range,
    time::{Duration, Instant},
};

use draft::CommentTarget;
use gpui::{
    App, AppContext, Bounds, Context, Entity, HighlightStyle, IntoElement, KeyDownEvent,
    LineFragment, MouseButton, ScrollWheelEvent, Window, canvas, div, prelude::*, px, rems, rgb,
    rgba, size,
};
use gpui_base::{
    RangeHighlight, RenderedText, SelectableText, TextSelection, TextView, TextViewState,
    TextViewStyle, Textarea, input::TextareaState, text::SelectionFormat,
};
use gpui_component::{ActiveTheme, Icon, shimmer::ShimmerText};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use crate::{
    Cowork,
    caret::{paint_caret_label, selection_rects},
    composer_attachments::AttachmentCard,
    participant::ParticipantId,
    protocol::RunOutcome,
    thread_draft::{CARET_LABEL_DURATION, EditorSlot, RemoteCaret},
    timeline::{
        AgentMessage, AgentStep, AgentToolCall, MessageAuthor, StepView, ThreadMessageId,
        TimelineMessage, UserComment, UserCommentBody, UserMessageGroup,
    },
};

/// The text view of a segment, identified by where the segment starts in its
/// message so that one that grows or shrinks keeps its view.
pub(crate) struct SegmentTextView {
    pub(crate) state: Entity<TextViewState>,
    pub(crate) text: String,
    /// The highlights last set on `state`, and the text they were resolved
    /// against. Setting highlights redraws the view, so they are only set
    /// again when either changes.
    pub(crate) highlights: Option<(RenderedText, Vec<RangeHighlight>)>,
    pub(crate) rendered_at: u64,
}

/// A piece of a message split after the lines its comments are on, followed
/// by the inline comments anchored in it.
#[derive(Clone)]
pub(crate) struct MessageSegment {
    pub(crate) source_range: Range<usize>,
    pub(crate) state: Entity<TextViewState>,
    /// Whether the view renders the segment's text yet. gpui-kit parses large
    /// Markdown in the background, and until then a new view renders nothing
    /// and a changed one its previous text.
    parsed: bool,
    pub(crate) comments: Vec<Uuid>,
}

/// The segments a message is shown split into.
pub(crate) struct ShownSegments {
    /// `None` while the message is still shown whole.
    pub(crate) segments: Option<Vec<MessageSegment>>,
    /// Since when newer segments have been waiting to be parsed.
    pub(crate) pending_since: Option<Instant>,
    pub(crate) rendered_at: u64,
}

/// How long newly split segments may stay unparsed before they are shown
/// anyway.
const SEGMENT_PARSE_TIMEOUT: Duration = Duration::from_secs(1);

impl Cowork {
    fn selected_message_source_range(
        &self,
        thread_message_id: ThreadMessageId,
        text_view: &Entity<TextViewState>,
        cx: &App,
    ) -> Option<Range<usize>> {
        let segments = self
            .shown_segments
            .get(&thread_message_id)
            .and_then(|shown| shown.segments.as_deref())
            .unwrap_or_default();
        // Each segment's view renders a slice of the message's Markdown.
        let mut selected_ranges = segments.iter().filter_map(|segment| {
            let range = segment.state.read(cx).selected_source_range()?;
            let start = segment.source_range.start;
            Some((range.start + start)..(range.end + start))
        });
        let first = selected_ranges.next();
        let segmented = selected_ranges.fold(first, |combined, range| {
            Some(match combined {
                Some(combined) => combined.start.min(range.start)..combined.end.max(range.end),
                None => range,
            })
        });

        segmented.or_else(|| text_view.read(cx).selected_source_range())
    }

    pub(crate) fn begin_inline_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .is_some_and(|thread| !thread.read(cx).ownership.can_write())
        {
            return;
        }

        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.platform
            || event.keystroke.modifiers.function
        {
            return;
        }
        let Some(initial_text) = event.keystroke.key_char.as_deref() else {
            return;
        };
        if initial_text.chars().all(char::is_control) {
            return;
        }
        let quote = TextSelection::selected_text(window, cx).trim().to_string();
        let (Some(thread_id), Some(preferred_message_id)) =
            (self.active_thread_id, self.selection_message_id)
        else {
            return;
        };
        if quote.is_empty() {
            return;
        }
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let (message_id, source_range) = {
            let thread = thread.read(cx);
            let mut targets = thread
                .timeline
                .iter()
                .filter_map(|entry| match entry {
                    TimelineMessage::Agent(message) => Some(
                        std::iter::once((message.id, message.text_view.clone())).chain(
                            message
                                .comment_responses
                                .iter()
                                .map(|response| (response.id, response.response_view.clone())),
                        ),
                    ),
                    TimelineMessage::User(_) => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            targets.sort_by_key(|(message_id, _)| *message_id != preferred_message_id);
            let Some(target) = targets.into_iter().find_map(|(message_id, text_view)| {
                let thread_message_id = ThreadMessageId {
                    thread_id,
                    message_id,
                };
                self.selected_message_source_range(thread_message_id, &text_view, cx)
                    .map(|source_range| (message_id, source_range))
            }) else {
                return;
            };
            target
        };

        let (draft_id, comment_id) = thread.update(cx, |thread, _| {
            let target = CommentTarget {
                message_id,
                range: source_range,
                quote,
            };
            let draft = &mut thread.draft;
            let comment_id = draft
                .doc
                .create_comment(draft.author.as_uuid(), target, initial_text);
            draft.comments_folded = false;
            let draft_id = draft.id;
            thread.flush_draft();
            (draft_id, comment_id)
        });
        TextSelection::clear(window, cx);
        self.focus_draft_editor(
            draft_id,
            EditorSlot::CommentInline(comment_id),
            None,
            window,
            cx,
        );
        window.prevent_default();
        cx.stop_propagation();
    }

    fn toggle_comment_group(&mut self, group_id: Uuid, cx: &mut Context<Self>) {
        if self.new_thread_draft.id == group_id {
            self.new_thread_draft.comments_folded = !self.new_thread_draft.comments_folded;
            cx.notify();
            return;
        }

        let threads = self.thread_store.read(cx).threads.clone();
        for thread in threads {
            let toggled = thread.update(cx, |thread, _| {
                if thread.draft.id == group_id {
                    thread.draft.comments_folded = !thread.draft.comments_folded;
                    return true;
                }
                for entry in &mut thread.timeline {
                    if let TimelineMessage::User(group) = entry
                        && group.id == group_id
                    {
                        group.comments_folded = !group.comments_folded;
                        return true;
                    }
                }
                false
            });
            if toggled {
                break;
            }
        }
        cx.notify();
    }

    pub(crate) fn render_comment_group_toggle(
        group_id: Uuid,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = format!("{} comment{}", count, if count == 1 { "" } else { "s" });
        div()
            .id(format!("toggle-comments-{group_id}"))
            .h(px(24.))
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_color(cx.theme().muted_foreground)
            .hover(|this| this.text_color(cx.theme().secondary_foreground))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_comment_group(group_id, cx);
            }))
            .child(label)
            .child(if collapsed { "›" } else { "⌄" })
    }

    fn render_inline_comment(&self, comment: &UserComment, cx: &App) -> gpui::AnyElement {
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(cx.theme().secondary_foreground)
                .child(body.clone())
                .into_any_element(),
            UserCommentBody::Editing { inline, .. } => div()
                .id(format!("comment-editor-inline-{}", comment.id))
                .relative()
                .flex_1()
                .min_w_0()
                .child(Textarea::new(inline))
                .child(self.render_remote_carets(inline, comment.presence.carets.clone()))
                .into_any_element(),
        };

        let comment_id = comment.id;
        div()
            .id(format!("inline-comment-{comment_id}"))
            .debug_selector(move || format!("inline-comment-{comment_id}"))
            .w_full()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_l_2()
                    .border_color(rgb(self.color_of(comment.author)))
                    .child(self.render_layered_avatars(
                        comment.author,
                        &comment.presence.editors,
                        cx,
                    ))
                    .child(body),
            )
            .into_any_element()
    }

    fn markdown_style(cx: &App) -> TextViewStyle {
        let code_background = cx.theme().muted;

        TextViewStyle::default()
            .with_foreground(cx.theme().foreground)
            .with_muted_foreground(cx.theme().muted_foreground)
            .with_link(cx.theme().link)
            .with_selection(cx.theme().selection)
            .with_code_background(code_background)
            .with_border(cx.theme().border)
            .with_paragraph_gap(rems(0.75))
            .with_code_block(
                gpui::StyleRefinement::default()
                    .bg(code_background)
                    .text_color(cx.theme().foreground),
            )
            .with_inline_code(HighlightStyle {
                color: Some(cx.theme().secondary_foreground),
                background_color: Some(code_background),
                ..Default::default()
            })
            .with_table(
                gpui::StyleRefinement::default()
                    .bg(cx.theme().background)
                    .text_color(cx.theme().foreground),
            )
            .with_table_head(
                gpui::StyleRefinement::default()
                    .bg(code_background)
                    .text_color(cx.theme().secondary_foreground),
            )
            .with_table_cell(gpui::StyleRefinement::default().text_color(cx.theme().foreground))
            .with_dark(cx.theme().is_dark())
    }

    /// The background of the text a comment by `author` is on.
    fn comment_highlight(&self, author: ParticipantId) -> gpui::Hsla {
        gpui::Hsla::from(rgb(self.color_of(author))).opacity(0.3)
    }

    /// The segment of a message rendering `source_range` of its Markdown, which
    /// is `text`, with `highlights` given as ranges of `text`.
    fn message_segment(
        &mut self,
        thread_message_id: ThreadMessageId,
        source_range: Range<usize>,
        text: &str,
        highlights: &[(Range<usize>, gpui::Hsla)],
        cx: &mut Context<Self>,
    ) -> MessageSegment {
        let text_view = self
            .segment_text_views
            .entry((thread_message_id, source_range.start))
            .or_insert_with(|| SegmentTextView {
                state: cx.new(|cx| TextViewState::markdown(text, cx)),
                text: text.to_owned(),
                highlights: None,
                rendered_at: self.render_generation,
            });
        text_view.rendered_at = self.render_generation;
        if text_view.text != text {
            text_view.text.clear();
            text_view.text.push_str(text);
            text_view
                .state
                .update(cx, |view, cx| view.set_text(text, cx));
        }

        // The highlights address `text`, so they wait for the view to render
        // it; until then it keeps those it had, following its old text.
        let rendered = text_view.state.read(cx).rendered_text();
        let parsed = rendered.source() == text;
        if parsed {
            let highlights = highlights
                .iter()
                .filter_map(|(range, background)| {
                    let range = rendered.range_for_source(range.clone())?;
                    Some(RangeHighlight::new(range, *background))
                })
                .collect::<Vec<_>>();
            let applied = (rendered, highlights);
            if text_view.highlights.as_ref() != Some(&applied) {
                text_view.state.update(cx, |view, cx| {
                    let set = view.set_range_highlights(applied.1.clone(), cx);
                    debug_assert!(set.is_ok(), "highlights of its own text: {set:?}");
                });
                text_view.highlights = Some(applied);
            }
        }

        MessageSegment {
            source_range,
            state: text_view.state.clone(),
            parsed,
            comments: Vec::new(),
        }
    }

    fn render_message_segment(segment: &MessageSegment, cx: &App) -> gpui::AnyElement {
        TextView::new(&segment.state)
            .selection_format(SelectionFormat::Plain)
            .style(Self::markdown_style(cx))
            .w_full()
            .into_any_element()
    }

    /// `segments` with their inline comments, followed by those of
    /// `required` they do not place.
    fn render_segments(
        &self,
        segments: &[MessageSegment],
        comments: &[&UserComment],
        required: &[Uuid],
        cx: &App,
    ) -> Vec<gpui::AnyElement> {
        let mut content = Vec::new();
        let mut placed = HashSet::new();
        for segment in segments {
            content.push(Self::render_message_segment(segment, cx));
            for id in &segment.comments {
                if let Some(comment) = comments.iter().find(|comment| comment.id == *id) {
                    placed.insert(*id);
                    content.push(self.render_inline_comment(comment, cx));
                }
            }
        }
        content.extend(
            comments
                .iter()
                .filter(|comment| required.contains(&comment.id) && !placed.contains(&comment.id))
                .map(|comment| self.render_inline_comment(comment, cx)),
        );
        content
    }

    fn hard_line_start(text: &str, offset: usize) -> usize {
        text[..offset].rfind('\n').map_or(0, |offset| offset + 1)
    }

    fn wrapped_line_end(
        text: &str,
        selection_end: usize,
        wrap_width: gpui::Pixels,
        window: &mut Window,
    ) -> usize {
        let hard_line_start = Self::hard_line_start(text, selection_end);
        let hard_line_end = text[selection_end..]
            .find('\n')
            .map_or(text.len(), |offset| selection_end + offset);
        let hard_line = &text[hard_line_start..hard_line_end];
        let selected_end_in_line = selection_end - hard_line_start;
        let font_size = px(14.);
        let mut wrapper = window
            .text_system()
            .line_wrapper(window.text_style().font(), font_size);
        let fragments = [LineFragment::text(hard_line)];
        let end = wrapper
            .wrap_line(&fragments, wrap_width)
            .map(|boundary| boundary.ix)
            .find(|boundary| *boundary >= selected_end_in_line)
            .unwrap_or(hard_line.len());
        let source_end = hard_line_start + end;
        if source_end == hard_line_end && hard_line_end < text.len() {
            hard_line_end + 1
        } else {
            source_end
        }
    }

    /// A submitted user message; `cards` holds each block's attachments.
    fn render_user_message_group(
        &self,
        index: usize,
        group: &UserMessageGroup,
        cards: Vec<Vec<AttachmentCard>>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut rows = Vec::new();
        if !group.comments.is_empty() {
            let mut content = vec![
                Self::render_comment_group_toggle(
                    group.id,
                    group.comments.len(),
                    group.comments_folded,
                    cx,
                )
                .into_any_element(),
            ];
            if !group.comments_folded {
                content.extend(
                    group
                        .comments
                        .iter()
                        .map(|comment| self.render_composer_comment(comment, cx)),
                );
            }
            rows.push(self.render_comment_group_row(&group.comments, content, cx));
        }
        for (block_index, (block, cards)) in group.blocks.iter().zip(cards).enumerate() {
            let mut content = Vec::new();
            if !cards.is_empty() {
                content.push(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(
                            cards
                                .iter()
                                .map(|card| self.render_attachment(card, None, cx)),
                        )
                        .into_any_element(),
                );
            }
            if !block.text.trim().is_empty() {
                let block_id = block.id;
                content.push(
                    div()
                        .debug_selector(move || format!("timeline-user-text-{block_id}"))
                        .child(
                            SelectableText::new(
                                format!("timeline-user-text-{block_id}"),
                                block.text.clone(),
                            )
                            .document_order((index * 1_000 + block_index) as u64),
                        )
                        .into_any_element(),
                );
            }
            rows.push(self.render_message_row(Some(MessageAuthor::User(block.author)), content));
        }

        div()
            .id(("timeline-message", index))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.selection_message_id = None;
                    this.timeline_focus_handle.focus(window, cx);
                }),
            )
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .children(rows)
            .into_any_element()
    }

    /// The row of a group of comments. Its gutter shows who wrote them, so
    /// that a folded group is still recognizably someone's message.
    pub(crate) fn render_comment_group_row(
        &self,
        comments: &[UserComment],
        content: impl IntoIterator<Item = gpui::AnyElement>,
        cx: &App,
    ) -> gpui::Div {
        let authors = Self::comment_authors(comments);
        match authors.split_first() {
            Some((first, rest)) => self.render_presence_row(*first, rest, content, cx),
            None => self.render_message_row(None, content),
        }
    }

    /// The distinct authors of `comments`, in the order they first commented.
    pub(crate) fn comment_authors(comments: &[UserComment]) -> Vec<ParticipantId> {
        comments.iter().fold(Vec::new(), |mut authors, comment| {
            if !authors.contains(&comment.author) {
                authors.push(comment.author);
            }
            authors
        })
    }

    /// A composer row whose gutter shows `primary` with everyone else in the
    /// row layered below it.
    pub(crate) fn render_presence_row(
        &self,
        primary: ParticipantId,
        others: &[ParticipantId],
        content: impl IntoIterator<Item = gpui::AnyElement>,
        cx: &App,
    ) -> gpui::Div {
        self.render_message_row(None, content)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(40.))
                    .flex()
                    .justify_center()
                    .child(self.render_layered_avatars(primary, others, cx)),
            )
            .relative()
    }

    /// Paints other participants' carets and selections over `editor`.
    pub(crate) fn render_remote_carets(
        &self,
        editor: &Entity<TextareaState>,
        carets: Vec<RemoteCaret>,
    ) -> impl IntoElement {
        let editor = editor.clone();
        let labels = carets
            .iter()
            .map(|caret| {
                (
                    self.name_of(caret.participant),
                    self.color_of(caret.participant),
                )
            })
            .collect::<Vec<_>>();
        canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                // Laid out first: the editor cannot stay borrowed while
                // painting.
                let layout = {
                    let editor = editor.read(cx);
                    let text = editor.value();
                    let Some(text_bounds) = editor.text_bounds() else {
                        return;
                    };
                    carets
                        .iter()
                        .map(|caret| {
                            let clamp = |offset: usize| text.floor_char_boundary(offset);
                            let selection =
                                clamp(caret.selection.start)..clamp(caret.selection.end);
                            let head = clamp(caret.head);
                            (
                                caret,
                                selection_rects(editor, &text, selection, text_bounds),
                                editor.range_to_bounds(&(head..head)),
                            )
                        })
                        .collect::<Vec<_>>()
                };
                for ((caret, selection, head), (name, color)) in layout.into_iter().zip(&labels) {
                    let color = *color;
                    for rect in selection {
                        window.paint_quad(gpui::fill(rect, rgba((color << 8) | 0x40)));
                    }
                    let Some(head) = head else {
                        continue;
                    };
                    window.paint_quad(gpui::fill(
                        Bounds::new(head.origin, size(px(2.), head.size.height)),
                        rgb(color),
                    ));
                    if caret.moved_at.elapsed() < CARET_LABEL_DURATION {
                        paint_caret_label(color, name.clone(), head.origin, window, cx);
                    }
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// One row of the timeline or composer: an avatar gutter, the content, and
    /// a matching gap on the right.
    pub(crate) fn render_message_row(
        &self,
        author: Option<MessageAuthor>,
        content: impl IntoIterator<Item = gpui::AnyElement>,
    ) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .children(author.map(|author| self.render_avatar(author))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(content),
            )
            .child(div().w(px(40.)).flex_none())
    }

    /// Opens or closes step `step_index` of an agent message. Thinking in
    /// progress is always open.
    fn toggle_step(
        &mut self,
        thread_id: Uuid,
        message_id: Uuid,
        step_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        thread.update(cx, |thread, _| {
            for entry in &mut thread.timeline {
                if let TimelineMessage::Agent(message) = entry
                    && message.id == message_id
                {
                    if !message.output.thinking_in_progress(step_index)
                        && let Some(view) = message.step_views.get_mut(step_index)
                    {
                        view.set_expanded(!view.expanded());
                    }
                    break;
                }
            }
        });
        cx.notify();
    }

    /// A stretch of the agent's reasoning: open while the model is writing
    /// it, then collapsed to its header until someone opens it.
    fn render_thinking(
        thread_id: Uuid,
        message: &AgentMessage,
        step_index: usize,
        view: &Entity<TextViewState>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let message_id = message.id;
        let in_progress = message.output.thinking_in_progress(step_index);
        let expanded = in_progress
            || message
                .step_views
                .get(step_index)
                .is_some_and(StepView::expanded);
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .id(format!("toggle-thinking-{message_id}-{step_index}"))
                    .debug_selector(move || format!("toggle-thinking-{message_id}-{step_index}"))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground.opacity(0.7))
                    .when(!in_progress, |this| {
                        this.hover(|this| this.text_color(cx.theme().muted_foreground))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_step(thread_id, message_id, step_index, cx);
                            }))
                    })
                    .child(if in_progress {
                        "Thinking…"
                    } else {
                        "Thinking"
                    }),
            )
            .when(expanded, |this| {
                this.child(
                    div()
                        .debug_selector(move || format!("thinking-{message_id}-{step_index}"))
                        .pl_3()
                        .border_l_1()
                        .border_color(cx.theme().border)
                        .opacity(0.7)
                        .child(
                            TextView::new(view)
                                .selection_format(SelectionFormat::Plain)
                                .style(Self::markdown_style(cx))
                                .w_full(),
                        ),
                )
            })
            .into_any_element()
    }

    /// A tool call as one quiet line, which opens to show what went in and
    /// what came out.
    fn render_tool_call(
        thread_id: Uuid,
        message: &AgentMessage,
        step_index: usize,
        call: &AgentToolCall,
        expanded: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let message_id = message.id;
        let running = call.result.is_none() && message.is_generating();
        let label = |text: &'static str| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground.opacity(0.7))
                .child(text)
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(format!("tool-call-{message_id}-{step_index}"))
                    .debug_selector(move || format!("tool-call-{message_id}-{step_index}"))
                    .h(px(20.))
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground.opacity(0.7))
                    .hover(|this| this.text_color(cx.theme().muted_foreground))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_step(thread_id, message_id, step_index, cx);
                    }))
                    .child(
                        Icon::new(if expanded {
                            AssetIconName::ChevronDown
                        } else {
                            AssetIconName::ChevronRight
                        })
                        .size_3()
                        .flex_none(),
                    )
                    .child(div().flex_none().child(call.call.function.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .opacity(0.7)
                            .child(call.arguments_summary()),
                    )
                    .when(running, |this| {
                        this.child(div().flex_none().child("Running…"))
                    }),
            )
            .when(expanded, |this| {
                this.child(
                    div()
                        .id(format!("tool-call-details-{message_id}-{step_index}"))
                        .debug_selector(move || {
                            format!("tool-call-details-{message_id}-{step_index}")
                        })
                        .ml(px(6.))
                        .mt_1()
                        .mb_2()
                        .pl_3()
                        .border_l_1()
                        .border_color(cx.theme().border)
                        .opacity(0.7)
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_sm()
                        .child(label("Input"))
                        .child(div().child(call.arguments_text()))
                        .child(label("Output"))
                        .child(div().child(call.result_text().unwrap_or_else(|| {
                            if running {
                                "Running…".into()
                            } else {
                                "No result".into()
                            }
                        }))),
                )
            })
            .into_any_element()
    }

    /// An agent message's work in order: its steps except the response.
    /// Consecutive tool calls sit back to back; everything else gets the
    /// timeline's usual spacing.
    fn render_work(
        thread_id: Uuid,
        message: &AgentMessage,
        cx: &Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut rendered = Vec::new();
        let mut calls = Vec::new();
        let flush = |calls: &mut Vec<gpui::AnyElement>, rendered: &mut Vec<gpui::AnyElement>| {
            if !calls.is_empty() {
                rendered.push(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .children(calls.drain(..))
                        .into_any_element(),
                );
            }
        };
        for step_index in message.output.work() {
            let (Some(step), Some(view)) = (
                message.output.steps.get(step_index),
                message.step_views.get(step_index),
            ) else {
                continue;
            };
            match (step, view) {
                (AgentStep::ToolCall(call), view) => calls.push(Self::render_tool_call(
                    thread_id,
                    message,
                    step_index,
                    call,
                    view.expanded(),
                    cx,
                )),
                (AgentStep::Thinking(_), StepView::Thinking { view, .. }) => {
                    flush(&mut calls, &mut rendered);
                    rendered.push(Self::render_thinking(
                        thread_id, message, step_index, view, cx,
                    ));
                }
                (AgentStep::Text(_), StepView::Text { view }) => {
                    flush(&mut calls, &mut rendered);
                    rendered.push(
                        TextView::new(view)
                            .selection_format(SelectionFormat::Plain)
                            .style(Self::markdown_style(cx))
                            .w_full()
                            .into_any_element(),
                    );
                }
                // The views are kept in step with the output.
                _ => {}
            }
        }
        flush(&mut calls, &mut rendered);
        rendered
    }

    /// The line summing up an agent message's work, which opens and closes
    /// it: how long the agent has been working, or worked, and whether it
    /// was stopped or failed.
    fn render_work_summary(
        thread_id: Uuid,
        message: &AgentMessage,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let message_id = message.id;
        // Shimmering while the agent works, to draw the eye; static after.
        let label = match message.run.duration() {
            None => ShimmerText::new(format!(
                "Working for {}",
                format_duration(message.started_at.elapsed().unwrap_or_default())
            ))
            // The text changes every second; a stable id keeps the sweep
            // going instead of restarting it.
            .id(format!("working-{message_id}"))
            .into_any_element(),
            Some(duration) => {
                format!("Worked for {}", format_duration(duration)).into_any_element()
            }
        };
        let outcome = match message.run.outcome() {
            Some(RunOutcome::Stopped) => {
                Some(("Stopped", cx.theme().muted_foreground.opacity(0.7)))
            }
            Some(RunOutcome::Failed(_)) => Some(("Failed", cx.theme().danger)),
            Some(RunOutcome::Completed) | None => None,
        };
        div()
            .w_full()
            .pb_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .child(
                div()
                    .id(format!("toggle-work-{message_id}"))
                    .debug_selector(move || format!("toggle-work-{message_id}"))
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground.opacity(0.7))
                    .hover(|this| this.text_color(cx.theme().muted_foreground))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_work(thread_id, message_id, cx);
                    }))
                    .child(label)
                    .children(outcome.map(|(outcome, color)| {
                        div()
                            .flex()
                            .gap_1()
                            .child("\u{b7}")
                            .child(div().text_color(color).child(outcome))
                    }))
                    .child(
                        Icon::new(if message.work_expanded {
                            AssetIconName::ChevronDown
                        } else {
                            AssetIconName::ChevronRight
                        })
                        .size_3p5(),
                    ),
            )
            .into_any_element()
    }

    fn toggle_work(&mut self, thread_id: Uuid, message_id: Uuid, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        thread.update(cx, |thread, _| {
            for entry in &mut thread.timeline {
                if let TimelineMessage::Agent(message) = entry
                    && message.id == message_id
                {
                    message.work_expanded = !message.work_expanded;
                    break;
                }
            }
        });
        cx.notify();
    }

    /// Redraws every second while the active thread's agent is working, so
    /// its "Working for" line keeps counting.
    pub(crate) fn schedule_working_refresh(&mut self, cx: &mut Context<Self>) {
        if self.working_refresh.is_some()
            || !self
                .active_thread(cx)
                .is_some_and(|thread| thread.read(cx).generating)
        {
            return;
        }
        self.working_refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            _ = this.update(cx, |this, cx| {
                this.working_refresh = None;
                cx.notify();
            });
        }));
    }

    pub(crate) fn render_agent_text(
        &mut self,
        thread_message_id: ThreadMessageId,
        text: &str,
        text_view: &Entity<TextViewState>,
        comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut cursor = 0;
        let mut anchored_comments = comments
            .iter()
            .filter(|comment| comment.reference.message_id == thread_message_id.message_id)
            .filter(|comment| {
                comment.reference.range.start < comment.reference.range.end
                    && comment.reference.range.end <= text.len()
                    && text.is_char_boundary(comment.reference.range.start)
                    && text.is_char_boundary(comment.reference.range.end)
            })
            .collect::<Vec<_>>();
        anchored_comments.sort_by_key(|comment| comment.reference.range.start);

        let whole = |cx: &App| {
            (!text.is_empty()).then(|| {
                TextView::new(text_view)
                    .selection_format(SelectionFormat::Plain)
                    .style(Self::markdown_style(cx))
                    .w_full()
                    .into_any_element()
            })
        };
        if anchored_comments.is_empty() {
            self.shown_segments.remove(&thread_message_id);
            return whole(cx).into_iter().collect();
        }

        // Each comment's range, in the segments it falls in, with its
        // author's color.
        let comment_ranges = anchored_comments
            .iter()
            .map(|comment| {
                (
                    comment.reference.range.clone(),
                    self.comment_highlight(comment.author),
                )
            })
            .collect::<Vec<_>>();
        let highlights_in = |segment: &Range<usize>| {
            comment_ranges
                .iter()
                .filter_map(|(range, background)| {
                    let start = range.start.max(segment.start);
                    let end = range.end.min(segment.end);
                    (start < end)
                        .then(|| ((start - segment.start)..(end - segment.start), *background))
                })
                .collect::<Vec<_>>()
        };

        // The message is split after the (wrapped) line each group of
        // comments ends on, so that their editors sit right below it.
        let mut segments = Vec::new();
        let mut comment_index = 0;
        while comment_index < anchored_comments.len() {
            let first = anchored_comments[comment_index];
            let line_end =
                Self::wrapped_line_end(text, first.reference.range.end, wrap_width, window);
            let group_start = comment_index;
            while comment_index < anchored_comments.len()
                && anchored_comments[comment_index].reference.range.start < line_end
            {
                comment_index += 1;
            }
            let range = cursor..line_end;
            let mut segment = self.message_segment(
                thread_message_id,
                range.clone(),
                &text[range.clone()],
                &highlights_in(&range),
                cx,
            );
            segment.comments = anchored_comments[group_start..comment_index]
                .iter()
                .map(|comment| comment.id)
                .collect();
            segments.push(segment);
            cursor = line_end;
        }
        if cursor < text.len() {
            let range = cursor..text.len();
            segments.push(self.message_segment(
                thread_message_id,
                range.clone(),
                &text[range.clone()],
                &highlights_in(&range),
                cx,
            ));
        }

        // Until the new segments render their text, keep showing what they
        // replace. Swapping in views that are still empty would collapse the
        // timeline and clamp its scroll offset, jumping the view elsewhere.
        let generation = self.render_generation;
        let shown = self
            .shown_segments
            .entry(thread_message_id)
            .or_insert_with(|| ShownSegments {
                segments: None,
                pending_since: None,
                rendered_at: generation,
            });
        shown.rendered_at = generation;
        let placed_comments = segments
            .iter()
            .flat_map(|segment| segment.comments.iter().copied())
            .collect::<Vec<_>>();
        let parsed = segments.iter().all(|segment| segment.parsed);
        let timed_out = shown
            .pending_since
            .is_some_and(|since| since.elapsed() >= SEGMENT_PARSE_TIMEOUT);
        if parsed || timed_out {
            let content = self.render_segments(&segments, &anchored_comments, &placed_comments, cx);
            let shown = self
                .shown_segments
                .get_mut(&thread_message_id)
                .expect("shown segments were just inserted");
            shown.segments = Some(segments);
            shown.pending_since = None;
            return content;
        }

        shown.pending_since.get_or_insert_with(Instant::now);
        let previous = shown.segments.clone();
        // The views parse in the background whether or not they are drawn;
        // look again next frame.
        window.request_animation_frame();
        // A newly created comment has no place in the old layout yet. Do not
        // append its focused editor after the whole response while parsing:
        // that would scroll the timeline away from the quoted text.
        match previous {
            Some(previous) => self.render_segments(&previous, &anchored_comments, &[], cx),
            None => whole(cx).into_iter().collect(),
        }
    }

    fn render_agent_message(
        &mut self,
        thread_id: Uuid,
        index: usize,
        message: &AgentMessage,
        comments: &[UserComment],
        submitted_comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = message.is_generating()
            && message.output.steps.is_empty()
            && message.output.text.is_empty();
        let mut submitted_comment_content = Vec::new();
        for comment in submitted_comments {
            submitted_comment_content.push(self.render_composer_comment(comment, cx));
            if let Some(response) = message
                .comment_responses
                .iter()
                .find(|response| response.comment_id == comment.id)
            {
                let response_id = response.id;
                let response_content = self.render_agent_text(
                    ThreadMessageId {
                        thread_id,
                        message_id: response.id,
                    },
                    &response.response,
                    &response.response_view,
                    comments,
                    wrap_width,
                    window,
                    cx,
                );
                submitted_comment_content.push(
                    div()
                        .id(format!("comment-response-{response_id}"))
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(cx.theme().muted)
                        .flex()
                        .flex_col()
                        .gap_3()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, _| {
                                this.selection_message_id = Some(response_id);
                            }),
                        )
                        .children(response_content)
                        .into_any_element(),
                );
            }
        }

        let message_content = self.render_agent_text(
            ThreadMessageId {
                thread_id,
                message_id: message.id,
            },
            &message.output.text,
            &message.text_view,
            comments,
            wrap_width,
            window,
            cx,
        );

        let message_id = message.id;
        // Only surfaced when the agent said nothing itself.
        let failure = message
            .run
            .failure()
            .filter(|_| message.output.text.is_empty())
            .map(|failure| {
                div()
                    .text_color(cx.theme().danger)
                    .child(failure.to_owned())
            });
        // A plain reply that completed needs no summary.
        let summarized = message.is_generating()
            || message.output.work().next().is_some()
            || !matches!(message.run.outcome(), Some(RunOutcome::Completed));
        let summary = summarized.then(|| Self::render_work_summary(thread_id, message, cx));
        let work = if message.work_expanded {
            Self::render_work(thread_id, message, cx)
        } else {
            Vec::new()
        };
        div()
            .id(("timeline-message", index))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.selection_message_id = Some(message_id);
                }),
            )
            .w_full()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(self.render_avatar(MessageAuthor::Agent)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(message.run.failure().is_some(), |this| {
                        this.text_color(cx.theme().danger)
                    })
                    .children(submitted_comment_content)
                    .children(summary)
                    .when(waiting, |this| {
                        this.child(
                            ShimmerText::new("Thinking\u{2026}")
                                .id(("agent-waiting", index))
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .children(work)
                    .children(message_content)
                    .children(failure),
            )
            .child(div().w(px(40.)).flex_none())
            .into_any_element()
    }

    pub(crate) fn render_timeline_message(
        &mut self,
        thread_id: Uuid,
        index: usize,
        message: &TimelineMessage,
        comments: &[UserComment],
        submitted_comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match message {
            TimelineMessage::User(group) => {
                let cards = self
                    .thread_store
                    .read(cx)
                    .thread(thread_id, cx)
                    .map(|thread| {
                        let draft = &thread.read(cx).draft;
                        group
                            .blocks
                            .iter()
                            .map(|block| {
                                block
                                    .attachments
                                    .iter()
                                    .map(|record| AttachmentCard::new(draft, record))
                                    .collect()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.render_user_message_group(index, group, cards, cx)
            }
            TimelineMessage::Agent(message) => self.render_agent_message(
                thread_id,
                index,
                message,
                comments,
                submitted_comments,
                wrap_width,
                window,
                cx,
            ),
        }
    }

    pub(crate) fn timeline_scrolled(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        let delta_y = event.delta.pixel_delta(window.line_height()).y;
        let max_offset = self.timeline_scroll_handle.max_offset().y;

        if delta_y > px(0.) && max_offset > px(0.) {
            self.follow_generation = false;
        } else if delta_y < px(0.) {
            let projected_offset = self.timeline_scroll_handle.offset().y + delta_y;
            if projected_offset <= -max_offset + px(1.) {
                self.follow_generation = true;
            }
        }
    }
}

/// A duration as the work summary shows it: `8s`, `2m 5s`, `1h 3m`.
fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match (seconds / 3600, seconds / 60 % 60, seconds % 60) {
        (0, 0, seconds) => format!("{seconds}s"),
        (0, minutes, seconds) => format!("{minutes}m {seconds}s"),
        (hours, minutes, _) => format!("{hours}h {minutes}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_in_the_largest_units() {
        let format = |seconds| format_duration(Duration::from_secs(seconds));
        assert_eq!(format(0), "0s");
        assert_eq!(format(53), "53s");
        assert_eq!(format(125), "2m 5s");
        assert_eq!(format(3_600), "1h 0m");
        assert_eq!(format(3_780), "1h 3m");
        assert_eq!(format_duration(Duration::from_millis(1_999)), "1s");
    }
}
