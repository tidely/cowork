//! A host and a collaborator connected through the real protocol code:
//! the shared draft, submissions, presence, and profiles.

use super::*;

#[test]
fn copied_endpoint_id_is_accepted_by_join_input() {
    let endpoint_id = iroh::SecretKey::from_bytes(&[42; 32]).public();
    let copied_text = endpoint_id.to_string();

    assert!(endpoint_id_input_is_complete(&copied_text));
    assert_eq!(copied_text.parse::<EndpointId>().unwrap(), endpoint_id);
}

#[gpui::test]
fn collaborators_receive_the_draft_and_edit_it_live(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    assert_eq!(session.bodies(&collaborator_thread), ["from the host"]);
    let collaborator_id =
        collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);

    // The collaborator continues the host's block.
    let collaborator = session.collaborator.clone();
    session.focus(&collaborator);
    session.cx.simulate_input("!");
    session.wait_until("the host sees the collaborator's edit", |this| {
        this.bodies(&host_thread) == ["from the host!"]
    });

    // A block the collaborator creates is theirs.
    session.cx.simulate_keystrokes("down");
    session.cx.simulate_input("mine");
    session.wait_until("the host sees the collaborator's block", |this| {
        this.bodies(&host_thread).len() == 2
    });
    let items = session.items(&host_thread);
    assert_eq!(items[1].body, "mine");
    assert_eq!(items[1].creator, collaborator_id.as_uuid());

    // The host's edits appear in the collaborator's editors.
    let host = session.host.clone();
    let block = items[1].id;
    host.update(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        host.update_draft(draft_id, cx, |draft| {
            draft.doc.set_body(block, "mine, and the host's");
        });
    });
    session.wait_until("the collaborator's editor shows the host's edit", |this| {
        this.collaborator.read_with(this.cx, |collaborator, cx| {
            let thread = collaborator.active_thread(cx).expect("joined");
            thread
                .read(cx)
                .draft
                .editor(EditorSlot::Prompt(block))
                .is_some_and(|editor| editor.read(cx).value() == "mine, and the host's")
        })
    });
    assert_eq!(
        session.bodies(&collaborator_thread),
        session.bodies(&host_thread)
    );
}

#[gpui::test]
fn the_host_accepts_one_submission_per_sequence(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();

    let collaborator = session.collaborator.clone();
    session.cx.update(|window, cx| {
        collaborator.update(cx, |collaborator, cx| {
            collaborator.submit_composer(window, cx)
        });
    });
    session.wait_until("the collaborator sees the submission", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
    });
    assert!(session.items(&host_thread).is_empty());
    assert!(session.items(&collaborator_thread).is_empty());
    host_thread.read_with(session.cx, |thread, _| {
        let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
            panic!("expected the submitted message first");
        };
        assert_eq!(message.blocks[0].text, "from the host");
    });

    // The test's agent never answers, so end its run by hand.
    host_thread.update(session.cx, |thread, cx| {
        let id = thread.running_agent_message_id().expect("a running agent");
        thread.emit(
            protocol::HostMessage::AgentEnded {
                id: id.into_bytes(),
                failure: None,
                duration: Duration::ZERO,
            },
            cx,
        );
    });

    // A submission that raced the one just accepted is ignored, even
    // though the draft has new content by now.
    let host = session.host.clone();
    host.update(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        host.update_draft(draft_id, cx, |draft| {
            draft.doc.create_prompt(draft.author.as_uuid(), "later");
        });
    });
    session.wait_until("the collaborator sees the new block", |this| {
        this.bodies(&collaborator_thread) == ["later"]
    });
    collaborator_thread.read_with(session.cx, |thread, _| {
        thread.request(protocol::CollaboratorMessage::Submit { sequence: 0 });
    });
    session.settle();
    assert_eq!(
        host_thread.read_with(session.cx, |thread, _| thread.submission_count()),
        1
    );
    assert_eq!(session.bodies(&host_thread), ["later"]);

    // With the current sequence it goes through.
    collaborator_thread.read_with(session.cx, |thread, _| {
        thread.request(protocol::CollaboratorMessage::Submit { sequence: 1 });
    });
    session.wait_until("the second submission is accepted", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 2)
    });
    assert!(session.items(&host_thread).is_empty());
}

#[gpui::test]
fn concurrent_blocks_converge_to_one_order(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();

    // Both append before either hears of the other's block.
    let host = session.host.clone();
    host.update(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        host.update_draft(draft_id, cx, |draft| {
            draft.doc.create_prompt(draft.author.as_uuid(), "host");
        });
    });
    let collaborator = session.collaborator.clone();
    collaborator.update(session.cx, |collaborator, cx| {
        let draft_id = collaborator_thread.read(cx).draft.id;
        collaborator.update_draft(draft_id, cx, |draft| {
            draft
                .doc
                .create_prompt(draft.author.as_uuid(), "collaborator");
        });
    });

    session.wait_until("both have both blocks", |this| {
        this.items(&host_thread).len() == 3 && this.items(&collaborator_thread).len() == 3
    });
    assert_eq!(
        session.bodies(&host_thread),
        session.bodies(&collaborator_thread)
    );
}

#[gpui::test]
fn a_submitted_block_moves_its_typist_to_the_draft_position(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let collaborator = session.collaborator.clone();
    session.focus(&collaborator);
    let focused = |session: &mut Collaboration| {
        session.cx.update(|window, cx| {
            collaborator
                .read(cx)
                .focused_draft_editor(window, cx)
                .map(|(_, slot, _)| slot)
        })
    };
    assert!(matches!(focused(&mut session), Some(EditorSlot::Prompt(_))));

    collaborator_thread.read_with(session.cx, |thread, _| {
        thread.request(protocol::CollaboratorMessage::Submit { sequence: 0 });
    });
    session.wait_until("the submission arrives", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
    });
    session.settle();
    assert_eq!(focused(&mut session), Some(EditorSlot::DraftPosition));
}

#[test]
fn caret_labels_only_reappear_when_the_caret_moves() {
    let mut draft = ThreadDraft::new(ParticipantId::new());
    let other = ParticipantId::new();
    let at_draft_position = protocol::Presence {
        focus: Some(protocol::PresenceFocus::DraftPosition),
        ..Default::default()
    };
    draft.set_presence(other, at_draft_position.clone());
    let moved_at = draft.presence[&other].1;

    std::thread::sleep(Duration::from_millis(5));
    let mut reading = at_draft_position.clone();
    reading.pending_reads.push(protocol::PendingRead {
        id: [1; 16],
        name: "big.png".into(),
        is_image: true,
        progress: Some(10),
        block: None,
    });
    draft.set_presence(other, reading);
    assert_eq!(draft.presence[&other].1, moved_at);

    draft.set_presence(other, protocol::Presence::default());
    assert!(draft.presence[&other].1 > moved_at);
}

#[test]
fn empty_items_someone_else_is_in_are_kept() {
    let mut draft = ThreadDraft::new(ParticipantId::new());
    let attended = draft.doc.create_prompt(Uuid::new_v4(), "");
    let unattended = draft.doc.create_prompt(Uuid::new_v4(), "");
    let in_item = |id: ItemId| protocol::Presence {
        focus: Some(protocol::PresenceFocus::Item(id.as_uuid().into_bytes())),
        ..Default::default()
    };
    draft.set_presence(ParticipantId::new(), in_item(attended));
    // The local user's own presence does not count.
    draft.set_presence(draft.author, in_item(unattended));

    assert!(!draft.remove_if_unattended(attended));
    assert!(draft.remove_if_unattended(unattended));
    assert_eq!(
        draft
            .doc
            .items()
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        [attended]
    );
}

#[gpui::test]
fn rebasing_keeps_local_edits_the_host_has_not_seen(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let host_draft = Draft::new();
    host_draft.create_prompt(Uuid::new_v4(), "host");
    let welcome = |host_draft: &Draft| protocol::Welcome {
        participant_id: [3; 16],
        thread: protocol::ThreadSnapshot {
            id: [1; 16],
            title: "Shared".into(),
            participants: Vec::new(),
            profiles: Vec::new(),
            models: ModelCatalog::default(),
            model: None,
            context_tokens: None,
            streamed_bytes: 0,
            messages: Vec::new(),
        },
        draft: host_draft.encode_state(),
        presence: Vec::new(),
        stored_attachments: Vec::new(),
    };

    let thread = cx.new(|cx| {
        Thread::from_welcome(
            welcome(&host_draft),
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        )
    });
    thread.update(cx, |thread, cx| {
        let author = thread.draft.author.as_uuid();
        thread.draft.doc.create_prompt(author, "unsent");
        thread.apply(protocol::HostMessage::Welcome(welcome(&host_draft)), cx);
        let bodies = thread
            .draft
            .doc
            .items()
            .into_iter()
            .map(|item| item.body)
            .collect::<Vec<_>>();
        assert_eq!(bodies, ["host", "unsent"]);
    });
}

/// Typing into an editor that has not been drawn since someone else's
/// edit arrived must not revert that edit.
#[gpui::test]
fn keystrokes_merge_with_edits_the_editor_does_not_show_yet(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let host = session.host.clone();
    session.focus(&host);
    session.settle();
    let block = session.items(&host_thread)[0].id;

    // The collaborator's edit, as the host receives it.
    let collaborator_id =
        collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
    let remote = Draft::new();
    remote
        .apply_update(
            &host_thread.read_with(session.cx, |thread, _| thread.draft.doc.encode_state()),
        )
        .expect("copy the draft");
    remote.edit_body(
        block,
        &TextEdit {
            range: 0..0,
            insert: "X".into(),
        },
    );
    let update = remote.take_local_update().expect("an update");
    let editor = host.read_with(session.cx, |_, cx| {
        host_thread
            .read(cx)
            .draft
            .editor(EditorSlot::Prompt(block))
            .expect("host editor")
    });

    // Applied and typed over within one update, so no frame is drawn in
    // between that would let the editor catch up. Typing goes through
    // the input handler, as simulated input would draw first.
    session.cx.update(|window, cx| {
        host.update(cx, |host, cx| {
            host.collaborator_request(
                &host_thread,
                collaborator_id,
                protocol::CollaboratorMessage::DraftUpdate(update),
                cx,
            )
            .expect("a valid update");
        });
        assert_eq!(editor.read(cx).value(), "from the host");
        editor.update(cx, |editor, cx| {
            editor.replace_text_in_range(None, "!", window, cx);
        });
    });
    session.wait_until("both edits reach both sides", |this| {
        this.bodies(&host_thread) == ["Xfrom the host!"]
            && this.bodies(&collaborator_thread) == ["Xfrom the host!"]
    });
    host.read_with(session.cx, |host, cx| {
        let editor = host_thread
            .read(cx)
            .draft
            .editor(EditorSlot::Prompt(block))
            .expect("host editor");
        assert_eq!(editor.read(cx).value(), "Xfrom the host!");
        let _ = host;
    });
}

/// Draft updates and the submission travel on one ordered stream, so the
/// last keystroke before Ctrl-Enter is part of the submission.
#[gpui::test]
fn a_submission_includes_the_last_keystroke(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let host_thread = session.host_thread.clone();
    let collaborator = session.collaborator.clone();
    session.focus(&collaborator);
    session.settle();

    session.cx.simulate_input("?");
    session.cx.update(|window, cx| {
        collaborator.update(cx, |collaborator, cx| {
            collaborator.submit_composer(window, cx)
        });
    });
    session.wait_until("the host accepts the submission", |this| {
        host_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
    });
    host_thread.read_with(session.cx, |thread, _| {
        let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
            panic!("expected the submitted message first");
        };
        assert_eq!(message.blocks[0].text, "from the host?");
    });
}

#[gpui::test]
fn joining_a_thread_scrolls_to_the_bottom(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start_with_history(cx, true);
    session.settle();

    session
        .collaborator
        .read_with(session.cx, |collaborator, _| {
            let scroll = &collaborator.timeline_scroll_handle;
            assert!(collaborator.follow_generation);
            assert!(
                scroll.max_offset().y > px(500.),
                "history must overflow the timeline: {:?}",
                scroll.max_offset().y
            );
            assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        });
}

#[gpui::test]
fn a_joined_thread_closes_when_the_host_stops_sharing(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let host_thread = session.host_thread.clone();
    host_thread.update(session.cx, |thread, _| {
        thread.sharing = ThreadSharing::NotShared;
        thread.participants.clear();
    });
    session.wait_until("the collaborator's thread closes", |this| {
        this.collaborator.read_with(this.cx, |collaborator, cx| {
            collaborator.active_thread_id.is_none()
                && collaborator.thread_store.read(cx).threads.is_empty()
        })
    });
}

#[gpui::test]
fn the_host_sees_where_the_collaborator_is_typing(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let collaborator_id =
        collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
    let block = session.items(&host_thread)[0].id;

    let collaborator = session.collaborator.clone();
    session.focus(&collaborator);
    session.wait_until("the host sees the collaborator in the block", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread.draft.editors_of(block, &thread.participants) == [collaborator_id]
        })
    });
    let caret = host_thread.read_with(session.cx, |thread, _| {
        thread.draft.remote_carets(block, &thread.participants)
    });
    let end = "from the host".len();
    assert!(matches!(
        caret.as_slice(),
        [RemoteCaret { participant, head, selection, .. }]
            if *participant == collaborator_id && *head == end && *selection == (end..end)
    ));

    // The caret follows what the collaborator types, and the host's
    // composer lists them as an editor of the host's block.
    session.cx.simulate_input("!!");
    session.wait_until("the caret moves along", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread
                .draft
                .remote_carets(block, &thread.participants)
                .first()
                .is_some_and(|caret| caret.head == end + 2)
        })
    });
    let host = session.host.clone();
    let editors = session.cx.update(|window, cx| {
        let host = host.read(cx);
        let draft_id = host_thread.read(cx).draft.id;
        let model = host.composer_model(draft_id, window, cx).expect("composer");
        model.blocks[0].presence.editors.clone()
    });
    assert_eq!(editors, [collaborator_id]);

    // Leaving the draft clears the presence.
    collaborator.update(session.cx, |collaborator, cx| {
        collaborator.active_thread_id = None;
        cx.notify();
    });
    session.wait_until("the host sees the collaborator leave the block", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread
                .draft
                .editors_of(block, &thread.participants)
                .is_empty()
        })
    });
}

#[gpui::test]
fn the_host_sees_the_collaborators_selection(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let host_thread = session.host_thread.clone();
    let block = session.items(&host_thread)[0].id;
    let collaborator = session.collaborator.clone();
    session.focus(&collaborator);
    session.settle();
    session
        .cx
        .simulate_keystrokes("shift-left shift-left shift-left shift-left");
    let end = "from the host".len();
    session.wait_until("the host sees the selection", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread
                .draft
                .remote_carets(block, &thread.participants)
                .first()
                .is_some_and(|caret| caret.selection == (end - 4..end) && caret.head == end - 4)
        })
    });
}

#[test]
fn comment_groups_show_each_author_once_in_comment_order() {
    let (alice, bob) = (ParticipantId::new(), ParticipantId::new());
    let comment = |author| UserComment {
        id: Uuid::new_v4(),
        author,
        presence: ItemPresence::default(),
        reference: CommentReference {
            message_id: Uuid::new_v4(),
            range: 0..1,
            quote: "q".into(),
        },
        body: UserCommentBody::Submitted("c".into()),
    };
    assert_eq!(
        Cowork::comment_authors(&[comment(bob), comment(alice), comment(bob)]),
        [bob, alice]
    );
    assert!(Cowork::comment_authors(&[]).is_empty());
}

#[gpui::test]
fn an_empty_block_stays_while_someone_else_is_in_it(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let block = session.items(&host_thread)[0].id;
    let host = session.host.clone();
    host.update(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        host.update_draft(draft_id, cx, |draft| {
            draft.doc.set_body(block, "");
        });
    });
    let collaborator = session.collaborator.clone();
    session.focus(&collaborator);
    session.wait_until("the host sees the collaborator in the block", |this| {
        host_thread.read_with(this.cx, |thread, _| thread.draft.is_attended(block, None))
    });

    // The host cannot remove it while the collaborator is in it.
    let removed = host_thread.update(session.cx, |thread, _| {
        thread.draft.remove_if_unattended(block)
    });
    assert!(!removed);

    // The collaborator, although not its creator, removes it on leaving.
    session.cx.update(|window, cx| {
        collaborator.update(cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
        });
    });
    session.wait_until("the block is removed everywhere", |this| {
        this.items(&host_thread).is_empty() && this.items(&collaborator_thread).is_empty()
    });
}

#[gpui::test]
fn the_host_removes_the_empty_block_a_leaving_collaborator_was_in(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let collaborator = session.collaborator.clone();
    let block = collaborator.update(session.cx, |collaborator, cx| {
        let draft_id = collaborator_thread.read(cx).draft.id;
        collaborator
            .update_draft(draft_id, cx, |draft| {
                draft.doc.create_prompt(draft.author.as_uuid(), "")
            })
            .expect("draft")
    });
    session.cx.update(|window, cx| {
        collaborator.update(cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator.focus_draft_editor(draft_id, EditorSlot::Prompt(block), None, window, cx);
        });
    });
    session.wait_until("the host sees the collaborator in the block", |this| {
        host_thread.read_with(this.cx, |thread, _| thread.draft.is_attended(block, None))
    });

    // Closing the collaborator's end disconnects it.
    collaborator_thread.update(session.cx, |thread, _| {
        thread.sharing = ThreadSharing::NotShared;
    });
    session.wait_until("the host drops the collaborator and the block", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread.participants.len() == 1 && !thread.draft.doc.contains(block)
        })
    });
    assert_eq!(session.bodies(&host_thread), ["from the host"]);
}

/// Two people leaving an empty block at once each still see the other
/// in it; the host removes it once both are gone.
#[gpui::test]
fn the_host_removes_an_empty_block_everyone_left(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let block = session.items(&host_thread)[0].id;
    let collaborator_id =
        collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
    let host = session.host.clone();
    host.update(session.cx, |host, cx| {
        let draft_id = host_thread.read(cx).draft.id;
        host.update_draft(draft_id, cx, |draft| {
            draft.doc.set_body(block, "");
        });
    });
    let in_block = protocol::Presence {
        focus: Some(protocol::PresenceFocus::Item(block.as_uuid().into_bytes())),
        ..Default::default()
    };
    let host_id = host_thread.read_with(session.cx, |thread, _| thread.participant_id);
    host_thread.update(session.cx, |thread, cx| {
        thread.host_presence(host_id, in_block.clone(), cx);
        thread.host_presence(collaborator_id, in_block, cx);
        // The host leaves first: the collaborator is still there.
        thread.host_presence(host_id, protocol::Presence::default(), cx);
        assert!(thread.draft.doc.contains(block));
        thread.host_presence(collaborator_id, protocol::Presence::default(), cx);
        assert!(!thread.draft.doc.contains(block));
    });
    session.wait_until("the collaborator sees the block go", |this| {
        this.items(&collaborator_thread).is_empty()
    });
}

#[gpui::test]
fn sharing_again_announces_the_hosts_presence_again(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let host_thread = session.host_thread.clone();
    let host = session.host.clone();
    session.focus(&host);
    session.settle();
    let host_id = host_thread.read_with(session.cx, |thread, _| thread.participant_id);
    let announced = |session: &mut Collaboration| {
        host_thread.read_with(session.cx, |thread, _| {
            thread.draft.presence.contains_key(&host_id)
        })
    };
    assert!(announced(&mut session));

    host_thread.update(session.cx, |thread, _| {
        thread.draft.presence.clear();
        thread.sharing = ThreadSharing::NotShared;
    });
    session.settle();
    let endpoint = session
        ._runtime
        .block_on(Endpoint::builder(presets::Minimal).bind())
        .expect("bind endpoint");
    host_thread.update(session.cx, |thread, _| {
        thread.sharing = ThreadSharing::Shared {
            endpoint,
            events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
        };
    });
    session.settle();
    assert!(announced(&mut session));
}

#[gpui::test]
fn profiles_reach_everyone_in_the_thread(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    let (collaborator_id, host_id) = collaborator_thread.read_with(session.cx, |thread, _| {
        (thread.participant_id, thread.participants[0])
    });
    let shown_name = |thread: &Thread, participant| {
        participant_name(participant, thread.profiles.get(&participant)).to_string()
    };
    // Joined with the profile it had, which only carries its generated
    // name, the same one its own profile page shows.
    let generated_name = session
        .collaborator
        .read_with(session.cx, |collaborator, _| collaborator.profile_name());
    assert_eq!(
        host_thread.read_with(session.cx, |thread, _| shown_name(thread, collaborator_id)),
        generated_name.to_string()
    );

    let picture =
        Arc::new(profile_picture(&encoded_image(5, image::ImageFormat::Bmp)).expect("a picture"));
    let collaborator = session.collaborator.clone();
    session.cx.update(|_, cx| {
        collaborator.update(cx, |collaborator, cx| {
            collaborator.set_profile(
                Profile {
                    name: Some("Ada".into()),
                    picture: Some(picture.clone()),
                    ..collaborator.profile.clone()
                },
                cx,
            );
        });
    });
    session.wait_until("the host sees the collaborator's profile", |this| {
        host_thread.read_with(this.cx, |thread, _| {
            thread
                .profiles
                .get(&collaborator_id)
                .is_some_and(|profile| {
                    profile.name.as_deref() == Some("Ada")
                        && profile
                            .picture
                            .as_ref()
                            .is_some_and(|shown| shown.bytes() == picture.bytes())
                })
        })
    });

    let host = session.host.clone();
    session.cx.update(|_, cx| {
        host.update(cx, |host, cx| {
            host.set_profile(
                Profile {
                    name: Some("Grace".into()),
                    picture: None,
                    ..host.profile.clone()
                },
                cx,
            );
        });
    });
    session.wait_until("the collaborator sees the host's profile", |this| {
        collaborator_thread.read_with(this.cx, |thread, _| {
            shown_name(thread, host_id) == "Grace" && shown_name(thread, collaborator_id) == "Ada"
        })
    });

    // The agent is told the names people chose.
    let prompt_names = host.read_with(session.cx, |host, cx| {
        let profiles = host.profiles_for(Some(host_thread.read(cx)));
        (
            participant_name(host_id, profiles.get(&host_id)),
            participant_name(collaborator_id, profiles.get(&collaborator_id)),
        )
    });
    assert_eq!(prompt_names, ("Grace".into(), "Ada".into()));
}

#[gpui::test]
fn generated_profiles_look_the_same_in_every_thread(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let collaborator_id =
        collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
    let (host, collaborator) = (session.host.clone(), session.collaborator.clone());
    session.settle();

    let (local_id, own_name, own_view) = collaborator.read_with(session.cx, |collaborator, _| {
        (
            collaborator.local_participant_id,
            collaborator.profile_name(),
            (
                collaborator.name_of(collaborator_id),
                collaborator.color_of(collaborator_id),
            ),
        )
    });
    let host_view = host.read_with(session.cx, |host, _| {
        (
            host.name_of(collaborator_id),
            host.color_of(collaborator_id),
        )
    });

    // Not derived from the id the host assigned for this join.
    assert_eq!(own_name, SharedString::from(local_id.display_name()));
    assert_eq!(own_view, (own_name.clone(), local_id.color()));
    assert_eq!(host_view, own_view);
}

#[gpui::test]
fn invalid_profiles_disconnect_the_collaborator(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();

    collaborator_thread.read_with(session.cx, |thread, _| {
        thread.request(protocol::CollaboratorMessage::Profile(protocol::Profile {
            name: None,
            picture: Some(b"not a picture".to_vec()),
            appearance: None,
        }));
    });
    session.wait_until("the host drops the collaborator", |this| {
        host_thread.read_with(this.cx, |thread, _| thread.participants.len() == 1)
    });
}

#[gpui::test]
fn misattributed_draft_updates_disconnect_the_collaborator(cx: &mut gpui::TestAppContext) {
    let mut session = Collaboration::start(cx);
    let collaborator_thread = session.collaborator_thread().expect("joined");
    let host_thread = session.host_thread.clone();
    assert_eq!(
        host_thread.read_with(session.cx, |thread, _| thread.participants.len()),
        2
    );

    let forged = Draft::new();
    forged.create_prompt(Uuid::new_v4(), "not mine");
    let update = forged.take_local_update().expect("an update");
    collaborator_thread.read_with(session.cx, |thread, _| {
        thread.request(protocol::CollaboratorMessage::DraftUpdate(update));
    });
    session.wait_until("the host drops the collaborator", |this| {
        host_thread.read_with(this.cx, |thread, _| thread.participants.len() == 1)
    });
}
