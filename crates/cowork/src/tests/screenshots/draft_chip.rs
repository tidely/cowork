//! The empty-draft placeholder, the send-key cap, and its pulse when
//! someone joins.

use std::time::Duration;

use gpui::AppContext as _;
use uuid::Uuid;

use super::SceneContext;
use crate::{
    composer::SEND_KEY_FADE, participant::ParticipantId, protocol, thread::ThreadStore,
    thread_draft::ThreadDraft,
};

pub(super) fn scene(context: &SceneContext) -> anyhow::Result<()> {
    let mut stage = context.open(|_, cx| (cx.new(|_| ThreadStore::default()), None))?;
    stage.update(|cowork, window, cx| {
        window.activate_window();
        cowork.focus_composer(window, cx);
    })?;
    stage.capture("1-empty")?;

    stage.update(|cowork, window, cx| {
        let draft_id = cowork.new_thread_draft.id;
        let author = cowork.new_thread_draft.author.as_uuid();
        cowork.update_draft(draft_id, cx, |draft| {
            draft.create_prompt(author, "Hello");
        });
        cowork.focus_composer(window, cx);
    })?;
    stage.capture("2-typed")?;

    let author = ParticipantId::new();
    let id = Uuid::new_v4();
    let mut shared = context.open(|_, cx| {
        let mut draft = ThreadDraft::new(author);
        draft.create_prompt(author.as_uuid(), "Hello");
        let thread = cx.new(|_| super::super::test_thread(id, Vec::new(), draft));
        let store = cx.new(|cx| {
            let mut store = ThreadStore::default();
            store.push_front_active(thread, cx);
            store
        });
        (store, Some(id))
    })?;
    shared.update(|cowork, window, cx| {
        window.activate_window();
        cowork.focus_composer(window, cx);
        let thread = cowork.active_thread(cx).expect("the chat");
        thread.update(cx, |thread, cx| thread.set_generating_for_test(true, cx));
    })?;
    shared.capture("3-generating")?;

    let mut with_peer = context.open(|_, cx| {
        let mut draft = ThreadDraft::new(author);
        draft.create_prompt(author.as_uuid(), "Hello");
        let thread = cx.new(|_| super::super::test_thread(id, Vec::new(), draft));
        let store = cx.new(|cx| {
            let mut store = ThreadStore::default();
            store.push_front_active(thread, cx);
            store
        });
        (store, Some(id))
    })?;
    with_peer.update(|cowork, window, cx| {
        window.activate_window();
        cowork.focus_composer(window, cx);
        let peer = ParticipantId::new();
        let thread = cowork.active_thread(cx).expect("the chat");
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(
                protocol::HostMessage::ParticipantJoined {
                    participant: peer.into_bytes(),
                    profile: protocol::Profile::default(),
                },
                cx,
            );
        });
    })?;
    // The pulse starts on the render that sees the new key. Draw it, then
    // step the clock to the middle of the fade.
    with_peer.capture("4-pulse-start")?;
    with_peer.advance(SEND_KEY_FADE / 2);
    with_peer.capture("5-pulse-mid")?;
    with_peer.advance(Duration::from_secs(3));
    with_peer.capture("6-others")?;
    Ok(())
}
