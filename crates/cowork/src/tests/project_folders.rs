//! The project's folders: where they are kept before and after the thread
//! exists, how collaborators learn of them, and that only the host can
//! change them.

use std::path::Path;

use crate::project_folders::ProjectFolder;

use super::*;

fn names(folders: &[ProjectFolder]) -> Vec<String> {
    folders
        .iter()
        .map(|folder| folder.name.to_string())
        .collect()
}

#[gpui::test]
fn project_folders_move_into_the_new_thread(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    // With no folders there is only the add button.
    assert!(cx.debug_bounds("project-folders-add").is_some());
    assert!(cx.debug_bounds("project-folders-folder-0").is_none());
    assert!(cx.debug_bounds("project-folders-separator-0").is_none());

    let zed = PathBuf::from("/src/zed");
    let cowork_folder = PathBuf::from("/src/cowork");
    cowork.update(cx, |cowork, cx| {
        cowork.add_project_folders(None, vec![zed.clone(), cowork_folder.clone()], cx);
        cowork.remove_project_folder(None, &cowork_folder, cx);
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("project-folders-folder-0").is_some());
    assert!(cx.debug_bounds("project-folders-folder-1").is_none());
    assert!(cx.debug_bounds("project-folders-add").is_some());
    // One line, between the folder and the add button.
    assert!(cx.debug_bounds("project-folders-separator-0").is_some());
    assert!(cx.debug_bounds("project-folders-separator-1").is_none());

    cx.simulate_input("question");
    cx.run_until_parked();
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx)));
    let thread = cowork.read_with(cx, |cowork, cx| {
        assert!(cowork.new_thread_project_folders.is_empty());
        cowork.active_thread(cx).expect("a new thread")
    });
    assert_eq!(
        thread.read_with(cx, |thread, _| thread.project_folders().to_vec()),
        [ProjectFolder::local(zed.clone())]
    );

    // Folders added later go to the thread, once each.
    cowork.update(cx, |cowork, cx| {
        let target = Some(thread.downgrade());
        cowork.add_project_folders(target, vec![cowork_folder.clone(), zed.clone()], cx);
        let (folders, editable) = cowork.active_project(cx);
        assert!(editable);
        assert_eq!(
            folders,
            [
                ProjectFolder::local(zed),
                ProjectFolder::local(cowork_folder)
            ]
        );
        assert!(cowork.new_thread_project_folders.is_empty());
    });
    cx.run_until_parked();
    // A line between the two folders, and one before the add button.
    let separators = [
        cx.debug_bounds("project-folders-separator-0"),
        cx.debug_bounds("project-folders-separator-1"),
    ];
    let [Some(between), Some(before_add)] = separators else {
        panic!("expected two separators, got {separators:?}");
    };
    let first = cx.debug_bounds("project-folders-folder-0").unwrap();
    let second = cx.debug_bounds("project-folders-folder-1").unwrap();
    let add = cx.debug_bounds("project-folders-add").unwrap();
    assert!(first.right() <= between.left() && between.right() <= second.left());
    assert!(second.right() <= before_add.left() && before_add.right() <= add.left());
    assert!(cx.debug_bounds("project-folders-separator-2").is_none());
}

#[gpui::test]
fn collaborators_see_folder_names_as_the_host_changes_them(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let host_thread = session.host_thread.downgrade();
    session.host.update(session.cx, |host, cx| {
        host.add_project_folders(
            Some(host_thread.clone()),
            vec![
                PathBuf::from("/home/host/src/zed"),
                PathBuf::from("/home/host/src/cowork"),
            ],
            cx,
        );
    });
    session.wait_until("the collaborator sees both folders", |session| {
        session.collaborator_thread().is_some_and(|thread| {
            thread.read_with(session.cx, |thread, _| thread.project_folders().len() == 2)
        })
    });
    let mirror = session.collaborator_thread().expect("joined thread");
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(names(thread.project_folders()), ["zed", "cowork"]);
        // Paths never leave the host.
        assert!(
            thread
                .project_folders()
                .iter()
                .all(|folder| folder.path.is_none())
        );
    });
    session
        .collaborator
        .read_with(session.cx, |collaborator, cx| {
            assert!(!collaborator.active_project(cx).1);
        });

    session.host.update(session.cx, |host, cx| {
        host.remove_project_folder(Some(host_thread), Path::new("/home/host/src/zed"), cx);
    });
    session.wait_until("the collaborator sees the removal", |session| {
        mirror.read_with(session.cx, |thread, _| thread.project_folders().len() == 1)
    });
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(names(thread.project_folders()), ["cowork"]);
    });
}

#[gpui::test]
fn joiners_get_the_folder_names_in_their_snapshot(cx: &mut gpui::TestAppContext) {
    let session = Collaboration::start(cx);
    let host_thread = session.host_thread.clone();
    let welcome = host_thread.update(session.cx, |thread, _| {
        thread.add_project_folders(vec![PathBuf::from("/home/host/src/zed")]);
        protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: thread.to_protocol(),
            draft: thread.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        }
    });
    assert_eq!(welcome.thread.project_folders, ["zed"]);
    let mirror = session.cx.new(|cx| {
        Thread::from_welcome(
            welcome,
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        )
    });
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(
            thread.project_folders(),
            ProjectFolder::mirrored(vec!["zed".into()])
        );
    });
}

#[gpui::test]
fn only_the_host_changes_the_project(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    session.settle();
    let mirror = session.collaborator_thread().expect("joined thread");
    mirror.update(session.cx, |thread, _| {
        assert!(!thread.add_project_folders(vec![PathBuf::from("/home/peer/src")]));
        assert!(!thread.remove_project_folder(Path::new("/home/peer/src")));
        assert!(thread.project_folders().is_empty());
    });
    session.settle();
    session.host_thread.read_with(session.cx, |thread, _| {
        assert!(thread.project_folders().is_empty());
    });
}
