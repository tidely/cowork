//! Attaching files to a draft: picking, pasting, and dropping them, and
//! which block they land in.

use super::*;

/// The attachments of a thread's draft, in draft order.
fn thread_draft_attachments(
    cowork: &Entity<Cowork>,
    thread_id: Uuid,
    cx: &mut gpui::VisualTestContext,
) -> Vec<FileAttachment> {
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork
            .thread_store
            .read(cx)
            .thread(thread_id, cx)
            .expect("thread")
            .read(cx);
        draft_attachments(thread.draft())
    })
}

#[gpui::test]
fn attachments_land_on_their_thread_after_switching_away(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let path = std::env::temp_dir().join(format!("cowork-{}.md", Uuid::new_v4()));
    std::fs::write(&path, "# Notes").expect("write test attachment");

    cowork.update(cx, |cowork, cx| {
        let draft_id = cowork.writable_draft_id(cx).expect("writable thread");
        cowork.add_attachments(
            draft_id,
            AttachmentTarget::NewBlock(Uuid::new_v4()),
            vec![AttachmentSource::Path(path.clone())],
            None,
            cx,
        );
        assert!(cowork.draft_is_loading_attachments(draft_id, cx));
        cowork.active_thread_id = None;
    });
    cx.run_until_parked();
    std::fs::remove_file(&path).expect("remove test attachment");

    let attachments = thread_draft_attachments(&cowork, thread_id, cx);
    assert!(matches!(
        attachments.as_slice(),
        [FileAttachment { content: FileAttachmentContent::Text(text), .. }] if text == "# Notes"
    ));
    cowork.read_with(cx, |cowork, _| {
        assert!(draft_attachments(&cowork.new_thread_draft).is_empty());
        assert!(cowork.pending_attachments.is_empty());
    });
}

#[gpui::test]
fn pasting_an_image_attaches_it_to_the_draft(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let bmp = encoded_image(3, image::ImageFormat::Bmp);
    cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(
        gpui::ImageFormat::Bmp,
        bmp,
    )));

    cowork.update_in(cx, |cowork, window, cx| {
        // Pasting attaches to the block, or here the draft position, being
        // typed in.
        cowork.focus_composer(window, cx);
        cowork.paste_attachments(&Paste, window, cx);
    });
    cx.run_until_parked();

    let attachments = thread_draft_attachments(&cowork, thread_id, cx);
    assert!(matches!(
        attachments.as_slice(),
        [FileAttachment { name, content: FileAttachmentContent::Png(_) }] if name == "Pasted image.png"
    ));
}

#[gpui::test]
fn unplaced_attachments_target_the_focused_block_only(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let target = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| cowork.read(cx).attachment_target_at_focus(window, cx))
    };
    assert!(matches!(target(cx), AttachmentTarget::NewBlock(_)));

    cx.simulate_input("block");
    cx.run_until_parked();
    let block = new_thread_items(&cowork, cx)[0].id;
    assert_eq!(target(cx), AttachmentTarget::Block(block));

    let comment = cowork.update(cx, |cowork, _| {
        let draft = &mut cowork.new_thread_draft;
        draft.create_comment(
            draft.author.as_uuid(),
            CommentTarget {
                message_id: Uuid::new_v4(),
                range: 0..5,
                quote: "quote".into(),
            },
            "note",
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
    assert!(matches!(target(cx), AttachmentTarget::NewBlock(_)));
}

#[gpui::test]
fn dropping_an_image_keeps_the_caret_in_its_block(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("question");
    cx.run_until_parked();
    let block = new_thread_items(&cowork, cx)[0].id;
    let path = std::env::temp_dir().join(format!("cowork-{}.png", Uuid::new_v4()));
    std::fs::write(&path, encoded_image(2, image::ImageFormat::Png)).expect("write test image");
    let mut paths = ExternalPaths::default();
    paths.0.push(path.clone());

    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.drop_attachments(&paths, window, cx);
        });
    });
    cx.run_until_parked();
    std::fs::remove_file(&path).expect("remove test image");

    let items = new_thread_items(&cowork, cx);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, block);
    assert!(matches!(
        &items[0].kind,
        DraftItemKind::Prompt { attachments } if attachments.len() == 1
    ));
    cx.update(|window, cx| {
        assert!(matches!(
            cowork.read(cx).focused_draft_editor(window, cx),
            Some((_, EditorSlot::Prompt(id), _)) if id == block
        ));
    });
}

#[gpui::test]
fn dropping_an_image_into_an_empty_composer_focuses_its_block(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let path = std::env::temp_dir().join(format!("cowork-{}.png", Uuid::new_v4()));
    std::fs::write(&path, encoded_image(2, image::ImageFormat::Png)).expect("write test image");
    let mut paths = ExternalPaths::default();
    paths.0.push(path.clone());

    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| cowork.drop_attachments(&paths, window, cx));
    });
    cx.run_until_parked();
    std::fs::remove_file(&path).expect("remove test image");

    let items = new_thread_items(&cowork, cx);
    assert_eq!(items.len(), 1);
    let block = items[0].id;
    assert!(matches!(
        &items[0].kind,
        DraftItemKind::Prompt { attachments } if attachments.len() == 1
    ));
    cx.update(|window, cx| {
        assert!(matches!(
            cowork.read(cx).focused_draft_editor(window, cx),
            Some((_, EditorSlot::Prompt(id), _)) if id == block
        ));
    });
    cx.simulate_input("caption");
    cx.run_until_parked();
    let items = new_thread_items(&cowork, cx);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, block);
    assert_eq!(items[0].body, "caption");
}

#[gpui::test]
fn files_for_a_removed_block_land_in_a_new_one(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("block");
    cx.run_until_parked();
    let block = new_thread_items(&cowork, cx)[0].id;
    let path = std::env::temp_dir().join(format!("cowork-{}.txt", Uuid::new_v4()));
    std::fs::write(&path, "notes").expect("write test attachment");

    cowork.update(cx, |cowork, cx| {
        let draft_id = cowork.new_thread_draft.id;
        cowork.add_attachments(
            draft_id,
            AttachmentTarget::Block(block),
            vec![AttachmentSource::Path(path.clone())],
            None,
            cx,
        );
        cowork.new_thread_draft.remove_items(&[block]);
    });
    cx.run_until_parked();
    std::fs::remove_file(&path).expect("remove test attachment");

    let items = new_thread_items(&cowork, cx);
    assert_eq!(items.len(), 1);
    assert_ne!(items[0].id, block);
    assert!(matches!(
        &items[0].kind,
        DraftItemKind::Prompt { attachments } if attachments.len() == 1
    ));
    cowork.read_with(cx, |cowork, _| {
        assert!(matches!(
            draft_attachments(&cowork.new_thread_draft).as_slice(),
            [FileAttachment { content: FileAttachmentContent::Text(text), .. }] if text == "notes"
        ));
    });
}
