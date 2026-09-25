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
                .doc
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
        let item = draft.doc.item(id).expect("block");
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
    cx.simulate_input("first");
    cx.run_until_parked();

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

    cx.simulate_input("second");
    cx.run_until_parked();
    assert_eq!(prompt_bodies(&cowork, cx), ["first", "second"]);
    let items = new_thread_items(&cowork, cx);
    assert_eq!(
        focused_slot(&cowork, cx),
        Some(EditorSlot::Prompt(items[1].id))
    );

    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    assert_eq!(
        focused_slot(&cowork, cx),
        Some(EditorSlot::Prompt(items[0].id))
    );
    // Up from the first line of the first editor stays put.
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    assert_eq!(
        focused_slot(&cowork, cx),
        Some(EditorSlot::Prompt(items[0].id))
    );
}

#[gpui::test]
fn up_stays_inside_a_block_until_its_first_line(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("one");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("two");
    cx.run_until_parked();
    let first = new_thread_items(&cowork, cx)[0].id;
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::Prompt(first)));
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    // Moved to the first line of the block rather than out of it.
    assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::Prompt(first)));
    cowork.read_with(cx, |cowork, cx| {
        let editor = cowork
            .new_thread_draft
            .editor(EditorSlot::Prompt(first))
            .expect("block editor");
        assert!(editor.read(cx).cursor() <= "one".len());
    });
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
        let draft = &cowork.new_thread_draft;
        draft.doc.create_comment(
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
        assert_eq!(draft.doc.body(comment).as_deref(), Some("xyz"));
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
        draft.doc.create_prompt(author, "first");
        let empty_comment = draft.doc.create_comment(author, target.clone(), "  ");
        draft.doc.create_comment(author, target, "why?");
        draft.doc.create_prompt(author, "");
        draft.doc.create_prompt(author, "second");
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
        let draft = &cowork.new_thread_draft;
        draft.doc.create_comment(
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
        let remaining = thread.draft.doc.items();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, comment);
        let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
            panic!("expected the submitted message first");
        };
        assert_eq!(message.blocks.len(), 1);
        assert_eq!(message.blocks[0].text, "question");
        assert_eq!(thread.summary.title, "question");
    });
}
