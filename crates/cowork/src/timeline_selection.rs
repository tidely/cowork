//! Text selection across the virtualized timeline.
//!
//! GPUI Base's window selection knows only the text painted this frame: an
//! endpoint whose text stops painting ends the selection, and Copy takes
//! only painted text. The timeline paints just the rows near the view, so:
//!
//! - The rows holding the selection's ends ([`TimelineSelection`]) stay
//!   built and painted, wherever they scroll, while the selection lasts.
//! - Copy of a selection spanning rows is built here, row by row in
//!   timeline order: the selected text of the two end rows, and the whole
//!   text of every row between them, painted or not, from the views and
//!   prompts each row draws ([`Cowork::copy_pieces`]).
//!
//! Prompts are drawn with [`PromptText`], which records what of them is
//! selected; GPUI Base's own selectable text keeps that to itself.

use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc};

use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, Context, Corners, Edges, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, PaintQuad,
    Pixels, Point, SharedString, StyledText, Window, transparent_black,
};
use gpui_base::{
    TextSelection, TextSelectionHandle, TextSelectionRegistration, TextSelectionRun, TextViewState,
    Theme, input::Copy,
};
use uuid::Uuid;

use crate::{Cowork, timeline_view::TimelineRow};

/// The rows a selection in the timeline began and ends in, by message, and
/// what of each prompt is selected, as last painted.
#[derive(Default)]
pub(crate) struct TimelineSelection {
    pub(crate) anchor: Option<Uuid>,
    pub(crate) cursor: Option<Uuid>,
    prompts: HashMap<Uuid, Range<usize>>,
}

impl TimelineSelection {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether `message_id`'s row must stay built: it holds an end of the
    /// selection.
    pub(crate) fn pins(&self, message_id: Uuid) -> bool {
        self.anchor == Some(message_id) || self.cursor == Some(message_id)
    }
}

/// What one view or prompt of a row contributes to Copy.
pub(crate) enum CopyPiece {
    Text(gpui::Entity<TextViewState>),
    Prompt { block_id: Uuid, text: SharedString },
}

impl CopyPiece {
    /// All of it, as Copy of a selection covering it gives it.
    fn whole(&self, cx: &App) -> String {
        match self {
            Self::Text(view) => view.read(cx).rendered_text().as_str().to_owned(),
            Self::Prompt { text, .. } => text.to_string(),
        }
    }

    /// What of it is selected, as last painted.
    fn selected(&self, selection: &TimelineSelection, cx: &App) -> String {
        match self {
            Self::Text(view) => view.read(cx).selected_text(),
            Self::Prompt { block_id, text } => selection
                .prompts
                .get(block_id)
                .and_then(|range| text.get(range.clone()))
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

impl Cowork {
    /// The text Copy takes from a selection spanning timeline rows, or
    /// `None` for any other selection, which GPUI Base copies itself.
    pub(crate) fn timeline_selection_text(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        let (anchor, cursor) = {
            let selection = self.timeline_selection.borrow();
            (selection.anchor?, selection.cursor?)
        };
        if anchor == cursor || !TextSelection::has_selection(window, cx) {
            return None;
        }
        let thread_id = self.active_thread_id?;
        let thread = self.thread_store.read(cx).thread(thread_id, cx)?;
        let timeline = &thread.read(cx).timeline;
        let (rows, steering_groups) = self.plan_timeline_rows(timeline);
        let position = |id| rows.iter().position(|row| row.message_id == id);
        let (anchor, cursor) = (position(anchor)?, position(cursor)?);
        let (first, last) = (anchor.min(cursor), anchor.max(cursor));
        let selection = self.timeline_selection.borrow();
        let mut texts = Vec::new();
        for (offset, row) in rows[first..=last].iter().enumerate() {
            let Some(row) =
                TimelineRow::capture(timeline, row.index, row.message_id, &steering_groups)
            else {
                continue;
            };
            let end = offset == 0 || first + offset == last;
            for piece in self.copy_pieces(thread_id, &row, cx) {
                texts.push(if end {
                    piece.selected(&selection, cx)
                } else {
                    piece.whole(cx)
                });
            }
        }
        // Joined as GPUI Base joins its participants' text, without the
        // line break Markdown text ends with.
        let text = texts
            .iter()
            .map(|text| text.trim_end_matches('\n'))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Some(text.trim().to_owned())
    }

    /// Copies a selection spanning timeline rows, before the focused view's
    /// own Copy, which would take only the painted text.
    pub(crate) fn copy_timeline_selection(
        &mut self,
        _: &Copy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = self.timeline_selection_text(window, cx)
            && !text.is_empty()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            cx.stop_propagation();
        }
    }
}

/// A prompt's text, selectable as part of the window's selection, which
/// records the range of it selected for [`Cowork::timeline_selection_text`].
/// Otherwise as GPUI Base's `SelectableText`.
pub(crate) struct PromptText {
    id: ElementId,
    block_id: Uuid,
    text: SharedString,
    styled_text: StyledText,
    document_order: u64,
    selection: Rc<RefCell<TimelineSelection>>,
}

impl PromptText {
    pub(crate) fn new(
        block_id: Uuid,
        text: SharedString,
        document_order: u64,
        selection: Rc<RefCell<TimelineSelection>>,
    ) -> Self {
        Self {
            id: format!("timeline-user-text-{block_id}").into(),
            block_id,
            styled_text: StyledText::new(text.clone()),
            text,
            document_order,
            selection,
        }
    }
}

impl IntoElement for PromptText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for PromptText {
    type RequestLayoutState = TextSelectionHandle;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, TextSelectionHandle) {
        let handle = window.with_element_state(
            id.expect("prompt text has an ID"),
            |retained: Option<TextSelectionHandle>, _| {
                let handle =
                    retained.unwrap_or_else(|| TextSelectionHandle::new(self.text.clone(), cx));
                (handle.clone(), handle)
            },
        );
        let (layout, ()) = self
            .styled_text
            .request_layout(id, inspector_id, window, cx);
        (layout, handle)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut TextSelectionHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.styled_text
            .prepaint(id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration = TextSelectionRegistration::new(hitbox.clone(), bounds)
            .with_document_order(self.document_order)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(handle, window, cx);
        handle.register(registration, window, cx);
        hitbox
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut TextSelectionHandle,
        _: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled_text.layout().clone();
        let projection = handle.update_runs(
            &[
                TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                    .with_document_order(self.document_order),
            ],
            cx,
        );
        let range = projection.ranges().first().cloned().flatten();
        let changed = {
            let prompts = &mut self.selection.borrow_mut().prompts;
            let previous = match range.clone().filter(|range| !range.is_empty()) {
                Some(range) => prompts.insert(self.block_id, range.clone()),
                None => prompts.remove(&self.block_id),
            };
            previous != range.clone().filter(|range| !range.is_empty())
        };
        if changed {
            // What Copy takes changed with the projection; let everything
            // that shows it catch up.
            window.refresh();
        }
        let color = Theme::global(cx).tokens.colors.selection;
        if let Some(range) = range
            && let (Some(start), Some(end)) = (
                layout.position_for_index(range.start),
                layout.position_for_index(range.end),
            )
        {
            for bounds in selection_quads(start, end, layout.bounds(), layout.line_height()) {
                window.paint_quad(PaintQuad {
                    bounds,
                    background: color.into(),
                    corner_radii: Corners::default(),
                    border_widths: Edges::default(),
                    border_color: transparent_black(),
                    border_style: BorderStyle::default(),
                });
            }
        }
        self.styled_text
            .paint(id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

/// The highlight of text from `start` to `end` over lines `line_height`
/// tall, within `bounds`, as GPUI Base's selectable text paints it.
fn selection_quads(
    start: Point<Pixels>,
    end: Point<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Vec<Bounds<Pixels>> {
    if start.y == end.y {
        return vec![Bounds::from_corners(
            start,
            Point::new(end.x, end.y + line_height),
        )];
    }
    let mut quads = vec![Bounds::from_corners(
        start,
        Point::new(bounds.right(), start.y + line_height),
    )];
    if end.y > start.y + line_height {
        quads.push(Bounds::from_corners(
            Point::new(bounds.left(), start.y + line_height),
            Point::new(bounds.right(), end.y),
        ));
    }
    quads.push(Bounds::from_corners(
        Point::new(bounds.left(), end.y),
        Point::new(end.x, end.y + line_height),
    ));
    quads
}
