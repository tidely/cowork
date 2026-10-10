use gpui_component::WindowExt as _;

use super::*;
use crate::{
    protocol::{AgentRun, RunOutcome},
    search_palette::PaletteTarget,
    timeline::{AgentMessage, AgentOutput},
};

fn titled_thread(title: &str, timeline: Vec<TimelineMessage>) -> (Uuid, Thread) {
    let thread_id = Uuid::new_v4();
    let mut thread = test_thread(thread_id, timeline, ThreadDraft::new(ParticipantId::new()));
    thread.summary.title = title.into();
    (thread_id, thread)
}

fn prompt(text: &str) -> TimelineMessage {
    TimelineMessage::User(UserMessageGroup {
        id: Uuid::new_v4(),
        comments: Vec::new(),
        blocks: vec![PromptBlock {
            id: Uuid::new_v4(),
            author: ParticipantId::new(),
            text: text.into(),
            attachments: Vec::new(),
        }],
        comments_folded: false,
    })
}

fn reply(text: &str, cx: &mut App) -> TimelineMessage {
    TimelineMessage::Agent(AgentMessage {
        id: Uuid::new_v4(),
        comment_group_id: None,
        started_at: SystemTime::UNIX_EPOCH,
        comment_responses: Vec::new(),
        prompt: 0,
        pending_events: Vec::new(),
        run: AgentRun::Ended {
            outcome: RunOutcome::Completed,
            duration: Duration::ZERO,
        },
        output: AgentOutput {
            thinking_complete: true,
            text: text.into(),
            ..Default::default()
        },
        committed: Default::default(),
        comment_calls_checked: 0,
        awaiting_approval: None,
        step_views: Vec::new(),
        work_expanded: false,
        text_view: cx.new(|cx| TextViewState::markdown(text, cx)),
    })
}

#[gpui::test]
fn search_button_sits_at_the_right_of_the_sidebar_header(cx: &mut gpui::TestAppContext) {
    let (_cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));

    let button = cx
        .debug_bounds("search-chats")
        .expect("search button should be rendered");

    // Past the middle of the sidebar, and no further from its right edge
    // than the header's own padding.
    assert!(
        button.left() > SIDEBAR_WIDTH / 2. && button.right() >= SIDEBAR_WIDTH - px(32.),
        "search button {button:?} should hug the right of a {SIDEBAR_WIDTH:?} sidebar"
    );
}

#[gpui::test]
fn search_palette_finds_chats_by_title_prompt_and_reply(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let tokio_handle = runtime.handle().clone();
    let mut ids = None;
    let (root, cx) = cx.add_window_view(|window, cx| {
        let (bread_id, bread) = titled_thread(
            "Alpha plans",
            vec![
                prompt("How do I bake sourdough?"),
                reply("Feed the starter twice a day.\n\nKeep it warm.", cx),
            ],
        );
        let (review_id, review) = titled_thread("Beta review", Vec::new());
        ids = Some((bread_id, review_id));
        let thread_store = cx.new(|cx| ThreadStore {
            threads: VecDeque::from([cx.new(|_| bread), cx.new(|_| review)]),
            ..Default::default()
        });
        Root::new(
            cx.new(|cx| test_cowork(thread_store, None, tokio_handle, window, cx)),
            window,
            cx,
        )
    });
    let (bread_id, review_id) = ids.expect("threads were created");
    let cowork = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<Cowork>()
            .expect("root shows cowork")
    });
    cx.update(|window, _| window.activate_window());
    cx.update(|window, cx| window.draw(cx).clear(cx));

    let found = |query: &str, cx: &mut gpui::VisualTestContext| {
        cowork.read_with(cx, |cowork, cx| {
            cowork
                .search_palette_sections(query, cx)
                .into_iter()
                .flat_map(|section| section.rows)
                .map(|row| {
                    let excerpt = row
                        .excerpt
                        .map(|excerpt| excerpt.text[excerpt.matched.clone()].to_owned());
                    (row.target, excerpt)
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        found("", cx),
        [
            (PaletteTarget::Chat(bread_id), None),
            (PaletteTarget::Chat(review_id), None),
        ]
    );
    // Only the agent's reply has it.
    assert_eq!(
        found("starter twice", cx),
        [(PaletteTarget::Chat(bread_id), Some("starter twice".into()))]
    );

    let open_palette_and_pick = |query: &str, cx: &mut gpui::VisualTestContext| {
        let button = cx
            .debug_bounds("search-chats")
            .expect("search button should be rendered");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
        cx.simulate_input(query);
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
        cowork.read_with(cx, |cowork, _| cowork.active_thread_id)
    };
    // Not a substring of either title: only fuzzy matching finds it.
    assert_eq!(open_palette_and_pick("btrv", cx), Some(review_id));
    // Only the prompt has it.
    assert_eq!(open_palette_and_pick("sourdough", cx), Some(bread_id));
}
