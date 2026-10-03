//! Access controls exercised through real host/mirror state and transport.

use super::*;
use crate::thread::{ChangeModel, DenialReason, PermissionDenied, PermissionOperation};
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
        cowork.main_stage = MainStage::Profile;
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
    assert!(mirror.read_with(session.cx, |thread, _| thread.can_edit_draft()));
    assert!(!mirror.read_with(session.cx, |thread, _| thread.can_control_generation()));

    click_access_mode(&mut session, &format!("{}-Admin", actor.as_uuid()));
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::Admin
    );
    click_access_mode(&mut session, "default-Read only");
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(thread.peer_permissions().default_mode(), PeerMode::ReadOnly);
        assert_eq!(
            thread.peer_permissions().override_for(actor),
            Some(PeerMode::Admin)
        );
        assert_eq!(thread.local_mode(), PeerMode::Admin);
    });

    redraw_access_menu(&mut session);
    click_access_mode(&mut session, &format!("{}-inherit", actor.as_uuid()));
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(thread.peer_permissions().override_for(actor), None);
        assert_eq!(thread.local_mode(), PeerMode::ReadOnly);
    });
    click_access_mode(&mut session, "default-Admin");
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::Admin
    );
    session.cx.simulate_keystrokes("escape");
    session.settle();
    assert!(session.cx.debug_bounds("thread-sharing-menu").is_none());
    assert!(!session.cx.update(|window, cx| window.has_active_dialog(cx)));

    // An Admin collaborator still cannot manage the host's peer policy.
    host.update(session.cx, |cowork, cx| {
        cowork.main_stage = MainStage::Profile;
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
    for mode in [
        PeerMode::Write,
        PeerMode::ReadOnly,
        PeerMode::Admin,
        PeerMode::ReadOnly,
    ] {
        set_default(&mut session, mode);
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
        assert_eq!(
            mirror.read_with(session.cx, |thread, _| thread.local_mode()),
            mode
        );
    }

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
    // Choosing the effective default explicitly still creates an override.
    click_access_mode(&mut session, arrival_selector);
    click_access_mode(&mut session, "default-Admin");
    mirror.read_with(session.cx, |thread, _| {
        assert_eq!(thread.local_mode(), PeerMode::Admin);
        assert_eq!(
            thread.peer_permissions().override_for(arrival),
            Some(PeerMode::ReadOnly)
        );
        assert_eq!(
            thread.peer_permissions().mode_for(arrival),
            PeerMode::ReadOnly
        );
    });

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
    redraw_access_menu(&mut session);
    assert_eq!(access_track_width(&mut session, &peer_id), peer_width);
    assert_eq!(
        session
            .cx
            .debug_bounds("thread-sharing-menu")
            .unwrap()
            .size
            .width,
        panel_width
    );
    assert!(session.cx.debug_bounds(inherit_selector).is_some());
    assert!(
        session
            .cx
            .debug_bounds(Box::leak(
                format!("{}-inherit", arrival.as_uuid()).into_boxed_str()
            ))
            .is_none()
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
    for (selector, before) in ["top-bar-content", "bottom-bar", "composer"]
        .into_iter()
        .zip(stable_bounds)
    {
        assert_eq!(session.cx.debug_bounds(selector).unwrap(), before);
    }

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
    let mirror = session.collaborator_thread().expect("joined");
    let actor = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    show_host_access_controls(&mut session);
    set_default(&mut session, PeerMode::Write);
    set_override(&mut session, Some(PeerMode::ReadOnly));
    let host_thread = session.host_thread.clone();
    let (thread_id, draft_id, draft_state, model, transcript) =
        host_thread.read_with(session.cx, |thread, _| {
            (
                thread.instance_id,
                thread.draft().id,
                thread.draft().encode_state(),
                thread.model().cloned(),
                serde_json::to_string(&thread.transcript).expect("serialize transcript"),
            )
        });
    assert!(session.cx.debug_bounds("toggle-sharing").is_none());
    assert!(session.cx.debug_bounds("copy-endpoint-id").is_none());
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
        assert!(thread.is_host());
        assert_eq!(thread.local_mode(), PeerMode::Admin);
        assert!(thread.participants().is_empty());
        assert!(thread.draft().presence.is_empty());
        assert_eq!(thread.peer_permissions().default_mode(), PeerMode::Write);
        assert_eq!(thread.peer_permissions().override_for(actor), None);
        assert_eq!(thread.instance_id, thread_id);
        assert_eq!(thread.draft().id, draft_id);
        assert_eq!(thread.draft().encode_state(), draft_state);
        assert_eq!(thread.model().cloned(), model);
        assert_eq!(
            serde_json::to_string(&thread.transcript).unwrap(),
            transcript
        );
    });
    assert_eq!(
        session
            .host
            .read_with(session.cx, |cowork, _| cowork.active_thread_id),
        Some(thread_id)
    );
    assert!(!session.cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(session.cx.debug_bounds("copy-endpoint-id").is_none());
    assert!(session.cx.debug_bounds("participants").is_none());
    assert!(session.cx.debug_bounds("composer").is_some());
}

#[gpui::test]
fn denial_feedback_targets_its_origin_when_another_thread_is_active(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let origin = session.collaborator_thread().expect("joined");
    let collaborator = session.collaborator.clone();
    let (origin_id, origin_draft_id, actor, before) = origin.read_with(session.cx, |thread, _| {
        assert_ne!(
            thread.instance_id, thread.summary.id,
            "mirrors have distinct local and shared identities"
        );
        (
            thread.instance_id,
            thread.draft().id,
            thread.participant_id(),
            thread.draft().encode_state(),
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
        for (operation, reason, expected) in [
            (
                PermissionOperation::ChangeModel,
                DenialReason::InsufficientMode,
                "You no longer have permission to change the model.",
            ),
            (
                PermissionOperation::EditDraft,
                DenialReason::StaleDraftGeneration,
                "The host reset your draft. Edits queued before the reset were discarded.",
            ),
        ] {
            let denied = PermissionDenied {
                participant: actor.into_bytes(),
                operation,
                reason,
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
            assert_eq!(feedback(origin_draft_id), expected);
            assert_eq!(feedback(other_draft_id), "unrelated active-thread error");
            assert_eq!(feedback(new_draft_id), "unrelated new-thread error");
        }
        assert_eq!(origin.read(cx).draft().encode_state(), before);
    });
}

fn set_default(session: &mut Collaboration<'_>, mode: PeerMode) {
    session.host_thread.update(session.cx, |thread, cx| {
        thread
            .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                auth.set_default_mode(mode, cx);
            })
            .expect("host manages access");
    });
    session.wait_until("the mirror receives the default mode", |this| {
        let mirror = this.collaborator_thread().expect("joined");
        mirror.read_with(this.cx, |thread, _| {
            thread.peer_permissions().default_mode() == mode
        })
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
fn modes_separate_edit_generation_and_model_authority(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    for mode in [PeerMode::ReadOnly, PeerMode::Write, PeerMode::Admin] {
        set_default(&mut session, mode);
        mirror.update(session.cx, |thread, cx| {
            assert_eq!(thread.local_mode(), mode);
            assert_eq!(thread.can_edit_draft(), mode != PeerMode::ReadOnly);
            assert_eq!(thread.can_control_generation(), mode == PeerMode::Admin);
            assert_eq!(thread.can_change_model(), mode == PeerMode::Admin);
            let actor = thread.participant_id();
            assert_eq!(
                thread
                    .with_authorized::<EditDraft, _>(actor, |_| ())
                    .is_ok(),
                mode != PeerMode::ReadOnly
            );
            assert_eq!(
                thread
                    .with_authorized::<ControlGeneration, _>(actor, |_| ())
                    .is_ok(),
                mode == PeerMode::Admin
            );
            assert_eq!(
                thread.select_model(ollama_qwen(), cx).is_ok(),
                mode == PeerMode::Admin
            );
            let denial = thread
                .with_authorized::<ManageAccess, _>(actor, |_| ())
                .expect_err("even Admin cannot manage access");
            assert_eq!(denial.reason, DenialReason::HostOnly);
        });
        session.host_thread.update(session.cx, |thread, _| {
            let actor = thread.participant_id();
            assert_eq!(thread.local_mode(), PeerMode::Admin);
            assert!(
                thread
                    .with_authorized::<EditDraft, _>(actor, |_| ())
                    .is_ok()
            );
            assert!(
                thread
                    .with_authorized::<ControlGeneration, _>(actor, |_| ())
                    .is_ok()
            );
            assert!(
                thread
                    .with_authorized::<ChangeModel, _>(actor, |_| ())
                    .is_ok()
            );
        });
    }
}

#[gpui::test]
fn overrides_inherit_the_live_default_and_explicit_equal_modes_survive_changes(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let actor = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::Admin
    );
    set_override(&mut session, Some(PeerMode::Admin));
    set_default(&mut session, PeerMode::ReadOnly);
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::Admin
    );
    set_override(&mut session, None);
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::ReadOnly
    );
    set_default(&mut session, PeerMode::Write);
    assert_eq!(
        mirror.read_with(session.cx, |thread, _| thread.local_mode()),
        PeerMode::Write
    );
    session.host_thread.read_with(session.cx, |thread, _| {
        assert_eq!(thread.peer_permissions().mode_for(actor), PeerMode::Write);
        assert_eq!(thread.peer_permissions().override_for(actor), None);
        assert_eq!(thread.local_mode(), PeerMode::Admin);
        assert_eq!(
            thread.to_protocol().peer_permissions,
            *thread.peer_permissions()
        );
    });
}

#[gpui::test]
fn readonly_and_write_cannot_submit_by_keyboard_or_direct_actions(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let collaborator = session.collaborator.clone();
    for mode in [PeerMode::Write, PeerMode::ReadOnly] {
        set_default(&mut session, mode);
        session.focus(&collaborator);
        session.cx.simulate_keystrokes("ctrl-enter");
        session.cx.update(|window, cx| {
            collaborator.update(cx, |cowork, cx| {
                cowork.submit_composer_action(&SubmitComposer, window, cx);
                cowork.submit_composer(window, cx);
                cowork.stop_generation(cx);
                cowork.select_model(recommended_qwen(), cx);
                if mode == PeerMode::ReadOnly {
                    let draft_id = mirror.read(cx).draft().id;
                    assert!(
                        cowork
                            .update_draft(draft_id, cx, |draft| {
                                draft.create_prompt(draft.author.as_uuid(), "denied direct edit")
                            })
                            .is_none()
                    );
                }
            });
        });
        session.settle();
        let host_thread = session.host_thread.clone();
        assert_eq!(session.bodies(&host_thread), ["from the host"]);
        session.host_thread.read_with(session.cx, |thread, _| {
            assert_eq!(thread.submission_count(), 0);
            assert!(!thread.generating);
            assert!(thread.transcript.is_empty());
            assert_eq!(thread.model().cloned(), Some(ollama_qwen()));
        });
        collaborator.read_with(session.cx, |cowork, _| {
            assert_eq!(cowork.new_thread_model, None);
            assert!(cowork.active_generations.is_empty());
        });
    }
    // Direct calls still edit in Write mode; UI gating is not the authority.
    set_default(&mut session, PeerMode::Write);
    let block = session.items(&mirror)[0].id;
    collaborator.update(session.cx, |cowork, cx| {
        let draft_id = mirror.read(cx).draft().id;
        assert!(
            cowork
                .update_draft(draft_id, cx, |draft| {
                    draft.set_body(block, "written without Admin")
                })
                .is_some()
        );
    });
    let host_thread = session.host_thread.clone();
    session.wait_until("Write edits reach the host", |this| {
        this.bodies(&host_thread) == ["written without Admin"]
    });
}

#[gpui::test]
fn host_acceptance_checks_the_remote_actor_not_the_local_host_mode(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let actor = mirror.read_with(session.cx, |thread, _| thread.participant_id());
    let host_thread = session.host_thread.clone();
    let host = session.host.clone();
    for mode in [PeerMode::ReadOnly, PeerMode::Write] {
        set_override(&mut session, Some(mode));
        let before = host_thread.read_with(session.cx, |thread, _| {
            (
                thread.draft().encode_state(),
                postcard::to_stdvec(&thread.to_protocol()).unwrap(),
            )
        });
        host.update(session.cx, |cowork, cx| {
            let draft_id = host_thread.read(cx).draft().id;
            assert!(!cowork.accept_submission(
                draft_id,
                Some(host_thread.clone()),
                actor,
                false,
                cx
            ));
        });
        let after = host_thread.read_with(session.cx, |thread, _| {
            (
                thread.draft().encode_state(),
                postcard::to_stdvec(&thread.to_protocol()).unwrap(),
            )
        });
        assert_eq!(
            before, after,
            "denied acceptance must not consume the draft"
        );
    }
    set_override(&mut session, Some(PeerMode::Admin));
    host.update(session.cx, |cowork, cx| {
        let draft_id = host_thread.read(cx).draft().id;
        assert!(cowork.accept_submission(draft_id, Some(host_thread.clone()), actor, false, cx));
    });
    assert!(session.items(&host_thread).is_empty());
    session.wait_until("authorized submission reaches the mirror", |this| {
        mirror.read_with(this.cx, |thread, _| {
            thread.submission_count() == 1 && thread.generating
        })
    });
}

#[gpui::test]
fn readonly_peers_keep_receiving_drafts_models_and_agent_output(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    set_default(&mut session, PeerMode::ReadOnly);
    let block = session.items(&mirror)[0].id;
    let message_id = Uuid::new_v4();
    session.host_thread.update(session.cx, |thread, cx| {
        thread
            .with_authorized::<EditDraft, _>(thread.participant_id(), |auth| {
                auth.edit(|draft| draft.set_body(block, "still visible"))
            })
            .expect("host edit");
        thread
            .select_model(recommended_qwen(), cx)
            .expect("host model selection");
        thread.emit_for_test(agent_started(message_id, None, "visible prompt"), cx);
        let mut events = streamed_block(
            "answer",
            rig::streaming::BlockKind::Text {
                additional_params: None,
            },
            [rig::streaming::Delta::Text {
                text: "visible answer".into(),
            }],
            rig::streaming::BlockClose::Text,
        );
        events.push(turn_ended(42));
        for event in agent::test_support::canonical(events) {
            thread.emit_for_test(agent_event(message_id, event), cx);
        }
        thread.emit_for_test(
            protocol::HostMessage::AgentEnded {
                id: message_id.into_bytes(),
                outcome: protocol::RunOutcome::Completed,
                duration: Duration::from_secs(1),
            },
            cx,
        );
    });
    let host_thread = session.host_thread.clone();
    session.wait_until("readonly mirror sees all host changes", |this| {
        this.bodies(&mirror) == ["still visible"]
            && mirror.read_with(this.cx, |thread, _| {
                thread.model().cloned() == Some(recommended_qwen()) && thread.transcript.len() == 2
            })
    });
    mirror.read_with(session.cx, |thread, cx| {
        assert_eq!(thread.local_mode(), PeerMode::ReadOnly);
        assert_eq!(thread.conversation(), host_thread.read(cx).conversation());
        assert_eq!(thread.context_tokens, Some(42));
        let TimelineMessage::Agent(message) = &thread.timeline[0] else {
            panic!("agent message");
        };
        assert_eq!(message.output.text, "visible answer");
        assert!(!message.is_generating());
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
            .expect("Admin before revocation");
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
    mirror.update(session.cx, |thread, _| {
        let denied = thread
            .with_authorized::<EditDraft, _>(thread.participant_id(), |auth| {
                auth.edit(|draft| draft.set_body(block, "must not run"))
            })
            .expect_err("revoked editing is checked again");
        assert_eq!(denied.operation, PermissionOperation::EditDraft);
    });
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
fn queued_old_generation_edits_stay_rejected_after_write_is_regranted(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let (actor, old_generation, initial_state) = mirror.read_with(session.cx, |thread, _| {
        (
            thread.participant_id(),
            thread.draft_generation(),
            thread.draft().encode_state(),
        )
    });
    let block = session.items(&mirror)[0].id;
    let queued = Draft::new();
    queued
        .apply_update(&initial_state)
        .expect("copy the pre-revocation replica");
    queued.set_body(block, "first revoked edit");
    let first = queued.take_local_update().expect("first queued update");
    queued.set_body(block, "later queued edit");
    let later = queued.take_local_update().expect("dependent queued update");

    set_default(&mut session, PeerMode::ReadOnly);
    mirror.read_with(session.cx, |thread, _| {
        assert!(request_unchecked(
            thread,
            protocol::CollaboratorMessage::DraftUpdate {
                generation: old_generation,
                update: first,
            }
        ));
    });
    session.wait_until("the rejected replica receives a new generation", |this| {
        mirror.read_with(this.cx, |thread, _| {
            thread.draft_generation() == old_generation + 1
        }) && host_thread.read_with(this.cx, |thread, _| {
            thread.draft_generation_for(actor) == old_generation + 1
        })
    });
    set_default(&mut session, PeerMode::Write);
    mirror.read_with(session.cx, |thread, _| {
        let stale = protocol::CollaboratorMessage::DraftUpdate {
            generation: old_generation,
            update: later,
        };
        assert!(
            !thread.request(stale.clone()),
            "the mirror rejects old epochs too"
        );
        assert!(request_unchecked(thread, stale));
    });
    session.settle();
    assert!(session.collaborator_thread().is_some());
    assert_eq!(session.bodies(&host_thread), ["from the host"]);
    assert_eq!(session.bodies(&mirror), ["from the host"]);
    host_thread.read_with(session.cx, |thread, _| {
        assert_eq!(
            thread.draft_generation_for(actor),
            old_generation + 1,
            "stale queued bytes must not keep advancing the epoch"
        );
        assert_eq!(thread.draft().encode_state(), initial_state);
    });
    mirror.update(session.cx, |thread, _| {
        thread
            .with_authorized::<EditDraft, _>(thread.participant_id(), |auth| {
                auth.edit(|draft| draft.set_body(block, "fresh edit after regrant"))
            })
            .expect("Write edits use the replacement replica");
    });
    session.wait_until(
        "fresh edits converge after an old-epoch rejection",
        |this| {
            this.bodies(&host_thread) == ["fresh edit after regrant"]
                && this.bodies(&mirror) == ["fresh edit after regrant"]
        },
    );
}

#[gpui::test]
fn matching_resets_merge_fresh_edits_but_newer_resets_replace_rejected_history(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let connected = session.collaborator_thread().expect("joined");
    let actor = connected.read_with(session.cx, |thread, _| thread.participant_id());
    let welcome = session
        .host_thread
        .read_with(session.cx, |thread, _| protocol::Welcome {
            participant_id: actor.into_bytes(),
            thread: thread.to_protocol(),
            draft: thread.draft().encode_state(),
            draft_generation: thread.draft_generation_for(actor),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        });
    // No transport: these edits remain unsent while snapshots arrive. The
    // mirror is still built from the actual session's authoritative Welcome.
    let mirror = session.cx.new(|cx| {
        Thread::from_welcome(
            welcome.clone(),
            ThreadDraft::new(actor),
            ThreadSharing::NotShared,
            cx,
        )
    });
    let block = session.items(&mirror)[0].id;
    let editor = session
        .cx
        .update(|window, cx| Cowork::new_draft_editor("from the host", window, cx));
    mirror.update(session.cx, |thread, cx| {
        thread.update_draft_editors(|_, cache| {
            cache.editors.insert(
                block,
                crate::thread_draft::ItemEditors::Prompt(editor.clone()),
            );
            cache
                .synced_text
                .insert(editor.entity_id(), "from the host".into());
        });
        let rejected = thread
            .with_authorized::<EditDraft, _>(actor, |auth| {
                auth.edit(|draft| draft.create_prompt(actor.as_uuid(), "unsent item"))
            })
            .expect("authorized local edit");
        thread.apply_for_test(
            protocol::HostMessage::DraftReset {
                generation: welcome.draft_generation,
                state: welcome.draft.clone(),
            },
            cx,
        );
        assert!(
            thread.draft().contains(rejected),
            "matching epochs retain unsent edits"
        );
        assert_eq!(
            thread
                .draft()
                .editor(EditorSlot::Prompt(block))
                .unwrap()
                .entity_id(),
            editor.entity_id()
        );

        thread.apply_for_test(
            protocol::HostMessage::DraftReset {
                generation: welcome.draft_generation + 1,
                state: welcome.draft.clone(),
            },
            cx,
        );
        assert_eq!(thread.draft_generation(), welcome.draft_generation + 1);
        assert!(
            !thread.draft().contains(rejected),
            "a new epoch discards rejected CRDT history"
        );
        assert!(thread.draft().editor(EditorSlot::Prompt(block)).is_none());
        assert!(thread.draft().synced_text.is_empty());
        thread
            .with_authorized::<EditDraft, _>(actor, |auth| {
                auth.edit(|draft| draft.set_body(block, "fresh in replacement replica"))
            })
            .expect("editing resumes in the fresh document");
        for generation in [welcome.draft_generation + 1, welcome.draft_generation] {
            thread.apply_for_test(
                protocol::HostMessage::DraftReset {
                    generation,
                    state: welcome.draft.clone(),
                },
                cx,
            );
            assert_eq!(
                thread.draft().body(block).as_deref(),
                Some("fresh in replacement replica")
            );
            assert_eq!(thread.draft_generation(), welcome.draft_generation + 1);
        }
    });
}

#[gpui::test]
fn newer_welcome_replaces_optimistic_history_instead_of_merging_it(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let connected = session.collaborator_thread().expect("joined");
    let actor = connected.read_with(session.cx, |thread, _| thread.participant_id());
    let mut welcome = session
        .host_thread
        .read_with(session.cx, |thread, _| protocol::Welcome {
            participant_id: actor.into_bytes(),
            thread: thread.to_protocol(),
            draft: thread.draft().encode_state(),
            draft_generation: thread.draft_generation_for(actor),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        });
    let mirror = session.cx.new(|cx| {
        Thread::from_welcome(
            welcome.clone(),
            ThreadDraft::new(actor),
            ThreadSharing::NotShared,
            cx,
        )
    });
    mirror.update(session.cx, |thread, cx| {
        let optimistic = thread
            .with_authorized::<EditDraft, _>(actor, |auth| {
                auth.edit(|draft| {
                    draft.create_prompt(actor.as_uuid(), "must not survive the epoch change")
                })
            })
            .expect("optimistic edit");
        welcome.draft_generation += 1;
        thread
            .try_apply_for_test(
                protocol::HostMessage::Welcome(Box::new(welcome.clone())),
                cx,
            )
            .expect("a valid new-generation snapshot");
        assert_eq!(thread.draft_generation(), welcome.draft_generation);
        assert!(!thread.draft().contains(optimistic));
        assert_eq!(thread.draft().items().len(), 1);
        assert_eq!(thread.draft().items()[0].body, "from the host");
    });
}

#[gpui::test]
fn direct_cancel_rechecks_current_actor_permissions_before_aborting(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let actor = ParticipantId::new();
    let message_id = Uuid::new_v4();
    let task = runtime.spawn(std::future::pending::<()>());
    let cancelled = Arc::new(AtomicBool::new(false));
    cowork.update(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).expect("thread");
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(joined(actor), cx);
            thread
                .with_authorized::<ControlGeneration, _>(actor, |_| ())
                .expect("initial Admin");
            thread
                .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                    auth.set_override(actor, Some(PeerMode::Write), cx);
                })
                .expect("revoke controls");
        });
        cowork.active_generations.insert(
            thread_id,
            ActiveGeneration::for_test(message_id, task.abort_handle(), cancelled.clone()),
        );
        cowork.cancel_generation(thread_id, actor, Some(message_id), cx);
        assert!(!cancelled.load(Ordering::Acquire));
        cowork.cancel_generation(thread_id, ParticipantId::new(), None, cx);
        assert!(!cancelled.load(Ordering::Acquire));
        thread.update(cx, |thread, cx| {
            thread
                .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                    auth.set_override(actor, Some(PeerMode::Admin), cx);
                })
                .expect("restore controls");
        });
        cowork.cancel_generation(thread_id, actor, Some(Uuid::new_v4()), cx);
        assert!(
            !cancelled.load(Ordering::Acquire),
            "stale stops still do not cancel newer runs"
        );
        cowork.cancel_generation(thread_id, actor, Some(message_id), cx);
        assert!(cancelled.load(Ordering::Acquire));
    });
    assert!(
        runtime
            .block_on(task)
            .expect_err("authorized abort")
            .is_cancelled()
    );
}

#[gpui::test]
fn unauthorized_wire_controls_do_not_mutate_state_or_disconnect_the_peer(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Collaboration::start(cx);
    let mirror = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let message_id = Uuid::new_v4();
    let task = session._runtime.spawn(std::future::pending::<()>());
    let cancelled = Arc::new(AtomicBool::new(false));
    session.host.update(session.cx, |cowork, cx| {
        cowork.active_generations.insert(
            host_thread.read(cx).instance_id,
            ActiveGeneration::for_test(message_id, task.abort_handle(), cancelled.clone()),
        );
    });
    for mode in [PeerMode::Write, PeerMode::ReadOnly] {
        set_default(&mut session, mode);
        let before = host_thread.read_with(session.cx, |thread, _| {
            (
                thread.draft().encode_state(),
                postcard::to_stdvec(&thread.to_protocol()).unwrap(),
            )
        });
        // Intentionally bypass the mirror's guards: the host must reject
        // commands from a malicious client as well as disabled UI actions.
        mirror.read_with(session.cx, |thread, _| {
            for request in [
                protocol::CollaboratorMessage::Submit { sequence: 0 },
                protocol::CollaboratorMessage::SelectModel(recommended_qwen()),
                protocol::CollaboratorMessage::Stop {
                    message_id: message_id.into_bytes(),
                },
            ] {
                assert!(
                    !thread.request(request.clone()),
                    "local guards reject the command too"
                );
                assert!(request_unchecked(thread, request));
            }
        });
        session.settle();
        assert!(!cancelled.load(Ordering::Acquire));
        assert_eq!(
            host_thread.read_with(session.cx, |thread, _| {
                (
                    thread.draft().encode_state(),
                    postcard::to_stdvec(&thread.to_protocol()).unwrap(),
                )
            }),
            before
        );
        assert!(
            session.collaborator_thread().is_some(),
            "permission denial is not a protocol disconnect"
        );
        assert_eq!(session.bodies(&mirror), ["from the host"]);
    }
    task.abort();
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
    host_thread.update(session.cx, |thread, _| {
        let denial = thread
            .with_authorized::<EditDraft, _>(actor, |_| ())
            .expect_err("departed actor is not a participant");
        assert_eq!(denial.reason, DenialReason::NotParticipant);
    });
}

#[gpui::test]
fn attachment_completion_is_bounded_to_records_accepted_before_revocation(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(gpui_component::init);
    let actor = ParticipantId::new();
    let host = cx.new(|_| {
        test_thread(
            Uuid::new_v4(),
            Vec::new(),
            ThreadDraft::new(ParticipantId::new()),
        )
    });
    let mut upload = ThreadDraft::new(actor);
    let block = upload.create_prompt(actor.as_uuid(), "attachment");
    let text = "x".repeat(protocol::ATTACHMENT_CHUNK_SIZE + 1);
    let file = text_attachment("accepted.txt", &text);
    let record = AttachmentRecord {
        id: AttachmentId::new(),
        name: file.name.clone(),
        kind: file.kind(),
        size: file.len(),
        creator: actor.as_uuid(),
    };
    upload.add_attachment(block, record.clone());
    upload.files.insert(record.id, file);
    host.update(cx, |thread, cx| {
        thread.apply_for_test(joined(actor), cx);
        thread
            .apply_collaborator_update_for_test(
                actor,
                thread.draft_generation_for(actor),
                upload.encode_state(),
            )
            .expect("accepted record");
        thread
            .receive_upload(actor, upload.chunk(record.id, 0).expect("first chunk"))
            .expect("accepted bytes");
        thread
            .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                auth.set_override(actor, Some(PeerMode::ReadOnly), cx);
            })
            .expect("revoke editing");
        thread
            .receive_upload(
                actor,
                upload
                    .chunk(record.id, protocol::ATTACHMENT_CHUNK_SIZE as u64)
                    .expect("last chunk"),
            )
            .expect("already accepted transfer can complete");
        assert!(thread.draft().stored.contains(&record.id));
        assert_eq!(thread.draft().files[&record.id].len(), record.size);
        let new_id = AttachmentId::new();
        upload
            .files
            .insert(new_id, text_attachment("not accepted.txt", "new bytes"));
        let denied = thread
            .receive_upload(actor, upload.chunk(new_id, 0).expect("unaccepted bytes"))
            .expect_err("revocation disallows a new transfer");
        assert!(denied.downcast_ref::<PermissionDenied>().is_some());
        assert!(!thread.draft().incoming.contains_key(&new_id));
        assert!(!thread.draft().files.contains_key(&new_id));
    });
}
