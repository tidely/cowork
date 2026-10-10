//! The sidebar's thread rows, archiving threads from them, and the archive.

use gpui_component::WindowExt as _;

use super::*;

/// The debug selector `name` gives `thread_id`'s row or button.
fn selector(name: &str, thread_id: Uuid) -> &'static str {
    Box::leak(format!("{name}-{thread_id}").into_boxed_str())
}

/// Adds one of the user's own threads, whose one run took `seconds`.
fn add_thread(cowork: &Entity<Cowork>, seconds: u64, cx: &mut gpui::VisualTestContext) -> Uuid {
    cowork.update(cx, |cowork, cx| {
        let thread_id = Uuid::new_v4();
        let draft = ThreadDraft::new(cowork.local_participant_id);
        let thread = cx.new(|_| test_thread(thread_id, Vec::new(), draft));
        let id = Uuid::new_v4().into_bytes();
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(agent_started(Uuid::from_bytes(id), None, "prompt"), cx);
            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id,
                    outcome: crate::protocol::RunOutcome::Completed,
                    duration: Duration::from_secs(seconds),
                },
                cx,
            );
        });
        cowork
            .thread_store
            .update(cx, |store, _| store.threads.push_front(thread));
        cx.notify();
        thread_id
    })
}

fn draw(cx: &mut gpui::VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

fn click(selector: &'static str, cx: &mut gpui::VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be drawn"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    draw(cx);
}

/// The ids of the sidebar's threads, then of the archived ones, in order.
fn thread_ids(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) -> (Vec<Uuid>, Vec<Uuid>) {
    cowork.read_with(cx, |cowork, cx| {
        let store = cowork.thread_store.read(cx);
        let ids = |threads: &VecDeque<Entity<Thread>>| {
            threads
                .iter()
                .map(|thread| thread.read(cx).instance_id)
                .collect()
        };
        (ids(&store.threads), ids(&store.archived))
    })
}

/// Archives `thread_id` by hovering its row and pressing its button.
fn archive_from_sidebar(thread_id: Uuid, cx: &mut gpui::VisualTestContext) {
    let row = cx
        .debug_bounds(selector("sidebar-thread", thread_id))
        .unwrap();
    cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
    draw(cx);
    let archive = cx
        .debug_bounds(selector("archive-thread", thread_id))
        .unwrap();
    assert!(
        archive.right() <= row.right() && archive.left() > row.center().x,
        "the archive button {archive:?} should sit at the right of its row {row:?}"
    );
    cx.simulate_click(archive.center(), gpui::Modifiers::default());
    draw(cx);
}

#[gpui::test]
fn usage_statistics_outlast_the_threads_of_an_earlier_session(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let dir = std::env::temp_dir().join(format!("cowork-statistics-{}", Uuid::new_v4()));
    let path = dir.join("statistics.json");
    cowork.update(cx, |cowork, _| cowork.start_statistics(Some(path.clone())));
    add_thread(&cowork, 20, cx);
    let used = add_thread(&cowork, 90, cx);
    cowork.update(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(used, cx).unwrap();
        let usage = Usage::new().total_tokens(100);
        cowork.record_turn_usage(&thread, usage, SystemTime::now(), Duration::ZERO, cx);
        cowork.save_statistics(cx);
    });
    let before = cowork.read_with(cx, |cowork, cx| cowork.statistics(cx));

    // Threads aren't saved, so a restart starts with none of them.
    cowork.update(cx, |cowork, cx| {
        cowork
            .thread_store
            .update(cx, |store, _| store.threads.clear());
        cowork.tokens_used = 0;
        cowork.token_activity.clear();
        cowork.deleted_chats = 0;
        cowork.longest_deleted_chat = Duration::ZERO;
        cowork.start_statistics(Some(path.clone()));
    });

    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.statistics(cx), before);
        assert_eq!(cowork.tokens_used, 100);
        assert_eq!(cowork.token_activity.len(), 1);
        assert_eq!(cowork.total_chats(cx), 2);
        assert_eq!(cowork.longest_chat(cx), Duration::from_secs(90));
    });
    add_thread(&cowork, 5, cx);
    cowork.read_with(cx, |cowork, cx| assert_eq!(cowork.total_chats(cx), 3));

    _ = std::fs::remove_dir_all(dir);
}

#[gpui::test]
fn archiving_a_thread_from_the_sidebar_keeps_the_usage_statistics(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let kept = add_thread(&cowork, 20, cx);
    let archived = add_thread(&cowork, 90, cx);
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            let thread = cowork.thread_store.read(cx).thread(archived, cx).unwrap();
            let usage = Usage::new().total_tokens(100);
            cowork.record_turn_usage(&thread, usage, SystemTime::now(), Duration::ZERO, cx);
            cowork.open_thread(archived, window, cx);
        })
    });
    draw(cx);

    archive_from_sidebar(archived, cx);

    assert_eq!(thread_ids(&cowork, cx), (vec![kept], vec![archived]));
    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.active_thread_id, None);
        assert_eq!(cowork.tokens_used, 100);
        assert_eq!(cowork.longest_chat(cx), Duration::from_secs(90));
        assert_eq!(cowork.total_chats(cx), 2);
    });
}

#[gpui::test]
fn a_long_title_is_cut_off_inside_its_row(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let thread_id = add_thread(&cowork, 1, cx);
    cowork.update(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        let title = Cowork::thread_title(&"a very long first prompt ".repeat(10));
        assert!(!title.ends_with('…'));
        thread.update(cx, |thread, _| thread.summary.title = title);
        cx.notify();
    });
    draw(cx);

    let row = cx
        .debug_bounds(selector("sidebar-thread", thread_id))
        .unwrap();
    let title = cx
        .debug_bounds(selector("sidebar-thread-title", thread_id))
        .unwrap();
    assert!(row.size.width <= SIDEBAR_WIDTH);
    assert!(
        title.right() < row.right(),
        "the title {title:?} should end inside its row {row:?}"
    );
}

#[gpui::test]
fn the_archive_button_only_takes_clicks_while_its_row_is_hovered(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let thread_id = add_thread(&cowork, 1, cx);
    draw(cx);

    // The button isn't drawn until the row is hovered.
    assert!(
        cx.debug_bounds(selector("archive-thread", thread_id))
            .is_none()
    );
    // Pressed without hovering first, where the button shows, the row opens.
    let buttons = cx
        .debug_bounds(selector("sidebar-thread-buttons", thread_id))
        .unwrap();
    let at = gpui::point(buttons.right() - px(12.), buttons.center().y);
    cx.simulate_click(at, gpui::Modifiers::default());
    draw(cx);

    assert_eq!(thread_ids(&cowork, cx), (vec![thread_id], vec![]));
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(cowork.active_thread_id, Some(thread_id));
    });
}

#[gpui::test]
fn archiving_a_shared_thread_disconnects_its_collaborators(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let host_thread_id = session
        .host_thread
        .read_with(session.cx, |thread, _| thread.instance_id);
    let mirror_id = session
        .collaborator_thread()
        .unwrap()
        .read_with(session.cx, |thread, _| thread.instance_id);
    session.settle();
    // Joined threads belong to their host, so they can't be archived.
    assert!(
        session
            .cx
            .debug_bounds(selector("sidebar-thread-buttons", mirror_id))
            .is_none()
    );
    assert!(
        session
            .cx
            .debug_bounds(selector("sidebar-thread-buttons", host_thread_id))
            .is_some()
    );

    let (host, collaborator) = (session.host.clone(), session.collaborator.clone());
    session.cx.update(|window, cx| {
        host.update(cx, |host, cx| {
            host.archive_thread(host_thread_id, window, cx)
        });
        // Ignored, as the collaborator doesn't own its mirror.
        collaborator.update(cx, |collaborator, cx| {
            collaborator.archive_thread(mirror_id, window, cx)
        });
    });

    assert_eq!(
        thread_ids(&session.collaborator, session.cx).0,
        vec![mirror_id]
    );
    assert_eq!(
        thread_ids(&session.host, session.cx),
        (vec![], vec![host_thread_id])
    );
    assert!(matches!(
        session
            .host_thread
            .read_with(session.cx, |thread, _| thread.sharing.status()),
        crate::thread::SharingStatus::NotShared
    ));
    session.wait_until("the collaborator is disconnected", |this| {
        this.collaborator_thread().is_none()
    });
}

#[gpui::test]
fn archived_threads_are_restored_or_deleted_from_the_archive(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let restored = add_thread(&cowork, 20, cx);
    let deleted = add_thread(&cowork, 90, cx);
    let open = add_thread(&cowork, 5, cx);
    draw(cx);
    archive_from_sidebar(restored, cx);
    archive_from_sidebar(deleted, cx);

    click("archived-threads", cx);
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(cowork.main_stage, MainStage::Archive)
    });
    assert!(
        cx.debug_bounds(selector("archived-thread", restored))
            .is_some()
    );

    click(selector("restore-thread", restored), cx);
    assert_eq!(
        thread_ids(&cowork, cx),
        (vec![restored, open], vec![deleted])
    );

    click(selector("delete-thread", deleted), cx);
    assert_eq!(thread_ids(&cowork, cx), (vec![restored, open], vec![]));
    assert!(
        cx.debug_bounds(selector("archived-thread", deleted))
            .is_none()
    );
    // What was done in the deleted thread still counts.
    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.longest_chat(cx), Duration::from_secs(90));
        assert_eq!(cowork.total_chats(cx), 3);
    });

    // Opening a thread from the sidebar leaves the archive.
    click(selector("sidebar-thread", open), cx);
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(cowork.main_stage, MainStage::Thread);
        assert_eq!(cowork.active_thread_id, Some(open));
    });
}

#[gpui::test]
fn the_archive_restores_or_deletes_everything_at_once(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let first = add_thread(&cowork, 30, cx);
    let second = add_thread(&cowork, 10, cx);
    draw(cx);
    archive_from_sidebar(first, cx);
    archive_from_sidebar(second, cx);
    click("archived-threads", cx);

    // Restored in the archive's order, most recently archived first.
    click("restore-all-threads", cx);
    assert_eq!(thread_ids(&cowork, cx), (vec![second, first], vec![]));

    archive_from_sidebar(first, cx);
    archive_from_sidebar(second, cx);
    click("archived-threads", cx);

    // Deleting everything asks first.
    click("delete-all-threads", cx);
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(thread_ids(&cowork, cx), (vec![], vec![second, first]));
    cx.simulate_keystrokes("enter");
    draw(cx);

    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(thread_ids(&cowork, cx), (vec![], vec![]));
    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.longest_chat(cx), Duration::from_secs(30));
        assert_eq!(cowork.total_chats(cx), 2);
    });
}
