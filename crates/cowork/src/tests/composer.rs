//! The composer: its editors, moving between them, and submitting.

use super::*;

struct ComposerTestView {
    composer: Entity<TextareaState>,
}

impl Render for ComposerTestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_start()
            .child(Cowork::render_composer_input(&self.composer))
    }
}

#[gpui::test]
fn composer_grows_beyond_four_lines(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let composer = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, usize::MAX));
        ComposerTestView { composer }
    });
    let composer = view.read_with(cx, |view, _| view.composer.clone());

    cx.update(|window, cx| {
        composer.update(cx, |composer, cx| {
            composer.set_value("1\n2\n3\n4\n5\n6\n7\n8\n9\n10", window, cx);
        });
    });
    cx.run_until_parked();

    let composer_bounds = cx
        .debug_bounds("composer")
        .expect("composer should be rendered");
    assert!(
        composer_bounds.size.height >= px(200.),
        "ten text lines should expand the composer, got {composer_bounds:?}"
    );
}

#[gpui::test]
fn submission_waits_for_a_selected_model(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update_in(cx, |cowork, window, cx| {
        cowork.new_thread_model = None;
        cowork.sync_model_picker(window, cx);
    });
    cx.simulate_input("question");
    cx.run_until_parked();
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx)));
    cowork.read_with(cx, |cowork, cx| {
        assert!(cowork.active_thread(cx).is_none());
        assert!(
            cowork
                .new_thread_draft
                .items()
                .iter()
                .any(|item| !item.is_empty())
        );
    });
    // A model the catalog no longer offers cannot be sent with either.
    cowork.update(cx, |cowork, _| {
        cowork.new_thread_model = Some(ollama_model("no-such-model"));
    });
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx)));
    cowork.read_with(cx, |cowork, cx| {
        assert!(cowork.active_thread(cx).is_none());
    });
    cowork.update(cx, |cowork, cx| cowork.select_model(ollama_qwen(), cx));
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx)));
    cowork.read_with(cx, |cowork, cx| {
        assert!(cowork.active_thread(cx).is_some());
    });
}

#[gpui::test]
fn blank_timeline_space_keeps_composer_focus(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let timeline = cx.debug_bounds("timeline-scroll").unwrap();
    let composer = cx.debug_bounds("composer").unwrap();
    let blank = point(timeline.center().x, timeline.bottom() - px(20.));
    assert!(blank.y > composer.bottom());

    cx.simulate_click(blank, gpui::Modifiers::none());
    cx.simulate_input("first");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), ["first"]);

    // Typing turns the draft position into a prompt block. The unused
    // scroll area must not steal that editor's focus either.
    cx.simulate_click(blank, gpui::Modifiers::none());
    cx.simulate_input(" second");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), ["first second"]);
}

#[gpui::test]
fn typing_at_the_draft_position_turns_it_into_a_block(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));
    let draft_position = cowork.read_with(cx, |cowork, _| {
        cowork
            .new_thread_draft
            .draft_position
            .clone()
            .expect("draft position editor")
    });

    cx.simulate_input("hi");
    cx.run_until_parked();

    assert_eq!(prompt_bodies(&cowork, cx), ["hi"]);
    let Some(EditorSlot::Prompt(id)) = focused_slot(&cowork, cx) else {
        panic!("the new block should keep focus");
    };
    cowork.read_with(cx, |cowork, cx| {
        let draft = &cowork.new_thread_draft;
        // The same editor carries on, so typing is uninterrupted.
        let block_editor = draft.editor(EditorSlot::Prompt(id)).expect("block editor");
        assert_eq!(block_editor.entity_id(), draft_position.entity_id());
        assert_eq!(block_editor.read(cx).value(), "hi");
        let item = draft.item(id).expect("block");
        assert_eq!(item.creator, draft.author.as_uuid());
        assert_ne!(
            draft.draft_position.as_ref().map(Entity::entity_id),
            Some(draft_position.entity_id())
        );
    });
}

#[gpui::test]
fn up_and_down_move_between_blocks_and_the_draft_position(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("one");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("two");
    cx.run_until_parked();

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

    cx.simulate_input("second");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), ["one\ntwo", "second"]);
    let items = new_thread_items(&cowork, cx);
    let first = EditorSlot::Prompt(items[0].id);
    assert_eq!(
        focused_slot(&cowork, cx),
        Some(EditorSlot::Prompt(items[1].id))
    );

    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    assert_eq!(focused_slot(&cowork, cx), Some(first));
    // Up stays inside a block until its first line, and stays put there.
    for _ in 0..2 {
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(focused_slot(&cowork, cx), Some(first));
        cowork.read_with(cx, |cowork, cx| {
            let editor = cowork.new_thread_draft.editor(first).expect("block editor");
            assert!(editor.read(cx).cursor() <= "one".len());
        });
    }
}

#[gpui::test]
fn emptied_blocks_are_removed_by_escape_backspace_and_leaving(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("a");
    cx.simulate_keystrokes("down");
    cx.simulate_input("b");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), ["a", "b"]);

    // Emptying a block keeps it while the caret is in it; Escape removes
    // it and returns to the draft position.
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), ["a", ""]);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), ["a"]);
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

    // Backspace in the empty draft position steps back into the block,
    // and once that is empty, Backspace removes it.
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    let first = new_thread_items(&cowork, cx)[0].id;
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::Prompt(first)));
    cx.simulate_keystrokes("backspace backspace");
    cx.run_until_parked();
    assert!(prompt_bodies(&cowork, cx).is_empty());
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

    // Leaving an emptied block removes it too.
    cx.simulate_input("c");
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), [""]);
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            let draft_id = cowork.new_thread_draft.id;
            cowork.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
        });
    });
    // Focus changes are dispatched when the next frame is drawn.
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    assert!(prompt_bodies(&cowork, cx).is_empty());
}

#[gpui::test]
fn both_editors_of_a_comment_show_the_same_text(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let comment = cowork.update(cx, |cowork, _| {
        let draft = &mut cowork.new_thread_draft;
        draft.create_comment(
            draft.author.as_uuid(),
            CommentTarget {
                message_id: Uuid::new_v4(),
                range: 0..5,
                quote: "quote".into(),
            },
            "x",
        )
    });
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            let draft_id = cowork.new_thread_draft.id;
            cowork.focus_draft_editor(
                draft_id,
                EditorSlot::CommentComposer(comment),
                Some(1),
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();
    cx.simulate_input("yz");
    cx.run_until_parked();

    cowork.read_with(cx, |cowork, cx| {
        let draft = &cowork.new_thread_draft;
        assert_eq!(draft.body(comment).as_deref(), Some("xyz"));
        let inline = draft
            .editor(EditorSlot::CommentInline(comment))
            .expect("inline editor");
        assert_eq!(inline.read(cx).value(), "xyz");
    });
}

#[gpui::test]
fn submissions_take_non_empty_items_and_leave_empty_ones(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let (empty_comment, submission) = cowork.update(cx, |cowork, _| {
        let draft = &mut cowork.new_thread_draft;
        let author = draft.author.as_uuid();
        let target = CommentTarget {
            message_id: Uuid::new_v4(),
            range: 0..5,
            quote: "quote".into(),
        };
        draft.create_prompt(author, "first");
        let empty_comment = draft.create_comment(author, target.clone(), "  ");
        draft.create_comment(author, target, "why?");
        draft.create_prompt(author, "");
        draft.create_prompt(author, "second");
        (empty_comment, Cowork::take_submission(draft))
    });

    let (comments, blocks, _) = submission.expect("something to submit");
    assert_eq!(
        blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(matches!(
        &comments[..],
        [UserComment { body: UserCommentBody::Submitted(body), .. }] if body.as_ref() == "why?"
    ));
    let remaining = new_thread_items(&cowork, cx);
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].id, empty_comment);
    assert!(remaining.iter().all(DraftItem::is_empty));

    let nothing_left = cowork.update(cx, |cowork, _| {
        Cowork::take_submission(&mut cowork.new_thread_draft)
    });
    assert!(nothing_left.is_none());
}

#[gpui::test]
fn submission_keeps_attachment_only_blocks_and_their_files(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update(cx, |cowork, _| {
        let draft = &mut cowork.new_thread_draft;
        let author = ParticipantId::new();
        let empty = draft.create_prompt(draft.author.as_uuid(), "  ");
        let block = draft.create_prompt(author.as_uuid(), "");
        let file = text_attachment("notes.txt", "file contents");
        let record = AttachmentRecord {
            id: AttachmentId::new(),
            name: file.name.clone(),
            kind: file.kind(),
            size: file.len(),
            creator: author.as_uuid(),
        };
        draft.add_attachment(block, record.clone());
        draft.files.insert(record.id, file);
        draft.stored.insert(record.id);
        draft.update_draft_editors(|_, editors| editors.comments_folded = true);

        let (comments, blocks, folded) =
            Cowork::take_submission(draft).expect("attachment submission");
        assert!(comments.is_empty());
        assert!(folded);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id, block.as_uuid());
        assert_eq!(blocks[0].author, author);
        assert!(blocks[0].text.is_empty());
        assert_eq!(blocks[0].attachments, vec![record.clone()]);
        let remaining = draft.items();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, empty);
        assert!(draft.attachment_records().is_empty());
        assert_eq!(draft.files[&record.id].len(), record.size);
        assert!(draft.stored.contains(&record.id));
        assert!(!draft.discarded.contains(&record.id));
        assert!(Cowork::take_submission(draft).is_none());
    });
}

#[gpui::test]
fn editor_lookup_includes_both_comment_views_when_folded(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update_in(cx, |cowork, window, cx| {
        let draft = &mut cowork.new_thread_draft;
        let prompt = draft.create_prompt(draft.author.as_uuid(), "prompt");
        let comment = draft.create_comment(
            draft.author.as_uuid(),
            CommentTarget {
                message_id: Uuid::new_v4(),
                range: 0..5,
                quote: "quote".into(),
            },
            "comment",
        );
        draft.update_draft_editors(|_, editors| editors.comments_folded = true);
        let draft_id = draft.id;
        cowork.prepare_draft(draft_id, window, cx);

        let draft = &cowork.new_thread_draft;
        assert_eq!(draft.all_editors().count(), 4);
        assert_eq!(
            draft
                .navigation_chain()
                .iter()
                .map(|(slot, _)| *slot)
                .collect::<Vec<_>>(),
            [EditorSlot::Prompt(prompt), EditorSlot::DraftPosition]
        );
        for slot in [
            EditorSlot::DraftPosition,
            EditorSlot::Prompt(prompt),
            EditorSlot::CommentInline(comment),
            EditorSlot::CommentComposer(comment),
        ] {
            let editor = draft.editor(slot).expect("editor");
            assert_eq!(draft.slot_of(editor.entity_id()), Some(slot));
            editor.focus_handle(cx).focus(window, cx);
            let (focused_draft, focused_slot, focused_editor) = cowork
                .focused_draft_editor(window, cx)
                .expect("focused editor");
            assert_eq!(focused_draft, draft_id);
            assert_eq!(focused_slot, slot);
            assert_eq!(focused_editor.entity_id(), editor.entity_id());
        }
        let unrelated = cx.new(|cx| TextareaState::new(window, cx));
        assert_eq!(draft.slot_of(unrelated.entity_id()), None);
        unrelated.focus_handle(cx).focus(window, cx);
        assert!(cowork.focused_draft_editor(window, cx).is_none());
    });
}

#[gpui::test]
fn syncing_editors_leaves_an_ime_composition_alone(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("abc");
    cx.run_until_parked();
    let id = new_thread_items(&cowork, cx)[0].id;
    let editor = cowork.read_with(cx, |cowork, _| {
        cowork
            .new_thread_draft
            .editor(EditorSlot::Prompt(id))
            .expect("block editor")
    });

    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.replace_and_mark_text_in_range(None, "ka", None, window, cx);
        });
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();

    cx.update(|window, cx| {
        let marked = editor.update(cx, |editor, cx| editor.marked_text_range(window, cx));
        assert!(marked.is_some(), "the composition should still be active");
        assert_eq!(editor.read(cx).value(), "abcka");
    });
}

#[gpui::test]
fn submitting_keeps_focus_on_an_item_that_stays(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("question");
    cx.run_until_parked();
    let comment = cowork.update(cx, |cowork, _| {
        let draft = &mut cowork.new_thread_draft;
        draft.create_comment(
            draft.author.as_uuid(),
            CommentTarget {
                message_id: Uuid::new_v4(),
                range: 0..5,
                quote: "quote".into(),
            },
            "",
        )
    });
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            let draft_id = cowork.new_thread_draft.id;
            cowork.focus_draft_editor(
                draft_id,
                EditorSlot::CommentComposer(comment),
                None,
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();

    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx));
    });
    cx.run_until_parked();

    assert_eq!(
        focused_slot(&cowork, cx),
        Some(EditorSlot::CommentComposer(comment))
    );
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork
            .active_thread(cx)
            .expect("the submission started a thread");
        let thread = thread.read(cx);
        let remaining = thread.draft().items();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, comment);
        let timeline = &thread.timeline;
        let [TimelineMessage::User(message), ..] = timeline.as_slice() else {
            panic!("expected the submitted message first");
        };
        assert_eq!(message.record.blocks.len(), 1);
        assert_eq!(message.record.blocks[0].text, "question");
        assert_eq!(thread.summary.title, "question");
    });
}

#[gpui::test]
fn generation_button_prioritizes_steering_and_shift_shortcut_queues(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update_in(cx, |cowork, window, cx| {
        let draft = std::mem::replace(
            &mut cowork.new_thread_draft,
            ThreadDraft::new(cowork.local_participant_id),
        );
        let thread = Cowork::new_empty_local_thread(
            draft,
            cowork.local_participant_id,
            cowork.models.clone(),
            cowork.new_thread_model.clone(),
            cx,
        );
        let id = thread.read(cx).instance_id;
        thread.update(cx, |thread, cx| thread.set_generating_for_test(true, cx));
        cowork
            .thread_store
            .update(cx, |store, cx| store.push_front(thread, cx));
        cowork
            .thread_store
            .update(cx, |store, cx| assert!(store.activate(id, cx)));
        cowork.focus_composer(window, cx);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("stop-generation").is_some());
    assert!(cx.debug_bounds("steer-message").is_none());

    cx.simulate_input("hello");
    cx.run_until_parked();
    assert!(cx.debug_bounds("steer-message").is_some());
    assert!(cx.debug_bounds("stop-generation").is_none());

    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    });
    cx.run_until_parked();
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).unwrap();
        assert_eq!(
            thread.read(cx).pending_submissions()[0].mode,
            protocol::SubmissionMode::Steering
        );
    });
    cx.simulate_input("later");
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-shift-enter"
    } else {
        "ctrl-shift-enter"
    });
    cx.run_until_parked();
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).unwrap();
        assert_eq!(
            thread.read(cx).pending_submissions()[1].mode,
            protocol::SubmissionMode::Queue
        );
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("stop-generation").is_some());
    assert!(cx.debug_bounds("steer-message").is_none());
}

/// A pending message's cancel tooltip goes with its button when the message
/// is sent while the pointer rests on it.
#[gpui::test]
fn cancel_tooltip_disappears_with_its_sent_message(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let (thread, id) = cowork.update_in(cx, |cowork, _, cx| {
        let mut draft = std::mem::replace(
            &mut cowork.new_thread_draft,
            ThreadDraft::new(cowork.local_participant_id),
        );
        draft.create_prompt(cowork.local_participant_id.as_uuid(), "steer this");
        let thread = Cowork::new_empty_local_thread(
            draft,
            cowork.local_participant_id,
            cowork.models.clone(),
            cowork.new_thread_model.clone(),
            cx,
        );
        thread.update(cx, |thread, cx| thread.set_generating_for_test(true, cx));
        cowork
            .thread_store
            .update(cx, |store, cx| store.push_front(thread.clone(), cx));
        cowork.thread_store.update(cx, |store, cx| {
            assert!(store.activate(thread.read(cx).instance_id, cx))
        });
        assert!(cowork.accept_submission(
            thread.read(cx).draft().id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor: cowork.local_participant_id,
                submitted_locally: true,
                mode: protocol::SubmissionMode::Steering,
            },
            cx,
        ));
        cx.notify();
        let id = thread.read(cx).pending_submissions()[0].id;
        (thread, id)
    });
    cx.run_until_parked();
    let button: &'static str = Box::leak(format!("cancel-pending-{id}").into_boxed_str());
    let center = cx.debug_bounds(button).expect("cancel button").center();
    cx.simulate_mouse_move(center, None, gpui::Modifiers::default());
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    // Delivered without the pointer moving.
    thread.update(cx, |thread, cx| {
        thread.pending.clear();
        cx.notify();
    });
    cowork.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert!(cx.debug_bounds(button).is_none());
}

#[gpui::test]
fn idle_pending_messages_show_resume_until_a_new_draft_is_written(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update_in(cx, |cowork, window, cx| {
        let mut draft = std::mem::replace(
            &mut cowork.new_thread_draft,
            ThreadDraft::new(cowork.local_participant_id),
        );
        draft.create_prompt(cowork.local_participant_id.as_uuid(), "queued message");
        let thread = Cowork::new_empty_local_thread(
            draft,
            cowork.local_participant_id,
            cowork.models.clone(),
            cowork.new_thread_model.clone(),
            cx,
        );
        let id = thread.read(cx).instance_id;
        thread.update(cx, |thread, cx| thread.set_generating_for_test(true, cx));
        cowork
            .thread_store
            .update(cx, |store, cx| store.push_front(thread.clone(), cx));
        cowork
            .thread_store
            .update(cx, |store, cx| assert!(store.activate(id, cx)));
        assert!(cowork.accept_submission(
            thread.read(cx).draft().id,
            Some(thread.clone()),
            crate::thread::submission::SubmissionRequest {
                actor: cowork.local_participant_id,
                submitted_locally: true,
                mode: protocol::SubmissionMode::Queue,
            },
            cx,
        ));
        thread.update(cx, |thread, cx| thread.set_generating_for_test(false, cx));
        cowork.focus_composer(window, cx);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("resume-messages").is_some());
    assert!(cx.debug_bounds("send-message").is_none());

    cx.simulate_input("new message");
    cx.run_until_parked();
    assert!(cx.debug_bounds("resume-messages").is_none());
    assert!(cx.debug_bounds("send-message").is_some());
}

use crate::composer::{DRAFT_PLACEHOLDER, SEND_KEY_CUE, SendAction, SendKey, SendKeyCue};

fn shows_placeholder(editor: &Entity<TextareaState>, cx: &mut gpui::VisualTestContext) -> bool {
    cx.update(|window, cx| {
        let placeholder = editor.read(cx).presentation().placeholder().to_string();
        let empty = editor.read(cx).value().is_empty();
        let marked = editor
            .update(cx, |editor, cx| editor.marked_text_range(window, cx))
            .is_some();
        placeholder == DRAFT_PLACEHOLDER && empty && !marked
    })
}

fn placeholder_text(editor: &Entity<TextareaState>, cx: &mut gpui::VisualTestContext) -> String {
    cx.update(|_, cx| editor.read(cx).presentation().placeholder().to_string())
}

fn chip_selector(action: SendAction) -> &'static str {
    Box::leak(format!("send-key-chip:{}:{}", action.label(), action.tooltip()).into_boxed_str())
}

fn cue_selector(cue: SendKeyCue) -> &'static str {
    Box::leak(format!("send-key-cue:{}", cue.text()).into_boxed_str())
}

fn any_send_key_chip(cx: &mut gpui::VisualTestContext) -> bool {
    [
        SendAction::Send(SendKey::Enter),
        SendAction::Steer(SendKey::Enter),
        SendAction::Send(SendKey::ModifierEnter),
        SendAction::Steer(SendKey::ModifierEnter),
    ]
    .into_iter()
    .any(|action| cx.debug_bounds(chip_selector(action)).is_some())
}

#[gpui::test]
fn placeholder_shows_on_an_empty_draft_until_the_first_character(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let editor = cowork.read_with(cx, |cowork, _| {
        cowork
            .new_thread_draft
            .draft_position
            .clone()
            .expect("draft position")
    });
    assert!(shows_placeholder(&editor, cx));
    assert!(
        cx.debug_bounds(chip_selector(SendAction::Send(SendKey::Enter)))
            .is_some()
    );

    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.timeline_focus_handle.focus(window, cx);
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        assert!(!editor.focus_handle(cx).is_focused(window));
    });
    assert!(
        shows_placeholder(&editor, cx),
        "an empty draft keeps the placeholder when it is not focused"
    );

    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx));
    });
    cx.simulate_input("a");
    cx.run_until_parked();
    assert!(!shows_placeholder(&editor, cx));
    assert!(
        cx.debug_bounds(chip_selector(SendAction::Send(SendKey::Enter)))
            .is_some()
    );

    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert!(shows_placeholder(&editor, cx));
}

#[gpui::test]
fn marked_text_hides_the_placeholder_until_the_buffer_is_empty(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let editor = cowork.read_with(cx, |cowork, _| {
        cowork
            .new_thread_draft
            .draft_position
            .clone()
            .expect("draft position")
    });
    assert!(shows_placeholder(&editor, cx));

    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.replace_and_mark_text_in_range(None, "ni", None, window, cx);
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        assert!(
            editor
                .update(cx, |editor, cx| editor.marked_text_range(window, cx))
                .is_some()
        );
        assert!(!editor.read(cx).value().is_empty());
    });
    assert!(!shows_placeholder(&editor, cx));

    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.replace_and_mark_text_in_range(None, "", None, window, cx);
        });
    });
    cx.run_until_parked();
    assert!(shows_placeholder(&editor, cx));
}

#[gpui::test]
fn placeholder_is_only_on_the_last_empty_block(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let (draft_id, first, second) = cowork.update(cx, |cowork, cx| {
        let author = cowork.new_thread_draft.author.as_uuid();
        let draft_id = cowork.new_thread_draft.id;
        let ids = cowork
            .update_draft(draft_id, cx, |draft| {
                (
                    draft.create_prompt(author, ""),
                    draft.create_prompt(author, ""),
                )
            })
            .expect("the new chat's draft");
        cx.notify();
        (draft_id, ids.0, ids.1)
    });
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.focus_draft_editor(draft_id, EditorSlot::Prompt(second), None, window, cx);
        });
    });
    cx.run_until_parked();

    let (first_editor, second_editor, position) = cowork.read_with(cx, |cowork, _| {
        let draft = &cowork.new_thread_draft;
        (
            draft
                .editor(EditorSlot::Prompt(first))
                .expect("first block"),
            draft
                .editor(EditorSlot::Prompt(second))
                .expect("last block"),
            draft
                .editor(EditorSlot::DraftPosition)
                .expect("draft position"),
        )
    });
    assert_ne!(placeholder_text(&first_editor, cx), DRAFT_PLACEHOLDER);
    assert!(shows_placeholder(&second_editor, cx));
    assert_ne!(placeholder_text(&position, cx), DRAFT_PLACEHOLDER);

    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
        });
    });
    cx.run_until_parked();
    assert!(shows_placeholder(&position, cx));
    assert_ne!(placeholder_text(&second_editor, cx), DRAFT_PLACEHOLDER);
    assert_ne!(placeholder_text(&first_editor, cx), DRAFT_PLACEHOLDER);
}

fn open_read_only<'a>(
    cx: &'a mut gpui::TestAppContext,
    block: Option<&str>,
) -> (
    Entity<Cowork>,
    tokio::runtime::Runtime,
    &'a mut gpui::VisualTestContext,
) {
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::theme::init(cx);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let tokio_handle = runtime.handle().clone();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let host_id = ParticipantId::new();
        let actor = ParticipantId::new();
        let mut draft = ThreadDraft::new(host_id);
        if let Some(body) = block {
            draft.create_prompt(host_id.as_uuid(), body);
        }
        let host = Thread::new_local(
            "test".into(),
            Vec::new(),
            draft,
            host_id,
            std::sync::Arc::default(),
            None,
        );
        let mut snapshot = host.to_protocol();
        snapshot.participants = vec![host_id.into_bytes(), actor.into_bytes()];
        let welcome = protocol::Welcome {
            participant_id: actor.into_bytes(),
            thread: snapshot,
            draft: host.draft().encode_state(),
            draft_generation: 0,
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let thread = cx.new(|cx| {
            let mut thread = Thread::from_prepared_welcome(
                Thread::prepare_welcome(welcome).expect("valid welcome"),
                ThreadDraft::new(actor),
                crate::thread::ThreadSharing::NotShared,
                cx,
            );
            thread.apply_for_test(
                protocol::HostMessage::DefaultPeerModeChanged(crate::thread::PeerMode::ReadOnly),
                cx,
            );
            thread
        });
        let thread_id = thread.read(cx).instance_id;
        let store = cx.new(|cx| {
            let mut store = ThreadStore::default();
            store.push_front_active(thread, cx);
            store
        });
        let cowork =
            cx.new(|cx| super::test_cowork(store, Some(thread_id), tokio_handle, window, cx));
        gpui_component::Root::new(cowork, window, cx)
    });
    let cowork = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<Cowork>()
            .expect("root shows cowork")
    });
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    (cowork, runtime, cx)
}

#[gpui::test]
fn read_only_chat_shows_neither_placeholder_nor_chip(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = open_read_only(cx, None);
    assert!(
        !cowork.read_with(cx, |cowork, cx| {
            cowork
                .readable_draft_id(cx)
                .is_some_and(|draft_id| cowork.draft_can_edit(draft_id, cx))
        }),
        "the peer cannot edit"
    );
    assert!(!any_send_key_chip(cx));
    let editors = cowork.read_with(cx, |cowork, cx| {
        let draft_id = cowork.readable_draft_id(cx).expect("draft");
        cowork
            .read_draft(draft_id, cx, |draft| {
                draft
                    .all_editors()
                    .map(|(_, editor)| editor.clone())
                    .collect::<Vec<_>>()
            })
            .expect("draft")
    });
    assert!(editors.iter().all(|editor| !shows_placeholder(editor, cx)));
}

#[gpui::test]
fn read_only_prompt_block_has_no_placeholder(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = open_read_only(cx, Some(""));
    assert!(!any_send_key_chip(cx));
    let editors = cowork.read_with(cx, |cowork, cx| {
        let draft_id = cowork.readable_draft_id(cx).expect("draft");
        cowork
            .read_draft(draft_id, cx, |draft| {
                draft
                    .all_editors()
                    .map(|(_, editor)| editor.clone())
                    .collect::<Vec<_>>()
            })
            .expect("draft")
    });
    assert!(
        editors
            .iter()
            .any(|editor| placeholder_text(editor, cx).is_empty()),
        "the empty block is edited nowhere"
    );
    assert!(editors.iter().all(|editor| {
        placeholder_text(editor, cx).is_empty() && !shows_placeholder(editor, cx)
    }));
}

#[gpui::test]
fn send_key_chip_tooltip_is_steer_while_generating(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let author = cowork.read_with(cx, |cowork, _| cowork.local_participant_id);
    let thread_id = uuid::Uuid::new_v4();
    let mut draft = ThreadDraft::new(author);
    draft.create_prompt(author.as_uuid(), "hello");
    let thread = cx.update(|_, cx| cx.new(|_| super::test_thread(thread_id, Vec::new(), draft)));
    cowork.update(cx, |cowork, cx| {
        cowork
            .thread_store
            .update(cx, |store, cx| store.push_front_active(thread.clone(), cx));
        cx.notify();
    });
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx));
    });
    cx.run_until_parked();
    let draft_id = thread.read_with(cx, |thread, _| thread.draft().id);
    assert!(
        cowork
            .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
            .is_none()
    );
    assert!(
        cx.debug_bounds(chip_selector(SendAction::Send(SendKey::Enter)))
            .is_some()
    );

    thread.update(cx, |thread, cx| thread.set_generating_for_test(true, cx));
    cowork.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert!(
        thread.read_with(cx, |thread, _| thread.is_generating()),
        "the fixture run is going"
    );
    assert!(
        cx.debug_bounds(chip_selector(SendAction::Steer(SendKey::Enter)))
            .is_some()
    );
    assert!(
        cowork
            .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
            .is_none()
    );
}

#[gpui::test]
fn send_key_chip_pulses_only_when_the_key_changes(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let author = cowork.read_with(cx, |cowork, _| cowork.local_participant_id);
    let thread_id = uuid::Uuid::new_v4();
    let thread = cx.update(|_, cx| {
        cx.new(|_| super::test_thread(thread_id, Vec::new(), ThreadDraft::new(author)))
    });
    cowork.update(cx, |cowork, cx| {
        cowork
            .thread_store
            .update(cx, |store, cx| store.push_front_active(thread.clone(), cx));
        cx.notify();
    });
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx));
    });
    cx.simulate_input("ab");
    cx.run_until_parked();

    let draft_id = thread.read_with(cx, |thread, _| thread.draft().id);
    assert!(
        cowork
            .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
            .is_none()
    );
    assert!(
        cx.debug_bounds(chip_selector(SendAction::Send(SendKey::Enter)))
            .is_some()
    );
    let (cursor, focused) = cx.update(|window, cx| {
        let editor = cowork
            .read(cx)
            .focused_draft_editor(window, cx)
            .expect("caret")
            .2;
        (editor.read(cx).cursor(), editor.entity_id())
    });

    let peer = ParticipantId::new();
    thread.update(cx, |thread, cx| {
        thread.apply_for_test(super::joined(peer), cx)
    });
    cowork.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let (cue, generation) = cowork
        .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
        .expect("a peer joining changes the send key");
    assert_eq!(cue, SendKeyCue::OthersHere);
    assert!(
        cx.debug_bounds(chip_selector(SendAction::Send(SendKey::ModifierEnter)))
            .is_some()
    );
    assert!(
        cx.debug_bounds(cue_selector(SendKeyCue::OthersHere))
            .is_some()
    );
    cx.update(|window, cx| {
        let editor = cowork
            .read(cx)
            .focused_draft_editor(window, cx)
            .expect("caret")
            .2;
        assert_eq!(editor.entity_id(), focused);
        assert_eq!(editor.read(cx).cursor(), cursor);
    });

    let other = ParticipantId::new();
    thread.update(cx, |thread, cx| {
        thread.apply_for_test(super::joined(other), cx)
    });
    cowork.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    cx.simulate_input("c");
    cx.run_until_parked();
    assert_eq!(
        cowork
            .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
            .map(|pulse| pulse.1),
        Some(generation),
        "another arrival, and typing, leave the send key alone"
    );

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(
        cowork
            .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
            .map(|pulse| pulse.0),
        Some(SendKeyCue::OthersHere)
    );

    thread.update(cx, |thread, cx| {
        thread.apply_for_test(
            protocol::HostMessage::ParticipantLeft(peer.into_bytes()),
            cx,
        );
        thread.apply_for_test(
            protocol::HostMessage::ParticipantLeft(other.into_bytes()),
            cx,
        );
    });
    cowork.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let (cue, next) = cowork
        .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
        .expect("leaving restores Enter");
    assert_eq!(cue, SendKeyCue::JustYou);
    assert_ne!(next, generation);
    assert!(
        cx.debug_bounds(chip_selector(SendAction::Send(SendKey::Enter)))
            .is_some()
    );
    assert!(cx.debug_bounds(cue_selector(SendKeyCue::JustYou)).is_some());

    // The first cue's timer was dropped with the replacement pulse.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(
        cowork
            .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
            .map(|pulse| pulse.1),
        Some(next)
    );
    cx.executor().advance_clock(SEND_KEY_CUE);
    cx.run_until_parked();
    assert!(
        cowork
            .read_with(cx, |cowork, _| cowork.send_key_pulse(draft_id))
            .is_none()
    );
    assert!(cx.debug_bounds(cue_selector(SendKeyCue::JustYou)).is_none());
}
