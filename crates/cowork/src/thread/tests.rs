use std::{cell::Cell, sync::Arc};

use ::draft::{AttachmentId, AttachmentKind, AttachmentRecord, Draft};
use uuid::Uuid;

use super::{
    ChangeModel, ControlGeneration, DenialReason, EditDraft, ManageAccess, PeerMode,
    PeerPermissions, PermissionOperation, Thread, ThreadOwnership, ThreadSharing,
    draft::ThreadDraft,
};
use crate::{participant::ParticipantId, protocol};

fn local_thread() -> Thread {
    let host = ParticipantId::new();
    Thread::new_local(
        "Test".into(),
        Vec::new(),
        ThreadDraft::new(host),
        host,
        Arc::default(),
        None,
    )
}

fn add_peer(thread: &mut Thread) -> ParticipantId {
    let peer = ParticipantId::new();
    if thread.participants.is_empty() {
        thread.participants.push(thread.participant_id);
    }
    thread.participants.push(peer);
    peer
}

#[test]
fn sparse_overrides_inherit_live_and_equal_default_stays_explicit() {
    let inherited = ParticipantId::new();
    let explicit = ParticipantId::new();
    let mut permissions = PeerPermissions::default();
    permissions.set_override(explicit, Some(PeerMode::Admin));
    permissions.set_default_mode(PeerMode::ReadOnly);
    assert_eq!(permissions.mode_for(inherited), PeerMode::ReadOnly);
    assert_eq!(permissions.mode_for(explicit), PeerMode::Admin);
    permissions.set_override(explicit, None);
    assert_eq!(permissions.override_for(explicit), None);
    assert_eq!(permissions.mode_for(explicit), PeerMode::ReadOnly);
    permissions.set_default_mode(PeerMode::Write);
    assert_eq!(permissions.mode_for(explicit), PeerMode::Write);
}

#[test]
fn peer_modes_gate_operations_and_only_the_host_manages_access() {
    let mut thread = local_thread();
    let host = thread.participant_id;
    let writer = add_peer(&mut thread);
    let admin = add_peer(&mut thread);
    thread.peer_permissions.set_default_mode(PeerMode::Write);
    thread
        .peer_permissions
        .set_override(host, Some(PeerMode::ReadOnly));
    thread
        .peer_permissions
        .set_override(admin, Some(PeerMode::Admin));
    assert_eq!(thread.local_mode(), PeerMode::Admin);
    assert!(
        thread
            .with_authorized::<ManageAccess, _>(host, |_| ())
            .is_ok()
    );
    assert!(
        thread
            .with_authorized::<EditDraft, _>(writer, |_| ())
            .is_ok()
    );
    assert_eq!(
        thread
            .with_authorized::<ControlGeneration, _>(writer, |_| ())
            .unwrap_err()
            .reason,
        DenialReason::InsufficientMode
    );
    assert_eq!(
        thread
            .with_authorized::<ChangeModel, _>(writer, |_| ())
            .unwrap_err()
            .reason,
        DenialReason::InsufficientMode
    );
    assert!(
        thread
            .with_authorized::<ControlGeneration, _>(admin, |_| ())
            .is_ok()
    );
    assert_eq!(
        thread
            .with_authorized::<ManageAccess, _>(admin, |_| ())
            .unwrap_err()
            .reason,
        DenialReason::HostOnly
    );
}

#[test]
fn authorization_checks_current_membership_not_retained_profiles_or_policy() {
    let mut thread = local_thread();
    let peer = add_peer(&mut thread);
    assert!(thread.with_authorized::<EditDraft, _>(peer, |_| ()).is_ok());
    thread
        .peer_permissions
        .set_override(peer, Some(PeerMode::Admin));
    thread.participants.retain(|id| *id != peer);
    for operation in [
        PermissionOperation::EditDraft,
        PermissionOperation::ControlGeneration,
        PermissionOperation::ChangeModel,
        PermissionOperation::ManageAccess,
    ] {
        assert_eq!(
            thread.check_permission(peer, operation).unwrap_err().reason,
            DenialReason::NotParticipant
        );
    }
}

#[test]
fn read_only_rejection_precedes_closure_and_crdt_application() {
    let mut thread = local_thread();
    let peer = add_peer(&mut thread);
    thread.peer_permissions.set_default_mode(PeerMode::ReadOnly);
    let before = thread.draft().encode_state();
    let replica = Draft::new();
    replica.apply_update(&before).unwrap();
    replica.create_prompt(peer.as_uuid(), "rejected");
    let update = replica.take_local_update().unwrap();
    let called = Cell::new(false);
    assert!(
        thread
            .with_authorized::<EditDraft, _>(peer, |authorized| {
                called.set(true);
                authorized.edit(|draft| draft.create_prompt(peer.as_uuid(), "never called"))
            })
            .is_err()
    );
    assert!(!called.get());
    let error = thread
        .apply_collaborator_update(peer, 0, update)
        .unwrap_err();
    assert!(error.downcast_ref::<super::PermissionDenied>().is_some());
    assert_eq!(thread.draft().encode_state(), before);
    // Even malformed bytes are rejected as permission failures before decode.
    assert!(
        thread
            .apply_collaborator_update(peer, 1, vec![255])
            .unwrap_err()
            .downcast_ref::<super::PermissionDenied>()
            .is_some()
    );
}

#[test]
fn admin_can_consume_without_local_write_and_remote_cannot_consume() {
    let mut thread = local_thread();
    let host = thread.participant_id;
    let peer = add_peer(&mut thread);
    thread.peer_permissions.set_default_mode(PeerMode::ReadOnly);
    thread
        .peer_permissions
        .set_override(peer, Some(PeerMode::Admin));
    let prompt = thread
        .with_authorized::<EditDraft, _>(host, |authorized| {
            authorized.edit(|draft| draft.create_prompt(host.as_uuid(), "prompt"))
        })
        .unwrap();
    thread
        .with_authorized::<ControlGeneration, _>(peer, |authorized| {
            authorized.take_submission(|draft| draft.take_items(&[prompt]))
        })
        .unwrap()
        .unwrap();
    assert!(!thread.draft().contains(prompt));
    thread.ownership = ThreadOwnership::Remote;
    thread.participant_id = peer;
    assert_eq!(
        thread
            .with_authorized::<ControlGeneration, _>(peer, |authorized| authorized
                .take_submission(|_| ()))
            .unwrap()
            .unwrap_err()
            .reason,
        DenialReason::HostOnly
    );
}

#[test]
fn mirrored_request_api_also_checks_local_permissions() {
    let mut thread = local_thread();
    let peer = add_peer(&mut thread);
    let (sender, receiver) = async_channel::unbounded();
    let (uploads, _) = async_channel::unbounded();
    thread.ownership = ThreadOwnership::Remote;
    thread.participant_id = peer;
    thread.sharing = ThreadSharing::Connected {
        host: sender,
        uploads,
        link: None,
    };
    thread.peer_permissions.set_default_mode(PeerMode::Write);
    assert!(!thread.request(protocol::CollaboratorMessage::Submit { sequence: 0 }));
    assert!(!thread.request(protocol::CollaboratorMessage::Stop {
        message_id: [1; 16]
    }));
    assert!(receiver.try_recv().is_err());
    thread.peer_permissions.set_default_mode(PeerMode::Admin);
    assert!(
        thread
            .with_authorized::<ControlGeneration, _>(peer, |authorized| authorized.request_submit())
            .unwrap()
    );
    assert_eq!(
        receiver.try_recv().unwrap(),
        protocol::CollaboratorMessage::Submit { sequence: 0 }
    );
}

fn chunk(id: AttachmentId, offset: u64, bytes: &[u8]) -> protocol::AttachmentChunk {
    protocol::AttachmentChunk {
        id: id.as_uuid().into_bytes(),
        name: "a.txt".into(),
        kind: protocol::AttachmentKind::Text,
        total: 2,
        offset,
        bytes: bytes.to_vec(),
    }
}

#[gpui::test]
fn downgrade_keeps_accepted_transfers_but_rejects_unannounced_bytes(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    let host = thread.participant_id;
    let peer = add_peer(&mut thread);
    let prompt = thread.draft.create_prompt(peer.as_uuid(), "prompt");
    let accepted = AttachmentId::from_uuid(Uuid::new_v4());
    let unannounced = AttachmentId::from_uuid(Uuid::new_v4());
    thread.draft.add_attachment(
        prompt,
        AttachmentRecord {
            id: accepted,
            name: "a.txt".into(),
            kind: AttachmentKind::Text,
            size: 2,
            creator: peer.as_uuid(),
        },
    );
    thread
        .receive_upload(peer, chunk(accepted, 0, b"a"))
        .unwrap();
    thread
        .receive_upload(peer, chunk(unannounced, 0, b"a"))
        .unwrap();
    thread
        .with_authorized::<ManageAccess, _>(host, |mut authorized| {
            authorized.set_default_mode(PeerMode::ReadOnly, cx);
        })
        .unwrap();
    assert!(thread.draft.incoming.contains_key(&accepted));
    assert!(!thread.draft.incoming.contains_key(&unannounced));
    thread
        .receive_upload(peer, chunk(accepted, 1, b"b"))
        .unwrap();
    assert!(thread.draft.stored.contains(&accepted));
    assert!(
        thread
            .receive_upload(peer, chunk(unannounced, 0, b"ab"))
            .unwrap_err()
            .downcast_ref::<super::PermissionDenied>()
            .is_some()
    );
    // An accepted record also permits a transfer whose first chunk was queued
    // before downgrade but reached the host afterwards.
    let queued = AttachmentId::from_uuid(Uuid::new_v4());
    thread.draft.add_attachment(
        prompt,
        AttachmentRecord {
            id: queued,
            name: "a.txt".into(),
            kind: AttachmentKind::Text,
            size: 2,
            creator: peer.as_uuid(),
        },
    );
    thread.receive_upload(peer, chunk(queued, 0, b"a")).unwrap();
    assert!(thread.draft.incoming.contains_key(&queued));
}

#[test]
fn accepted_upload_sender_can_continue_after_downgrade() {
    let mut mirror = local_thread();
    let peer = add_peer(&mut mirror);
    let prompt = mirror.draft.create_prompt(peer.as_uuid(), "prompt");
    let id = AttachmentId::from_uuid(Uuid::new_v4());
    mirror.draft.add_attachment(
        prompt,
        AttachmentRecord {
            id,
            name: "a.txt".into(),
            kind: AttachmentKind::Text,
            size: 2,
            creator: peer.as_uuid(),
        },
    );
    mirror.draft.files.insert(
        id,
        crate::attachments::FileAttachment::from_bytes(
            "a.txt".into(),
            AttachmentKind::Text,
            b"ab".to_vec(),
        )
        .unwrap(),
    );
    let (sender, receiver) = async_channel::unbounded();
    let (uploads, _) = async_channel::unbounded();
    mirror.ownership = ThreadOwnership::Remote;
    mirror.participant_id = peer;
    mirror.sharing = ThreadSharing::Connected {
        host: sender,
        uploads,
        link: None,
    };
    mirror.peer_permissions.set_default_mode(PeerMode::ReadOnly);
    assert!(mirror.mark_upload_progress(id, 1));
    assert!(!mirror.mark_upload_progress(id, 3));
    assert!(
        mirror.request(protocol::CollaboratorMessage::AttachmentData(chunk(
            id, 1, b"b"
        )))
    );
    assert!(receiver.try_recv().is_ok());
    let unannounced = AttachmentId::from_uuid(Uuid::new_v4());
    assert!(
        !mirror.request(protocol::CollaboratorMessage::AttachmentData(chunk(
            unannounced,
            0,
            b"ab"
        )))
    );
    assert!(receiver.try_recv().is_err());
}

#[gpui::test]
fn policy_events_mirror_and_management_is_host_only(cx: &mut gpui::TestAppContext) {
    let mut host = local_thread();
    let peer = add_peer(&mut host);
    let mut mirror = local_thread();
    mirror.ownership = ThreadOwnership::Remote;
    mirror.participant_id = peer;
    mirror.participants = host.participants.clone();
    let host_id = host.participant_id;
    host.with_authorized::<ManageAccess, _>(host_id, |mut authorized| {
        authorized.set_default_mode(PeerMode::ReadOnly, cx);
        authorized.set_override(peer, Some(PeerMode::Write), cx);
    })
    .unwrap();
    mirror
        .try_apply(
            protocol::HostMessage::DefaultPeerModeChanged(PeerMode::ReadOnly),
            cx,
        )
        .unwrap();
    mirror
        .try_apply(
            protocol::HostMessage::PeerModeOverrideChanged {
                participant: peer.into_bytes(),
                mode: Some(PeerMode::Write),
            },
            cx,
        )
        .unwrap();
    assert_eq!(mirror.peer_permissions(), host.peer_permissions());
    assert_eq!(mirror.local_mode(), PeerMode::Write);
    assert!(
        mirror
            .with_authorized::<ManageAccess, _>(host_id, |_| ())
            .is_err()
    );
    mirror
        .try_apply(
            protocol::HostMessage::PeerModeOverrideChanged {
                participant: peer.into_bytes(),
                mode: None,
            },
            cx,
        )
        .unwrap();
    assert_eq!(mirror.local_mode(), PeerMode::ReadOnly);
}

#[test]
fn compaction_replaces_request_prefix_but_keeps_complete_transcript() {
    let mut thread = local_thread();
    let first = rig::completion::Message::user("Original request");
    let later = rig::completion::Message::user("Next request");
    thread.transcript.append(first.clone());
    thread
        .transcript
        .install_compaction("First summary".into())
        .unwrap();
    thread.transcript.append(later.clone());
    assert_eq!(
        thread.request_history(),
        vec![
            rig::completion::Message::user("Summary of the conversation so far:\nFirst summary"),
            later.clone()
        ]
    );
    thread
        .transcript
        .install_compaction("Second summary".into())
        .unwrap();
    assert_eq!(
        thread.request_history(),
        vec![rig::completion::Message::user(
            "Summary of the conversation so far:\nSecond summary"
        )]
    );
    assert_eq!(thread.transcript.messages(), vec![first, later]);
}

#[gpui::test]
fn compaction_events_preserve_previous_context_on_failure_and_stop(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    thread
        .transcript
        .append(rig::completion::Message::user("Earlier request"));
    let previous: String = "Earlier summary".into();
    thread
        .transcript
        .install_compaction(previous.clone())
        .unwrap();
    let id = Uuid::new_v4().into_bytes();
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id: None,
                started_at: std::time::SystemTime::now(),
                prompt: protocol::Json::from_rig(&rig::completion::Message::user("Continue")),
            },
            cx,
        )
        .unwrap();
    thread
        .try_apply_for_test(protocol::HostMessage::CompactionStarted, cx)
        .unwrap();
    thread
        .try_apply_for_test(
            protocol::HostMessage::CompactionFinished { context: None },
            cx,
        )
        .unwrap();
    assert_eq!(thread.transcript.latest_compaction(), Some(&previous));
    assert!(!thread.is_compacting());
    thread
        .try_apply_for_test(protocol::HostMessage::CompactionStarted, cx)
        .unwrap();
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentEnded {
                id,
                outcome: protocol::RunOutcome::Stopped,
                duration: std::time::Duration::ZERO,
            },
            cx,
        )
        .unwrap();
    assert!(!thread.is_compacting());
    assert_eq!(thread.transcript.latest_compaction(), Some(&previous));
    assert_eq!(thread.transcript.messages().len(), 2);
}

#[gpui::test]
fn compaction_keeps_agent_message_views(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    let id = Uuid::new_v4().into_bytes();
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id: None,
                started_at: std::time::SystemTime::now(),
                prompt: protocol::Json::from_rig(&rig::completion::Message::user("Request")),
            },
            cx,
        )
        .unwrap();
    let view = thread.agent_message_mut(id).unwrap().ui.text_view.clone();
    thread.agent_message_mut(id).unwrap().ui.work_expanded = false;

    thread
        .try_apply_for_test(protocol::HostMessage::CompactionStarted, cx)
        .unwrap();
    thread
        .try_apply_for_test(
            protocol::HostMessage::CompactionFinished {
                context: Some("Summary".into()),
            },
            cx,
        )
        .unwrap();
    let ui = &thread.agent_message_mut(id).unwrap().ui;
    assert_eq!(ui.text_view.entity_id(), view.entity_id());
    assert!(!ui.work_expanded);
    assert_eq!(thread.transcript.messages().len(), 1);
}

#[gpui::test]
fn welcome_rejects_duplicate_timeline_ids(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    let id = Uuid::new_v4().into_bytes();
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id: None,
                started_at: std::time::SystemTime::now(),
                prompt: protocol::Json::from_rig(&rig::completion::Message::user("Request")),
            },
            cx,
        )
        .unwrap();
    let mut welcome = protocol::Welcome {
        participant_id: thread.participant_id.into_bytes(),
        thread: thread.to_protocol(),
        draft: thread.draft.encode_state(),
        draft_generation: 0,
        presence: Vec::new(),
        stored_attachments: Vec::new(),
    };
    welcome
        .thread
        .messages
        .push(protocol::TimelineMessage::User(protocol::UserMessage {
            id,
            blocks: Vec::new(),
            comments: Vec::new(),
        }));
    let error = Thread::prepare_welcome(welcome)
        .err()
        .expect("duplicate UUID rejected");
    assert!(
        error
            .to_string()
            .contains("duplicate timeline message identity")
    );
}

/// Joining during preparation must not manufacture the upcoming prompt or fold
/// a summary as an ordinary Rig message.
#[gpui::test]
fn pre_run_summary_is_recorded_before_prompt_and_replays_at_every_phase(
    cx: &mut gpui::TestAppContext,
) {
    use crate::transcript::TranscriptEntry;
    for summary in [Some("Earlier conversation".to_owned()), None] {
        let mut host = local_thread();
        let peer = add_peer(&mut host);
        use agent::test_support::{MockStreamEvent, mock_final_with_total_tokens, turn};
        let earlier = Uuid::new_v4().into_bytes();
        host.try_apply_for_test(
            protocol::HostMessage::AgentStarted {
                id: earlier,
                comment_group_id: None,
                started_at: std::time::SystemTime::now(),
                prompt: protocol::Json::from_rig(&rig::completion::Message::user(
                    "Earlier request",
                )),
            },
            cx,
        )
        .unwrap();
        for event in turn([
            MockStreamEvent::Text("Earlier answer".into()),
            MockStreamEvent::FinalResponse(mock_final_with_total_tokens(100)),
        ]) {
            host.apply_agent_event(Uuid::from_bytes(earlier), protocol::Json::shared(event), cx)
                .unwrap();
        }
        host.try_apply_for_test(
            protocol::HostMessage::AgentEnded {
                id: earlier,
                outcome: protocol::RunOutcome::Completed,
                duration: std::time::Duration::ZERO,
            },
            cx,
        )
        .unwrap();
        let id = Uuid::new_v4().into_bytes();
        let started_at = std::time::SystemTime::now();
        host.try_apply_for_test(
            protocol::HostMessage::AgentPreparing {
                id,
                comment_group_id: None,
                started_at,
            },
            cx,
        )
        .unwrap();
        host.try_apply_for_test(protocol::HostMessage::CompactionStarted, cx)
            .unwrap();
        let snapshot = |thread: &Thread, cx: &mut gpui::TestAppContext| {
            let welcome = protocol::Welcome {
                participant_id: peer.into_bytes(),
                thread: thread.to_protocol(),
                draft: thread.draft.encode_state(),
                draft_generation: 0,
                presence: Vec::new(),
                stored_attachments: Vec::new(),
            };
            Thread::from_prepared_welcome(
                Thread::prepare_welcome(welcome).unwrap(),
                ThreadDraft::new(peer),
                ThreadSharing::NotShared,
                cx,
            )
        };
        let mut mirror = snapshot(&host, cx);
        assert!(mirror.is_compacting());
        assert_eq!(mirror.transcript.entries.len(), 2);
        let start = mirror.agent_message_mut(id).unwrap().record.start;
        assert!(!mirror.transcript.has_prompt(start));
        let finish = protocol::HostMessage::CompactionFinished {
            context: summary.clone(),
        };
        host.try_apply_for_test(finish.clone(), cx).unwrap();
        mirror.try_apply_for_test(finish, cx).unwrap();
        assert_eq!(mirror.transcript, host.transcript);
        let mut joined = snapshot(&host, cx);
        assert_eq!(joined.transcript, host.transcript);
        assert_eq!(
            joined.agent_message_mut(earlier).unwrap().ui.output,
            host.agent_message_mut(earlier).unwrap().ui.output
        );
        let start = protocol::HostMessage::AgentStarted {
            id,
            comment_group_id: None,
            started_at,
            prompt: protocol::Json::from_rig(&rig::completion::Message::user("Next request")),
        };
        host.try_apply_for_test(start.clone(), cx).unwrap();
        mirror.try_apply_for_test(start.clone(), cx).unwrap();
        assert_eq!(mirror.transcript, host.transcript);
        if summary.is_some() {
            assert!(matches!(
                host.transcript.entries.as_slice(),
                [
                    TranscriptEntry::Message(_),
                    TranscriptEntry::Message(_),
                    TranscriptEntry::Compaction(_),
                    TranscriptEntry::Message(_)
                ]
            ));
        } else {
            assert_eq!(host.transcript.entries.len(), 3);
        }
        let recorded = host.transcript.clone();
        assert!(host.try_apply_for_test(start, cx).is_err());
        assert_eq!(host.transcript, recorded);
        let mut restored = snapshot(&host, cx);
        assert_eq!(
            restored.agent_message_mut(id).unwrap().ui.output,
            host.agent_message_mut(id).unwrap().ui.output
        );
    }
}

#[gpui::test]
fn summary_steps_survive_streaming_and_stay_with_their_run(cx: &mut gpui::TestAppContext) {
    use crate::timeline::AgentStep;
    use agent::test_support::{MockStreamEvent, mock_final_with_total_tokens, turn};
    let mut thread = local_thread();
    let peer = add_peer(&mut thread);
    let id = Uuid::new_v4().into_bytes();
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id: None,
                started_at: std::time::SystemTime::now(),
                prompt: protocol::Json::from_rig(&rig::completion::Message::user("Request")),
            },
            cx,
        )
        .unwrap();
    for event in turn([
        MockStreamEvent::Reasoning {
            id: "thinking".into(),
            text: "Think first".into(),
        },
        MockStreamEvent::Text("Before".into()),
        MockStreamEvent::FinalResponse(mock_final_with_total_tokens(100)),
    ]) {
        thread
            .apply_agent_event(Uuid::from_bytes(id), protocol::Json::shared(event), cx)
            .unwrap();
    }
    let thinking_view = {
        let ui = &mut thread.agent_message_mut(id).unwrap().ui;
        ui.step_view_mut(0).unwrap().set_expanded(true);
        let crate::timeline::StepView::Thinking { view, .. } = ui.step_view(0).unwrap() else {
            panic!("thinking view")
        };
        view.entity_id()
    };
    for summary in ["First", "Second"] {
        thread
            .try_apply_for_test(protocol::HostMessage::CompactionStarted, cx)
            .unwrap();
        thread
            .try_apply_for_test(
                protocol::HostMessage::CompactionFinished {
                    context: Some(summary.into()),
                },
                cx,
            )
            .unwrap();
    }
    let events = turn([
        MockStreamEvent::Text("Partial".into()),
        MockStreamEvent::FinalResponse(mock_final_with_total_tokens(100)),
    ]);
    for event in events
        .into_iter()
        .take_while(|event| !matches!(event, agent::AgentEvent::TurnEnded { .. }))
    {
        thread
            .apply_agent_event(Uuid::from_bytes(id), protocol::Json::shared(event), cx)
            .unwrap();
    }
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentEnded {
                id,
                outcome: protocol::RunOutcome::Stopped,
                duration: std::time::Duration::ZERO,
            },
            cx,
        )
        .unwrap();
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentStarted {
                id: Uuid::new_v4().into_bytes(),
                comment_group_id: None,
                started_at: std::time::SystemTime::now(),
                prompt: protocol::Json::from_rig(&rig::completion::Message::user("Continue")),
            },
            cx,
        )
        .unwrap();
    assert_eq!(
        thread.agent_message_mut(id).unwrap().ui.output.steps(),
        &[
            AgentStep::Thinking("Think first".into()),
            AgentStep::Text("Before".into()),
            AgentStep::Compaction,
            AgentStep::Compaction,
            AgentStep::Text("Partial".into())
        ]
    );
    let ui = &thread.agent_message_mut(id).unwrap().ui;
    let crate::timeline::StepView::Thinking { view, expanded } = ui.step_view(0).unwrap() else {
        panic!("thinking view")
    };
    assert_eq!(view.entity_id(), thinking_view);
    assert!(*expanded);
    assert_eq!(ui.views().count(), ui.output.steps().len());
    let welcome = protocol::Welcome {
        participant_id: peer.into_bytes(),
        thread: thread.to_protocol(),
        draft: thread.draft.encode_state(),
        draft_generation: 0,
        presence: Vec::new(),
        stored_attachments: Vec::new(),
    };
    let mut restored = Thread::from_prepared_welcome(
        Thread::prepare_welcome(welcome).unwrap(),
        ThreadDraft::new(peer),
        ThreadSharing::NotShared,
        cx,
    );
    assert_eq!(
        restored.agent_message_mut(id).unwrap().ui.output,
        thread.agent_message_mut(id).unwrap().ui.output
    );
}
