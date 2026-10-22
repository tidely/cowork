use std::{cell::Cell, sync::Arc};

use ::draft::{AttachmentId, AttachmentKind, AttachmentRecord, Draft};
use uuid::Uuid;

use super::{
    ChangeModel, ControlGeneration, DenialReason, EditDraft, ManageAccess, PeerMode,
    PeerPermissions, PermissionOperation, Thread, ThreadOwnership, ThreadSharing,
    draft::ThreadDraft,
};
use crate::{
    participant::ParticipantId,
    protocol,
    test_support::{ScriptedHost, streamed_text},
};

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

fn apply_all(
    thread: &mut Thread,
    events: Vec<protocol::HostMessage>,
    cx: &mut gpui::TestAppContext,
) {
    for event in events {
        thread.try_apply_for_test(event, cx).unwrap();
    }
}

fn snapshot_for(thread: &Thread, peer: ParticipantId, cx: &mut gpui::TestAppContext) -> Thread {
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
}

/// A failed or stopped compaction leaves the context as it was; an accepted
/// one replaces the older part for later requests, and every entry stays.
#[gpui::test]
fn compaction_keeps_the_transcript_and_survives_failure_and_stop(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    let mut host = ScriptedHost::default();
    let earlier = Uuid::new_v4();
    let mut events = host.start(earlier, "Earlier request");
    events.extend(host.turn(
        earlier,
        [streamed_text("a", &"Answer ".repeat(50), false)],
        100,
    ));
    events.push(host.ended(earlier, protocol::RunOutcome::Completed));
    let id = Uuid::new_v4();
    events.extend(host.start(id, "Continue"));
    events.push(protocol::HostMessage::AgentEvent {
        id: id.into_bytes(),
        event: protocol::RunEvent::CompactionStarted,
    });
    apply_all(&mut thread, events, cx);
    assert!(thread.is_compacting());
    thread
        .try_apply_for_test(
            protocol::HostMessage::AgentEvent {
                id: id.into_bytes(),
                event: protocol::RunEvent::CompactionFailed,
            },
            cx,
        )
        .unwrap();
    assert!(!thread.is_compacting());
    assert!(thread.transcript.conversation().compactions().is_empty());

    let mut compaction = host.compaction(id, 2, "Earlier conversation");
    let compacted = compaction.pop().unwrap();
    apply_all(&mut thread, compaction, cx);
    apply_all(
        &mut thread,
        vec![host.ended(id, protocol::RunOutcome::Stopped)],
        cx,
    );
    assert!(!thread.is_compacting());
    assert!(thread.try_apply_for_test(compacted.clone(), cx).is_err());
    assert!(thread.transcript.conversation().compactions().is_empty());

    let last = Uuid::new_v4();
    apply_all(&mut thread, vec![host.started(last, None)], cx);
    apply_all(
        &mut thread,
        vec![
            protocol::HostMessage::AgentEvent {
                id: last.into_bytes(),
                event: protocol::RunEvent::CompactionStarted,
            },
            protocol::HostMessage::AgentEvent {
                id: last.into_bytes(),
                event: match compacted {
                    protocol::HostMessage::AgentEvent { event, .. } => event,
                    _ => unreachable!(),
                },
            },
        ],
        cx,
    );
    assert_eq!(thread.transcript.conversation().compactions().len(), 1);
    assert_eq!(thread.transcript.len(), 3);
    assert_eq!(thread.transcript.conversation().model_messages().len(), 2);
}

#[gpui::test]
fn compaction_keeps_agent_message_views(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    let mut host = ScriptedHost::default();
    let id = Uuid::new_v4();
    apply_all(&mut thread, host.start(id, &"Request ".repeat(20)), cx);
    let view = thread
        .agent_message_mut(id.into_bytes())
        .unwrap()
        .ui
        .text_view
        .clone();
    thread
        .agent_message_mut(id.into_bytes())
        .unwrap()
        .ui
        .work_expanded = false;
    apply_all(&mut thread, host.compaction(id, 1, "Summary"), cx);
    let ui = &thread.agent_message_mut(id.into_bytes()).unwrap().ui;
    assert_eq!(ui.text_view.entity_id(), view.entity_id());
    assert!(!ui.work_expanded);
    assert_eq!(thread.transcript.len(), 1);
}

#[gpui::test]
fn welcome_rejects_duplicate_timeline_ids(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    let id = Uuid::new_v4();
    apply_all(
        &mut thread,
        vec![ScriptedHost::default().started(id, None)],
        cx,
    );
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
            id: id.into_bytes(),
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

/// Someone joining while the context is being summarized, before a run's
/// prompt, or after, ends up with the host's transcript and output.
#[gpui::test]
fn joining_during_compaction_replays_at_every_phase(cx: &mut gpui::TestAppContext) {
    let mut thread = local_thread();
    let peer = add_peer(&mut thread);
    let mut host = ScriptedHost::default();
    let earlier = Uuid::new_v4();
    let mut events = host.start(earlier, &"Earlier request ".repeat(20));
    events.extend(host.turn(earlier, [streamed_text("a", "Earlier answer", false)], 100));
    events.push(host.ended(earlier, protocol::RunOutcome::Completed));
    apply_all(&mut thread, events, cx);

    let id = Uuid::new_v4();
    apply_all(&mut thread, vec![host.started(id, None)], cx);
    let mut joined = snapshot_for(&thread, peer, cx);
    assert_eq!(joined.transcript, thread.transcript);
    let mut events = vec![host.prompt(id, "Next request")];
    events.extend(host.compaction(id, 2, "Earlier conversation"));
    let compacted = events.pop().unwrap();
    for replica in [&mut thread, &mut joined] {
        apply_all(replica, events.clone(), cx);
    }
    let mut mirror = snapshot_for(&thread, peer, cx);
    assert!(mirror.is_compacting());
    for replica in [&mut thread, &mut joined, &mut mirror] {
        apply_all(replica, vec![compacted.clone()], cx);
    }
    let mut restored = snapshot_for(&thread, peer, cx);
    for replica in [&mut joined, &mut mirror, &mut restored] {
        assert_eq!(replica.transcript, thread.transcript);
        assert!(!replica.is_compacting());
        for message in [earlier, id] {
            let id = message.into_bytes();
            assert_eq!(
                replica.agent_message_mut(id).unwrap().ui.output,
                thread.agent_message_mut(id).unwrap().ui.output
            );
        }
    }
}

#[gpui::test]
fn summary_steps_survive_streaming_and_stay_with_their_run(cx: &mut gpui::TestAppContext) {
    use crate::timeline::AgentStep;
    let mut thread = local_thread();
    let peer = add_peer(&mut thread);
    let mut host = ScriptedHost::default();
    let id = Uuid::new_v4();
    let mut events = host.start(id, &"Request ".repeat(20));
    events.extend(host.turn(
        id,
        [
            streamed_text("thinking", "Think first", true),
            streamed_text("answer", "Before", false),
        ],
        100,
    ));
    apply_all(&mut thread, events, cx);
    let thinking_view = {
        let ui = &mut thread.agent_message_mut(id.into_bytes()).unwrap().ui;
        ui.step_view_mut(0).unwrap().set_expanded(true);
        let crate::timeline::StepView::Thinking { view, .. } = ui.step_view(0).unwrap() else {
            panic!("thinking view")
        };
        view.entity_id()
    };
    let first = host.compaction(id, 1, "First");
    let second = host.compaction(id, 2, "Second");
    apply_all(&mut thread, first, cx);
    apply_all(&mut thread, second, cx);
    // A reply that never completes: its preview only.
    let mut partial = host.turn(id, [streamed_text("answer", "Partial", false)], 100);
    partial.truncate(1);
    partial.push(host.ended(id, protocol::RunOutcome::Stopped));
    partial.push(host.started(Uuid::new_v4(), None));
    apply_all(&mut thread, partial, cx);
    assert_eq!(
        thread
            .agent_message_mut(id.into_bytes())
            .unwrap()
            .ui
            .output
            .steps(),
        &[
            AgentStep::Thinking("Think first".into()),
            AgentStep::Text("Before".into()),
            AgentStep::Compaction,
            AgentStep::Compaction,
            AgentStep::Text("Partial".into())
        ]
    );
    let ui = &thread.agent_message_mut(id.into_bytes()).unwrap().ui;
    let crate::timeline::StepView::Thinking { view, expanded } = ui.step_view(0).unwrap() else {
        panic!("thinking view")
    };
    assert_eq!(view.entity_id(), thinking_view);
    assert!(*expanded);
    assert_eq!(ui.views().count(), ui.output.steps().len());
    let mut restored = snapshot_for(&thread, peer, cx);
    assert_eq!(
        restored
            .agent_message_mut(id.into_bytes())
            .unwrap()
            .ui
            .output,
        thread.agent_message_mut(id.into_bytes()).unwrap().ui.output
    );
}
