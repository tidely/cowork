//! Geometry and selection regressions for deferred history in a real Cowork
//! window. No provider, VM, timing measurements, or private row-cache hooks.

use super::*;
use gpui::{Pixels, VisualTestContext, size};
use gpui_base::TextSelection;

const MESSAGE_COUNT: usize = 100;

fn user_text(index: usize) -> String {
    format!("Virtualization row {index:04}.")
}

struct Fixture {
    cowork: Entity<Cowork>,
    thread: Entity<Thread>,
    // Debug selectors are static in GPUI's test API, as in the layout tests.
    users: Vec<(usize, &'static str)>,
}

impl Fixture {
    fn new<'a>(
        cx: &'a mut gpui::TestAppContext,
        response: &str,
    ) -> (Self, tokio::runtime::Runtime, &'a mut VisualTestContext) {
        let (cowork, runtime, cx) = composer_test_cowork(cx);
        cx.simulate_resize(size(px(1200.), px(760.)));
        let (thread, users) = cx.update(|window, cx| {
            let mut messages = timeline_bench::timeline(MESSAGE_COUNT, response, cx);
            let users = messages
                .iter_mut()
                .enumerate()
                .filter_map(|(index, message)| match message {
                    TimelineMessage::User(group) => {
                        group.blocks[0].text = user_text(index);
                        let selector: &'static str = Box::leak(
                            format!("timeline-user-text-{}", group.blocks[0].id).into_boxed_str(),
                        );
                        Some((index, selector))
                    }
                    TimelineMessage::Agent(_) => None,
                })
                .collect();
            let thread_id = Uuid::new_v4();
            let thread = cx
                .new(|_| test_thread(thread_id, messages, ThreadDraft::new(ParticipantId::new())));
            cowork.update(cx, |cowork, cx| {
                cowork.thread_store.update(cx, |store, cx| {
                    store.threads.push_front(thread.clone());
                    cx.notify();
                });
                cowork.active_thread_id = Some(thread_id);
                cowork.sidebar_open = false;
                cowork.follow_generation = false;
                cowork
                    .timeline_scroll_handle
                    .set_offset(point(px(0.), px(0.)));
                cowork.timeline_focus_handle.focus(window, cx);
                cx.notify();
            });
            (thread, users)
        });
        let fixture = Self {
            cowork,
            thread,
            users,
        };
        settle(cx);
        fixture.top(cx);
        (fixture, runtime, cx)
    }

    fn geometry(&self, cx: &VisualTestContext) -> (Pixels, Pixels) {
        self.cowork.read_with(cx, |cowork, _| {
            (
                cowork.timeline_scroll_handle.offset().y,
                cowork.timeline_scroll_handle.max_offset().y,
            )
        })
    }

    fn redraw(&self, cx: &mut VisualTestContext) {
        self.cowork.update(cx, |_, cx| cx.notify());
        settle(cx);
    }

    fn top(&self, cx: &mut VisualTestContext) {
        self.cowork.update(cx, |cowork, cx| {
            cowork.follow_generation = false;
            cowork
                .timeline_scroll_handle
                .set_offset(point(px(0.), px(0.)));
            cx.notify();
        });
        settle(cx);
    }

    fn bottom(&self, cx: &mut VisualTestContext) {
        self.cowork.update(cx, |cowork, cx| {
            cowork.timeline_scroll_handle.scroll_to_bottom();
            cx.notify();
        });
        settle(cx);
    }

    fn assert_culled(&self, cx: &mut VisualTestContext) {
        assert!(
            cx.debug_bounds(self.users[self.users.len() / 2].1)
                .is_none(),
            "a stable middle row must not be prepainted outside the viewport"
        );
        let visible = self
            .users
            .iter()
            .filter(|(_, selector)| cx.debug_bounds(selector).is_some())
            .count();
        assert!(visible > 0, "the viewport must still contain user text");
        assert!(
            visible < self.users.len() / 2,
            "only near-viewport text should be prepainted, found {visible} user rows"
        );
    }

    fn select_first_user(&self, cx: &mut VisualTestContext) {
        self.top(cx);
        let bounds = cx.debug_bounds(self.users[0].1).expect("first user text");
        let start = point(bounds.left() + px(1.), bounds.center().y);
        let end = point(bounds.right() - px(1.), bounds.center().y);
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        self.redraw(cx);
        assert_selection(cx);
    }

    fn clear_selection(&self, cx: &mut VisualTestContext) {
        cx.update(TextSelection::clear);
        self.redraw(cx);
        cx.update(|window, cx| assert!(!TextSelection::has_selection(window, cx)));
    }
}

fn settle(cx: &mut VisualTestContext) {
    // Drain Markdown parsing, owner notifications, and any placeholder
    // correction frames before inspecting the painted selectors/geometry.
    for _ in 0..4 {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    cx.run_until_parked();
}

fn assert_selection(cx: &mut VisualTestContext) {
    cx.update(|window, cx| assert!(TextSelection::has_selection(window, cx)));
}

fn assert_same_geometry(actual: (Pixels, Pixels), expected: (Pixels, Pixels)) {
    assert!(
        (actual.0 - expected.0).abs() < px(1.),
        "scroll offset changed: {expected:?} -> {actual:?}"
    );
    assert!(
        (actual.1 - expected.1).abs() < px(1.),
        "scroll extent changed: {expected:?} -> {actual:?}"
    );
}

#[gpui::test]
fn selection_survives_crossing_the_deferred_history_threshold(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    let next = fixture.thread.update(cx, |thread, _| {
        let next = thread.timeline[63].clone();
        thread.timeline.truncate(63);
        next
    });
    fixture.redraw(cx);
    assert!(cx.debug_bounds(fixture.users[25].1).is_some());
    fixture.select_first_user(cx);
    let selected = cx.update(TextSelection::selected_text);
    assert!(!selected.is_empty());
    fixture
        .thread
        .update(cx, |thread, _| thread.timeline.push(next));
    fixture.redraw(cx);
    assert_selection(cx);
    assert_eq!(
        cx.update(TextSelection::selected_text),
        selected,
        "changing the history rendering mode must not remount the selection anchor"
    );
    fixture.clear_selection(cx);
    fixture.assert_culled(cx);
}

#[gpui::test]
fn settled_history_culls_middle_rows_and_bottom_keeps_the_full_extent(
    cx: &mut gpui::TestAppContext,
) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    fixture.assert_culled(cx);
    assert!(cx.debug_bounds(fixture.users[0].1).is_some());
    assert!(cx.debug_bounds(fixture.users.last().unwrap().1).is_none());
    let (offset, max) = fixture.geometry(cx);
    assert_eq!(offset, px(0.));
    assert!(max > px(1000.), "the fixture must overflow substantially");

    fixture.bottom(cx);
    fixture.assert_culled(cx);
    assert!(cx.debug_bounds(fixture.users[0].1).is_none());
    assert!(cx.debug_bounds(fixture.users.last().unwrap().1).is_some());
    assert_same_geometry(fixture.geometry(cx), (-max, max));

    fixture.top(cx);
    assert_same_geometry(fixture.geometry(cx), (px(0.), max));
}

#[gpui::test]
fn stable_redraws_leave_an_interior_scroll_offset_and_extent_unchanged(
    cx: &mut gpui::TestAppContext,
) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    let (_, max) = fixture.geometry(cx);
    fixture.cowork.update(cx, |cowork, cx| {
        cowork
            .timeline_scroll_handle
            .set_offset(point(px(0.), -max / 3.));
        cx.notify();
    });
    settle(cx);
    let before = fixture.geometry(cx);
    assert!((before.0 + max / 3.).abs() < px(1.));
    for _ in 0..8 {
        fixture.redraw(cx);
        assert_same_geometry(fixture.geometry(cx), before);
    }
    // Repeated parking exercises the idle path without a wall-clock assertion.
    cx.run_until_parked();
    assert_same_geometry(fixture.geometry(cx), before);
}

#[gpui::test]
fn width_changes_remeasure_offscreen_rows_and_match_native_selection_layout(
    cx: &mut gpui::TestAppContext,
) {
    let response = "A completed paragraph with enough ordinary words to wrap differently when the timeline becomes narrower. ".repeat(12);
    let (fixture, _runtime, cx) = Fixture::new(cx, &response);
    let wide = fixture.geometry(cx);
    fixture.select_first_user(cx);
    // Active selection is the production all-native baseline, with precisely
    // the same content, outer scroll handle, and window as the deferred path.
    assert!(cx.debug_bounds(fixture.users[25].1).is_some());
    assert_same_geometry(fixture.geometry(cx), wide);
    fixture.clear_selection(cx);
    fixture.assert_culled(cx);

    cx.simulate_resize(size(px(640.), px(760.)));
    fixture.redraw(cx);
    let narrow = fixture.geometry(cx);
    assert!(
        narrow.1 > wide.1 + px(500.),
        "narrower text must increase the full history height: {wide:?} -> {narrow:?}"
    );
    fixture.select_first_user(cx);
    assert_same_geometry(fixture.geometry(cx), narrow);
    fixture.clear_selection(cx);
    fixture.assert_culled(cx);
    assert_same_geometry(fixture.geometry(cx), narrow);

    cx.simulate_resize(size(px(1200.), px(760.)));
    fixture.redraw(cx);
    assert_same_geometry(fixture.geometry(cx), wide);
}

#[gpui::test]
fn late_offscreen_source_parse_invalidates_height_and_keeps_follow_bottom_pinned(
    cx: &mut gpui::TestAppContext,
) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    fixture.bottom(cx);
    fixture
        .cowork
        .update(cx, |cowork, _| cowork.follow_generation = true);
    let (_, before) = fixture.geometry(cx);
    assert!(cx.debug_bounds(fixture.users[0].1).is_none());

    // Change an already measured, offscreen source without notifying Cowork.
    // Only the source view's asynchronous parse/observer should invalidate its
    // cached height and request the owner frame that follows the new bottom.
    let replacement = "A late parsed paragraph grows an offscreen completed reply.\n\n".repeat(80);
    fixture.thread.update(cx, |thread, cx| {
        let TimelineMessage::Agent(message) = &mut thread.timeline[1] else {
            panic!("the benchmark's second row is a completed agent reply");
        };
        message.output.text = replacement.clone();
        message
            .text_view
            .update(cx, |view, cx| view.set_text(&replacement, cx));
    });
    settle(cx);
    let after = fixture.geometry(cx);
    assert!(
        after.1 > before + px(500.),
        "offscreen source changes must invalidate measured height: {before:?} -> {after:?}"
    );
    assert!(
        (after.0 + after.1).abs() < px(1.),
        "follow-bottom lost its pin: {after:?}"
    );
    fixture.assert_culled(cx);
    assert!(cx.debug_bounds(fixture.users.last().unwrap().1).is_some());
    for _ in 0..3 {
        fixture.redraw(cx);
        assert_same_geometry(fixture.geometry(cx), after);
    }
}

#[gpui::test]
fn cross_row_drag_copies_the_whole_history_then_culls_again_after_clear(
    cx: &mut gpui::TestAppContext,
) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    fixture.assert_culled(cx);
    let (_, max) = fixture.geometry(cx);
    let first = cx
        .debug_bounds(fixture.users[0].1)
        .expect("first user text");
    let start = point(first.left() + px(1.), first.center().y);
    cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
    // Scroll before moving the pointer: the drag already has an anchor, but
    // GPUI reports no selection until its endpoints differ.
    fixture.redraw(cx);
    cx.update(|window, cx| {
        assert!(!TextSelection::has_selection(window, cx));
    });
    assert!(
        cx.debug_bounds(fixture.users[25].1).is_some(),
        "active drag must restore native geometry even for offscreen rows"
    );
    assert_same_geometry(fixture.geometry(cx), (px(0.), max));

    // Scrolling with the button held is deterministic and does not depend on
    // auto-scroll timing. The anchor must survive deferred/native redraws.
    fixture.bottom(cx);
    let last = cx
        .debug_bounds(fixture.users.last().unwrap().1)
        .expect("last user text");
    let end = point(last.right() - px(1.), last.center().y);
    cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
    cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
    fixture.redraw(cx);
    assert_selection(cx);
    assert_same_geometry(fixture.geometry(cx), (-max, max));
    cx.write_to_clipboard(ClipboardItem::new_string("stale clipboard".into()));
    cx.simulate_keystrokes(cfg_select! {
        target_os = "macos" => "cmd-c",
        _ => "ctrl-c",
    });
    cx.run_until_parked();
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("copied text");
    for (index, _) in &fixture.users {
        assert!(
            copied.contains(&user_text(*index)),
            "selection lost user row {index}: {copied:?}"
        );
    }
    assert_eq!(
        copied.matches(timeline_bench::SHORT).count(),
        MESSAGE_COUNT / 2 - 1,
        "copy must include every intervening agent reply, but not the reply after the endpoint"
    );

    fixture.clear_selection(cx);
    fixture.assert_culled(cx);
    assert_same_geometry(fixture.geometry(cx), (-max, max));
    assert!(cx.debug_bounds(fixture.users.last().unwrap().1).is_some());
}
