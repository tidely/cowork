//! Access controls exercised through real host/mirror state and transport.

use super::*;
use crate::thread::{DenialReason, PermissionDenied, PermissionOperation};
use gpui_component::WindowExt as _;

fn click_access_control(session: &mut Collaboration<'_>, selector: &str) {
    let bounds = session
        .cx
        .debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
        .unwrap_or_else(|| panic!("peer access control {selector} should be rendered"));
    session
        .cx
        .simulate_click(bounds.center(), gpui::Modifiers::default());
    session.settle();
}

fn redraw_access_menu(session: &mut Collaboration<'_>) {
    for _ in 0..3 {
        session.cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        session.cx.run_until_parked();
    }
}

// PairRoot renders both apps in one window; hide the other thread page so
// duplicate titlebar selectors cannot resolve to the wrong app's controls.
fn show_host_access_controls(session: &mut Collaboration<'_>) {
    session.collaborator.update(session.cx, |cowork, cx| {
        cowork.main_stage = MainStage::Settings(SettingsPage::Profile);
        cx.notify();
    });
    let host = session.host.clone();
    session.focus(&host);
    redraw_access_menu(session);
}

fn assert_access_menu_open(session: &mut Collaboration<'_>) {
    assert!(session.cx.debug_bounds("thread-sharing-menu").is_some());
    assert!(!session.cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(session.cx.debug_bounds("close-peer-access").is_none());
    assert!(session.cx.debug_bounds("peer-access-settings").is_none());
}

fn click_access_mode(session: &mut Collaboration<'_>, selector: &str) {
    click_access_control(session, selector);
    assert_access_menu_open(session);
}

fn access_track_width(session: &mut Collaboration<'_>, id: &str) -> gpui::Pixels {
    let modes = ["Read only", "Write", "Admin"].map(|mode| {
        session
            .cx
            .debug_bounds(Box::leak(format!("{id}-{mode}").into_boxed_str()))
            .expect("mode pill should be rendered")
    });
    for pair in modes.windows(2) {
        assert_eq!(pair[0].size, pair[1].size, "icon pills have equal sizes");
        assert_eq!(pair[0].top(), pair[1].top());
        assert!(pair[0].right() <= pair[1].left());
    }
    if id != "default" {
        let reset = session
            .cx
            .debug_bounds(Box::leak(format!("{id}-inherit").into_boxed_str()))
            .expect("inheritance reset should be rendered");
        assert!(
            modes[2].right() <= reset.left(),
            "the reset belongs outside the three-mode track"
        );
    }
    modes[2].right() - modes[0].left()
}

#[gpui::test]
fn host_access_popover_changes_live_defaults_overrides_and_inheritance(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let actor = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    let host = session.host.clone();
    show_host_access_controls(&mut session);
    click_access_control(&mut session, "thread-menu-trigger");
    assert_access_menu_open(&mut session);

    // Resetting an already inheriting peer is disabled, not an explicit
    // override of the effective default.
    click_access_mode(&mut session, &format!("{}-inherit", actor.as_uuid()));
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread
            .peer_permissions()
            .override_for(actor)),
        None
    );
    click_access_mode(&mut session, "default-Write");
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::Write
    );

    click_access_mode(&mut session, &format!("{}-Admin", actor.as_uuid()));
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::Admin
    );
    click_access_mode(&mut session, "default-Read only");

    redraw_access_menu(&mut session);
    click_access_mode(&mut session, &format!("{}-inherit", actor.as_uuid()));
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(thread.peer_permissions().override_for(actor), None);
        assert_eq!(thread.local_mode(), PeerMode::ReadOnly);
    });
    click_access_mode(&mut session, "default-Admin");
    session.cx.simulate_keystrokes("escape");
    session.settle();

    // An Admin collaborator still cannot manage the host's peer policy.
    host.update(session.cx, |cowork, cx| {
        cowork.main_stage = MainStage::Settings(SettingsPage::Profile);
        cx.notify();
    });
    let collaborator = session.collaborator.clone();
    collaborator.update(session.cx, |cowork, cx| {
        cowork.main_stage = MainStage::Thread;
        cx.notify();
    });
    session.focus(&collaborator);
    redraw_access_menu(&mut session);
    assert!(
        session.cx.debug_bounds("model-picker").is_some(),
        "the collaborator's thread page is visible"
    );
    click_access_control(&mut session, "thread-menu-trigger");
    assert_access_menu_open(&mut session);
    for mode in ["Read only", "Write", "Admin"] {
        for id in ["default".to_owned(), actor.as_uuid().to_string()] {
            assert!(
                session
                    .cx
                    .debug_bounds(Box::leak(format!("{id}-{mode}").into_boxed_str()))
                    .is_none(),
                "an Admin collaborator must not have permission setters"
            );
        }
    }
    assert!(
        session
            .cx
            .debug_bounds(Box::leak(
                format!("{}-inherit", actor.as_uuid()).into_boxed_str()
            ))
            .is_none()
    );
    assert!(session.cx.debug_bounds("copy-endpoint-id").is_none());
    click_access_control(&mut session, "toggle-sharing");
    session.wait_until("disconnect removes the mirrored thread", |this| {
        this.collaborator_thread().is_none()
            && !this
                .host_thread
                .read_with(this.cx, |thread, _| thread.participants().contains(&actor))
    });
}

#[gpui::test]
fn open_access_popover_redraws_live_defaults_and_participant_rows(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let actor = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    let host_thread = session.host_thread.clone();
    show_host_access_controls(&mut session);
    click_access_control(&mut session, "thread-menu-trigger");
    assert_access_menu_open(&mut session);
    // The pinned GPUI test helper requires static selector strings.
    let inherit_selector: &'static str =
        Box::leak(format!("{}-inherit", actor.as_uuid()).into_boxed_str());

    // External policy changes redraw the open menu, but icon tracks and the
    // reset stay fixed-width rather than growing with the default's name.
    let peer_id = actor.as_uuid().to_string();
    let panel_width = session
        .cx
        .debug_bounds("thread-sharing-menu")
        .unwrap()
        .size
        .width;
    let default_width = access_track_width(&mut session, "default");
    let peer_width = access_track_width(&mut session, &peer_id);
    let reset_size = session.cx.debug_bounds(inherit_selector).unwrap().size;
    set_default(&mut session, PeerMode::ReadOnly);
    redraw_access_menu(&mut session);
    assert_access_menu_open(&mut session);
    assert_eq!(access_track_width(&mut session, "default"), default_width);
    assert_eq!(access_track_width(&mut session, &peer_id), peer_width);
    assert_eq!(
        session.cx.debug_bounds(inherit_selector).unwrap().size,
        reset_size
    );
    assert_eq!(
        session
            .cx
            .debug_bounds("thread-sharing-menu")
            .unwrap()
            .size
            .width,
        panel_width
    );

    let arrival = ParticipantId::new();
    let arrival_selector: &'static str =
        Box::leak(format!("{}-Read only", arrival.as_uuid()).into_boxed_str());
    assert!(session.cx.debug_bounds(arrival_selector).is_none());
    host_thread.update(session.cx, |thread, cx| {
        thread.emit_for_test(joined(arrival), cx);
        cx.notify();
    });
    session.wait_until(
        "the joined peer appears in the open access popover",
        |this| {
            mirror.read_with(this.cx, |thread, _| {
                thread.participants().contains(&arrival)
            }) && this.cx.debug_bounds(arrival_selector).is_some()
        },
    );
    redraw_access_menu(&mut session);
    assert_access_menu_open(&mut session);
    assert_eq!(
        access_track_width(&mut session, &arrival.as_uuid().to_string()),
        peer_width
    );

    host_thread.update(session.cx, |thread, cx| {
        thread.participant_left(arrival, cx);
        cx.notify();
    });
    session.wait_until(
        "the departed peer disappears from the open access popover",
        |this| {
            !mirror.read_with(this.cx, |thread, _| {
                thread.participants().contains(&arrival)
            }) && this.cx.debug_bounds(arrival_selector).is_none()
        },
    );
    assert_access_menu_open(&mut session);
    session.cx.simulate_keystrokes("escape");
    session.settle();
    assert!(session.cx.debug_bounds("thread-sharing-menu").is_none());
}

#[gpui::test]
fn access_popover_dismisses_outside_without_moving_layout_and_closes_on_thread_switch(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    show_host_access_controls(&mut session);
    let stable_bounds = ["top-bar-content", "bottom-bar", "composer"].map(|selector| {
        session
            .cx
            .debug_bounds(selector)
            .expect("thread page layout")
    });
    click_access_control(&mut session, "thread-menu-trigger");
    assert_access_menu_open(&mut session);
    for (selector, before) in ["top-bar-content", "bottom-bar", "composer"]
        .into_iter()
        .zip(stable_bounds)
    {
        assert_eq!(session.cx.debug_bounds(selector).unwrap(), before);
    }

    // Click the bare titlebar, outside both the trigger and its panel.
    // No modal backdrop should be needed to restore the thread page.
    let outside = point(
        stable_bounds[0].left() + px(40.),
        stable_bounds[0].top() + px(2.),
    );
    session
        .cx
        .simulate_click(outside, gpui::Modifiers::default());
    session.settle();
    assert!(session.cx.debug_bounds("thread-sharing-menu").is_none());
    assert!(!session.cx.update(|window, cx| window.has_active_dialog(cx)));

    click_access_control(&mut session, "thread-menu-trigger");
    assert_access_menu_open(&mut session);
    let host = session.host.clone();
    let original_id = session
        .host_thread
        .read_with(session.cx, |thread, _| thread.instance_id);
    let other_id = Uuid::new_v4();
    session.cx.update(|window, cx| {
        host.update(cx, |cowork, cx| {
            let other = cx.new(|_| {
                test_thread(
                    other_id,
                    Vec::new(),
                    ThreadDraft::new(cowork.local_participant_id),
                )
            });
            cowork
                .thread_store
                .update(cx, |store, _| store.threads.push_front(other));
            cowork.open_thread(other_id, window, cx);
        });
    });
    session.settle();
    assert!(session.cx.debug_bounds("thread-sharing-menu").is_none());
    assert!(session.cx.debug_bounds("thread-menu-trigger").is_some());
    assert!(session.cx.debug_bounds("copy-endpoint-id").is_none());

    // Switching back must not resurrect the original thread's open popover.
    session.cx.update(|window, cx| {
        host.update(cx, |cowork, cx| cowork.open_thread(original_id, window, cx));
    });
    session.settle();
    assert!(session.cx.debug_bounds("thread-sharing-menu").is_none());
    click_access_control(&mut session, "thread-menu-trigger");
    assert_access_menu_open(&mut session);
    assert!(session.cx.debug_bounds("copy-endpoint-id").is_some());
}

#[gpui::test]
fn stopping_sharing_from_the_menu_preserves_the_canonical_local_thread(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    show_host_access_controls(&mut session);
    let host_thread = session.host_thread.clone();
    let (thread_id, draft_id, draft_state, model, transcript) =
        host_thread.read_with(session.cx, |thread, _| {
            (
                thread.instance_id,
                thread.draft().id,
                thread.draft().encode_state(),
                thread.model().cloned(),
                thread.transcript.clone(),
            )
        });
    click_access_control(&mut session, "thread-menu-trigger");
    assert_access_menu_open(&mut session);
    click_access_control(&mut session, "toggle-sharing");
    session.wait_until("stopping sharing disconnects the mirror", |this| {
        this.collaborator_thread().is_none()
    });
    host_thread.read_with(session.cx, |thread, _| {
        assert!(matches!(
            thread.sharing.status(),
            crate::thread::SharingStatus::NotShared
        ));
        assert_eq!(thread.instance_id, thread_id);
        assert_eq!(thread.draft().id, draft_id);
        assert_eq!(thread.draft().encode_state(), draft_state);
        assert_eq!(thread.model().cloned(), model);
        assert_eq!(thread.transcript, transcript);
    });
    assert_eq!(
        session
            .host
            .read_with(session.cx, |cowork, _| cowork.active_thread_id),
        Some(thread_id)
    );
}

#[gpui::test]
fn denial_feedback_targets_its_origin_when_another_thread_is_active(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let origin = session.collaborator_thread().expect("joined");
    let collaborator = session.collaborator.clone();
    let (origin_id, origin_draft_id, actor) = origin.read_with(session.cx, |thread, _| {
        assert_ne!(
            thread.instance_id, thread.summary.id,
            "mirrors have distinct local and shared identities"
        );
        (
            thread.instance_id,
            thread.draft().id,
            thread.participant_id(),
        )
    });
    collaborator.update(session.cx, |cowork, cx| {
        let other_id = Uuid::new_v4();
        let other = cx.new(|_| {
            test_thread(
                other_id,
                Vec::new(),
                ThreadDraft::new(cowork.local_participant_id),
            )
        });
        let other_draft_id = other.read(cx).draft().id;
        let new_draft_id = cowork.new_thread_draft.id;
        cowork
            .thread_store
            .update(cx, |store, _| store.threads.push_front(other));
        cowork.active_thread_id = Some(other_id);
        for (draft_id, message) in [
            (origin_draft_id, "previous origin feedback"),
            (other_draft_id, "unrelated active-thread error"),
            (new_draft_id, "unrelated new-thread error"),
        ] {
            cowork
                .attachment_errors
                .push(crate::composer_attachments::AttachmentError {
                    draft_id,
                    message: message.into(),
                });
        }
        let denied = PermissionDenied {
            participant: actor.into_bytes(),
            operation: PermissionOperation::ChangeModel,
            reason: DenialReason::InsufficientMode,
        };
        cowork.show_permission_denied(origin_id, &denied, cx);
        assert_eq!(cowork.active_thread_id, Some(other_id));
        assert_eq!(
            cowork.attachment_errors.len(),
            3,
            "only the origin's prior feedback is replaced"
        );
        let feedback = |draft_id| {
            cowork
                .attachment_errors
                .iter()
                .find(|error| error.draft_id == draft_id)
                .expect("feedback for draft")
                .message
                .as_str()
        };
        assert_eq!(
            feedback(origin_draft_id),
            "You no longer have permission to change the model."
        );
        assert_eq!(feedback(other_draft_id), "unrelated active-thread error");
        assert_eq!(feedback(new_draft_id), "unrelated new-thread error");
    });
}

fn set_override(session: &mut Collaboration<'_>, mode: Option<PeerMode>) {
    let mirror = session.collaborator_thread().expect("joined");
    let actor = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    session.host_thread.update(session.cx, |thread, cx| {
        thread
            .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                auth.set_override(actor, mode, cx);
            })
            .expect("host manages access");
    });
    session.wait_until("the mirror receives its override", |this| {
        mirror.read_with(this.cx, |thread, _| {
            thread.peer_permissions().override_for(actor) == mode
        })
    });
}

#[gpui::test]
fn write_peers_cannot_submit_by_keyboard_or_direct_actions(cx: &mut gpui::TestAppContext) {
    // Collaborators join as Write, which may edit but not control runs.
    let mut session = Collaboration::start(cx);
    let collaborator = session.collaborator.clone();
    session.focus(&collaborator);
    session.cx.simulate_keystrokes("ctrl-enter");
    session.cx.update(|window, cx| {
        collaborator.update(cx, |cowork, cx| {
            cowork.submit_composer_action(&SubmitComposer, window, cx);
            cowork.submit_composer(window, cx);
            cowork.stop_generation(cx);
            cowork.select_model(recommended_qwen(), cx);
        });
    });
    session.settle();
    let host_thread = session.host_thread.clone();
    assert_eq!(session.bodies(&host_thread), ["from the host"]);
    session.host_thread.read_with(session.cx, |thread, _| {
        assert_eq!(thread.submission_count(), 0);
        assert!(!thread.is_generating());
        assert!(thread.transcript.entries().is_empty());
        assert_eq!(thread.model().cloned(), Some(ollama_qwen()));
    });
    collaborator.read_with(session.cx, |cowork, _| {
        assert_eq!(cowork.new_thread_model, None);
        assert!(cowork.active_generations.is_empty());
    });
}

#[gpui::test]
fn readonly_peers_keep_receiving_draft_edits(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    set_default(&mut session, PeerMode::ReadOnly);
    let block = session.items(&mirror)[0].id;
    session.host_thread.update(session.cx, |thread, _| {
        thread
            .with_authorized::<EditDraft, _>(thread.participant_id(), |auth| {
                auth.edit(|draft| draft.set_body(block, "still visible"))
            })
            .expect("host edit");
    });
    session.wait_until("readonly mirror sees host edits", |this| {
        this.bodies(&mirror) == ["still visible"]
    });
}

#[gpui::test]
fn revoked_optimistic_edits_reset_without_mutating_the_host_and_edits_can_resume(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let block = session.items(&mirror)[0].id;
    // Queue an optimistic edit, then revoke before the host processes it.
    mirror.update(session.cx, |thread, _| {
        thread
            .with_authorized::<EditDraft, _>(thread.participant_id(), |auth| {
                auth.edit(|draft| draft.set_body(block, "revoked optimistic edit"))
            })
            .expect("Write before revocation");
    });
    assert_eq!(session.bodies(&mirror), ["revoked optimistic edit"]);
    let before = host_thread.read_with(session.cx, |thread, _| thread.draft().encode_state());
    set_default(&mut session, PeerMode::ReadOnly);
    session.wait_until(
        "the rejected edit is reset to authoritative state",
        |this| this.bodies(&mirror) == ["from the host"],
    );
    assert_eq!(
        host_thread.read_with(session.cx, |thread, _| thread.draft().encode_state()),
        before
    );
    // A new client document must not depend on the rejected update's clock.
    set_default(&mut session, PeerMode::Write);
    mirror.update(session.cx, |thread, _| {
        thread
            .with_authorized::<EditDraft, _>(thread.participant_id(), |auth| {
                auth.edit(|draft| draft.set_body(block, "authorized after reset"))
            })
            .expect("Write after reset");
    });
    session.wait_until("editing converges after reset", |this| {
        this.bodies(&host_thread) == ["authorized after reset"]
            && this.bodies(&mirror) == ["authorized after reset"]
    });
}

#[gpui::test]
fn direct_cancel_only_aborts_the_run_it_names(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let actor = ParticipantId::new();
    let message_id = Uuid::new_v4();
    let cancelled = Arc::new(AtomicBool::new(false));
    cowork.update(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).expect("thread");
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(joined(actor), cx);
            thread
                .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                    auth.set_override(actor, Some(PeerMode::Admin), cx);
                })
                .expect("grant controls");
        });
        cowork.active_generations.insert(
            thread_id,
            ActiveGeneration::for_test(message_id, cancelled.clone()),
        );
        cowork.cancel_generation(thread_id, actor, Some(Uuid::new_v4()), cx);
        assert!(
            !cancelled.load(Ordering::Acquire),
            "stale stops do not cancel newer runs"
        );
        cowork.cancel_generation(thread_id, actor, Some(message_id), cx);
        assert!(cancelled.load(Ordering::Acquire));
    });
}

#[gpui::test]
fn unauthorized_wire_controls_do_not_mutate_state_or_disconnect_the_peer(
    cx: &mut gpui::TestAppContext,
) {
    // Collaborators join as Write, which may edit but not control runs.
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let message_id = Uuid::new_v4();
    let cancelled = Arc::new(AtomicBool::new(false));
    session.host.update(session.cx, |cowork, cx| {
        cowork.active_generations.insert(
            host_thread.read(cx).instance_id,
            ActiveGeneration::for_test(message_id, cancelled.clone()),
        );
    });
    let snapshot = |session: &mut Collaboration<'_>| {
        host_thread.read_with(session.cx, |thread, _| {
            (
                thread.draft().encode_state(),
                postcard::to_stdvec(&thread.to_protocol()).unwrap(),
            )
        })
    };
    let before = snapshot(&mut session);
    // Intentionally bypass the mirror's guards: the host must reject
    // commands from a malicious client as well as disabled UI actions.
    mirror.read_with(session.cx, |thread, _| {
        for request in [
            protocol::CollaboratorMessage::Submit { sequence: 0 },
            protocol::CollaboratorMessage::Queue { sequence: 0 },
            protocol::CollaboratorMessage::SelectModel(recommended_qwen()),
            protocol::CollaboratorMessage::Stop {
                message_id: message_id.into_bytes(),
            },
        ] {
            assert!(request_unchecked(thread, request));
        }
    });
    session.settle();
    assert!(!cancelled.load(Ordering::Acquire));
    assert_eq!(snapshot(&mut session), before);
    assert!(
        session.collaborator_thread().is_some(),
        "permission denial is not a protocol disconnect"
    );
    assert_eq!(session.bodies(&mirror), ["from the host"]);
}

#[gpui::test]
fn departed_peers_lose_their_overrides(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let actor = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    set_override(&mut session, Some(PeerMode::ReadOnly));
    mirror.update(session.cx, |thread, _| {
        thread.sharing = ThreadSharing::NotShared
    });
    let host_thread = session.host_thread.clone();
    session.wait_until("the host clears departed peer access", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            !thread.participants().contains(&actor)
                && thread.peer_permissions().override_for(actor).is_none()
        })
    });
}
