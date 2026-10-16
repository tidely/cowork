//! The project mode's toggle: where it sits, its menu, and moving the mode
//! into the new thread. How collaborators learn of it is in
//! `project_folders.rs`, with the folders.

use gpui_component::WindowExt as _;

use crate::project_mode::ProjectMode;

use super::*;

#[gpui::test]
fn the_mode_starts_as_read_and_moves_into_the_new_thread(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    // Between the context ring and the model picker.
    let context = cx.debug_bounds("context-indicator").expect("context ring");
    let mode = cx.debug_bounds("project-mode").expect("mode toggle");
    let picker = cx.debug_bounds("model-picker").expect("model picker");
    assert!(context.right() <= mode.left() && mode.right() <= picker.left());

    cowork.update(cx, |cowork, cx| {
        cowork.set_project_mode(None, ProjectMode::Write, cx);
    });
    cx.simulate_input("question");
    cx.run_until_parked();
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx)));
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).expect("a new thread");
        assert_eq!(thread.read(cx).project_mode(), ProjectMode::Write);
        // The next new thread starts over at Read.
        assert_eq!(cowork.new_thread_project_mode, ProjectMode::Read);
    });
}

/// Write and Network warn while hovered, and picking one applies it from
/// the next command on, with nothing to confirm.
#[gpui::test]
fn the_mode_menu_warns_on_hover_and_applies_without_asking(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let trigger = cx.debug_bounds("project-mode").expect("mode toggle");
    cx.simulate_click(trigger.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let write = cx
        .debug_bounds("project-mode-write")
        .expect("the menu lists Write");
    assert!(write.bottom() <= trigger.top(), "the menu opens above");
    assert!(cx.debug_bounds("project-mode-write-warning").is_none());

    cx.simulate_mouse_move(write.center(), None, gpui::Modifiers::none());
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    let warning = cx
        .debug_bounds("project-mode-write-warning")
        .expect("hovering Write shows the warning");
    // The text wraps within the tooltip instead of running past it.
    let text = cx
        .debug_bounds("project-mode-write-warning-text")
        .expect("the warning's text");
    assert!(warning.size.width <= px(280.), "{warning:?}");
    assert!(text.right() <= warning.right(), "{text:?} in {warning:?}");
    assert!(text.size.height > px(30.), "more than one line: {text:?}");

    let network = cx
        .debug_bounds("project-mode-network")
        .expect("the menu lists Network");
    cx.simulate_mouse_move(network.center(), None, gpui::Modifiers::none());
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("project-mode-network-warning").is_some(),
        "hovering Network shows its warning"
    );

    cx.simulate_click(write.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.active_project_mode(cx).0, ProjectMode::Write);
    });
}
