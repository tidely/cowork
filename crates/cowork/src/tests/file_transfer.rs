//! Moving attachment bytes between host and collaborators.

use super::*;

#[gpui::test]
fn files_the_host_is_reading_hold_back_everyones_submission(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let host = session.host.clone();
    host.update(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        host.pending_attachments.push(PendingAttachment {
            id: Uuid::new_v4(),
            draft_id,
            target: AttachmentTarget::NewBlock(Uuid::new_v4()),
            name: "big.png".into(),
            is_image: true,
            progress: Some(30.),
        });
        cx.notify();
    });
    let collaborator = session.collaborator.clone();
    session.wait_until("the collaborator sees the pending read", |this| {
        collaborator.read_with(this.cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator.draft_is_loading_attachments(draft_id, cx)
        })
    });
    let pending = collaborator.read_with(session.cx, |collaborator, cx| {
        collaborator.pending_reads(&collaborator_thread.read(cx).draft)
    });
    assert!(matches!(
        pending.as_slice(),
        [PendingAttachment { name, progress: Some(progress), .. }]
            if name == "big.png" && *progress == 30.
    ));

    host.update(session.cx, |host, cx| {
        host.pending_attachments.clear();
        cx.notify();
    });
    session.wait_until("the collaborator sees the read finish", |this| {
        !collaborator.read_with(this.cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator.draft_is_loading_attachments(draft_id, cx)
        })
    });
}

/// A text file big enough to take several chunks.
fn big_text_file() -> (PathBuf, String) {
    let text = "0123456789abcdef\n".repeat(9_000);
    assert!(text.len() > 2 * protocol::ATTACHMENT_CHUNK_SIZE);
    let path = std::env::temp_dir().join(format!("cowork-{}.txt", Uuid::new_v4()));
    std::fs::write(&path, &text).expect("write test attachment");
    (path, text)
}

fn file_text(draft: &ThreadDraft, id: AttachmentId) -> Option<String> {
    match &draft.files.get(&id)?.content {
        FileAttachmentContent::Text(text) => Some(text.clone()),
        _ => None,
    }
}

#[gpui::test]
fn a_collaborators_file_is_uploaded_and_stored(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let block = session.items(&host_thread)[0].id;
    let (path, text) = big_text_file();

    let collaborator = session.collaborator.clone();
    collaborator.update(session.cx, |collaborator, cx| {
        let draft_id = collaborator_thread.read(cx).draft.id;
        collaborator.add_attachments(
            draft_id,
            AttachmentTarget::Block(block),
            vec![AttachmentSource::Path(path.clone())],
            None,
            cx,
        );
    });
    session.wait_until("the host stores the upload", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| {
            thread.draft.uploads.is_empty() && thread.draft.stored.len() == 1
        })
    });
    std::fs::remove_file(&path).expect("remove test attachment");

    let record = host_thread.read_with(session.cx, |thread, _| {
        let records = thread.draft.attachment_records();
        assert_eq!(records.len(), 1);
        let record = records[0].clone();
        assert!(thread.draft.stored.contains(&record.id));
        assert_eq!(
            file_text(&thread.draft, record.id).as_deref(),
            Some(text.as_str())
        );
        assert!(thread.draft.incoming.is_empty());
        record
    });
    let collaborator_id =
        collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
    assert_eq!(record.creator, collaborator_id.as_uuid());
    // Nothing holds the collaborator's submission back anymore.
    collaborator.read_with(session.cx, |collaborator, cx| {
        let draft_id = collaborator_thread.read(cx).draft.id;
        assert!(!collaborator.draft_is_loading_attachments(draft_id, cx));
    });

    // Submitting sends it along, and the submitted message shows it.
    collaborator_thread.read_with(session.cx, |thread, _| {
        thread.request(protocol::CollaboratorMessage::Submit { sequence: 0 });
    });
    session.wait_until("the collaborator sees the submission", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
    });
    collaborator_thread.read_with(session.cx, |thread, _| {
        let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
            panic!("expected the submitted message first");
        };
        assert_eq!(message.blocks[0].attachments, std::slice::from_ref(&record));
        assert!(thread.draft.files.contains_key(&record.id));
    });
    host_thread.read_with(session.cx, |thread, _| {
        let files = &thread.draft.files;
        let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
            panic!("expected the submitted message first");
        };
        let RigMessage::User { content } =
            agent_message(None, &message.blocks, files, &HashMap::new())
        else {
            panic!("expected a user message");
        };
        assert!(content.iter().any(
            |part| matches!(part, UserContent::Text(part) if part.text.contains("0123456789abcdef"))
        ));
    });
    // The collaborator mirrors the prompt the agent was sent, file included.
    session.wait_until("the collaborator mirrors the conversation", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| thread.conversation())
            == host_thread.read_with(this.cx, |thread, _| thread.conversation())
    });
    collaborator_thread.read_with(session.cx, |thread, _| {
        assert_eq!(thread.transcript.len(), 1);
        assert_eq!(thread.prompt_names.len(), 1);
    });
}

#[gpui::test]
fn the_hosts_files_are_downloaded_by_collaborators(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let block = session.items(&host_thread)[0].id;
    let (path, text) = big_text_file();

    let host = session.host.clone();
    host.update(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        host.add_attachments(
            draft_id,
            AttachmentTarget::Block(block),
            vec![AttachmentSource::Path(path.clone())],
            None,
            cx,
        );
    });
    session.wait_until("the collaborator has the file", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| thread.draft.files.len() == 1)
    });
    std::fs::remove_file(&path).expect("remove test attachment");
    collaborator_thread.read_with(session.cx, |thread, _| {
        let record = &thread.draft.attachment_records()[0];
        assert!(thread.draft.stored.contains(&record.id));
        assert_eq!(
            file_text(&thread.draft, record.id).as_deref(),
            Some(text.as_str())
        );
    });
}

#[gpui::test]
fn files_left_unfinished_by_a_leaving_collaborator_are_removed(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let block = session.items(&host_thread)[0].id;
    // A record whose bytes never come.
    let collaborator = session.collaborator.clone();
    collaborator.update(session.cx, |collaborator, cx| {
        let draft_id = collaborator_thread.read(cx).draft.id;
        collaborator.update_draft(draft_id, cx, |draft| {
            draft.doc.add_attachment(
                block,
                AttachmentRecord {
                    id: AttachmentId::new(),
                    name: "never.txt".into(),
                    kind: AttachmentKind::Text,
                    size: 10,
                    creator: draft.author.as_uuid(),
                },
            );
        });
    });
    session.wait_until("the host sees the record", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread.draft.attachment_records().len() == 1
        })
    });
    // Nobody can submit it while its bytes are missing.
    let host = session.host.clone();
    host.read_with(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        assert!(host.draft_is_loading_attachments(draft_id, cx));
    });

    collaborator_thread.update(session.cx, |thread, _| {
        thread.sharing = ThreadSharing::NotShared;
    });
    session.wait_until("the host removes the record", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread.participants.len() == 1 && thread.draft.attachment_records().is_empty()
        })
    });
    assert_eq!(session.bodies(&host_thread), ["from the host"]);
}

/// Bytes and the record announcing them travel separately, so the bytes
/// can come first.
#[gpui::test]
fn uploads_wait_for_their_record(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let host = cx.new(|_| {
        test_thread(
            Uuid::new_v4(),
            Vec::new(),
            ThreadDraft::new(ParticipantId::new()),
        )
    });
    let uploader = ParticipantId::new();
    let file = text_attachment("notes.txt", "some notes");
    let mut sender = ThreadDraft::new(uploader);
    let block = sender.doc.create_prompt(uploader.as_uuid(), "");
    let record = AttachmentRecord {
        id: AttachmentId::new(),
        name: file.name.clone(),
        kind: file.kind(),
        size: file.len(),
        creator: uploader.as_uuid(),
    };
    sender.files.insert(record.id, file);
    sender.doc.add_attachment(block, record.clone());
    let update = sender.doc.take_local_update().expect("an update");
    let chunk = sender.chunk(record.id, 0).expect("a chunk");

    host.update(cx, |host, _| {
        host.receive_upload(uploader, chunk).expect("valid data");
        assert!(!host.draft.stored.contains(&record.id));
        host.apply_collaborator_update(uploader, update)
            .expect("valid update");
        assert!(host.draft.stored.contains(&record.id));
        assert_eq!(
            file_text(&host.draft, record.id).as_deref(),
            Some("some notes")
        );
        // Removing the record discards the file.
        let removal = Draft::new();
        removal
            .apply_update(&host.draft.doc.encode_state())
            .expect("copy the draft");
        removal.remove_attachment(record.id);
        let update = removal.take_local_update().expect("an update");
        host.apply_collaborator_update(uploader, update)
            .expect("valid update");
        assert!(!host.draft.files.contains_key(&record.id));
        // A piece still in flight is ignored rather than started over.
        let late = sender.chunk(record.id, 0).expect("a chunk");
        host.receive_upload(uploader, late).expect("ignored");
        assert!(host.draft.incoming.is_empty());
    });
}

#[gpui::test]
fn a_cancelled_upload_is_discarded(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let uploader = ParticipantId::new();
    let mut sender = ThreadDraft::new(uploader);
    let id = AttachmentId::new();
    let text = "x".repeat(protocol::ATTACHMENT_CHUNK_SIZE + 1);
    sender.files.insert(id, text_attachment("big.txt", &text));
    let first = sender.chunk(id, 0).expect("a chunk");
    let second = sender
        .chunk(id, protocol::ATTACHMENT_CHUNK_SIZE as u64)
        .expect("a chunk");

    cowork.update(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).expect("thread");
        for request in [
            protocol::CollaboratorMessage::AttachmentData(first),
            protocol::CollaboratorMessage::AttachmentCancelled(id.as_uuid().into_bytes()),
            protocol::CollaboratorMessage::AttachmentData(second),
        ] {
            cowork
                .collaborator_request(&thread, uploader, request, cx)
                .expect("valid request");
        }
        let draft = &thread.read(cx).draft;
        assert!(draft.incoming.is_empty());
        assert!(!draft.files.contains_key(&id));
    });
}

#[gpui::test]
fn joining_peers_are_sent_the_files_they_do_not_have(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let joiner = ParticipantId::new();
    let (timeline_records, timeline_files) = attached([text_attachment("old.txt", "old")]);
    let timeline = vec![TimelineMessage::User(UserMessageGroup {
        id: Uuid::new_v4(),
        comments: Vec::new(),
        blocks: vec![PromptBlock {
            id: Uuid::new_v4(),
            author: ParticipantId::new(),
            text: "earlier".into(),
            attachments: timeline_records.clone(),
        }],
        comments_folded: false,
    })];
    let thread = cx.new(|_| {
        let mut draft = ThreadDraft::new(ParticipantId::new());
        let block = draft.doc.create_prompt(draft.author.as_uuid(), "");
        let mut own = timeline_files;
        for (creator, name) in [
            (Uuid::new_v4(), "new.txt"),
            (joiner.as_uuid(), "theirs.txt"),
        ] {
            let file = text_attachment(name, name);
            let record = AttachmentRecord {
                id: AttachmentId::new(),
                name: name.into(),
                kind: file.kind(),
                size: file.len(),
                creator,
            };
            draft.doc.add_attachment(block, record.clone());
            own.insert(record.id, file);
        }
        draft.stored = own.keys().copied().collect();
        draft.files = own;
        test_thread(Uuid::new_v4(), timeline, draft)
    });

    thread.read_with(cx, |thread, _| {
        let names = thread
            .files_for(joiner)
            .into_iter()
            .map(|id| thread.draft.files[&id].name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, ["old.txt", "new.txt"]);
    });
}

#[test]
fn files_are_chunked_and_put_back_together() {
    let text = "x".repeat(protocol::ATTACHMENT_CHUNK_SIZE * 2 + 5);
    let mut sender = ThreadDraft::new(ParticipantId::new());
    let id = AttachmentId::new();
    sender.files.insert(id, text_attachment("big.txt", &text));
    let mut receiver = ThreadDraft::new(ParticipantId::new());

    let mut offset = 0;
    let mut chunks = 0;
    let completed = loop {
        let chunk = sender.chunk(id, offset).expect("a chunk");
        assert!(chunk.bytes.len() <= protocol::ATTACHMENT_CHUNK_SIZE);
        offset = chunk.offset + chunk.bytes.len() as u64;
        chunks += 1;
        if let Some(done) = receiver.receive_chunk(chunk, None).expect("valid chunk") {
            break done;
        }
    };
    assert_eq!(chunks, 3);
    assert_eq!(completed, id);
    assert!(sender.chunk(id, offset).is_none());
    receiver.complete_file(id).expect("a text file");
    assert_eq!(file_text(&receiver, id), Some(text));

    // A piece that continues nothing is ignored, and an oversized file
    // is refused.
    let mut stray = sender
        .chunk(id, protocol::ATTACHMENT_CHUNK_SIZE as u64)
        .expect("chunk");
    stray.id = [9; 16];
    assert_eq!(receiver.receive_chunk(stray, None).expect("ignored"), None);
    let mut oversized = sender.chunk(id, 0).expect("chunk");
    oversized.id = [8; 16];
    oversized.total = MAX_TEXT_ATTACHMENT_BYTES + 1;
    assert!(receiver.receive_chunk(oversized, None).is_err());
}
