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
    TimelineMessage::User(
        UserMessageGroup {
            id: Uuid::new_v4(),
            comments: Vec::new(),
            blocks: vec![PromptBlock {
                id: Uuid::new_v4(),
                author: ParticipantId::new(),
                text: text.into(),
                attachments: Vec::new(),
            }],
            comments_folded: false,
        }
        .into(),
    )
}

fn reply(text: &str, cx: &mut App) -> TimelineMessage {
    TimelineMessage::Agent(AgentMessage {
        record: crate::timeline::AgentRunRecord {
            id: Uuid::new_v4(),
            comment_group_id: None,
            started_at: SystemTime::UNIX_EPOCH,
            first_content_at: None,
            start: crate::transcript::TranscriptIndex(0),
            pending_events: Vec::new(),
            run: AgentRun::Ended {
                outcome: RunOutcome::Completed,
                duration: Duration::ZERO,
            },
            tokens: 0,
            model: None,
            awaiting_approval: None,
        },
        ui: crate::timeline::AgentMessageUi {
            output: AgentOutput::for_test(Vec::new(), text.into(), true),
            committed: Default::default(),
            comment_calls_checked: 0,
            step_views: Default::default(),
            text_view: cx.new(|cx| TextViewState::markdown(text, cx)),
            file_views: Default::default(),
            comment_responses: Vec::new(),
            work_expanded: false,
        },
    })
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
        let (_, review) = titled_thread("Beta review", Vec::new());
        ids = Some(bread_id);
        let thread_store =
            cx.new(|cx| ThreadStore::with_recents([cx.new(|_| bread), cx.new(|_| review)], cx));
        Root::new(
            cx.new(|cx| test_cowork(thread_store, None, tokio_handle, window, cx)),
            window,
            cx,
        )
    });
    let bread_id = ids.expect("threads were created");
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
    // Only the agent's reply has it.
    assert_eq!(
        found("starter twice", cx),
        [(PaletteTarget::Chat(bread_id), Some("starter twice".into()))]
    );

    // Only the prompt has it; picking it opens the chat.
    let button = cx
        .debug_bounds("search-chats")
        .expect("search button should be rendered");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    cx.simulate_input("sourdough");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(
        cowork.read_with(cx, |cowork, cx| cowork.active_thread_id(cx)),
        Some(bread_id)
    );
}

/// A chat that isn't loaded is found by its saved text once the database
/// answers, matched as a loaded chat's text is.
#[gpui::test]
fn search_palette_finds_unloaded_chats_by_saved_text(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    // The database answers from a Tokio thread.
    cx.executor().allow_parking();
    let (_test_db, db) = crate::chat_db::TestDb::open();
    let bread_id = cx.update(|_, cx| {
        let (bread_id, bread) = titled_thread("Alpha plans", vec![prompt("Bake Sourdough?")]);
        let bread = cx.new(|_| bread);
        cowork.update(cx, |cowork, cx| {
            cowork.chat_db = Some(db);
            let store = cowork.thread_store.clone();
            store.update(cx, |store, cx| store.push_back_loaded(bread.clone(), cx));
            cowork.save_thread(&bread, cx);
            futures::executor::block_on(cowork.chat_db.as_ref().unwrap().flush());
            store.update(cx, |store, cx| assert!(store.unload_for_test(bread_id, cx)));
        });
        bread_id
    });
    let found = |query: &str, cx: &mut gpui::VisualTestContext| {
        cowork.read_with(cx, |cowork, cx| {
            cowork
                .search_palette_sections(query, cx)
                .into_iter()
                .flat_map(|section| section.rows)
                .map(|row| (row.target, row.excerpt.map(|excerpt| excerpt.text)))
                .collect::<Vec<_>>()
        })
    };
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.search_saved_chats("sourdough", window, cx)
        })
    });
    assert_eq!(found("sourdough", cx), [], "not until the database answers");
    for _ in 0..200 {
        cx.run_until_parked();
        if !found("sourdough", cx).is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        found("sourdough", cx),
        [(
            PaletteTarget::Chat(bread_id),
            Some("Bake Sourdough?".into())
        )]
    );
    // Smart case, as for any text: an uppercase letter must match.
    assert_eq!(found("SOURDOUGH", cx), []);
}
