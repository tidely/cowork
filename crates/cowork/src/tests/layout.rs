//! Where things are drawn: bars, buttons, and pages lining up, and mouse
//! handling that depends on layout.

use super::*;

struct MouseDragTestView {
    editor: Entity<TextareaState>,
}

impl Render for MouseDragTestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(Textarea::new(&self.editor))
    }
}

#[gpui::test]
fn sidebar_bottom_bar_lines_up_with_the_main_bottom_bar(cx: &mut gpui::TestAppContext) {
    let (_cowork, _runtime, cx) = composer_test_cowork(cx);

    let sidebar_bar = cx
        .debug_bounds("sidebar-bottom-bar")
        .expect("sidebar bottom bar should be rendered");
    let main_bar = cx
        .debug_bounds("bottom-bar")
        .expect("main bottom bar should be rendered");

    assert_eq!(sidebar_bar.origin.y, main_bar.origin.y);
    assert_eq!(sidebar_bar.size.height, main_bar.size.height);
    assert_eq!(sidebar_bar.origin.x, px(0.));
    assert_eq!(sidebar_bar.size.width, SIDEBAR_WIDTH);

    let button = cx
        .debug_bounds("identity-button")
        .expect("identity button should be rendered");
    let content = cx
        .debug_bounds("identity-button-content")
        .expect("identity button content should be rendered");
    assert_eq!(button.left(), sidebar_bar.left() + px(4.));
    assert_eq!(button.right(), sidebar_bar.right() - px(4.));
    assert_eq!(content.left(), button.left() + px(6.));
}

#[gpui::test]
fn tool_calls_expand_and_collapse_in_the_timeline(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let thread_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let mut message = AgentMessage::new(
            message_id,
            None,
            SystemTime::UNIX_EPOCH,
            0,
            crate::protocol::AgentRun::Ended {
                failure: None,
                duration: Duration::from_secs(1),
            },
            Vec::new(),
            cx,
        );
        let call = |id: &str, arguments: serde_json::Value| {
            rig::message::ToolCall::new(
                rig::message::ToolCallId::new(id).expect("a valid id"),
                rig::message::ToolFunction::new("calculate".into(), arguments),
            )
        };
        message.output.tool_calls = vec![
            crate::timeline::AgentToolCall {
                call: call(
                    "first",
                    serde_json::json!({"a": 1, "b": 2, "operation": "add"}),
                ),
                result: Some(vec![rig::completion::message::ToolResultContent::text("3")]),
            },
            crate::timeline::AgentToolCall {
                call: call(
                    "second",
                    serde_json::json!({"a": 3, "b": 2, "operation": "multiply"}),
                ),
                result: None,
            },
        ];
        let thread = cx.new(|_| {
            test_thread(
                thread_id,
                vec![TimelineMessage::Agent(message)],
                ThreadDraft::new(ParticipantId::new()),
            )
        });
        let thread_store = cx.new(|_| ThreadStore {
            threads: VecDeque::from([thread]),
        });
        Root::new(
            cx.new(|cx| {
                test_cowork(
                    thread_store,
                    Some(thread_id),
                    runtime.handle().clone(),
                    window,
                    cx,
                )
            }),
            window,
            cx,
        )
    });
    let cowork = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<Cowork>().unwrap()
    });
    let header_id: &'static str =
        Box::leak(format!("toggle-tool-calls-{message_id}").into_boxed_str());
    let first_id: &'static str = Box::leak(format!("tool-call-{message_id}-0").into_boxed_str());
    let second_id: &'static str = Box::leak(format!("tool-call-{message_id}-1").into_boxed_str());
    cx.run_until_parked();
    let header = cx.debug_bounds(header_id).expect("tool call summary");
    assert!(cx.debug_bounds(first_id).is_none());
    cx.simulate_click(header.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds(first_id).is_some());
    assert!(cx.debug_bounds(second_id).is_some());
    assert!(cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        matches!(&thread.read(cx).timeline[0], TimelineMessage::Agent(message) if message.tool_calls_expanded)
    }));
    let header = cx.debug_bounds(header_id).unwrap();
    cx.simulate_click(header.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds(first_id).is_none());
}

#[gpui::test]
fn context_indicator_sits_left_of_the_model_picker(cx: &mut gpui::TestAppContext) {
    let (_cowork, _runtime, cx) = composer_test_cowork(cx);

    let indicator = cx
        .debug_bounds("context-indicator")
        .expect("context indicator should be rendered");
    let picker = cx
        .debug_bounds("model-picker")
        .expect("model picker should be rendered");

    assert!(indicator.right() <= picker.left());
    assert!(indicator.top() < picker.bottom() && picker.top() < indicator.bottom());
}

#[gpui::test]
fn the_profile_button_opens_the_profile_page(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);

    let button = cx
        .debug_bounds("identity-button")
        .expect("identity button should be rendered");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    assert!(cowork.read_with(cx, |cowork, _| cowork.profile_open));
    assert!(cx.debug_bounds("profile-page").is_some());
    assert!(cx.debug_bounds("profile-picture").is_some());
    assert!(cx.debug_bounds("usage-stats").is_some());
    assert!(cx.debug_bounds("token-activity").is_some());
    assert!(cx.debug_bounds("bottom-bar").is_none());

    // The chart starts on the last hour, and the options switch it.
    assert_eq!(
        cowork.read_with(cx, |cowork, _| cowork.activity_range),
        ActivityRange::Hour
    );
    let one_day = cx
        .debug_bounds("activity-range-Day")
        .expect("Day option should be rendered");
    cx.simulate_click(one_day.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cowork.read_with(cx, |cowork, _| cowork.activity_range),
        ActivityRange::Day
    );

    // The peak is marked once there is any activity.
    assert!(cx.debug_bounds("token-activity-peak").is_none());
    cowork.update(cx, |cowork, cx| {
        cowork.token_activity.push(TokenActivity {
            at: SystemTime::now(),
            duration: Duration::ZERO,
            tokens: 1_200,
        });
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("token-activity-peak").is_some());
    let title = cx
        .debug_bounds("token-activity-title")
        .expect("title should be rendered");
    let peak_label = cx
        .debug_bounds("token-activity-peak-label")
        .expect("peak label should be rendered");
    // It reads as a caption to the title rather than a part of the plot.
    let gap = peak_label.top() - title.bottom();
    assert!(gap >= px(0.) && gap <= px(8.), "gap is {gap:?}");

    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            let name = cx.new(|cx| InputState::new(window, cx).default_value("  Ada  "));
            assert!(cowork.save_profile_name(&name, cx));
            assert_eq!(cowork.profile_name(), "Ada");

            let blank = cx.new(|cx| InputState::new(window, cx).default_value("  "));
            assert!(!cowork.save_profile_name(&blank, cx));
            assert_eq!(cowork.profile_name(), "Ada");
        });
    });
}

#[gpui::test]
fn participants_sit_beside_the_copy_link_button(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let endpoint = runtime
        .block_on(Endpoint::builder(presets::Minimal).bind())
        .expect("bind endpoint");
    let tokio_handle = runtime.handle().clone();
    let thread_id = Uuid::new_v4();
    let (_, cx) = cx.add_window_view(|window, cx| {
        let draft = ThreadDraft::new(ParticipantId::new());
        let mut thread = test_thread(thread_id, Vec::new(), draft);
        thread.participants = vec![
            thread.participant_id,
            ParticipantId::new(),
            ParticipantId::new(),
        ];
        thread.sharing = ThreadSharing::Shared {
            endpoint,
            events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
        };
        let thread = cx.new(|_| thread);
        let thread_store = cx.new(|_| ThreadStore {
            threads: VecDeque::from([thread]),
        });
        let cowork =
            cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
        Root::new(cowork, window, cx)
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));

    let participants = cx
        .debug_bounds("participants")
        .expect("participants should be rendered");
    let copy_button = cx
        .debug_bounds("copy-endpoint-id")
        .expect("copy link button should be rendered");

    assert!(
        participants.size.width >= px(24. * 3. - 6. * 2.),
        "three overlapping avatars need room, got {participants:?}"
    );
    assert!(
        participants.right() <= copy_button.left(),
        "participants {participants:?} overlap the copy link button {copy_button:?}"
    );
}

#[gpui::test]
fn synthetic_mouse_up_ends_a_stale_text_drag(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = TextareaState::new(window, cx);
            editor.set_value("selectable text", window, cx);
            editor
        });
        editor.focus_handle(cx).focus(window, cx);
        MouseDragTestView { editor }
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));

    cx.simulate_mouse_down(
        gpui::point(px(8.), px(8.)),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    cx.update(Cowork::end_stale_mouse_drag);
    cx.simulate_mouse_move(
        gpui::point(px(120.), px(8.)),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );

    assert!(view.read_with(cx, |view, cx| {
        view.editor.read(cx).selected_range().is_empty()
    }));
}
