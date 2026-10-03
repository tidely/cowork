//! Regression cases for revocation, lifecycle cleanup, and replica epochs.

use std::sync::Arc;

use ::draft::{AttachmentId, AttachmentKind, AttachmentRecord, ItemId};
use gpui::AppContext as _;
use uuid::Uuid;

use super::{
    DenialReason, EditDraft, ManageAccess, PeerMode, Thread, ThreadOwnership, ThreadSharing,
    draft::{EditorSlot, ItemEditors, ThreadDraft},
};
use crate::{participant::ParticipantId, protocol};

fn host_with_prompt(body: &str) -> (Thread, ItemId) {
    let actor = ParticipantId::new();
    let mut draft = ThreadDraft::new(actor);
    let prompt = draft.create_prompt(actor.as_uuid(), body);
    (
        Thread::new_local(
            "Test".into(),
            Vec::new(),
            draft,
            actor,
            Arc::default(),
            None,
        ),
        prompt,
    )
}

fn add_peer(thread: &mut Thread) -> ParticipantId {
    let actor = ParticipantId::new();
    if thread.participants.is_empty() {
        thread.participants.push(thread.participant_id);
    }
    thread.participants.push(actor);
    thread.peer_draft_generations.insert(actor, 0);
    actor
}

fn reading(id: ItemId) -> protocol::Presence {
    protocol::Presence {
        focus: Some(protocol::PresenceFocus::Item(id.as_uuid().into_bytes())),
        selection: None,
        pending_reads: vec![protocol::PendingRead {
            id: [9; 16],
            name: "pending.txt".into(),
            is_image: false,
            progress: None,
            block: Some(id.as_uuid().into_bytes()),
        }],
    }
}

#[gpui::test]
fn readonly_presence_cannot_block_submission_or_remove_empty_items(cx: &mut gpui::TestAppContext) {
    let (mut host, empty) = host_with_prompt("");
    let actor = add_peer(&mut host);
    host.with_authorized::<ManageAccess, _>(host.participant_id, |mut auth| {
        auth.set_default_mode(PeerMode::ReadOnly, cx);
    })
    .unwrap();
    let before = host.draft().encode_state();
    host.host_presence(actor, reading(empty), cx);
    assert_eq!(host.draft.presence[&actor].0, protocol::Presence::default());
    assert!(!host.draft().others_are_reading_files());
    assert!(!host.draft().has_announced_reads_into(empty));
    host.host_presence(actor, protocol::Presence::default(), cx);
    assert!(host.draft().contains(empty));
    assert_eq!(host.draft().encode_state(), before);
}

#[gpui::test]
fn downgrade_clears_existing_work_without_deleting_its_empty_item(cx: &mut gpui::TestAppContext) {
    let (mut host, empty) = host_with_prompt("");
    let revoked = add_peer(&mut host);
    let retained = add_peer(&mut host);
    host.host_presence(revoked, reading(empty), cx);
    host.host_presence(retained, reading(empty), cx);
    host.with_authorized::<ManageAccess, _>(host.participant_id, |mut auth| {
        auth.set_override(revoked, Some(PeerMode::ReadOnly), cx);
    })
    .unwrap();
    assert_eq!(
        host.draft.presence[&revoked].0,
        protocol::Presence::default()
    );
    assert_eq!(host.draft.presence[&retained].0, reading(empty));
    assert!(host.draft().contains(empty));
    host.with_authorized::<ManageAccess, _>(host.participant_id, |mut auth| {
        auth.set_default_mode(PeerMode::ReadOnly, cx);
    })
    .unwrap();
    assert_eq!(
        host.draft.presence[&retained].0,
        protocol::Presence::default()
    );
    assert!(!host.draft().others_are_reading_files());
    assert!(host.draft().contains(empty));
}

#[gpui::test]
fn host_session_cleanup_removes_unfinished_uploads_before_membership(
    cx: &mut gpui::TestAppContext,
) {
    let (mut host, prompt) = host_with_prompt("ready once the unfinished file is removed");
    let actor = add_peer(&mut host);
    let id = AttachmentId::from_uuid(Uuid::new_v4());
    host.draft.add_attachment(
        prompt,
        AttachmentRecord {
            id,
            name: "a.txt".into(),
            kind: AttachmentKind::Text,
            size: 2,
            creator: actor.as_uuid(),
        },
    );
    host.receive_upload(
        actor,
        protocol::AttachmentChunk {
            id: id.as_uuid().into_bytes(),
            name: "a.txt".into(),
            kind: protocol::AttachmentKind::Text,
            total: 2,
            offset: 0,
            bytes: b"a".to_vec(),
        },
    )
    .unwrap();
    host.peer_permissions
        .set_override(actor, Some(PeerMode::ReadOnly));
    assert!(host.draft().has_unstored_attachments());
    // stop_hosting uses this same cleanup before replacing Shared or clearing
    // membership. Testing the state phase needs no network Endpoint.
    host.cleanup_hosted_participants(cx);
    assert!(host.participants().is_empty());
    assert!(host.peer_draft_generations.is_empty());
    assert!(host.draft.incoming.is_empty());
    assert!(host.draft().attachment_records().is_empty());
    assert!(!host.draft().has_unstored_attachments());
    assert_eq!(host.peer_permissions().override_for(actor), None);
    assert!(host.draft().contains(prompt));
    let state = host.draft().encode_state();
    host.participant_left(actor, cx);
    assert_eq!(host.draft().encode_state(), state);
}

#[test]
fn old_epoch_updates_remain_fenced_after_regrant_and_fresh_updates_succeed() {
    let (mut host, prompt) = host_with_prompt("host history");
    let actor = add_peer(&mut host);
    let other = add_peer(&mut host);
    let state = host.draft().encode_state();
    let mut old = ThreadDraft::new(actor);
    old.doc.apply_update(&state).unwrap();
    old.set_body(prompt, "rejected first edit");
    let first = old.take_local_update().unwrap();
    old.set_body(prompt, "dependent queued edit");
    let second = old.take_local_update().unwrap();
    host.peer_permissions
        .set_override(actor, Some(PeerMode::ReadOnly));
    let denied = host
        .apply_collaborator_update(actor, 0, first.clone())
        .unwrap_err();
    assert_eq!(
        denied
            .downcast_ref::<super::PermissionDenied>()
            .unwrap()
            .reason,
        DenialReason::InsufficientMode
    );
    assert_eq!(host.draft_generation_for(actor), 1);
    assert_eq!(host.draft_generation_for(other), 0);
    let reset = host.draft_reset_for(actor);
    let stale = host
        .apply_collaborator_update(actor, 0, second.clone())
        .unwrap_err();
    assert_eq!(
        stale
            .downcast_ref::<super::PermissionDenied>()
            .unwrap()
            .reason,
        DenialReason::StaleDraftGeneration
    );
    assert_eq!(host.draft_reset_for(actor), reset);
    host.peer_permissions
        .set_override(actor, Some(PeerMode::Write));
    let stale = host
        .apply_collaborator_update(actor, 0, second)
        .unwrap_err();
    assert_eq!(
        stale
            .downcast_ref::<super::PermissionDenied>()
            .unwrap()
            .reason,
        DenialReason::StaleDraftGeneration
    );
    assert_eq!(host.draft().encode_state(), state);
    assert_eq!(host.draft_generation_for(actor), 1);
    // Even malformed/future-epoch bytes are fenced before Yrs decode.
    let stale = host
        .apply_collaborator_update(actor, 99, vec![255])
        .unwrap_err();
    assert_eq!(
        stale
            .downcast_ref::<super::PermissionDenied>()
            .unwrap()
            .reason,
        DenialReason::StaleDraftGeneration
    );
    assert_eq!(host.draft_generation_for(actor), 1);
    let protocol::HostMessage::DraftReset { generation, state } = reset else {
        panic!("reset");
    };
    let mut fresh = ThreadDraft::new(actor);
    fresh.reset(&state).unwrap();
    fresh.set_body(prompt, "fresh authorized edit");
    host.with_authorized::<EditDraft, _>(actor, |auth| {
        auth.apply_collaborator_update(generation, fresh.take_local_update().unwrap())
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        host.draft().body(prompt).as_deref(),
        Some("fresh authorized edit")
    );
    assert!(host.apply_collaborator_update(actor, 0, first).is_err());
    assert_eq!(
        host.draft().body(prompt).as_deref(),
        Some("fresh authorized edit")
    );
    assert_eq!(host.draft_generation_for(actor), generation);
}

fn welcome_for(host: &Thread, actor: ParticipantId, generation: u64) -> protocol::Welcome {
    protocol::Welcome {
        participant_id: actor.into_bytes(),
        thread: host.to_protocol(),
        draft: host.draft().encode_state(),
        draft_generation: generation,
        presence: Vec::new(),
        stored_attachments: Vec::new(),
    }
}

#[gpui::test]
fn welcome_merges_matching_epochs_but_replaces_changed_epochs(cx: &mut gpui::TestAppContext) {
    let (mut host, prompt) = host_with_prompt("authoritative");
    let actor = add_peer(&mut host);
    let mut mirror = Thread::from_prepared_welcome(
        Thread::prepare_welcome(welcome_for(&host, actor, 0)).unwrap(),
        ThreadDraft::new(actor),
        ThreadSharing::NotShared,
        cx,
    );
    let local = mirror
        .with_authorized::<EditDraft, _>(actor, |auth| {
            auth.edit(|draft| draft.create_prompt(actor.as_uuid(), "unsent local item"))
        })
        .unwrap();
    mirror.try_rebase(welcome_for(&host, actor, 0), cx).unwrap();
    assert!(mirror.draft().contains(local));
    mirror.try_rebase(welcome_for(&host, actor, 1), cx).unwrap();
    assert!(!mirror.draft().contains(local));
    assert_eq!(mirror.draft_generation(), 1);
    mirror
        .with_authorized::<EditDraft, _>(actor, |auth| {
            auth.edit(|draft| draft.set_body(prompt, "fresh optimistic text"))
        })
        .unwrap();
    let repeated = protocol::HostMessage::DraftReset {
        generation: 1,
        state: host.draft().encode_state(),
    };
    mirror.try_apply(repeated, cx).unwrap();
    assert_eq!(
        mirror.draft().body(prompt).as_deref(),
        Some("fresh optimistic text")
    );
    mirror
        .try_apply(
            protocol::HostMessage::DraftReset {
                generation: 0,
                state: host.draft().encode_state(),
            },
            cx,
        )
        .unwrap();
    assert_eq!(mirror.draft_generation(), 1);
    assert_eq!(
        mirror.draft().body(prompt).as_deref(),
        Some("fresh optimistic text")
    );
}

#[test]
fn flushing_fresh_replica_stamps_its_current_epoch() {
    let (mut mirror, prompt) = host_with_prompt("host history");
    let actor = add_peer(&mut mirror);
    mirror.ownership = ThreadOwnership::Remote;
    mirror.participant_id = actor;
    mirror.draft.author = actor;
    let (host, requests) = async_channel::unbounded();
    let (uploads, _) = async_channel::unbounded();
    mirror.sharing = ThreadSharing::Connected {
        host,
        uploads,
        link: None,
    };
    let snapshot = mirror.draft().encode_state();
    mirror.install_draft_state(3, &snapshot).unwrap();
    mirror
        .with_authorized::<EditDraft, _>(actor, |auth| {
            auth.edit(|draft| draft.set_body(prompt, "new replica edit"))
        })
        .unwrap();
    let protocol::CollaboratorMessage::DraftUpdate { generation, update } =
        requests.try_recv().unwrap()
    else {
        panic!("draft update");
    };
    assert_eq!(generation, 3);
    assert!(!update.is_empty());
}

struct EditorTestView {
    item: gpui::Entity<gpui_base::input::TextareaState>,
    position: gpui::Entity<gpui_base::input::TextareaState>,
}

impl gpui::Render for EditorTestView {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
    }
}

#[gpui::test]
fn reset_invalidates_item_and_draft_position_editor_ids_but_retains_folding(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::theme::init(cx);
    });
    let (view, cx) = cx.add_window_view(|window, cx| EditorTestView {
        item: cx.new(|cx| gpui_base::input::TextareaState::new(window, cx)),
        position: cx.new(|cx| gpui_base::input::TextareaState::new(window, cx)),
    });
    let (old_item, old_position) =
        view.read_with(cx, |view, _| (view.item.clone(), view.position.clone()));
    cx.update(|_, _| {
        let actor = ParticipantId::new();
        let mut draft = ThreadDraft::new(actor);
        let prompt = draft.create_prompt(actor.as_uuid(), "accepted");
        let state = draft.encode_state();
        draft.update_draft_editors(|_, cache| {
            cache
                .editors
                .insert(prompt, ItemEditors::Prompt(old_item.clone()));
            cache
                .synced_text
                .insert(old_item.entity_id(), "rejected text".into());
            cache.draft_position = Some(old_position.clone());
            cache.comments_folded = true;
        });
        assert_eq!(
            draft.slot_of(old_position.entity_id()),
            Some(EditorSlot::DraftPosition)
        );
        draft.reset(&state).unwrap();
        assert_eq!(draft.slot_of(old_item.entity_id()), None);
        assert_eq!(draft.slot_of(old_position.entity_id()), None);
        assert!(draft.editors.is_empty());
        assert!(draft.synced_text.is_empty());
        assert!(draft.draft_position.is_none());
        assert!(draft.comments_folded);
        // The same slot lookup used by routed Change events cannot turn an
        // old draft-position event into a new prompt after Write is regranted.
        if draft.slot_of(old_position.entity_id()) == Some(EditorSlot::DraftPosition) {
            draft.create_prompt(actor.as_uuid(), "must not replay");
        }
        assert_eq!(draft.items().len(), 1);
        assert_eq!(draft.body(prompt).as_deref(), Some("accepted"));
    });
}
