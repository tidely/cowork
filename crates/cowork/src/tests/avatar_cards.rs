//! The cards avatars show on hover.

use super::*;

/// A prompt's and a reply's avatars each open their card after the tooltip
/// delay, and moving off the avatar closes it.
#[gpui::test]
fn avatars_show_their_card_while_hovered(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let block_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let author = cx.update(|_, cx| {
        cowork.update(cx, |cowork, cx| {
            let thread = Cowork::new_empty_local_thread(
                ThreadDraft::new(cowork.local_participant_id),
                cowork.local_participant_id,
                cowork.models.clone(),
                None,
                cx,
            );
            cowork
                .thread_store
                .update(cx, |store, cx| store.push_front(thread.clone(), cx));
            cowork.thread_store.update(cx, |store, cx| {
                assert!(store.activate(thread.read(cx).instance_id, cx))
            });
            let author = thread.update(cx, |thread, cx| {
                let author = thread.participant_id();
                let group = UserMessageGroup {
                    id: Uuid::new_v4(),
                    comments: Vec::new(),
                    comments_folded: false,
                    blocks: vec![PromptBlock {
                        id: block_id,
                        author,
                        text: "Hello".into(),
                        attachments: Vec::new(),
                    }],
                };
                thread.timeline.push(TimelineMessage::User(group.into()));
                let mut host = ScriptedHost::default();
                let mut events = host.start(message_id, "Hello");
                events.extend(host.turn(
                    message_id,
                    [streamed_text("answer", "Hi there", false)],
                    100,
                ));
                events.push(host.ended(message_id, protocol::RunOutcome::Completed));
                for event in events {
                    thread.apply_for_test(event, cx);
                }
                author
            });
            cx.notify();
            author
        })
    });
    cx.run_until_parked();

    let selector = |id: String| -> &'static str { Box::leak(id.into_boxed_str()) };
    let hover = |selector: &'static str, cx: &mut gpui::VisualTestContext| {
        let avatar = cx.debug_bounds(selector).expect("the avatar");
        cx.simulate_mouse_move(avatar.center(), None, gpui::Modifiers::default());
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
    };

    let prompt_avatar = selector(format!("avatar-{block_id}-{}", author.as_uuid()));
    hover(prompt_avatar, cx);
    assert!(cx.debug_bounds("avatar-card").is_some());

    let text = selector(format!("timeline-user-text-{block_id}"));
    let away = cx.debug_bounds(text).expect("the prompt").center();
    cx.simulate_mouse_move(away, None, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("avatar-card").is_none());

    hover(selector(format!("avatar-{message_id}-{message_id}")), cx);
    assert!(cx.debug_bounds("avatar-card").is_some());
}
