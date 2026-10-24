//! Full virtualization of the timeline's rows inside its scroll container.
//!
//! [`VirtualTimeline`] stands in for the column of rows: it reports their
//! total height to the scroll container and builds, lays out and paints only
//! the rows near the viewport, plus pinned ones. Each frame's cost is the
//! rows it builds, not the history's length.
//!
//! Heights are measured when a row is built, at the width it is drawn at,
//! and kept in [`TimelineLayout`]; a row never built yet counts as the
//! average measured row. Those are hints: every row drawn is measured again,
//! and when a measurement differs, the rows are placed around an anchor (the
//! first visible row, or the end of the history while the view is at the
//! bottom) and the scroll offset moves by the same amount, so that what is
//! on screen stays put while the extent converges. Nothing invalidates an
//! offscreen height: whatever changed is measured when the row is next
//! drawn. Width and typography changes cost only the rows drawn.
//!
//! Selection doesn't build the history either: the row a press lands in
//! stays built while the button is held (see [`TimelinePress`]), and the
//! rows holding a selection's ends while it lasts. Copy takes the rows
//! between them from the thread; see `timeline_selection`.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Display, Element, ElementId, FlexDirection,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    Pixels, Point, Refineable as _, ScrollHandle, Style, TextStyle, WeakEntity, Window, point, px,
    relative, size,
};
use uuid::Uuid;

use crate::{
    Cowork,
    timeline::{AgentStep, TimelineMessage},
    timeline_selection::TimelineSelection,
};

/// How far beyond the visible area rows are built, so that a short scroll
/// finds them ready.
const OVERDRAW: Pixels = px(128.);

/// What a row never measured counts as before any row is.
const DEFAULT_ROW_HEIGHT: Pixels = px(120.);

/// How far a held pointer moves before the press is a drag, as GPUI's own
/// element drags count it. A click's jitter stays below it.
const DRAG_THRESHOLD: f64 = 2.;

/// The left-button press in progress anywhere in the window.
pub(crate) struct TimelinePress {
    origin: Point<Pixels>,
    /// The message whose timeline row the press landed in. The row stays
    /// built and painted, and with it the selection participant a later drag
    /// anchors on, while the button is held, even when it scrolls offscreen.
    pub(crate) row: Option<Uuid>,
    /// Whether the pointer has moved past [`DRAG_THRESHOLD`] while held.
    pub(crate) dragging: bool,
}

impl TimelinePress {
    pub(crate) fn new(origin: Point<Pixels>) -> Self {
        Self {
            origin,
            row: None,
            dragging: false,
        }
    }

    /// Notes the held pointer at `position`; true when this starts a drag.
    pub(crate) fn moved_to(&mut self, position: Point<Pixels>) -> bool {
        let started = !self.dragging && (position - self.origin).magnitude() > DRAG_THRESHOLD;
        self.dragging |= started;
        started
    }
}

/// The measured heights of the active thread's rows, by message. Owned by
/// [`Cowork`] so that they outlive the frames a row isn't drawn in.
#[derive(Default)]
pub(crate) struct TimelineLayout {
    thread: Option<Uuid>,
    heights: HashMap<Uuid, Pixels>,
    /// The sum of `heights`, for the estimate of rows never measured.
    measured_total: Pixels,
}

impl TimelineLayout {
    /// Forgets another thread's heights.
    fn show_thread(&mut self, thread: Uuid) {
        if self.thread != Some(thread) {
            *self = Self {
                thread: Some(thread),
                ..Self::default()
            };
        }
    }

    fn estimate(&self) -> Pixels {
        if self.heights.is_empty() {
            DEFAULT_ROW_HEIGHT
        } else {
            self.measured_total / self.heights.len() as f32
        }
    }

    fn height(&self, message_id: Uuid) -> Pixels {
        self.heights
            .get(&message_id)
            .copied()
            .unwrap_or_else(|| self.estimate())
    }

    fn record(&mut self, message_id: Uuid, height: Pixels) {
        if let Some(previous) = self.heights.insert(message_id, height) {
            self.measured_total -= previous;
        }
        self.measured_total += height;
    }
}

/// A row of the timeline: the message it draws, where it is in the
/// timeline, and whether it must be built wherever it is.
pub(crate) struct TimelineRowSlot {
    pub(crate) index: usize,
    pub(crate) message_id: Uuid,
    /// Built and painted even offscreen; see [`TimelinePress::row`].
    pub(crate) pinned: bool,
}

impl Cowork {
    /// The rows of `timeline`, scanned in place once per frame, and the
    /// timeline index of each user group drawn inside a run, where its steer
    /// joined it, rather than as a row of its own.
    pub(crate) fn plan_timeline_rows(
        &self,
        timeline: &[TimelineMessage],
    ) -> (Vec<TimelineRowSlot>, HashMap<Uuid, usize>) {
        let steered = timeline
            .iter()
            .filter_map(|entry| match entry {
                TimelineMessage::Agent(message) => Some(message.ui.output.steps()),
                TimelineMessage::User(_) => None,
            })
            .flatten()
            .filter_map(|step| match step {
                AgentStep::Steering(id) => Some(*id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let selection = self.timeline_selection.borrow();
        let mut steering_groups = HashMap::new();
        let mut rows = Vec::with_capacity(timeline.len());
        for (index, entry) in timeline.iter().enumerate() {
            let message_id = match entry {
                TimelineMessage::User(group) if steered.contains(&group.record.id) => {
                    steering_groups.insert(group.record.id, index);
                    continue;
                }
                TimelineMessage::User(group) => group.record.id,
                TimelineMessage::Agent(message) => message.record.id,
            };
            rows.push(TimelineRowSlot {
                index,
                message_id,
                pinned: selection.pins(message_id)
                    || self
                        .timeline_press
                        .as_ref()
                        .is_some_and(|press| press.row == Some(message_id)),
            });
        }
        // A cursor between two rows may have taken the text before them.
        if let Some(cursor) = rows
            .iter()
            .position(|row| selection.cursor == Some(row.message_id))
            .and_then(|cursor| cursor.checked_sub(1))
        {
            rows[cursor].pinned = true;
        }
        (rows, steering_groups)
    }
}

/// Builds the row of a timeline index and message, first in the timeline
/// or not.
pub(crate) type BuildRow = Rc<dyn Fn(usize, Uuid, bool, &mut Window, &mut App) -> AnyElement>;

/// The timeline's rows and, after them, its `tail` (pending submissions and
/// the composer), laid out natively. See the module documentation.
pub(crate) struct VirtualTimeline {
    layout: Rc<RefCell<TimelineLayout>>,
    thread_id: Option<Uuid>,
    rows: Vec<TimelineRowSlot>,
    build: BuildRow,
    tail: AnyElement,
    selection: Rc<RefCell<TimelineSelection>>,
    scroll: ScrollHandle,
    owner: WeakEntity<Cowork>,
    /// This frame's built rows: their slot, element and bounds.
    built: Vec<(usize, AnyElement, Bounds<Pixels>)>,
}

impl VirtualTimeline {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        layout: Rc<RefCell<TimelineLayout>>,
        thread_id: Option<Uuid>,
        rows: Vec<TimelineRowSlot>,
        build: BuildRow,
        tail: AnyElement,
        selection: Rc<RefCell<TimelineSelection>>,
        scroll: ScrollHandle,
        owner: WeakEntity<Cowork>,
    ) -> Self {
        Self {
            layout,
            thread_id,
            rows,
            build,
            tail,
            selection,
            scroll,
            owner,
            built: Vec::new(),
        }
    }
}

pub(crate) struct VirtualTimelineLayout {
    /// Each row's height as this frame's layout placed it.
    heights: Vec<Pixels>,
    /// The typography rows inherit, re-entered when they are built in
    /// prepaint, where divs no longer apply it.
    text_style: TextStyle,
    rem_size: Pixels,
}

impl VirtualTimeline {
    /// While a drag selects, notes the rows it began in and is over now,
    /// so that both stay built; see `timeline_selection`.
    fn track_selection(&self, mask: Bounds<Pixels>, window: &Window, cx: &App) {
        let Some(press) = self.owner.upgrade().and_then(|owner| {
            owner
                .read(cx)
                .timeline_press
                .as_ref()
                .map(|press| (press.row, press.dragging))
        }) else {
            return;
        };
        let (Some(anchor), true) = press else {
            return;
        };
        let pointer = window.mouse_position().y;
        let shown = self
            .built
            .iter()
            .filter(|(_, _, bounds)| bounds.intersects(&mask))
            .map(|(index, _, bounds)| (*index, bounds));
        // The row under the pointer, or the nearest when it is past them.
        let cursor = shown
            .min_by(|(_, a), (_, b)| {
                let distance = |bounds: &Bounds<Pixels>| {
                    if pointer < bounds.top() {
                        bounds.top() - pointer
                    } else if pointer >= bounds.bottom() {
                        pointer - bounds.bottom()
                    } else {
                        px(0.)
                    }
                };
                distance(a)
                    .partial_cmp(&distance(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| self.rows[index].message_id);
        let mut selection = self.selection.borrow_mut();
        selection.anchor = Some(anchor);
        if cursor.is_some() {
            selection.cursor = cursor;
        }
    }
}

impl IntoElement for VirtualTimeline {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for VirtualTimeline {
    type RequestLayoutState = VirtualTimelineLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some("virtual-timeline".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let heights = {
            let mut layout = self.layout.borrow_mut();
            if let Some(thread_id) = self.thread_id {
                layout.show_thread(thread_id);
            }
            self.rows
                .iter()
                .map(|row| layout.height(row.message_id))
                .collect::<Vec<_>>()
        };
        let total = heights.iter().fold(px(0.), |total, height| total + *height);
        let rows = window.request_layout(
            Style {
                size: size(relative(1.).into(), total.into()),
                flex_shrink: 0.,
                ..Style::default()
            },
            [],
            cx,
        );
        let tail = self.tail.request_layout(window, cx);
        let mut style = Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Column,
            flex_shrink: 0.,
            ..Style::default()
        };
        style.size.width = relative(1.).into();
        let layout = window.request_layout(style, [rows, tail], cx);
        (
            layout,
            VirtualTimelineLayout {
                heights,
                text_style: window.text_style(),
                rem_size: window.rem_size(),
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        planned: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let count = self.rows.len();
        let old = &planned.heights;
        let mut tops = Vec::with_capacity(count + 1);
        tops.push(px(0.));
        for height in old {
            tops.push(*tops.last().unwrap() + *height);
        }
        let old_end = tops[count];

        // The visible area, relative to the rows' top.
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        let visible = viewport.intersect(&window.content_mask().bounds);
        let visible_top = visible.top() - bounds.top();
        let visible_bottom = visible.bottom() - bounds.top();
        // While the end is in view (scrolled to the bottom, or the content
        // shorter than the view), it stays put; otherwise the first visible
        // row does.
        let at_bottom = bounds.bottom() <= visible.bottom() + px(1.);
        let anchor = if at_bottom {
            count
        } else {
            (0..count)
                .find(|&index| tops[index + 1] > visible_top)
                .unwrap_or(count)
        };

        let refinement = planned.text_style.subtract(&Default::default());
        let width = bounds.size.width;
        let build = self.build.clone();
        let mut built: Vec<(usize, AnyElement, Pixels)> = Vec::new();
        let mut measure = |slot: usize, row: &TimelineRowSlot, window: &mut Window| {
            window.with_rem_size(Some(planned.rem_size), |window| {
                window.with_text_style(Some(refinement.clone()), |window| {
                    let mut element = build(row.index, row.message_id, slot == 0, window, cx);
                    let measured = element.layout_as_root(
                        size(AvailableSpace::Definite(width), AvailableSpace::MinContent),
                        window,
                        cx,
                    );
                    (element, measured.height)
                })
            })
        };

        let mut chosen = vec![false; count];
        {
            // Down from the anchor until the visible area (and overdraw) is
            // filled with measured rows, then up from it the same way.
            let mut bottom = tops[anchor];
            for (index, row) in self.rows.iter().enumerate().skip(anchor) {
                if bottom >= visible_bottom + OVERDRAW {
                    break;
                }
                let (element, height) = measure(index, row, window);
                bottom += height;
                chosen[index] = true;
                built.push((index, element, height));
            }
            let mut top = tops[anchor];
            for index in (0..anchor).rev() {
                if top <= visible_top - OVERDRAW {
                    break;
                }
                let (element, height) = measure(index, &self.rows[index], window);
                top -= height;
                chosen[index] = true;
                built.push((index, element, height));
            }
        }
        for (index, row) in self.rows.iter().enumerate() {
            if !chosen[index] && row.pinned {
                let (element, height) = measure(index, row, window);
                built.push((index, element, height));
            }
        }

        // Place the rows with the heights the next frame lays them out with,
        // estimates included, so that the anchor stays where this frame's
        // layout put it.
        let heights = {
            let mut layout = self.layout.borrow_mut();
            for (index, _, height) in &built {
                layout.record(self.rows[*index].message_id, *height);
            }
            self.rows
                .iter()
                .map(|row| layout.height(row.message_id))
                .collect::<Vec<_>>()
        };
        let mut new_tops = Vec::with_capacity(count + 1);
        new_tops.push(px(0.));
        for height in &heights {
            new_tops.push(*new_tops.last().unwrap() + *height);
        }
        // How far the anchor moves the content, and with it the scroll
        // offset, within what the next frame's scroll container allows:
        // content shorter than the view doesn't scroll at all.
        let offset = self.scroll.offset().y;
        let overflow = bounds.bottom() - visible.bottom() - offset;
        let next_max = (overflow + new_tops[count] - old_end).max(px(0.));
        let next_offset = (offset + tops[anchor] - new_tops[anchor]).clamp(-next_max, px(0.));
        let shift = next_offset - offset;
        let changed = heights
            .iter()
            .zip(old)
            .any(|(new, old)| (*new - *old).abs() > px(0.5));
        for (index, mut element, height) in built {
            let origin = bounds.origin + point(px(0.), new_tops[index] + shift);
            element.prepaint_at(origin, window, cx);
            self.built
                .push((index, element, Bounds::new(origin, size(width, height))));
        }
        // The tail follows the rows as drawn, not as laid out.
        let tail_shift = new_tops[count] + shift - old_end;
        window.with_element_offset(point(px(0.), tail_shift), |window| {
            self.tail.prepaint(window, cx);
        });

        if changed {
            // Keep what is on screen there in the next frame, which lays
            // the rows out with these heights.
            if at_bottom && offset <= -self.scroll.max_offset().y + px(1.) {
                // Exactly at the end, whatever rounding the container applies.
                self.scroll.scroll_to_bottom();
            } else {
                self.scroll
                    .set_offset(point(self.scroll.offset().x, next_offset));
            }
            _ = self.owner.update(cx, |_, cx| cx.notify());
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _planned: &mut Self::RequestLayoutState,
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        // In timeline order, which is the order GPUI Base gives the text of
        // a selection.
        self.built.sort_by_key(|(index, _, _)| *index);
        let mask = window.content_mask().bounds;
        self.track_selection(mask, window, cx);
        for (index, element, bounds) in &mut self.built {
            // Only painted rows can be pressed: offscreen ones aren't under
            // the pointer. Capture runs before the row's own handlers can
            // stop it.
            let visible = bounds.intersect(&mask);
            let (message_id, owner) = (self.rows[*index].message_id, self.owner.clone());
            window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                if phase.capture()
                    && event.button == MouseButton::Left
                    && visible.contains(&event.position)
                {
                    _ = owner.update(cx, |owner, _| {
                        owner
                            .timeline_press
                            .get_or_insert_with(|| TimelinePress::new(event.position))
                            .row = Some(message_id);
                    });
                }
            });
            element.paint(window, cx);
        }
        self.tail.paint(window, cx);
    }
}
