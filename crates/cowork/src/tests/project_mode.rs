//! The project mode: its default, where it sits, how collaborators learn of
//! it, and that only the host can change it.

use gpui_component::WindowExt as _;

use crate::project_mode::ProjectMode;

use super::*;

#[gpui::test]
fn the_mode_starts_as_read_and_moves_into_the_new_thread(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.active_project_mode(cx), (ProjectMode::Read, true));
    });
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

#[gpui::test]
fn the_write_option_warns_while_hovered(cx: &mut gpui::TestAppContext) {
    let (_cowork, _runtime, cx) = composer_test_cowork(cx);
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
}

#[gpui::test]
fn picking_write_from_the_menu_applies_it_without_a_sandbox(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let trigger = cx.debug_bounds("project-mode").expect("mode toggle");
    cx.simulate_click(trigger.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let write = cx.debug_bounds("project-mode-write").expect("Write");
    cx.simulate_click(write.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    // Nothing runs yet, so there is nothing to restart and no question.
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.active_project_mode(cx).0, ProjectMode::Write);
    });
}

#[gpui::test]
fn collaborators_follow_the_hosts_mode_but_cannot_change_it(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    session.settle();
    let mirror = session.collaborator_thread().expect("joined thread");
    session
        .collaborator
        .read_with(session.cx, |collaborator, cx| {
            assert_eq!(
                collaborator.active_project_mode(cx),
                (ProjectMode::Read, false)
            );
        });

    let host_thread = session.host_thread.downgrade();
    session.host.update(session.cx, |host, cx| {
        host.set_project_mode(Some(host_thread), ProjectMode::Write, cx);
    });
    session.wait_until("the collaborator sees Write", |session| {
        mirror.read_with(session.cx, |thread, _| {
            thread.project_mode() == ProjectMode::Write
        })
    });

    mirror.update(session.cx, |thread, cx| {
        assert!(!thread.set_project_mode(ProjectMode::Read, cx));
        assert_eq!(thread.project_mode(), ProjectMode::Write);
    });
    session.settle();
    session.host_thread.read_with(session.cx, |thread, _| {
        assert_eq!(thread.project_mode(), ProjectMode::Write);
    });
}

#[gpui::test]
fn joiners_get_the_mode_in_their_snapshot(cx: &mut gpui::TestAppContext) {
    let session = Collaboration::start(cx);
    let welcome = session.host_thread.update(session.cx, |thread, cx| {
        thread.set_project_mode(ProjectMode::Write, cx);
        protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: thread.to_protocol(),
            draft: thread.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        }
    });
    let mirror = session.cx.new(|cx| {
        Thread::from_welcome(
            welcome,
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        )
    });
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(thread.project_mode(), ProjectMode::Write);
    });
}
