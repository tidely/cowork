//! The project's folders and mode: where they are kept before and after the
//! thread exists, how collaborators learn of them, and that only the host
//! can change them.

use std::path::Path;

use crate::{project_folders::ProjectFolder, project_mode::ProjectMode};

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
    let zed = PathBuf::from("/src/zed");
    let cowork_folder = PathBuf::from("/src/cowork");
    cowork.update(cx, |cowork, cx| {
        cowork.add_project_folders(None, vec![zed.clone(), cowork_folder.clone()], cx);
        cowork.remove_project_folder(None, &cowork_folder, cx);
    });
    cx.run_until_parked();

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

    // Folders added later go to the thread.
    cowork.update(cx, |cowork, cx| {
        let target = Some(thread.downgrade());
        cowork.add_project_folders(target, vec![cowork_folder.clone()], cx);
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
fn the_sandbox_mounts_what_the_host_chose(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update(cx, |cowork, cx| {
        cowork.add_project_folders(None, vec![PathBuf::from("/a/src")], cx);
        cowork.set_project_mode(None, ProjectMode::Write, cx);
    });
    cx.simulate_input("question");
    cx.run_until_parked();
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx)));
    let (thread, key) = cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).expect("a new thread");
        let key = thread.read(cx).instance_id.to_string();
        (thread, key)
    });
    let mounted = |cowork: &Cowork| cowork.sandboxes.project(&key);
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(
            mounted(cowork),
            sandbox::Project {
                folders: vec![sandbox::ProjectFolder {
                    name: "src".into(),
                    path: "/a/src".into(),
                }],
                access: sandbox::Access::Write,
            }
        );
    });

    // A second `src` is mounted beside the first, and changes follow.
    cowork.update(cx, |cowork, cx| {
        let target = Some(thread.downgrade());
        cowork.add_project_folders(target.clone(), vec![PathBuf::from("/b/src")], cx);
        cowork.remove_project_folder(target.clone(), Path::new("/a/src"), cx);
        cowork.set_project_mode(target, ProjectMode::Read, cx);
        let project = mounted(cowork);
        assert!(!project.writable());
        assert_eq!(
            project.folders,
            [sandbox::ProjectFolder {
                name: "src-2".into(),
                path: "/b/src".into(),
            }]
        );
        cowork.set_project_mode(Some(thread.downgrade()), ProjectMode::Network, cx);
        assert_eq!(mounted(cowork).access, sandbox::Access::Network);
    });
}

#[gpui::test]
fn collaborators_mirror_the_project_but_cannot_change_it(cx: &mut gpui::TestAppContext) {
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
        host.set_project_mode(Some(host_thread), ProjectMode::Write, cx);
    });
    session.wait_until("the collaborator sees the project", |session| {
        session.collaborator_thread().is_some_and(|thread| {
            thread.read_with(session.cx, |thread, _| {
                thread.project_folders().len() == 2 && thread.project_mode() == ProjectMode::Write
            })
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
            assert_eq!(
                collaborator.active_project_mode(cx),
                (ProjectMode::Write, false)
            );
        });

    mirror.update(session.cx, |thread, cx| {
        assert!(!thread.add_project_folders(vec![PathBuf::from("/home/peer/src")]));
        assert!(!thread.remove_project_folder(Path::new("/home/host/src/zed")));
        assert!(!thread.set_project_mode(ProjectMode::Read, cx));
        assert_eq!(names(thread.project_folders()), ["zed", "cowork"]);
        assert_eq!(thread.project_mode(), ProjectMode::Write);
    });
    session.settle();
    session.host_thread.read_with(session.cx, |thread, _| {
        assert_eq!(names(thread.project_folders()), ["zed", "cowork"]);
        assert_eq!(thread.project_mode(), ProjectMode::Write);
    });
}

#[gpui::test]
fn joiners_get_the_project_in_their_snapshot(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let thread = cx.new(|_| {
        test_thread(
            Uuid::new_v4(),
            Vec::new(),
            ThreadDraft::new(ParticipantId::new()),
        )
    });
    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            assert!(thread.add_project_folders(vec![PathBuf::from("/home/host/src/zed")]));
            thread.set_project_mode(ProjectMode::Write, cx);
        });
        let mirror = Thread::join_for_test(thread.read(cx).welcome_for_test(), cx);
        assert_eq!(
            mirror.project_folders(),
            ProjectFolder::mirrored(vec!["zed".into()])
        );
        assert_eq!(mirror.project_mode(), ProjectMode::Write);
    });
}
