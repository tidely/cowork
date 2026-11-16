use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use draft::AttachmentRecord;
use gpui::SharedString;

use super::*;
use crate::{
    models::{ModelCatalog, ModelProvider, ModelRef},
    participant::ParticipantId,
    profile::Profile,
    projects::ProjectId,
    protocol,
    statistics::Statistics,
    test_support::{ScriptedHost, attached, streamed_text, streamed_tool_call, text_attachment},
    thread_draft::ThreadDraft,
    timeline::{
        CommentReference, PromptBlock, TimelineMessage, UserComment, UserCommentBody,
        UserMessageGroup,
    },
    transcript::ReplyPreview,
    usage::TokenActivity,
};

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("cowork-{}", Uuid::new_v4())))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime")
}

fn open(root: &Root, runtime: &tokio::runtime::Runtime) -> (ChatDb, Vec<ListedRow>) {
    let (db, saved) = ChatDb::open(root.0.clone(), runtime.handle()).unwrap();
    (db, saved.chats)
}

fn placement(thread: &Thread) -> Placement {
    Placement::new(false, 0, thread.project()).unwrap()
}

fn flush(db: &ChatDb) {
    futures::executor::block_on(db.flush());
}

fn load(db: &ChatDb, id: Uuid, cx: &mut gpui::App) -> Thread {
    let meta = ChatMeta {
        id,
        title: String::new(),
        project: None,
        tokens_used: 0,
        generation_time: Duration::ZERO,
        last_run_at: None,
        token_activity: Vec::new(),
    };
    let rows = futures::executor::block_on((db.loader())(meta)).expect("a saved chat");
    rows.into_thread(ModelCatalog::default(), cx).unwrap()
}

/// What a chat shows and runs on: its snapshot, with each unfinished reply
/// as the reply it shows rather than the previews that made it.
fn shown(thread: &Thread) -> protocol::ThreadSnapshot {
    let mut snapshot = thread.to_protocol();
    for message in &mut snapshot.messages {
        if let protocol::TimelineMessage::Agent(run) = message {
            run.pending_events = ReplyPreview::of(&run.pending_events)
                .partial()
                .map(|reply| ReplyPreview::previews_of(&reply))
                .unwrap_or_default();
        }
    }
    snapshot
}

/// A chat with a user message attaching a file and commenting, a
/// collaborator's profile, a project folder, and a run calling a tool and
/// compacting.
fn thread_with_history(
    host: &mut ScriptedHost,
    folder: &Path,
    cx: &mut gpui::App,
) -> (Thread, AttachmentRecord) {
    let author = ParticipantId::new();
    let (records, files) = attached([text_attachment("notes.md", "# Notes")]);
    let group = UserMessageGroup {
        id: Uuid::new_v4(),
        comments: vec![UserComment {
            id: Uuid::new_v4(),
            author,
            presence: Default::default(),
            reference: CommentReference {
                message_id: Uuid::new_v4(),
                range: 0..4,
                quote: "text".into(),
            },
            body: UserCommentBody::Submitted("Look here".into()),
        }],
        blocks: vec![PromptBlock {
            id: Uuid::new_v4(),
            author,
            text: "Read the notes".into(),
            attachments: records.clone(),
        }],
        comments_folded: false,
    };
    let mut draft = ThreadDraft::new(author);
    draft.files = files;
    let mut thread = Thread::new_local(
        "Notes".into(),
        vec![TimelineMessage::User(group.into())],
        draft,
        author,
        Arc::default(),
        None,
    );
    thread.profiles.insert(
        ParticipantId::new(),
        Profile {
            name: Some(SharedString::new_static("Ada")),
            ..Profile::default()
        },
    );
    thread.add_project_folders(vec![folder.to_owned()]);
    thread.set_project(Some(ProjectId::new()));
    thread.tokens_used = 120;
    thread.token_activity.push(TokenActivity {
        at: SystemTime::UNIX_EPOCH + Duration::new(1_763_200_000, 123_456_789),
        duration: Duration::from_secs(2),
        tokens: 120,
    });

    let run = Uuid::new_v4();
    let mut events = host.start(run, "Read the notes");
    events.extend(host.turn(
        run,
        [streamed_tool_call(
            "call",
            "read_file",
            serde_json::json!({ "path": "notes.md" }),
        )],
        50,
    ));
    events.extend(host.results(run, "# Notes"));
    events.extend(host.compaction(run, 3, "Read."));
    events.extend(host.turn(run, [streamed_text("answer", "Done.", false)], 70));
    events.push(host.ended(run, protocol::RunOutcome::Completed));
    for event in events {
        thread.apply_for_test(event, cx);
    }
    for message in &mut thread.timeline {
        if let TimelineMessage::Agent(message) = message {
            message.record.model = Some(ModelRef {
                provider: ModelProvider::Anthropic,
                id: "claude".into(),
            });
        }
    }
    (thread, records.into_iter().next().unwrap())
}

fn add_stopped_run(thread: &mut Thread, host: &mut ScriptedHost, cx: &mut gpui::App) {
    let run = Uuid::new_v4();
    let mut events = host.start(run, "Again");
    events.extend(
        host.turn(run, [streamed_text("answer", "Half an ans", false)], 90)
            .into_iter()
            .take_while(|event| {
                !matches!(
                    event,
                    protocol::HostMessage::AgentEvent {
                        event: protocol::RunEvent::Usage { .. },
                        ..
                    }
                )
            }),
    );
    events.push(host.ended(run, protocol::RunOutcome::Stopped));
    for event in events {
        thread.apply_for_test(event, cx);
    }
}

#[test]
fn persistent_instances_require_exclusive_ownership() {
    let runtime = runtime();
    let root = Root::new();
    let (db, _) = open(&root, &runtime);
    let error = ChatDb::open(root.0.clone(), runtime.handle())
        .err()
        .unwrap();
    assert!(error.to_string().contains("already running"));
    futures::executor::block_on(db.quit());
    let (db, _) = open(&root, &runtime);
    futures::executor::block_on(db.quit());

    let invalid = Root::new();
    std::fs::write(&invalid.0, b"not a directory").unwrap();
    assert!(ChatDb::open(invalid.0.clone(), runtime.handle()).is_err());
    std::fs::remove_file(&invalid.0).unwrap();
}

/// Saved, then saved again with a run stopped partway through its reply, a
/// chat comes back as it was: listed by its row, then loaded whole.
#[gpui::test]
fn a_saved_chat_comes_back_as_it_was(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let runtime = runtime();
        let root = Root::new();
        let mut host = ScriptedHost::default();
        let (mut thread, attachment) = thread_with_history(&mut host, &std::env::temp_dir(), cx);
        let id = thread.instance_id;
        let (db, rows) = open(&root, &runtime);
        assert!(rows.is_empty());
        // Chats are filed only under projects the database has.
        db.saver().create_project(&crate::projects::ProjectRecord {
            id: thread.project().unwrap(),
            name: "Site".into(),
            folders: Vec::new(),
            created_at: SystemTime::now(),
        });
        assert!(!db.is_saved(&thread));
        db.save(&thread, placement(&thread)).unwrap();
        flush(&db);
        assert!(db.is_saved(&thread));
        add_stopped_run(&mut thread, &mut host, cx);
        assert!(!db.is_saved(&thread), "a new run isn't saved yet");
        db.save(&thread, placement(&thread)).unwrap();
        // Where an unsaved chat is listed doesn't make it saved.
        let never_saved = Uuid::new_v4();
        db.save_meta(never_saved, Placement::new(true, 3, None).unwrap());
        futures::executor::block_on(db.quit());

        let (db, rows) = open(&root, &runtime);
        let [row] = &rows[..] else {
            panic!("one saved chat, not {}", rows.len())
        };
        assert!(!row.archived);
        assert_eq!(row.meta, ChatMeta::of(&thread));
        let restored = load(&db, id, cx);
        assert_eq!(shown(&restored), shown(&thread));
        assert_eq!(restored.participant_id(), thread.participant_id());
        assert_eq!(restored.project_folders(), thread.project_folders());
        assert_eq!(restored.project(), thread.project());
        let ada = *thread.profiles.keys().next().unwrap();
        assert_eq!(restored.profiles[&ada].name.as_deref(), Some("Ada"));
        assert_eq!(restored.tokens_used, 120);
        assert_eq!(restored.token_activity, thread.token_activity);
        assert_eq!(restored.draft().files[&attachment.id].bytes(), b"# Notes");
        futures::executor::block_on(db.quit());
    });
}

/// A save that fails partway writes nothing, and the next one starts from
/// what the database holds rather than from what was queued.
#[gpui::test]
fn after_a_failed_save_the_next_resumes_from_the_database(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let runtime = runtime();
        let root = Root::new();
        let mut host = ScriptedHost::default();
        let (mut thread, _) = thread_with_history(&mut host, &std::env::temp_dir(), cx);
        let id = thread.instance_id;
        let (db, _) = open(&root, &runtime);
        db.save(&thread, placement(&thread)).unwrap();
        flush(&db);
        let before = shown(&thread);

        add_stopped_run(&mut thread, &mut host, cx);
        db.fail_next_save();
        db.save(&thread, placement(&thread)).unwrap();
        flush(&db);
        assert!(!db.is_saved(&thread));
        assert_eq!(
            shown(&load(&db, id, cx)),
            before,
            "nothing of it was written"
        );

        // Queued as if the failed one had been written: refused, then
        // resent from what is there.
        db.save(&thread, placement(&thread)).unwrap();
        flush(&db);
        assert!(db.is_saved(&thread));
        assert_eq!(shown(&load(&db, id, cx)), shown(&thread));
        futures::executor::block_on(db.quit());
    });
}

/// A chat in memory with less than its saved rows, such as one built from
/// its row alone, is refused rather than saved over the chat.
#[gpui::test]
fn a_chat_shorter_than_its_saved_one_is_refused(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let runtime = runtime();
        let root = Root::new();
        let mut host = ScriptedHost::default();
        let (thread, _) = thread_with_history(&mut host, &std::env::temp_dir(), cx);
        let id = thread.instance_id;
        let (db, _) = open(&root, &runtime);
        db.save(&thread, placement(&thread)).unwrap();
        flush(&db);
        db.forget(id);

        let author = thread.participant_id();
        let mut placeholder = Thread::new_local(
            "Notes".into(),
            Vec::new(),
            ThreadDraft::new(author),
            author,
            Arc::default(),
            None,
        );
        placeholder.instance_id = id;
        db.save(&placeholder, placement(&thread)).unwrap();
        flush(&db);
        assert!(!db.is_saved(&placeholder));
        assert_eq!(shown(&load(&db, id, cx)), shown(&thread));
        futures::executor::block_on(db.quit());
    });
}

/// Deleting a chat removes its rows, what search finds of it, and its
/// folder with its scratch folder.
#[gpui::test]
fn deleting_a_chat_removes_its_rows_search_texts_and_folder(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let runtime = runtime();
        let root = Root::new();
        let mut host = ScriptedHost::default();
        let (thread, _) = thread_with_history(&mut host, &std::env::temp_dir(), cx);
        let id = thread.instance_id;
        let (db, _) = open(&root, &runtime);
        db.save(&thread, placement(&thread)).unwrap();
        let scratch = db.root().join(id.to_string()).join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        flush(&db);
        // Case and diacritics aside, as the palette matches.
        let found = futures::executor::block_on(db.search("NÓTES")).unwrap();
        assert_eq!(
            found,
            [SearchHit {
                thread_id: id,
                text: "Read the notes".into()
            }]
        );

        let counted = Statistics {
            chats: 1,
            tokens_used: 7,
            longest_chat: Duration::from_secs(3),
            token_activity: vec![TokenActivity {
                at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                duration: Duration::from_millis(2_250),
                tokens: 7,
            }],
        };
        db.delete(id, &counted);
        // Usage of a run ending after it was deleted adds up, the longest
        // chat being the longer.
        let late = Statistics {
            tokens_used: 5,
            longest_chat: Duration::from_secs(2),
            ..Statistics::default()
        };
        db.saver().count_deleted(&late);
        flush(&db);
        assert!(
            futures::executor::block_on(db.search("notes"))
                .unwrap()
                .is_empty()
        );
        assert!(!root.0.join("threads").join(id.to_string()).exists());
        futures::executor::block_on(db.quit());
        let (db, saved) = ChatDb::open(root.0.clone(), runtime.handle()).unwrap();
        assert!(saved.chats.is_empty());
        assert_eq!(
            saved.state.deleted,
            Statistics {
                tokens_used: 12,
                ..counted
            }
        );
        futures::executor::block_on(db.quit());
    });
}

/// Removing a project unfiles its saved chats with it, and a chat saved
/// under it afterwards is saved unfiled.
#[gpui::test]
fn removing_a_project_unfiles_its_saved_chats(cx: &mut gpui::TestAppContext) {
    let root = Root::new();
    let runtime = runtime();
    cx.update(|cx| {
        let (mut filed, _) =
            thread_with_history(&mut ScriptedHost::default(), &std::env::temp_dir(), cx);
        let (mut late, _) =
            thread_with_history(&mut ScriptedHost::default(), &std::env::temp_dir(), cx);
        let (db, _) = open(&root, &runtime);
        let mut projects = crate::projects::Projects::new(Vec::new(), Some(db.saver()));
        let project = projects
            .create("Site", Vec::new(), SystemTime::now())
            .unwrap();
        filed.set_project(Some(project));
        db.save(&filed, placement(&filed)).unwrap();
        projects.remove(project);
        // Its project's removal was queued before this save.
        late.set_project(Some(project));
        db.save(&late, placement(&late)).unwrap();
        futures::executor::block_on(db.quit());

        let (db, saved) = ChatDb::open(root.0.clone(), runtime.handle()).unwrap();
        assert!(saved.state.projects.is_empty());
        assert_eq!(saved.chats.len(), 2);
        assert!(saved.chats.iter().all(|row| row.meta.project.is_none()));
        futures::executor::block_on(db.quit());
    });
}
