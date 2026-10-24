//! Viewport culling without replacing the timeline's outer scroll layout.
//!
//! Wrap only settled row bodies; keep their outer margins outside this
//! element. A settled row may contain open work, tool details and diffs: what
//! its height depends on besides width and typography is its
//! [`RowGeometry`], so a culled row is remeasured whenever that changes, even
//! offscreen. Disable culling for global selection/drag, and leave rows with
//! live or independently changing content (generation, approvals, comments,
//! images) on the original path.
//!
//! Selection needs every participant between its endpoints laid out, so an
//! actual drag or a nonempty window selection lays out the whole history
//! natively. A press alone doesn't: the row it lands in is visible, hence
//! already built, and stays built while the button is held (see
//! [`TimelinePress`]); clicks never pay for the whole history.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity, EntityId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, Pixels, Point,
    Refineable as _, SharedString, Style, Subscription, TextStyle, WeakEntity, Window, px,
    relative, size,
};
use gpui_base::TextViewState;
use uuid::Uuid;

use crate::{
    Cowork,
    file_change_view::FileChangeView,
    thread::Thread,
    timeline::{self, TimelineMessage},
    timeline_view::TimelineRow,
};

/// How far a held pointer moves before the press is a drag, as GPUI's own
/// element drags count it. A click's jitter stays below it; a selection that
/// starts sooner still switches to native layout through the window's
/// nonempty selection.
const DRAG_THRESHOLD: f64 = 2.;

/// The left-button press in progress anywhere in the window.
pub(crate) struct TimelinePress {
    origin: Point<Pixels>,
    /// The timeline row the press landed in. It keeps its native layout, and
    /// with it the selection participant a later drag anchors on, while the
    /// button is held, even when it scrolls offscreen.
    pub(crate) row: Option<SharedString>,
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

impl Cowork {
    /// Whether selection needs every history row laid out natively: a drag
    /// is under way or the window has a nonempty text selection.
    pub(crate) fn timeline_drag_started(&self, window: &mut Window, cx: &mut App) -> bool {
        self.timeline_press
            .as_ref()
            .is_some_and(|press| press.dragging)
            || gpui_base::TextSelection::has_selection(window, cx)
    }
}

/// A view drawn inside a row whose content can change after the row was
/// measured, without the row's own data changing.
pub(crate) enum HeightSource {
    /// Markdown, which parses in the background. Only a new rendered text
    /// changes its geometry; selection and highlight notifications don't.
    Text(Entity<TextViewState>),
    /// A file change, whose Diff / JSON switch changes its height.
    FileChange(Entity<FileChangeView>),
}

impl HeightSource {
    fn entity_id(&self) -> EntityId {
        match self {
            Self::Text(view) => view.entity_id(),
            Self::FileChange(view) => view.entity_id(),
        }
    }
}

/// Everything a row's measured height depends on besides width and
/// typography: the local state that decides what it draws (folding), as a
/// key, and the views it draws that can change on their own. A change to
/// either discards the measured height, offscreen too, so that a culled row
/// never keeps a stale height in the scroll extent.
#[derive(Default)]
pub(crate) struct RowGeometry {
    pub(crate) key: u64,
    pub(crate) sources: Vec<HeightSource>,
}

/// What a measured height is valid for. Not whether the row is culled: a
/// native frame measures every row it lays out, so its heights stay valid
/// when culling resumes after a drag, without another full-history frame.
#[derive(PartialEq)]
struct HeightKey {
    wrap_width: Pixels,
    text_style: TextStyle,
    rem_size: Pixels,
    content: u64,
}

/// Lives in GPUI element state even when the row has no constructed child.
struct RowState {
    key: HeightKey,
    height: Rc<Cell<Option<Pixels>>>,
    measured_width: Rc<Cell<Option<Pixels>>>,
    source_ids: Vec<EntityId>,
    subscriptions: Vec<Subscription>,
}

impl RowState {
    fn new(key: HeightKey) -> Self {
        Self {
            key,
            height: Rc::new(Cell::new(None)),
            measured_width: Rc::new(Cell::new(None)),
            source_ids: Vec::new(),
            subscriptions: Vec::new(),
        }
    }

    fn set_key(&mut self, key: HeightKey) {
        if self.key != key {
            self.key = key;
            self.height.set(None);
        }
    }

    fn observe_sources(
        &mut self,
        sources: &[HeightSource],
        owner: &WeakEntity<Cowork>,
        cx: &mut App,
    ) {
        if self
            .source_ids
            .iter()
            .copied()
            .eq(sources.iter().map(HeightSource::entity_id))
        {
            return;
        }
        self.source_ids = sources.iter().map(HeightSource::entity_id).collect();
        self.height.set(None);
        self.subscriptions = sources
            .iter()
            .map(|source| {
                let height = self.height.clone();
                let owner = owner.clone();
                match source {
                    HeightSource::Text(source) => {
                        let mut rendered = source.read(cx).rendered_text();
                        cx.observe(source, move |source, cx| {
                            let next = source.read(cx).rendered_text();
                            // Selection/highlight notifications do not change geometry.
                            if next != rendered {
                                rendered = next;
                                height.set(None);
                                notify_owner(&owner, cx);
                            }
                        })
                    }
                    HeightSource::FileChange(source) => cx.observe(source, move |_, cx| {
                        height.set(None);
                        notify_owner(&owner, cx);
                    }),
                }
            })
            .collect();
    }
}

fn notify_owner(owner: &WeakEntity<Cowork>, cx: &mut App) {
    _ = owner.update(cx, |owner, cx| {
        if owner.follow_generation {
            owner.timeline_scroll_handle.scroll_to_bottom();
        }
        cx.notify();
    });
}

type BuildRow = Box<dyn FnOnce(&mut Window, &mut App) -> AnyElement>;

/// Keeps every outer flex row in layout, but constructs and paints cached row
/// bodies only near the viewport. The ID must remain stable and unique within
/// its containing element; content changes must change `geometry` (its key
/// or a source notifying) or the ID.
pub(crate) struct DeferredTimelineRow {
    id: SharedString,
    wrap_width: Pixels,
    owner: WeakEntity<Cowork>,
    geometry: RowGeometry,
    culling: bool,
    /// Built and painted even offscreen; see [`TimelinePress::row`].
    pinned: bool,
    build: Option<BuildRow>,
    child: Option<AnyElement>,
}

impl DeferredTimelineRow {
    pub(crate) fn new(
        id: SharedString,
        wrap_width: Pixels,
        owner: WeakEntity<Cowork>,
        geometry: RowGeometry,
        culling: bool,
        build: impl FnOnce(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id,
            wrap_width,
            owner,
            geometry,
            culling,
            pinned: false,
            build: Some(Box::new(build)),
            child: None,
        }
    }

    /// Keeps the row built and painted wherever it scrolls, without leaving
    /// the deferred path (and its measured height) while it is visible.
    pub(crate) fn pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }

    fn build_child(&mut self, window: &mut Window, cx: &mut App) -> &mut AnyElement {
        self.child.get_or_insert_with(|| {
            self.build
                .take()
                .expect("row builder is consumed only once")(window, cx)
        })
    }
}

impl IntoElement for DeferredTimelineRow {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for DeferredTimelineRow {
    type RequestLayoutState = (
        Rc<Cell<Option<Pixels>>>,
        Rc<Cell<Option<Pixels>>>,
        bool,
        TextStyle,
        Pixels,
    );
    type PrepaintState = bool;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone().into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let key = HeightKey {
            wrap_width: self.wrap_width,
            text_style: window.text_style(),
            rem_size: window.rem_size(),
            content: self.geometry.key,
        };
        let text_style = key.text_style.clone();
        let rem_size = key.rem_size;
        let (height, measured_width) =
            window.with_element_state(id.expect("row has an ID"), |state, _| {
                let mut state = match state {
                    Some(mut state) => {
                        RowState::set_key(&mut state, key);
                        state
                    }
                    None => RowState::new(key),
                };
                state.observe_sources(&self.geometry.sources, &self.owner, cx);
                ((state.height.clone(), state.measured_width.clone()), state)
            });
        let layout = if let Some(measured) = height.get().filter(|_| self.culling) {
            window.request_layout(
                Style {
                    size: size(relative(1.).into(), measured.into()),
                    flex_shrink: 0.,
                    ..Style::default()
                },
                [],
                cx,
            )
        } else {
            self.build_child(window, cx).request_layout(window, cx)
        };
        (
            layout,
            (
                height,
                measured_width,
                self.child.is_some(),
                text_style,
                rem_size,
            ),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let (height, measured_width, native_layout, text_style, rem_size) = layout;
        // Native layout is authoritative, including on the unknown first
        // pass and in the uncullable selection/drag fallback.
        if *native_layout {
            height.set(Some(bounds.size.height));
            measured_width.set(Some(bounds.size.width));
        } else if measured_width.get() != Some(bounds.size.width) {
            // The parent's allocated width can change without `wrap_width`
            // changing (e.g. while the sidebar animates). Check even offscreen
            // placeholders so their next layout measures at the new width.
            height.set(None);
            notify_owner(&self.owner, cx);
        }
        if self.culling && !self.pinned {
            let viewport = Bounds::new(Default::default(), window.viewport_size());
            let visible = viewport.intersect(&window.content_mask().bounds);
            if visible.size.width <= px(0.)
                || visible.size.height <= px(0.)
                || !bounds.intersects(&visible.dilate(px(128.)))
            {
                return false;
            }
        }
        if self.child.is_none() {
            // Divs inherit typography during request_layout, not prepaint.
            // Re-enter that exact context when laying out a deferred child or
            // its wrapping/height would differ from the native first pass.
            let refinement = text_style.subtract(&Default::default());
            let measured = window.with_rem_size(Some(*rem_size), |window| {
                window.with_text_style(Some(refinement), |window| {
                    self.build_child(window, cx).layout_as_root(
                        size(
                            AvailableSpace::Definite(bounds.size.width),
                            AvailableSpace::MinContent,
                        ),
                        window,
                        cx,
                    )
                })
            });

            measured_width.set(Some(bounds.size.width));
            let measured = measured.height;
            let previous = height.replace(Some(measured));
            // Only placeholder corrections need another layout. Unknown rows
            // already occupied their native height in this frame's layout.
            if previous.is_some_and(|old| (old - measured).abs() > px(0.5)) {
                notify_owner(&self.owner, cx);
            }
        }
        let child = self.child.as_mut().unwrap();
        if *native_layout {
            child.prepaint(window, cx);
        } else {
            child.prepaint_at(bounds.origin, window, cx);
        }
        true
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _height: &mut Self::RequestLayoutState,
        prepainted: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !*prepainted {
            return;
        }
        // Only painted rows can be pressed: offscreen ones aren't under the
        // pointer. Capture runs before the row's own handlers can stop it.
        let visible = bounds.intersect(&window.content_mask().bounds);
        let (id, owner) = (self.id.clone(), self.owner.clone());
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
            if phase.capture()
                && event.button == MouseButton::Left
                && visible.contains(&event.position)
            {
                _ = owner.update(cx, |owner, _| {
                    owner
                        .timeline_press
                        .get_or_insert_with(|| TimelinePress::new(event.position))
                        .row = Some(id.clone());
                });
            }
        });
        self.child.as_mut().unwrap().paint(window, cx);
    }
}

/// A timeline row as `render_main_editor` decided to build it: deferred,
/// keeping only the message's identity until it is built near the viewport,
/// or `native`, already captured from the thread to build now.
pub(crate) struct PlannedRow {
    pub(crate) id: SharedString,
    pub(crate) index: usize,
    pub(crate) message_id: Uuid,
    pub(crate) geometry: RowGeometry,
    pub(crate) native: Option<TimelineRow>,
    /// The row a press in progress began in; see [`TimelinePress::row`].
    pub(crate) pinned: bool,
}

impl Cowork {
    /// Scans `thread`'s timeline in place, once per frame, deciding which
    /// rows may be culled and capturing only those that must be built now.
    /// Also returns the timeline index of each user group drawn inside a run,
    /// where its steer joined it.
    pub(crate) fn plan_timeline_rows(
        &self,
        thread: &Entity<Thread>,
        thread_id: Uuid,
        culling: bool,
        comments: &[timeline::UserComment],
        cx: &App,
    ) -> (Vec<PlannedRow>, HashMap<Uuid, usize>) {
        let messages = &thread.read(cx).timeline;
        let steered = messages
            .iter()
            .filter_map(|entry| match entry {
                TimelineMessage::Agent(message) => Some(message.ui.output.steps()),
                TimelineMessage::User(_) => None,
            })
            .flatten()
            .filter_map(|step| match step {
                timeline::AgentStep::Steering(id) => Some(*id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let steering_groups = messages
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match entry {
                TimelineMessage::User(group) if steered.contains(&group.record.id) => {
                    Some((group.record.id, index))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let user_stable = |group: &timeline::UserMessage| {
            group.record.comments.is_empty()
                && group
                    .record
                    .blocks
                    .iter()
                    .all(|block| block.attachments.is_empty())
        };
        let mut rows = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            // A steering group is a canonical timeline entry, but is drawn
            // only where its input joined the run.
            if let TimelineMessage::User(group) = message
                && steering_groups.contains_key(&group.record.id)
            {
                continue;
            }
            let stable = match message {
                TimelineMessage::User(group) => user_stable(group),
                TimelineMessage::Agent(message) => {
                    // A finished run's steers can't change, so it is stable
                    // when the groups it draws are. Open work and tool
                    // details are: their folding and their views' changes
                    // are the row's geometry.
                    let steers_stable = message.ui.output.steps().iter().all(|step| {
                        let timeline::AgentStep::Steering(id) = step else {
                            return true;
                        };
                        steering_groups.get(id).is_none_or(|&group| {
                            matches!(&messages[group], TimelineMessage::User(group) if user_stable(group))
                        })
                    });
                    !message.is_generating()
                        && steers_stable
                        && message.record.awaiting_approval.is_none()
                        && message.record.comment_group_id.is_none()
                        && message.ui.comment_responses.is_empty()
                        && !message.ui.output.text.contains("![")
                        && !message.ui.output.text.to_ascii_lowercase().contains("<img")
                        && !comments
                            .iter()
                            .any(|comment| comment.reference.message_id == message.record.id)
                }
            };
            let (message_id, geometry) = match message {
                TimelineMessage::User(group) => (group.record.id, RowGeometry::default()),
                TimelineMessage::Agent(message) => {
                    (message.record.id, Self::agent_row_geometry(message))
                }
            };
            let id: SharedString = format!("deferred-timeline-{thread_id}-{message_id}").into();
            // The row a press began in keeps its native participants while
            // the button is held, even scrolled away, so a drag that starts
            // later still has its anchor.
            let pinned = self
                .timeline_press
                .as_ref()
                .is_some_and(|press| press.row.as_ref() == Some(&id));
            let native = (!(stable && culling))
                .then(|| TimelineRow::capture(messages, index, message_id, &steering_groups))
                .flatten();
            rows.push(PlannedRow {
                id,
                index,
                message_id,
                geometry,
                native,
                pinned,
            });
        }
        (rows, steering_groups)
    }
}
