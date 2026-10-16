//! Accepted drafts are fenced independently from delivery to the agent.
use super::*;
use crate::thread::submission::{RunTotals, SubmissionMode};

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

fn accept(session: &mut Collaboration<'_>, text: &str, mode: SubmissionMode) {
    let thread = session.host_thread.clone();
    session.host.update(session.cx, |cowork, cx| {
        let draft_id = thread.read(cx).draft().id;
        cowork.update_draft(draft_id, cx, |draft| {
            let ids = draft.items().iter().map(|item| item.id).collect::<Vec<_>>();
            draft.take_items(&ids);
            draft.create_prompt(draft.author.as_uuid(), text);
        });
        let actor = thread.read(cx).participant_id();
        assert!(cowork.accept_submission_mode(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor,
                submitted_locally: true,
                mode
            },
            cx
        ));
    });
}

#[gpui::test]
fn pending_steering_merges_while_queue_is_fifo_and_cancel_is_fenced(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let (_message_id, task) = generating(&mut session);
    accept(&mut session, "steer one", SubmissionMode::Steering);
    accept(&mut session, "steer two", SubmissionMode::Steering);
    accept(&mut session, "queue one", SubmissionMode::Queue);
    accept(&mut session, "queue two", SubmissionMode::Queue);
    let thread = session.host_thread.clone();
    let (id, thread_id) = thread.read_with(session.cx, |thread, _| {
        assert_eq!(thread.submission_count(), 4);
        assert_eq!(thread.pending_submissions().len(), 3);
        assert_eq!(thread.pending_submissions()[0].group.blocks.len(), 2);
        assert_eq!(
            thread.pending_submissions()[1].group.blocks[0].text,
            "queue one"
        );
        assert_eq!(
            thread.pending_submissions()[2].group.blocks[0].text,
            "queue two"
        );
        assert!(thread.draft().items().is_empty());
        (thread.pending_submissions()[0].id, thread.instance_id)
    });
    session.host.update(session.cx, |cowork, cx| {
        cowork.cancel_pending_submission(thread_id, id, cx)
    });
    accept(&mut session, "new steering", SubmissionMode::Steering);
    session.host.update(session.cx, |cowork, cx| {
        cowork.cancel_pending_submission(thread_id, id, cx)
    });
    thread.read_with(session.cx, |thread, _| {
        assert_eq!(thread.pending_submissions().len(), 3);
        assert_eq!(
            thread.pending_submissions()[2].group.blocks[0].text,
            "new steering"
        );
        assert_ne!(thread.pending_submissions()[2].id, id);
    });
    let mirror = session.collaborator_thread().unwrap();
    session.wait_until("pending submissions reach the collaborator", |session| {
        mirror.read_with(session.cx, |thread, _| {
            thread.submission_count() == 5 && thread.pending_submissions().len() == 3
        })
    });
    task.abort();
}

#[gpui::test]
fn shared_pending_submission_requires_admin_and_fences_each_acceptance(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let (_message_id, task) = generating(&mut session);
    let thread = session.host_thread.clone();
    let mirror = session.collaborator_thread().unwrap();
    let peer = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    session.host.update(session.cx, |cowork, cx| {
        assert!(
            cowork
                .collaborator_request(
                    &thread,
                    peer,
                    protocol::CollaboratorMessage::SubmitPending {
                        sequence: 0,
                        mode: SubmissionMode::Queue
                    },
                    cx
                )
                .is_err()
        );
        assert_eq!(thread.read(cx).submission_count(), 0);
    });
    set_default(&mut session, PeerMode::Admin);
    session.host.update(session.cx, |cowork, cx| {
        cowork
            .collaborator_request(
                &thread,
                peer,
                protocol::CollaboratorMessage::SubmitPending {
                    sequence: 0,
                    mode: SubmissionMode::Queue,
                },
                cx,
            )
            .unwrap();
        let draft_id = thread.read(cx).draft().id;
        cowork.update_draft(draft_id, cx, |draft| {
            draft.create_prompt(draft.author.as_uuid(), "later");
        });
        cowork
            .collaborator_request(
                &thread,
                peer,
                protocol::CollaboratorMessage::SubmitPending {
                    sequence: 0,
                    mode: SubmissionMode::Queue,
                },
                cx,
            )
            .unwrap();
        assert_eq!(thread.read(cx).submission_count(), 1);
        assert_eq!(thread.read(cx).draft().items()[0].body, "later");
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
    accept(&mut session, "first queued", SubmissionMode::Queue);
    accept(&mut session, "second queued", SubmissionMode::Queue);
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
    let sequence = thread.read_with(session.cx, |thread, _| thread.submission_count());
    session.host.update(session.cx, |cowork, cx| {
        let actor = thread.read(cx).participant_id();
        let draft_id = thread.read(cx).draft().id;
        assert!(cowork.accept_submission_mode(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor,
                submitted_locally: true,
                mode: SubmissionMode::Steering
            },
            cx
        ));
        assert!(thread.read(cx).is_generating());
        assert!(thread.read(cx).pending_submissions().is_empty());
        assert_eq!(thread.read(cx).submission_count(), sequence);
        assert!(thread.read(cx).draft().items().is_empty());
    });
    task.abort();
}

#[gpui::test]
fn stop_wins_late_completion_and_resume_delivers_steering_before_the_queue(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let (message_id, task) = generating(&mut session);
    accept(&mut session, "queue one", SubmissionMode::Queue);
    accept(&mut session, "queue two", SubmissionMode::Queue);
    accept(&mut session, "steer first", SubmissionMode::Steering);
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
        assert!(cowork.accept_submission_mode(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor,
                submitted_locally: true,
                mode: SubmissionMode::Steering,
            },
            cx,
        ));
        assert!(thread.read(cx).is_generating());
        assert_eq!(thread.read(cx).pending_submissions().len(), 2);
        assert_eq!(thread.read(cx).submission_count(), 3);
        assert!(thread.read(cx).timeline.iter().any(|entry| matches!(
            entry, TimelineMessage::User(group) if group.record.blocks[0].text == "steer first"
        )));
        for (expected, remaining) in [("queue one", 1), ("queue two", 0)] {
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
fn consumed_steering_cannot_be_cancelled_and_mirrors_keep_the_prompt_out_of_output(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let (message_id, task) = generating(&mut session);
    accept(&mut session, "new instructions", SubmissionMode::Steering);
    let thread = session.host_thread.clone();
    let thread_id = thread.read_with(session.cx, |thread, _| thread.instance_id);
    session.host.update(session.cx, |cowork, cx| {
        let (group, aliases, prompt) =
            cowork.active_generations[&thread_id].take_steering_for_test();
        let id = group.id;
        // The agent thread has consumed it but its ordered metadata is still
        // queued. A click from the old display must not retract delivered input.
        cowork.cancel_pending_submission(thread_id, id, cx);
        assert_eq!(thread.read(cx).pending_submissions().len(), 1);
        thread.update(cx, |thread, cx| {
            thread.emit_for_test(
                protocol::HostMessage::PendingDelivered {
                    id: id.into_bytes(),
                    group: group.to_protocol(),
                    agent_id: Some(message_id.into_bytes()),
                    aliases,
                },
                cx,
            );
            thread.emit_for_test(
                agent_event(message_id, agent::AgentEvent::Steered { prompt }),
                cx,
            );
            for event in model_turn([streamed_text("t", "updated answer", false)], 10) {
                thread.emit_for_test(agent_event(message_id, event), cx);
            }
        });
        assert!(thread.read(cx).pending_submissions().is_empty());
        let state = thread.read(cx);
        // Steering joins the timeline when it was delivered, after the
        // message it steers, which shows it where it joined the run.
        let TimelineMessage::Agent(agent) = &state.timeline[0] else {
            panic!("the steered reply comes first");
        };
        let TimelineMessage::User(steering) = &state.timeline[1] else {
            panic!("steering follows the message it was delivered into");
        };
        assert_eq!(steering.record.id, id);
        assert_eq!(agent.record.steering, vec![id]);
        assert!(matches!(
            agent.ui.output.steps(),
            [AgentStep::Steering, AgentStep::Text(text)] if text == "updated answer"
        ));
        assert_eq!(agent.ui.output.text, "updated answer");
        assert_eq!(state.transcript.messages().len(), 3);
    });
    let mirror = session.collaborator_thread().unwrap();
    session.wait_until(
        "steering transcript and output reach the mirror",
        |session| {
            mirror.read_with(session.cx, |thread, _| {
                thread.transcript.messages().len() == 3 && thread.pending_submissions().is_empty()
            })
        },
    );
    assert_eq!(
        thread.read_with(session.cx, |thread, _| thread.conversation()),
        mirror.read_with(session.cx, |thread, _| thread.conversation())
    );
    mirror.read_with(session.cx, |thread, _| {
        let TimelineMessage::Agent(agent) = &thread.timeline[0] else {
            panic!("the mirror orders the timeline alike");
        };
        assert_eq!(agent.record.steering.len(), 1);
        assert!(matches!(thread.timeline[1], TimelineMessage::User(_)));
        assert!(matches!(agent.ui.output.steps()[0], AgentStep::Steering));
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
        assert!(cowork.accept_submission_mode(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor,
                submitted_locally: true,
                mode: SubmissionMode::Steering,
            },
            cx
        ));
        comment_id
    });
    let snapshot = thread.read_with(session.cx, |thread, _| {
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
        snapshot
    });
    let bytes = postcard::to_stdvec(&snapshot).unwrap();
    let decoded: protocol::ThreadSnapshot = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(snapshot, decoded);
    task.abort();
}

#[gpui::test]
fn empty_draft_resume_rejects_write_only_peer_before_starting_any_run(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let (message_id, task) = generating(&mut session);
    accept(&mut session, "pending", SubmissionMode::Queue);
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
        assert!(!cowork.accept_submission_mode(
            draft_id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor: peer,
                submitted_locally: false,
                mode: SubmissionMode::Steering
            },
            cx
        ));
        assert!(!thread.read(cx).is_generating());
        assert_eq!(thread.read(cx).pending_submissions().len(), 1);
        assert_eq!(thread.read(cx).submission_count(), 1);
    });
    task.abort();
}
