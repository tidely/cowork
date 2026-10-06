use std::{cell::Cell, sync::Arc};

use ::draft::{AttachmentId, AttachmentKind, AttachmentRecord, CommentTarget, Draft};
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
fn modes_keep_editing_and_generation_distinct() {
    assert_eq!(PeerMode::default(), PeerMode::Write);
    assert!(!PeerMode::ReadOnly.can_edit_draft());
    assert!(PeerMode::Write.can_edit_draft());
    assert!(!PeerMode::Write.can_control_generation());
    assert!(!PeerMode::Write.can_change_model());
    assert!(PeerMode::Admin.can_control_generation());
    assert!(PeerMode::Admin.can_change_model());
    assert_eq!(PeerMode::ReadOnly.label(), "Read only");
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
fn snapshot_round_trip_preserves_sparse_explicit_policy() {
    let mut thread = local_thread();
    let inherited = add_peer(&mut thread);
    let explicit = add_peer(&mut thread);
    thread.peer_permissions.set_default_mode(PeerMode::Write);
    thread
        .peer_permissions
        .set_override(explicit, Some(PeerMode::Write));
    let snapshot = thread.to_protocol();
    let bytes = postcard::to_stdvec(&snapshot).unwrap();
    let decoded: protocol::ThreadSnapshot = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, snapshot);
    assert_eq!(decoded.peer_permissions.override_for(inherited), None);
    assert_eq!(
        decoded.peer_permissions.override_for(explicit),
        Some(PeerMode::Write)
    );
}

#[test]
fn host_stays_admin_but_admin_peer_cannot_manage_access() {
    let mut thread = local_thread();
    let host = thread.participant_id;
    let peer = add_peer(&mut thread);
    thread.peer_permissions.set_default_mode(PeerMode::ReadOnly);
    thread
        .peer_permissions
        .set_override(host, Some(PeerMode::ReadOnly));
    assert_eq!(thread.local_mode(), PeerMode::Admin);
    assert!(
        thread
            .with_authorized::<ManageAccess, _>(host, |_| ())
            .is_ok()
    );
    thread
        .peer_permissions
        .set_override(peer, Some(PeerMode::Admin));
    assert!(
        thread
            .with_authorized::<ControlGeneration, _>(peer, |_| ())
            .is_ok()
    );
    assert_eq!(
        thread
            .with_authorized::<ManageAccess, _>(peer, |_| ())
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
    assert_eq!(
        thread
            .with_authorized::<EditDraft, _>(peer, |_| ())
            .unwrap_err()
            .reason,
        DenialReason::NotParticipant
    );
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
fn write_edits_everyones_prompts_comments_and_attachments_but_not_controls() {
    let mut thread = local_thread();
    let host = thread.participant_id;
    let peer = add_peer(&mut thread);
    thread.peer_permissions.set_default_mode(PeerMode::Write);
    let (prompt, comment) = thread
        .with_authorized::<EditDraft, _>(host, |authorized| {
            authorized.edit(|draft| {
                let prompt = draft.create_prompt(host.as_uuid(), "Host prompt");
                let comment = draft.create_comment(
                    host.as_uuid(),
                    CommentTarget {
                        message_id: Uuid::new_v4(),
                        range: 0..1,
                        quote: "a".into(),
                    },
                    "Host comment",
                );
                (prompt, comment)
            })
        })
        .unwrap();
    let attachment = AttachmentId::from_uuid(Uuid::new_v4());
    thread
        .with_authorized::<EditDraft, _>(peer, |authorized| {
            authorized.edit(|draft| {
                draft.set_body(prompt, "Peer edits host prompt");
                draft.set_body(comment, "Peer edits host comment");
                assert!(draft.add_attachment(
                    prompt,
                    AttachmentRecord {
                        id: attachment,
                        name: "a.txt".into(),
                        kind: AttachmentKind::Text,
                        size: 1,
                        creator: peer.as_uuid()
                    }
                ));
                assert!(draft.remove_attachment(attachment));
            })
        })
        .unwrap();
    assert_eq!(
        thread.draft().body(prompt).as_deref(),
        Some("Peer edits host prompt")
    );
    assert_eq!(
        thread.draft().body(comment).as_deref(),
        Some("Peer edits host comment")
    );
    assert!(
        thread
            .with_authorized::<ControlGeneration, _>(peer, |_| ())
            .is_err()
    );
    assert!(
        thread
            .with_authorized::<ChangeModel, _>(peer, |_| ())
            .is_err()
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

#[test]
fn reset_replaces_rejected_history_and_preserves_host_history() {
    let mut thread = local_thread();
    let host = thread.participant_id;
    let peer = add_peer(&mut thread);
    let accepted = thread
        .draft
        .create_prompt(host.as_uuid(), "accepted host history");
    let snapshot = thread.draft().encode_state();
    let mut mirrored = ThreadDraft::new(peer);
    mirrored.doc.apply_update(&snapshot).unwrap();
    mirrored.set_body(accepted, "rejected edit");
    let rejected = mirrored.create_prompt(peer.as_uuid(), "rejected item");
    mirrored.reset(&snapshot).unwrap();
    assert_eq!(
        mirrored.body(accepted).as_deref(),
        Some("accepted host history")
    );
    assert!(!mirrored.contains(rejected));
    assert!(mirrored.take_local_update().is_none());
    mirrored.set_body(accepted, "new authorized edit");
    thread
        .apply_collaborator_update(peer, 0, mirrored.take_local_update().unwrap())
        .unwrap();
    assert_eq!(
        thread.draft().body(accepted).as_deref(),
        Some("new authorized edit")
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

#[test]
fn downgrade_keeps_accepted_transfers_but_rejects_unannounced_bytes() {
    let mut thread = local_thread();
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
    thread.peer_permissions.set_default_mode(PeerMode::ReadOnly);
    thread.prune_unaccepted_uploads();
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
    thread.cancel_upload(peer, queued).unwrap();
    assert!(!thread.draft.incoming.contains_key(&queued));
    assert!(thread.draft.discarded.contains(&queued));
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
    assert!(mirror.can_edit_draft());
    assert!(!mirror.can_control_generation());
    assert!(!mirror.can_change_model());
    let model = crate::models::ModelRef {
        provider: crate::models::ModelProvider::Ollama,
        id: "test".into(),
    };
    assert_eq!(
        mirror.select_model(model, cx).unwrap_err().reason,
        DenialReason::InsufficientMode
    );
    assert!(mirror.model().is_none());
    let optimistic = mirror
        .with_authorized::<EditDraft, _>(peer, |authorized| {
            authorized.edit(|draft| draft.create_prompt(peer.as_uuid(), "optimistic"))
        })
        .unwrap();
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
    // The host assigns a new epoch when it rejects the old replica.
    host.peer_draft_generations.insert(peer, 1);
    mirror.try_apply(host.draft_reset_for(peer), cx).unwrap();
    assert!(!mirror.draft().contains(optimistic));
    assert!(mirror.draft.take_local_update().is_none());
    // Readers can maintain all projection caches without entering EditDraft.
    mirror.update_draft_editors(|items, state| {
        assert!(items.is_empty());
        state.comments_folded = true;
        state
            .editors
            .retain(|id, _| items.iter().any(|item| item.id == *id));
    });
    assert!(mirror.draft().comments_folded);
}
