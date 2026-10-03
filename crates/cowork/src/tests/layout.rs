//! Where things are drawn: bars, buttons, and pages lining up, and mouse
//! handling that depends on layout.

use super::*;
use gpui_component::WindowExt as _;

struct MouseDragTestView {
    editor: Entity<TextareaState>,
}

impl Render for MouseDragTestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(Textarea::new(&self.editor))
    }
}

fn thread_menu_gateway(cx: &mut gpui::VisualTestContext) -> gpui::Bounds<gpui::Pixels> {
    let content = cx
        .debug_bounds("top-bar-content")
        .expect("titlebar content");
    let gateway = cx
        .debug_bounds("thread-menu-trigger")
        .expect("titlebar thread menu gateway");
    assert_eq!(content.size.height, px(40.));
    assert_eq!(gateway.size.height, px(28.));
    assert_eq!(gateway.top(), content.top() + px(6.));
    assert!(
        gateway.right() <= content.right() && gateway.right() >= content.right() - px(12.),
        "the gateway {gateway:?} must sit at the right of the titlebar"
    );
    assert!(cx.debug_bounds("peer-access-settings").is_none());
    gateway
}

#[gpui::test]
fn continue_buttons_have_room_for_their_labels(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);

    for (stage, selector) in [
        (MainStage::Welcome, "welcome-continue"),
        (
            MainStage::ProviderSetup(ProviderSetupStage::Ollama),
            "setup-continue",
        ),
    ] {
        cx.update(|_, cx| {
            cowork.update(cx, |cowork, cx| {
                cowork.main_stage = stage;
                cowork.selected_welcome_provider = Some(ModelProvider::Ollama);
                cx.notify();
            });
        });
        cx.run_until_parked();
        let button = cx.debug_bounds(selector).expect("Continue is rendered");
        assert_eq!(button.size.height, px(44.), "{selector}");
    }
}

#[gpui::test]
fn title_bar_preserves_layout_and_the_sidebar_toggle(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let content = cx
        .debug_bounds("top-bar-content")
        .expect("title bar content should be rendered");
    let toggle = cx
        .debug_bounds("top-bar-sidebar-toggle")
        .expect("sidebar toggle should be rendered");

    thread_menu_gateway(cx);
    assert!(cx.debug_bounds("toggle-sharing").is_none());
    assert!(cx.debug_bounds("copy-endpoint-id").is_none());
    assert_eq!(content.top(), px(0.));
    assert_eq!(content.size.height, crate::top_bar::TOP_BAR_HEIGHT);
    assert_eq!(toggle.left(), content.left());
    assert_eq!(toggle.center().y, content.center().y);
    assert_eq!(
        content.left(),
        px(if cfg!(target_os = "macos") { 80. } else { 8. })
    );
    assert!(cowork.read_with(cx, |cowork, _| cowork.sidebar_open));

    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(!cowork.read_with(cx, |cowork, _| cowork.sidebar_open));
    thread_menu_gateway(cx);

    let toggle = cx.debug_bounds("top-bar-sidebar-toggle").unwrap();
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cowork.read_with(cx, |cowork, _| cowork.sidebar_open));
}

#[gpui::test]
fn switching_theme_preserves_draft_identity_and_layout(cx: &mut gpui::TestAppContext) {
    use gpui_component::{Theme, ThemeMode};

    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.simulate_input("A theme-independent draft");
    cx.run_until_parked();
    let items = new_thread_items(&cowork, cx);
    let focused = focused_slot(&cowork, cx);
    let content = cx.debug_bounds("top-bar-content").unwrap();
    let bottom_bar = cx.debug_bounds("bottom-bar").unwrap();
    let identity_color =
        cowork.read_with(cx, |cowork, _| cowork.color_of(cowork.local_participant_id));

    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        cx.update(|_, cx| Theme::change(mode, None, cx));
        cx.run_until_parked();

        assert_eq!(new_thread_items(&cowork, cx), items);
        assert_eq!(focused_slot(&cowork, cx), focused);
        assert_eq!(cx.debug_bounds("top-bar-content").unwrap(), content);
        assert_eq!(cx.debug_bounds("bottom-bar").unwrap(), bottom_bar);
        assert_eq!(
            cowork.read_with(cx, |cowork, _| cowork.color_of(cowork.local_participant_id)),
            identity_color,
        );
        cx.update(|_, cx| {
            assert_eq!(cx.theme().is_dark(), mode.is_dark());
            assert!(gpui_base::TextViewDefaults::global(cx).has_code_block_highlighter());
        });
    }

    let toggle = cx.debug_bounds("top-bar-sidebar-toggle").unwrap();
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(!cowork.read_with(cx, |cowork, _| cowork.sidebar_open));
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
fn agent_work_opens_under_its_summary_with_each_step_on_its_own(cx: &mut gpui::TestAppContext) {
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
                outcome: crate::protocol::RunOutcome::Completed,
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
        let steps = vec![
            AgentStep::Thinking("Plan the sums.".into()),
            AgentStep::ToolCall(crate::timeline::AgentToolCall {
                call: call(
                    "first",
                    serde_json::json!({"a": 1, "b": 2, "operation": "add"}),
                ),
                result: Some(vec![rig::completion::message::ToolResultContent::text("3")]),
            }),
            AgentStep::ToolCall(crate::timeline::AgentToolCall {
                call: call(
                    "second",
                    serde_json::json!({"a": 3, "b": 2, "operation": "multiply"}),
                ),
                result: None,
            }),
            AgentStep::Thinking("Check them.".into()),
        ];
        message.show_output(
            crate::timeline::AgentOutput {
                steps,
                thinking_complete: true,
                text: String::new(),
            },
            cx,
        );
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
    let leak = |id: String| -> &'static str { Box::leak(id.into_boxed_str()) };
    let first_row = leak(format!("tool-call-{message_id}-1"));
    let second_row = leak(format!("tool-call-{message_id}-2"));
    let first_details = leak(format!("tool-call-details-{message_id}-1"));
    let second_details = leak(format!("tool-call-details-{message_id}-2"));
    let first_thinking = leak(format!("toggle-thinking-{message_id}-0"));
    let last_thinking = leak(format!("toggle-thinking-{message_id}-3"));
    let work_summary = leak(format!("toggle-work-{message_id}"));
    cx.run_until_parked();

    // The run has ended, so its work is collapsed under the summary.
    let summary = cx.debug_bounds(work_summary).expect("the work summary");
    assert!(cx.debug_bounds(first_thinking).is_none());
    assert!(cx.debug_bounds(first_row).is_none());
    cx.simulate_click(summary.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    // Opened, the steps show below it in order, each thinking collapsed on
    // its own.
    let summary = cx.debug_bounds(work_summary).unwrap();
    let before = cx.debug_bounds(first_thinking).expect("the first thinking");
    let after = cx.debug_bounds(last_thinking).expect("the later thinking");
    let rows = cx.debug_bounds(first_row).expect("first tool call row");
    assert!(summary.bottom() <= before.top());
    assert!(before.bottom() <= rows.top());
    assert!(cx.debug_bounds(second_row).unwrap().bottom() <= after.top());
    assert!(
        cx.debug_bounds(leak(format!("thinking-{message_id}-0")))
            .is_none()
    );
    assert!(
        cx.debug_bounds(leak(format!("thinking-{message_id}-3")))
            .is_none()
    );

    // Both calls show as rows, one right after the other, with no details.
    let first = cx.debug_bounds(first_row).expect("first tool call row");
    let second = cx.debug_bounds(second_row).expect("second tool call row");
    assert_eq!(first.bottom(), second.top());
    assert!(cx.debug_bounds(first_details).is_none());
    assert!(cx.debug_bounds(second_details).is_none());

    // Opening one call leaves the other closed.
    cx.simulate_click(second.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds(first_details).is_none());
    assert!(cx.debug_bounds(second_details).is_some());
    let expanded = |cx: &mut gpui::VisualTestContext| {
        cowork.read_with(cx, |cowork, cx| {
            let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
            let TimelineMessage::Agent(message) = &thread.read(cx).timeline[0] else {
                panic!("expected the agent message");
            };
            message
                .step_views
                .iter()
                .enumerate()
                .filter(|(_, view)| view.expanded())
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(expanded(cx), vec![2]);

    let first = cx.debug_bounds(first_row).unwrap();
    cx.simulate_click(first.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds(first_details).is_some());
    assert_eq!(expanded(cx), vec![1, 2]);

    let second = cx.debug_bounds(second_row).unwrap();
    cx.simulate_click(second.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds(first_details).is_some());
    assert!(cx.debug_bounds(second_details).is_none());
    assert_eq!(expanded(cx), vec![1]);
}

/// Text selected in a submitted user message copies while the composer
/// still has focus, as agent text does.
#[gpui::test]
fn selected_user_message_text_copies(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let thread_id = Uuid::new_v4();
    let block_id = Uuid::new_v4();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let timeline = vec![TimelineMessage::User(UserMessageGroup {
            id: Uuid::new_v4(),
            comments: Vec::new(),
            blocks: vec![PromptBlock {
                id: block_id,
                author: ParticipantId::new(),
                text: "Copy these words".into(),
                attachments: Vec::new(),
            }],
            comments_folded: false,
        })];
        let thread =
            cx.new(|_| test_thread(thread_id, timeline, ThreadDraft::new(ParticipantId::new())));
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
    cx.update(|window, _| window.activate_window());
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx)));
    cx.run_until_parked();

    let selector: &'static str =
        Box::leak(format!("timeline-user-text-{block_id}").into_boxed_str());
    let text = cx.debug_bounds(selector).expect("the user message text");
    let y = text.center().y;
    cx.simulate_mouse_down(
        point(text.left() + px(1.), y),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(text.right() - px(1.), y),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(text.right() - px(1.), y),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-c");
    cx.run_until_parked();

    assert_eq!(
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("Copy these words")
    );
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

    assert_eq!(
        cowork.read_with(cx, |cowork, _| cowork.main_stage),
        MainStage::Profile
    );
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
fn participants_sit_beside_the_right_aligned_thread_menu_without_an_extra_access_row(
    cx: &mut gpui::TestAppContext,
) {
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
        assert!(thread.start_hosting(endpoint));
        thread.apply_for_test(joined(ParticipantId::new()), cx);
        thread.apply_for_test(joined(ParticipantId::new()), cx);
        let thread = cx.new(|_| thread);
        let thread_store = cx.new(|_| ThreadStore {
            threads: VecDeque::from([thread]),
        });
        let cowork =
            cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
        Root::new(cowork, window, cx)
    });
    cx.update(|window, _| window.activate_window());
    cx.update(|window, cx| window.draw(cx).clear(cx));

    let participants = cx
        .debug_bounds("participants")
        .expect("participants should be rendered");
    let gateway = thread_menu_gateway(cx);
    let content = cx.debug_bounds("top-bar-content").unwrap();
    let bottom_bar = cx.debug_bounds("bottom-bar").expect("main bottom bar");
    let composer = cx.debug_bounds("composer").expect("shared thread composer");
    assert!(cx.debug_bounds("copy-endpoint-id").is_none());
    assert!(cx.debug_bounds("toggle-sharing").is_none());
    assert!(cx.debug_bounds("thread-sharing-menu").is_none());

    assert!(
        participants.size.width >= px(24. * 3. - 6. * 2.),
        "three overlapping avatars need room, got {participants:?}"
    );
    assert!(
        participants.right() <= gateway.left(),
        "participants {participants:?} overlap the gateway {gateway:?}"
    );
    assert!(participants.right() <= content.right());
    assert_eq!(participants.center().y, content.center().y);

    cx.simulate_click(gateway.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let panel = cx
        .debug_bounds("thread-sharing-menu")
        .expect("thread sharing popover");
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(cx.debug_bounds("close-peer-access").is_none());
    assert!(
        panel.size.width >= px(280.) && panel.size.width <= px(320.),
        "compact panel: {panel:?}"
    );
    assert!(
        panel.top() >= gateway.bottom(),
        "the panel opens below its titlebar anchor"
    );
    assert!(panel.top() <= content.bottom() + px(16.));
    let viewport = cx.update(|window, _| window.viewport_size());
    assert!(panel.left() >= px(0.) && panel.top() >= px(0.));
    assert!(panel.right() <= viewport.width && panel.bottom() <= viewport.height);
    for selector in ["copy-endpoint-id", "toggle-sharing"] {
        let action = cx
            .debug_bounds(selector)
            .expect("sharing action belongs in the menu");
        assert!(action.left() >= panel.left() && action.right() <= panel.right());
        assert!(action.top() >= panel.top() && action.bottom() <= panel.bottom());
    }
    assert_eq!(cx.debug_bounds("top-bar-content").unwrap(), content);
    assert_eq!(thread_menu_gateway(cx), gateway);
    assert_eq!(cx.debug_bounds("participants").unwrap(), participants);
    assert_eq!(cx.debug_bounds("bottom-bar").unwrap(), bottom_bar);
    assert_eq!(cx.debug_bounds("composer").unwrap(), composer);

    // Returning to a local thread must not remove a permissions row and move
    // the editor: sharing and permissions live entirely in the popover.
    let stop_sharing = cx.debug_bounds("toggle-sharing").unwrap();
    cx.simulate_click(stop_sharing.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("participants").is_none());
    assert!(cx.debug_bounds("copy-endpoint-id").is_none());
    assert_eq!(cx.debug_bounds("top-bar-content").unwrap(), content);
    assert_eq!(cx.debug_bounds("bottom-bar").unwrap(), bottom_bar);
    assert_eq!(cx.debug_bounds("composer").unwrap(), composer);

    let gateway = thread_menu_gateway(cx);
    cx.simulate_click(gateway.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let local_panel = cx.debug_bounds("thread-sharing-menu").unwrap();
    assert_eq!(local_panel.size.width, panel.size.width);
    for selector in ["copy-endpoint-id", "default-mode-track", "toggle-sharing"] {
        assert!(cx.debug_bounds(selector).is_some(), "missing {selector}");
    }
}

#[gpui::test]
fn unshared_menu_can_configure_access_before_starting_sharing(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let draft_id = cowork.read_with(cx, |cowork, _| cowork.new_thread_draft.id);
    let gateway = thread_menu_gateway(cx);
    cx.simulate_click(gateway.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in ["copy-endpoint-id", "default-mode-track", "toggle-sharing"] {
        assert!(cx.debug_bounds(selector).is_some(), "missing {selector}");
    }
    let read_only = cx.debug_bounds("default-Read only").unwrap();
    cx.simulate_click(read_only.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).expect("configured local thread");
        let thread = thread.read(cx);
        assert_eq!(thread.draft().id, draft_id);
        assert_eq!(
            thread.peer_permissions().default_mode(),
            crate::thread::PeerMode::ReadOnly
        );
        assert!(matches!(
            thread.sharing.status(),
            crate::thread::SharingStatus::NotShared
        ));
    });
}

#[gpui::test]
fn starting_sharing_keeps_the_menu_open_for_new_and_existing_threads(
    cx: &mut gpui::TestAppContext,
) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    for existing_thread in [false, true] {
        cx.update(|_, cx| {
            cowork.update(cx, |cowork, cx| {
                if existing_thread {
                    cowork.prepare_thread_for_sharing(cx);
                }
            });
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let gateway = thread_menu_gateway(cx);
        cx.simulate_click(gateway.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let share = cx.debug_bounds("toggle-sharing").unwrap();
        cx.simulate_click(share.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("thread-sharing-menu").is_some());
        assert!(cx.debug_bounds("copy-endpoint-id").is_some());
        cowork.read_with(cx, |cowork, cx| {
            let thread = cowork.active_thread(cx).unwrap();
            assert!(matches!(
                thread.read(cx).sharing.status(),
                crate::thread::SharingStatus::Sharing
            ));
        });
        // Close the popover and return to an unshared thread for the next case.
        cx.simulate_click(gateway.center(), gpui::Modifiers::default());
        cx.update(|_, cx| {
            cowork.update(cx, |cowork, cx| {
                cowork.active_thread(cx).unwrap().update(cx, |thread, _| {
                    thread.sharing = crate::thread::ThreadSharing::NotShared;
                });
                cx.notify();
            });
        });
        cx.run_until_parked();
    }
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
