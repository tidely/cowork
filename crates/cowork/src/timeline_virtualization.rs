//! Viewport culling without replacing the timeline's outer scroll layout.
//!
//! Wrap only stable, text-only row bodies; keep their outer margins outside
//! this element. Disable culling for global selection/drag, and leave rows
//! with interactive or independently changing content on the original path.

use std::{cell::Cell, rc::Rc};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity, EntityId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Refineable as _, SharedString, Style,
    Subscription, TextStyle, WeakEntity, Window, px, relative, size,
};
use gpui_base::TextViewState;

use crate::Cowork;

#[derive(PartialEq)]
struct HeightKey {
    culling: bool,
    wrap_width: Pixels,
    text_style: TextStyle,
    rem_size: Pixels,
}

/// Lives in GPUI element state even when the row has no constructed child.
struct RowState {
    key: HeightKey,
    height: Rc<Cell<Option<Pixels>>>,
    measured_width: Rc<Cell<Option<Pixels>>>,
    source_id: Option<EntityId>,
    subscription: Option<Subscription>,
}

impl RowState {
    fn new(key: HeightKey) -> Self {
        Self {
            key,
            height: Rc::new(Cell::new(None)),
            measured_width: Rc::new(Cell::new(None)),
            source_id: None,
            subscription: None,
        }
    }

    fn set_key(&mut self, key: HeightKey) {
        if self.key != key {
            self.key = key;
            self.height.set(None);
        }
    }

    fn observe_source(
        &mut self,
        source: Option<&Entity<TextViewState>>,
        owner: &WeakEntity<Cowork>,
        cx: &mut App,
    ) {
        let source_id = source.map(Entity::entity_id);
        if self.source_id == source_id {
            return;
        }
        self.subscription = None;
        self.source_id = source_id;
        self.height.set(None);
        if let Some(source) = source {
            let mut rendered = source.read(cx).rendered_text();
            let height = self.height.clone();
            let owner = owner.clone();
            self.subscription = Some(cx.observe(source, move |source, cx| {
                let next = source.read(cx).rendered_text();
                // Selection/highlight notifications do not change geometry.
                if next != rendered {
                    rendered = next;
                    height.set(None);
                    notify_owner(&owner, cx);
                }
            }));
        }
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
/// its containing element; content changes must notify `source` or change ID.
pub(crate) struct DeferredTimelineRow {
    id: SharedString,
    wrap_width: Pixels,
    owner: WeakEntity<Cowork>,
    source: Option<Entity<TextViewState>>,
    culling: bool,
    build: Option<BuildRow>,
    child: Option<AnyElement>,
}

impl DeferredTimelineRow {
    pub(crate) fn new(
        id: SharedString,
        wrap_width: Pixels,
        owner: WeakEntity<Cowork>,
        source: Option<Entity<TextViewState>>,
        culling: bool,
        build: impl FnOnce(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id,
            wrap_width,
            owner,
            source,
            culling,
            build: Some(Box::new(build)),
            child: None,
        }
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
            culling: self.culling,
            wrap_width: self.wrap_width,
            text_style: window.text_style(),
            rem_size: window.rem_size(),
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
                state.observe_source(self.source.as_ref(), &self.owner, cx);
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
        if self.culling {
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
        _bounds: Bounds<Pixels>,
        _height: &mut Self::RequestLayoutState,
        prepainted: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if *prepainted {
            self.child.as_mut().unwrap().paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> HeightKey {
        HeightKey {
            culling: true,
            wrap_width: px(600.),
            text_style: TextStyle::default(),
            rem_size: px(16.),
        }
    }

    #[test]
    fn height_survives_same_key_but_not_width_text_style_or_rem_changes() {
        let mut state = RowState::new(key());
        let observer_height = state.height.clone();
        state.height.set(Some(px(120.)));
        state.set_key(key());
        assert_eq!(state.height.get(), Some(px(120.)));
        for changed in [
            HeightKey {
                culling: false,
                ..key()
            },
            HeightKey {
                wrap_width: px(500.),
                ..key()
            },
            HeightKey {
                text_style: TextStyle {
                    font_size: px(20.).into(),
                    ..TextStyle::default()
                },
                ..key()
            },
            HeightKey {
                rem_size: px(20.),
                ..key()
            },
        ] {
            state.set_key(key());
            state.height.set(Some(px(120.)));
            state.set_key(changed);
            assert_eq!(observer_height.get(), None);
        }
    }
}
