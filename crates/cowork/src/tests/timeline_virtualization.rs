//! Virtualization and selection regressions for the timeline in a real
//! Cowork window. No provider, VM, timing measurements, or private row-cache
//! hooks.

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

/// Replaces a completed reply's output with finished work: reasoning, a file
/// read with its output, an edit with its diff, and text between them, all
/// open, as someone would leave them after reviewing the run.
fn open_work(message: &mut crate::timeline::AgentMessage, index: usize, cx: &mut gpui::App) {
    use crate::timeline::{AgentOutput, AgentStep, AgentToolCall, OutputMark, tool_call};
    let response = timeline_bench::SHORT;
    let source = (0..12)
        .map(|line| format!("fn step_{index}_{line}() {{}}"))
        .collect::<Vec<_>>()
        .join("\n");
    let call = |id: &str, name: &str, arguments, result: &str| {
        AgentStep::ToolCall(AgentToolCall {
            call: tool_call(&format!("{id}-{index}"), name, arguments),
            result: Some(vec![agent::model::ToolResultContent::Text {
                text: result.into(),
            }]),
        })
    };
    let steps = vec![
        AgentStep::Thinking("Reading the module before changing it.\n\n".repeat(4)),
        call(
            "read",
            "read_file",
            serde_json::json!({ "path": "cowork/src/lib.rs" }),
            &source,
        ),
        AgentStep::Text("Now the edit.".into()),
        call(
            "edit",
            "edit_file",
            serde_json::json!({
                "path": "cowork/src/lib.rs",
                "old_string": "fn old() {}\nfn kept() {}",
                "new_string": "fn new() {}\nfn kept() {}\nfn added() {}",
            }),
            "Edited.",
        ),
        AgentStep::Text(response.into()),
    ];
    let steps_len = steps.len();
    message.show_output(AgentOutput::for_test(steps, response.into(), true), cx);
    message.show_file_changes(cx);
    message.ui.committed = OutputMark {
        steps: steps_len,
        tail: response.len(),
        answered: 2,
    };
    message.ui.work_expanded = true;
    for step in [0, 1, 3] {
        message.ui.step_view_mut(step).unwrap().set_expanded(true);
    }
}

impl Fixture {
    fn new<'a>(
        cx: &'a mut gpui::TestAppContext,
        response: &str,
    ) -> (Self, tokio::runtime::Runtime, &'a mut VisualTestContext) {
        Self::build(cx, response, false)
    }

    /// History whose every reply has finished, open work.
    fn with_open_work(
        cx: &mut gpui::TestAppContext,
    ) -> (Self, tokio::runtime::Runtime, &mut VisualTestContext) {
        Self::build(cx, timeline_bench::SHORT, true)
    }

    fn build<'a>(
        cx: &'a mut gpui::TestAppContext,
        response: &str,
        work: bool,
    ) -> (Self, tokio::runtime::Runtime, &'a mut VisualTestContext) {
        let (cowork, runtime, cx) = composer_test_cowork(cx);
        cx.simulate_resize(size(px(1200.), px(760.)));
        let (thread, users) = cx.update(|window, cx| {
            let mut messages = timeline_bench::timeline(MESSAGE_COUNT, response, cx);
            for (index, message) in messages.iter_mut().enumerate() {
                if let TimelineMessage::Agent(message) = message
                    && work
                {
                    open_work(message, index, cx);
                }
            }
            let users = messages
                .iter_mut()
                .enumerate()
                .filter_map(|(index, message)| match message {
                    TimelineMessage::User(group) => {
                        group.record.blocks[0].text = user_text(index);
                        let selector: &'static str = Box::leak(
                            format!("timeline-user-text-{}", group.record.blocks[0].id)
                                .into_boxed_str(),
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

    /// Only rows near the view are built, wherever it is.
    fn assert_culled(&self, cx: &mut VisualTestContext) {
        let visible = self
            .users
            .iter()
            .filter(|(_, selector)| cx.debug_bounds(selector).is_some())
            .count();
        assert!(
            visible < self.users.len() / 4,
            "only near-view rows should be built, found {visible} user rows"
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

    /// The debug selector of the read's details in the reply at `index`.
    fn read_details(&self, index: usize, cx: &VisualTestContext) -> &'static str {
        self.read_selector("tool-call-details", index, cx)
    }

    /// The debug selector `{prefix}-{reply id}-1` of the read in the reply at
    /// `index`.
    fn read_selector(&self, prefix: &str, index: usize, cx: &VisualTestContext) -> &'static str {
        let id = self.thread.read_with(cx, |thread, _| {
            let TimelineMessage::Agent(message) = &thread.timeline[index] else {
                panic!("replies are at odd indices");
            };
            message.record.id
        });
        Box::leak(format!("{prefix}-{id}-1").into_boxed_str())
    }

    /// Changes the local view state of the reply at `index`, as its toggles
    /// do, and redraws.
    fn update_reply(
        &self,
        index: usize,
        cx: &mut VisualTestContext,
        update: impl FnOnce(&mut crate::timeline::AgentMessageUi),
    ) {
        self.thread.update(cx, |thread, _| {
            let TimelineMessage::Agent(message) = &mut thread.timeline[index] else {
                panic!("replies are at odd indices");
            };
            update(&mut message.ui);
        });
        self.redraw(cx);
    }

    /// Scrolls through the whole history, so that every row has been drawn
    /// and measured, and returns to where the view was. Returns the extent
    /// then, which is exact.
    fn measure_all(&self, cx: &mut VisualTestContext) -> Pixels {
        let (start, _) = self.geometry(cx);
        let mut offset = px(0.);
        for _ in 0..1000 {
            self.cowork.update(cx, |cowork, cx| {
                cowork
                    .timeline_scroll_handle
                    .set_offset(point(px(0.), offset));
                cx.notify();
            });
            settle(cx);
            let (current, max) = self.geometry(cx);
            if current <= -max + px(1.) {
                break;
            }
            offset = current - px(500.);
        }
        let (_, max) = self.geometry(cx);
        self.cowork.update(cx, |cowork, cx| {
            cowork
                .timeline_scroll_handle
                .set_offset(point(px(0.), start));
            cx.notify();
        });
        settle(cx);
        max
    }

    fn clear_selection(&self, cx: &mut VisualTestContext) {
        cx.update(TextSelection::clear);
        self.redraw(cx);
        cx.update(|window, cx| assert!(!TextSelection::has_selection(window, cx)));
    }
}

/// Scrolls to the middle of the history, where no row has been measured.
fn middle(fixture: &Fixture, cx: &mut VisualTestContext) {
    let (_, max) = fixture.geometry(cx);
    fixture.cowork.update(cx, |cowork, cx| {
        cowork.follow_generation = false;
        cowork
            .timeline_scroll_handle
            .set_offset(point(px(0.), -max / 2.));
        cx.notify();
    });
    settle(cx);
}

/// A prompt's text, or a reply's work summary or tool call, in the view, and
/// where it is.
fn visible_row(fixture: &Fixture, cx: &mut VisualTestContext) -> (&'static str, Pixels) {
    let mut selectors = fixture
        .users
        .iter()
        .map(|(_, selector)| *selector)
        .collect::<Vec<_>>();
    for index in (1..MESSAGE_COUNT).step_by(2) {
        let id = fixture
            .thread
            .read_with(cx, |thread, _| match &thread.timeline[index] {
                TimelineMessage::Agent(message) => message.record.id,
                TimelineMessage::User(_) => unreachable!("replies are at odd indices"),
            });
        for selector in [
            format!("toggle-work-{id}"),
            format!("tool-call-{id}-1"),
            format!("tool-call-{id}-3"),
        ] {
            selectors.push(Box::leak(selector.into_boxed_str()));
        }
    }
    selectors
        .into_iter()
        .find_map(|selector| {
            cx.debug_bounds(selector)
                .filter(|bounds| bounds.top() > px(60.) && bounds.top() < px(600.))
                .map(|bounds| (selector, bounds.top()))
        })
        .expect("a row in the view")
}

fn scroll_wheel(delta: Pixels, cx: &mut VisualTestContext) {
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: point(px(600.), px(380.)),
        delta: gpui::ScrollDelta::Pixels(point(px(0.), delta)),
        ..Default::default()
    });
    settle(cx);
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
fn selection_survives_new_messages(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    let next = fixture
        .thread
        .update(cx, |thread, _| thread.timeline.pop().unwrap());
    fixture.redraw(cx);
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
        "a new row must not remount the selection anchor"
    );
    fixture.clear_selection(cx);
    fixture.assert_culled(cx);
}

#[gpui::test]
fn history_builds_only_rows_near_the_view(cx: &mut gpui::TestAppContext) {
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
    let (offset, max) = fixture.geometry(cx);
    assert_eq!(
        offset, -max,
        "the bottom stays the bottom as its rows are measured"
    );

    // Redraws leave an interior offset and the extent unchanged.
    middle(&fixture, cx);
    let before = fixture.geometry(cx);
    for _ in 0..8 {
        fixture.redraw(cx);
        assert_same_geometry(fixture.geometry(cx), before);
    }
    // Repeated parking exercises the idle path without a wall-clock assertion.
    cx.run_until_parked();
    assert_same_geometry(fixture.geometry(cx), before);
}

/// Rows never drawn count as an estimate; once drawn, their real height
/// replaces it without moving what is on screen. Open work makes every
/// reply much taller than the prompts around it, so estimates are wrong.
#[gpui::test]
fn measuring_rows_above_the_view_keeps_the_view_still(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::with_open_work(cx);
    middle(&fixture, cx);
    fixture.assert_culled(cx);
    for _ in 0..6 {
        let (selector, top) = visible_row(&fixture, cx);
        scroll_wheel(px(90.), cx);
        let moved = cx
            .debug_bounds(selector)
            .expect("the row is still in the view");
        assert!(
            (moved.top() - (top + px(90.))).abs() < px(1.),
            "the view must move by the wheel's distance: {top:?} -> {:?}",
            moved.top()
        );
    }
    fixture.assert_culled(cx);
}

/// Finished work left open, with its tool details and diffs, is built only
/// near the view too, and once every row has been measured, the extent no
/// longer changes.
#[gpui::test]
fn open_work_builds_only_near_the_view_and_its_extent_converges(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::with_open_work(cx);
    let first = fixture.read_details(1, cx);
    let middle_details = fixture.read_details(MESSAGE_COUNT / 2 + 1, cx);
    fixture.assert_culled(cx);
    assert!(
        cx.debug_bounds(first).is_some(),
        "open work near the top shows"
    );
    assert!(
        cx.debug_bounds(middle_details).is_none(),
        "open work far offscreen must not be built"
    );
    let max = fixture.measure_all(cx);
    fixture.assert_culled(cx);
    assert_same_geometry(fixture.geometry(cx), (px(0.), max));
    assert_eq!(fixture.measure_all(cx), max);
    for _ in 0..3 {
        fixture.redraw(cx);
        assert_same_geometry(fixture.geometry(cx), (px(0.), max));
    }

    fixture.bottom(cx);
    assert_same_geometry(fixture.geometry(cx), (-max, max));
    assert!(cx.debug_bounds(first).is_none());
    assert!(cx.debug_bounds(middle_details).is_none());
}

/// Folding work above the view changes a row that isn't drawn: the view
/// stays where it is, and the row shows folded when it is drawn again.
#[gpui::test]
fn folding_work_above_the_view_keeps_the_view_still(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::with_open_work(cx);
    fixture.measure_all(cx);
    middle(&fixture, cx);
    let (selector, top) = visible_row(&fixture, cx);
    let row = MESSAGE_COUNT / 4;
    let details = fixture.read_details(row, cx);
    let summary = fixture
        .thread
        .read_with(cx, |thread, _| match &thread.timeline[row] {
            TimelineMessage::Agent(message) => {
                &*Box::leak(format!("toggle-work-{}", message.record.id).into_boxed_str())
            }
            TimelineMessage::User(_) => unreachable!("replies are at odd indices"),
        });
    fixture.update_reply(row, cx, |ui| ui.work_expanded = false);
    assert_eq!(
        cx.debug_bounds(selector).map(|bounds| bounds.top()),
        Some(top)
    );
    fixture.top(cx);
    // Scroll down to the folded row, measuring the rows on the way.
    for _ in 0..200 {
        if cx.debug_bounds(summary).is_some() {
            break;
        }
        scroll_wheel(px(-300.), cx);
    }
    assert!(cx.debug_bounds(summary).is_some());
    assert!(cx.debug_bounds(details).is_none(), "the row shows folded");
}

/// A click in history, here on a tool's toggle, is not a drag: only rows
/// near the view are built while the button is held, and the click lands.
#[gpui::test]
fn clicking_a_history_toggle_keeps_history_culled(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::with_open_work(cx);
    let toggle = cx
        .debug_bounds(fixture.read_selector("tool-call", 1, cx))
        .expect("the first read's toggle shows")
        .center();
    let details = fixture.read_details(1, cx);
    let (_, open) = fixture.geometry(cx);

    cx.simulate_mouse_down(toggle, MouseButton::Left, gpui::Modifiers::default());
    fixture.redraw(cx);
    fixture.assert_culled(cx);
    assert!(cx.debug_bounds(details).is_some());
    cx.simulate_mouse_up(toggle, MouseButton::Left, gpui::Modifiers::default());
    fixture.redraw(cx);

    assert!(
        cx.debug_bounds(details).is_none(),
        "the click folds the details"
    );
    fixture.assert_culled(cx);
    let (_, folded) = fixture.geometry(cx);
    assert!(folded < open - px(100.));
}

/// A finished run that a steer joined draws the group there, and builds
/// only near the view like any row.
#[gpui::test]
fn runs_with_steers_build_only_near_the_view(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    let (group_index, _) = fixture.users[fixture.users.len() / 2];
    fixture.thread.update(cx, |thread, cx| {
        let TimelineMessage::User(group) = &thread.timeline[group_index] else {
            unreachable!("users are user groups");
        };
        let id = group.record.id;
        let TimelineMessage::Agent(message) = &mut thread.timeline[group_index - 1] else {
            unreachable!("agent messages alternate with users");
        };
        message.ui.steered(id, cx);
    });
    fixture.redraw(cx);
    fixture.assert_culled(cx);
    let before = fixture.geometry(cx);
    fixture.redraw(cx);
    assert_same_geometry(fixture.geometry(cx), before);
}

/// A width change measures only the rows drawn at the new width; once every
/// row has been measured, the extent is the native one at that width.
#[gpui::test]
fn width_changes_build_only_rows_near_the_view(cx: &mut gpui::TestAppContext) {
    let response = "A completed paragraph with enough ordinary words to wrap differently when the timeline becomes narrower. ".repeat(12);
    let (fixture, _runtime, cx) = Fixture::new(cx, &response);
    let wide = fixture.measure_all(cx);

    cx.simulate_resize(size(px(640.), px(760.)));
    fixture.redraw(cx);
    fixture.assert_culled(cx);
    let narrow = fixture.measure_all(cx);
    assert!(
        narrow > wide + px(500.),
        "narrower text must increase the full history height: {wide:?} -> {narrow:?}"
    );
    fixture.assert_culled(cx);
    assert_same_geometry(fixture.geometry(cx), (px(0.), narrow));
}

/// The rows at the bottom grow, as a streamed reply's does: following the
/// bottom keeps it in view. A change to a row far above isn't drawn, so it
/// doesn't move the view either.
#[gpui::test]
fn growing_rows_keep_follow_bottom_pinned(cx: &mut gpui::TestAppContext) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    fixture.bottom(cx);
    fixture
        .cowork
        .update(cx, |cowork, _| cowork.follow_generation = true);
    let (_, before) = fixture.geometry(cx);

    // Change replies' views without notifying Cowork: the views' own
    // notifications redraw the rows that draw them.
    let replacement = "A late parsed paragraph grows a completed reply.\n\n".repeat(80);
    for index in [1, MESSAGE_COUNT - 1] {
        fixture.thread.update(cx, |thread, cx| {
            let crate::timeline::TimelineMessage::Agent(record) = &thread.timeline[index] else {
                panic!("replies are at odd indices");
            };
            let message = thread
                .agent_message_mut(record.record.id.into_bytes())
                .unwrap();
            message.ui.output.text = replacement.clone();
            message
                .ui
                .text_view
                .update(cx, |view, cx| view.set_text(&replacement, cx));
        });
    }
    settle(cx);
    let after = fixture.geometry(cx);
    assert!(
        after.1 > before + px(500.),
        "the last reply grew: {before:?} -> {after:?}"
    );
    assert!(
        (after.0 + after.1).abs() < px(1.),
        "follow-bottom lost its pin: {after:?}"
    );
    fixture.assert_culled(cx);
    for _ in 0..3 {
        fixture.redraw(cx);
        assert_same_geometry(fixture.geometry(cx), after);
    }
}

#[gpui::test]
fn cross_row_drag_builds_only_its_rows_and_copies_every_row_in_order(
    cx: &mut gpui::TestAppContext,
) {
    let (fixture, _runtime, cx) = Fixture::new(cx, timeline_bench::SHORT);
    fixture.assert_culled(cx);
    let first = cx
        .debug_bounds(fixture.users[0].1)
        .expect("first user text");
    // From inside the first prompt's text to inside the last's.
    let start = point(first.left() + px(40.), first.center().y);
    cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
    // Scroll before moving the pointer: the drag already has an anchor, but
    // GPUI reports no selection until its endpoints differ.
    fixture.redraw(cx);
    cx.update(|window, cx| {
        assert!(!TextSelection::has_selection(window, cx));
    });
    // A press alone is not a drag: only rows near the view are built.
    fixture.assert_culled(cx);

    // Scrolling with the button held is deterministic and does not depend on
    // auto-scroll timing. The anchor's row stays built while held, though
    // offscreen, so the drag that starts below still has its anchor.
    fixture.bottom(cx);
    assert!(
        cx.debug_bounds(fixture.users[0].1).is_some(),
        "the pressed row must stay built while the button is held"
    );
    assert!(cx.debug_bounds(fixture.users[25].1).is_none());
    let last = cx
        .debug_bounds(fixture.users.last().unwrap().1)
        .expect("last user text");
    let end = point(last.left() + px(40.), last.center().y);
    cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
    cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
    fixture.redraw(cx);
    assert_selection(cx);
    let (offset, max) = fixture.geometry(cx);
    assert_eq!(
        offset, -max,
        "building every row keeps the view at the bottom"
    );
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
    // The end rows give what of them is selected; every row between, its
    // whole text, in timeline order: each prompt, then its reply.
    let (first_text, last_text) = (
        user_text(fixture.users[0].0),
        user_text(fixture.users.last().unwrap().0),
    );
    let head = copied.lines().next().unwrap();
    let tail = copied.lines().last().unwrap();
    assert!(
        !head.is_empty() && head.len() < first_text.len() && first_text.ends_with(head),
        "the first prompt gives the end of it: {head:?}"
    );
    assert!(
        !tail.is_empty() && tail.len() < last_text.len() && last_text.starts_with(tail),
        "the last prompt gives the start of it: {tail:?}"
    );
    let middle = fixture.users[1..fixture.users.len() - 1]
        .iter()
        .map(|(index, _)| user_text(*index))
        .collect::<Vec<_>>()
        .join(&format!("\n{}\n", timeline_bench::SHORT));
    assert_eq!(
        copied,
        format!(
            "{head}\n{reply}\n{middle}\n{reply}\n{tail}",
            reply = timeline_bench::SHORT
        )
    );

    fixture.clear_selection(cx);
    fixture.assert_culled(cx);
    assert_same_geometry(fixture.geometry(cx), (-max, max));
    assert!(cx.debug_bounds(fixture.users.last().unwrap().1).is_some());
}
