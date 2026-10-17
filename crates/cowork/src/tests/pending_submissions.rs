//! Accepted drafts are fenced independently from delivery to the agent.
use super::*;
use crate::thread::submission::RunTotals;

fn generating(session: &mut Collaboration<'_>) -> (Uuid, tokio::task::JoinHandle<()>) {
    let message_id = Uuid::new_v4();
    let task = session._runtime.spawn(std::future::pending::<()>());
    let thread = session.host_thread.clone();
    let thread_id = thread.read_with(session.cx, |thread, _| thread.instance_id);
    session.host.update(session.cx, |cowork, cx| {
        cowork.active_generations.insert(
            thread_id,
            ActiveGeneration::for_test(message_id, task.abort_handle(), Arc::default()),
        );
        thread.update(cx, |thread, cx| {
            thread.emit_for_test(agent_started(message_id, None, "initial"), cx)
        });
    });
    (message_id, task)
}

fn accept(session: &mut Collaboration<'_>, text: &str) {
    let thread = session.host_thread.clone();
    session.host.update(session.cx, |cowork, cx| {
        let draft_id = thread.read(cx).draft().id;
        cowork.update_draft(draft_id, cx, |draft| {
            let ids = draft.items().iter().map(|item| item.id).collect::<Vec<_>>();
            draft.take_items(&ids);
            draft.create_prompt(draft.author.as_uuid(), text);
        });
        let actor = thread.read(cx).participant_id();
        assert!(cowork.accept_submission(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor,
                submitted_locally: true,
            },
            cx
        ));
    });
}

#[gpui::test]
fn pending_submissions_queue_in_order_and_cancel_removes_only_one(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let (_message_id, task) = generating(&mut session);
    accept(&mut session, "one");
    accept(&mut session, "two");
    accept(&mut session, "three");
    let thread = session.host_thread.clone();
    let texts = |thread: &Thread| {
        thread
            .pending_submissions()
            .iter()
            .map(|pending| pending.group.blocks[0].text.clone())
            .collect::<Vec<_>>()
    };
    let (id, thread_id) = thread.read_with(session.cx, |thread, _| {
        assert_eq!(thread.submission_count(), 3);
        assert_eq!(texts(thread), ["one", "two", "three"]);
        assert!(thread.draft().items().is_empty());
        (thread.pending_submissions()[1].id, thread.instance_id)
    });
    session.host.update(session.cx, |cowork, cx| {
        cowork.cancel_pending_submission(thread_id, id, cx)
    });
    accept(&mut session, "four");
    // A repeated cancel finds nothing left to remove.
    session.host.update(session.cx, |cowork, cx| {
        cowork.cancel_pending_submission(thread_id, id, cx)
    });
    thread.read_with(session.cx, |thread, _| {
        assert_eq!(texts(thread), ["one", "three", "four"]);
    });
    task.abort();
}

#[gpui::test]
fn write_peers_cannot_cancel_pending_submissions(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let (_message_id, task) = generating(&mut session);
    let thread = session.host_thread.clone();
    let mirror = session.collaborator_thread().unwrap();
    let peer = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    set_default(&mut session, PeerMode::Admin);
    session.host.update(session.cx, |cowork, cx| {
        cowork
            .collaborator_request(
                &thread,
                peer,
                protocol::CollaboratorMessage::Submit { sequence: 0 },
                cx,
            )
            .unwrap();
    });
    let pending_id = thread.read_with(session.cx, |thread, _| thread.pending_submissions()[0].id);
    set_default(&mut session, PeerMode::Write);
    session.host.update(session.cx, |cowork, cx| {
        assert!(
            cowork
                .collaborator_request(
                    &thread,
                    peer,
                    protocol::CollaboratorMessage::CancelPending {
                        id: pending_id.into_bytes()
                    },
                    cx
                )
                .is_err()
        );
        assert_eq!(thread.read(cx).pending_submissions().len(), 1);
    });
    task.abort();
}

#[gpui::test]
fn completed_runs_start_one_queued_message_but_stopping_preserves_pending(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let (message_id, task) = generating(&mut session);
    accept(&mut session, "first queued");
    accept(&mut session, "second queued");
    let thread = session.host_thread.clone();
    session.host.update(session.cx, |cowork, cx| {
        cowork.finish_generation(
            &thread,
            message_id,
            protocol::RunOutcome::Completed,
            RunTotals {
                started_at: SystemTime::now(),
                duration: Duration::ZERO,
                usage: rig::completion::Usage::default(),
            },
            cx,
        )
    });
    thread.read_with(session.cx, |thread, _| {
        assert!(thread.is_generating());
        assert_eq!(thread.pending_submissions().len(), 1);
        assert_eq!(thread.pending_submissions()[0].group.blocks[0].text, "second queued");
        assert_eq!(thread.submission_count(), 2);
        assert!(thread.timeline.iter().any(|entry| matches!(entry, TimelineMessage::User(group) if group.record.blocks[0].text == "first queued")));
    });
    let next = thread.read_with(session.cx, |thread, _| {
        thread.running_agent_message_id().unwrap()
    });
    session.host.update(session.cx, |cowork, cx| {
        cowork.finish_generation(
            &thread,
            next,
            protocol::RunOutcome::Stopped,
            RunTotals {
                started_at: SystemTime::now(),
                duration: Duration::ZERO,
                usage: rig::completion::Usage::default(),
            },
            cx,
        )
    });
    thread.read_with(session.cx, |thread, _| {
        assert!(!thread.is_generating());
        assert_eq!(thread.pending_submissions().len(), 1);
    });
    task.abort();
}

#[gpui::test]
fn stop_wins_late_completion_and_resume_delivers_the_queue_in_order(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let (message_id, task) = generating(&mut session);
    accept(&mut session, "queue one");
    accept(&mut session, "queue two");
    accept(&mut session, "queue three");
    let thread = session.host_thread.clone();
    session.host.update(session.cx, |cowork, cx| {
        let actor = thread.read(cx).participant_id();
        let thread_id = thread.read(cx).instance_id;
        cowork.cancel_generation(thread_id, actor, Some(message_id), cx);
        // Completion was already on its way when Stop was clicked.
        cowork.finish_generation(
            &thread,
            message_id,
            protocol::RunOutcome::Completed,
            RunTotals {
                started_at: SystemTime::now(),
                duration: Duration::ZERO,
                usage: rig::completion::Usage::default(),
            },
            cx,
        );
        assert!(!thread.read(cx).is_generating());
        assert_eq!(thread.read(cx).pending_submissions().len(), 3);
    });
    let mirror = session.collaborator_thread().unwrap();
    session.wait_until(
        "stopped pending messages reach the collaborator",
        |session| {
            mirror.read_with(session.cx, |thread, _| {
                !thread.is_generating() && thread.pending_submissions().len() == 3
            })
        },
    );
    session.host.update(session.cx, |cowork, cx| {
        let actor = thread.read(cx).participant_id();
        let draft_id = thread.read(cx).draft().id;
        assert!(cowork.accept_submission(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor,
                submitted_locally: true,
            },
            cx,
        ));
        assert!(thread.read(cx).is_generating());
        assert_eq!(thread.read(cx).pending_submissions().len(), 2);
        assert_eq!(thread.read(cx).submission_count(), 3);
        assert!(thread.read(cx).timeline.iter().any(|entry| matches!(
            entry, TimelineMessage::User(group) if group.record.blocks[0].text == "queue one"
        )));
        for (expected, remaining) in [("queue two", 1), ("queue three", 0)] {
            let id = thread.read(cx).running_agent_message_id().unwrap();
            cowork.finish_generation(
                &thread,
                id,
                protocol::RunOutcome::Completed,
                RunTotals {
                    started_at: SystemTime::now(),
                    duration: Duration::ZERO,
                    usage: rig::completion::Usage::default(),
                },
                cx,
            );
            let state = thread.read(cx);
            assert!(state.is_generating());
            assert_eq!(state.pending_submissions().len(), remaining);
            let timeline = &state.timeline;
            let latest = timeline.iter().rev().find_map(|entry| match entry {
                TimelineMessage::User(group) => Some(group.record.blocks[0].text.as_str()),
                _ => None,
            });
            assert_eq!(latest, Some(expected));
            assert_eq!(state.submission_count(), 3);
        }
    });
    task.abort();
}

#[gpui::test]
fn pending_snapshot_keeps_comments_and_attachment_bytes_for_late_join(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let (message_id, task) = generating(&mut session);
    let thread = session.host_thread.clone();
    let file = crate::test_support::text_attachment("instructions.txt", "Keep this file");
    let attachment_id = AttachmentId::new();
    let comment_id = session.host.update(session.cx, |cowork, cx| {
        let actor = thread.read(cx).participant_id();
        let draft_id = thread.read(cx).draft().id;
        let comment_id = cowork
            .update_draft(draft_id, cx, |draft| {
                let ids = draft.items().iter().map(|item| item.id).collect::<Vec<_>>();
                draft.take_items(&ids);
                let block = draft.create_prompt(actor.as_uuid(), "with a file");
                draft.add_attachment(
                    block,
                    AttachmentRecord {
                        id: attachment_id,
                        name: file.name.clone(),
                        kind: file.kind(),
                        size: file.len(),
                        creator: actor.as_uuid(),
                    },
                );
                draft.files.insert(attachment_id, file);
                draft.stored.insert(attachment_id);
                draft.create_comment(
                    actor.as_uuid(),
                    CommentTarget {
                        message_id,
                        range: 0..4,
                        quote: "text".into(),
                    },
                    "answer this too",
                )
            })
            .unwrap()
            .as_uuid();
        assert!(cowork.accept_submission(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor,
                submitted_locally: true,
            },
            cx
        ));
        comment_id
    });
    thread.read_with(session.cx, |thread, _| {
        let snapshot = thread.to_protocol();
        assert_eq!(snapshot.submission_sequence, 1);
        assert_eq!(
            snapshot.pending[0].group.comments[0].id,
            comment_id.into_bytes()
        );
        assert_eq!(
            snapshot.pending[0].group.blocks[0].attachments[0].id,
            attachment_id.as_uuid().into_bytes()
        );
        assert!(
            thread
                .files_for(ParticipantId::new())
                .contains(&attachment_id)
        );
        assert!(thread.draft().files.contains_key(&attachment_id));
        assert!(thread.draft().items().is_empty());
    });
    task.abort();
}

#[gpui::test]
fn empty_draft_resume_rejects_write_only_peer_before_starting_any_run(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let (message_id, task) = generating(&mut session);
    accept(&mut session, "pending");
    let thread = session.host_thread.clone();
    let peer = session
        .collaborator_thread()
        .unwrap()
        .read_with(session.cx, |thread, _| thread.participant_id());
    session.host.update(session.cx, |cowork, cx| {
        cowork.finish_generation(
            &thread,
            message_id,
            protocol::RunOutcome::Stopped,
            RunTotals {
                started_at: SystemTime::now(),
                duration: Duration::ZERO,
                usage: rig::completion::Usage::default(),
            },
            cx,
        );
        let draft_id = thread.read(cx).draft().id;
        assert!(!cowork.accept_submission(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor: peer,
                submitted_locally: false,
            },
            cx
        ));
        assert!(!thread.read(cx).is_generating());
        assert_eq!(thread.read(cx).pending_submissions().len(), 1);
        assert_eq!(thread.read(cx).submission_count(), 1);
    });
    task.abort();
}
