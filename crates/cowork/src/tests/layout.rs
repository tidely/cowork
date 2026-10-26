//! Where things are drawn: bars, buttons, and pages lining up, and mouse
//! handling that depends on layout.

use super::*;

#[gpui::test]
fn inline_compaction_status_becomes_persistent_dividers(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let message_id = Uuid::new_v4();
    // The run's answer is its step 0; the summaries follow it.
    let first_marker: &'static str =
        Box::leak(format!("compaction-marker-{message_id}-1").into_boxed_str());
    let second_marker: &'static str =
        Box::leak(format!("compaction-marker-{message_id}-2").into_boxed_str());
    let host = Rc::new(RefCell::new(ScriptedHost::default()));
    let event = |id: Uuid, event: protocol::RunEvent| protocol::HostMessage::AgentEvent {
        id: id.into_bytes(),
        event,
    };
    cx.update(|_, cx| {
        cowork.update(cx, |cowork, cx| {
            let thread_id = Uuid::new_v4();
            let thread = cx.new(|cx| {
                let mut thread = test_thread(
                    thread_id,
                    Vec::new(),
                    ThreadDraft::new(ParticipantId::new()),
                );
                let mut host = host.borrow_mut();
                let mut events = host.start(message_id, &"Request ".repeat(20));
                events.extend(host.turn(
                    message_id,
                    [streamed_text("answer", &"Answer ".repeat(20), false)],
                    100,
                ));
                events.push(event(message_id, protocol::RunEvent::CompactionStarted));
                for event in events {
                    thread.apply_for_test(event, cx);
                }
                thread
            });
            cowork
                .thread_store
                .update(cx, |store, _| store.threads.push_back(thread));
            cowork.active_thread_id = Some(thread_id);
            cx.notify();
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("compacting-context").is_some());
    assert!(cx.debug_bounds(first_marker).is_none());

    // Successive accepted summaries at the same boundary both remain visible,
    // even with work collapsed and while another summary runs.
    cx.update(|_, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.active_thread(cx).unwrap().update(cx, |thread, cx| {
                let mut host = host.borrow_mut();
                // The first summary is the one already running.
                let mut events = host.compaction(message_id, 1, "Summary");
                events.remove(0);
                events.extend(host.compaction(message_id, 2, "Summary"));
                for event in events {
                    thread.apply_for_test(event, cx);
                }
                thread
                    .agent_message_mut(message_id.into_bytes())
                    .unwrap()
                    .ui
                    .work_expanded = false;
                thread.apply_for_test(event(message_id, protocol::RunEvent::CompactionStarted), cx);
            });
            cx.notify();
        })
    });
    cx.run_until_parked();
    let first = cx.debug_bounds(first_marker).unwrap();
    let second = cx.debug_bounds(second_marker).unwrap();
    let running = cx.debug_bounds("compacting-context").unwrap();
    assert!(first.bottom() <= second.top());
    assert!(second.bottom() <= running.top());

    cx.update(|_, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.active_thread(cx).unwrap().update(cx, |thread, cx| {
                let host = host.borrow();
                thread.apply_for_test(host.ended(message_id, protocol::RunOutcome::Stopped), cx);
            });
            cx.notify();
        })
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("compacting-context").is_none());
    assert!(cx.debug_bounds(first_marker).is_some());
    assert!(cx.debug_bounds(second_marker).is_some());

    // Starting another run leaves the mid-run dividers in their original
    // response, above the next submitted prompt.
    let block_id = Uuid::new_v4();
    let next_message_id = Uuid::new_v4();
    let next_marker: &'static str =
        Box::leak(format!("compaction-marker-{next_message_id}-1").into_boxed_str());
    cx.update(|_, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.active_thread(cx).unwrap().update(cx, |thread, cx| {
                let group = UserMessageGroup {
                    id: Uuid::new_v4(),
                    comments: Vec::new(),
                    comments_folded: false,
                    blocks: vec![PromptBlock {
                        id: block_id,
                        author: thread.participant_id(),
                        text: "Next request".into(),
                        attachments: Vec::new(),
                    }],
                };
                thread.timeline.push(TimelineMessage::User(group.into()));
                let mut host = host.borrow_mut();
                let mut events = host.start(next_message_id, "Next request");
                events.extend(host.turn(
                    next_message_id,
                    [streamed_text("answer", "Next answer", false)],
                    100,
                ));
                events.extend(host.compaction(next_message_id, 4, "Later summary"));
                events.push(host.ended(next_message_id, protocol::RunOutcome::Completed));
                for event in events {
                    thread.apply_for_test(event, cx);
                }
            });
            cx.notify();
        });
    });
    cx.run_until_parked();
    let user_selector = Box::leak(format!("timeline-user-text-{block_id}").into_boxed_str());
    let prompt = cx.debug_bounds(user_selector).unwrap();
    assert!(cx.debug_bounds(first_marker).unwrap().bottom() <= prompt.top());
    assert!(cx.debug_bounds(second_marker).unwrap().bottom() <= prompt.top());
    // Both runs have a marker at folded step 1; their UUIDs keep them distinct.
    assert!(cx.debug_bounds(next_marker).unwrap().top() >= prompt.bottom());
}

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
    assert!(
        gateway.right() <= content.right() && gateway.right() >= content.right() - px(12.),
        "the gateway {gateway:?} must sit at the right of the titlebar"
    );
    assert!(cx.debug_bounds("peer-access-settings").is_none());
    gateway
}

#[gpui::test]
fn the_model_picker_opens_from_its_button_not_the_gap(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.run_until_parked();
    let trigger = cx
        .debug_bounds("model-picker-trigger")
        .expect("model picker trigger");
    let project_mode = cx.debug_bounds("project-mode").expect("project mode");

    let gap = gpui::point(
        (project_mode.right() + trigger.left()) / 2.,
        trigger.center().y,
    );
    cx.simulate_click(gap, gpui::Modifiers::none());
    cx.simulate_input("qwen");
    cx.run_until_parked();
    assert!(cowork.read_with(cx, |cowork, cx| {
        cowork.model_picker.read(cx).query(cx).is_empty()
    }));

    cx.simulate_click(trigger.center(), gpui::Modifiers::none());
    cx.simulate_input("qwen");
    cx.run_until_parked();
    assert_eq!(
        cowork.read_with(cx, |cowork, cx| cowork.model_picker.read(cx).query(cx)),
        "qwen"
    );
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

    let sharing_button = thread_menu_gateway(cx);
    assert!(
        sharing_button.size.width > sharing_button.size.height * 2.,
        "the sharing button must fit its icon and label, not constrain them to an icon-sized square: {sharing_button:?}"
    );
    assert!(cx.debug_bounds("toggle-sharing").is_none());
    assert!(cx.debug_bounds("copy-endpoint-id").is_none());
    assert!(content.contains(&toggle.center()));
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
            crate::transcript::TranscriptIndex(0),
            crate::protocol::AgentRun::Ended {
                outcome: crate::protocol::RunOutcome::Completed,
                duration: Duration::from_secs(1),
            },
            Vec::new(),
            cx,
        );
        let call = crate::timeline::tool_call;
        let steps = vec![
            AgentStep::Thinking("Plan the sums.".into()),
            AgentStep::ToolCall(crate::timeline::AgentToolCall {
                call: call(
                    "first",
                    "run_command",
                    serde_json::json!({"command": "echo \"café\"; ".repeat(100)}),
                ),
                result: Some(vec![agent::model::ToolResultContent::Text {
                    text: "3".into(),
                }]),
                failed: false,
            }),
            AgentStep::ToolCall(crate::timeline::AgentToolCall {
                call: call(
                    "second",
                    "calculate",
                    serde_json::json!({"a": 3, "b": 2, "operation": "multiply"}),
                ),
                result: None,
                failed: false,
            }),
            AgentStep::Thinking("Check them.".into()),
        ];
        message.show_output(
            crate::timeline::AgentOutput::for_test(steps, String::new(), true),
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
            ..Default::default()
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

    // Both calls show as rows, with no details.
    let first = cx.debug_bounds(first_row).expect("first tool call row");
    let second = cx.debug_bounds(second_row).expect("second tool call row");
    assert_eq!(
        first.size.height,
        px(20.),
        "a long command stays on one row"
    );
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
                .ui
                .views()
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
        let timeline = vec![TimelineMessage::User(
            UserMessageGroup {
                id: Uuid::new_v4(),
                comments: Vec::new(),
                blocks: vec![PromptBlock {
                    id: block_id,
                    author: ParticipantId::new(),
                    text: "Copy these words".into(),
                    attachments: Vec::new(),
                }],
                comments_folded: false,
            }
            .into(),
        )];
        let thread =
            cx.new(|_| test_thread(thread_id, timeline, ThreadDraft::new(ParticipantId::new())));
        let thread_store = cx.new(|_| ThreadStore {
            threads: VecDeque::from([thread]),
            ..Default::default()
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

    cx.simulate_keystrokes(cfg_select! {
        target_os = "macos" => "cmd-c",
        _ => "ctrl-c",
    });
    cx.run_until_parked();

    assert_eq!(
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("Copy these words")
    );
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
        MainStage::Settings(SettingsPage::Profile)
    );
    assert!(cx.debug_bounds("profile-page").is_some());
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
        cowork.deleted.token_activity.push(TokenActivity {
            at: SystemTime::now(),
            duration: Duration::ZERO,
            tokens: 1_200,
        });
        cowork.profile_statistics = None;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("token-activity-peak").is_some());

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
fn settings_replace_the_sidebar_until_back_to_app(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update(cx, |cowork, cx| {
        cowork.main_stage = MainStage::Schedules;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("search-chats").is_some());
    assert!(cx.debug_bounds("settings-back").is_none());

    let button = cx
        .debug_bounds("identity-button")
        .expect("identity button should be rendered");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cowork.read_with(cx, |cowork, _| cowork.main_stage),
        MainStage::Settings(SettingsPage::Profile)
    );
    assert!(cx.debug_bounds("profile-page").is_some());
    assert!(cx.debug_bounds("search-chats").is_none());
    assert!(cx.debug_bounds("identity-button").is_some());

    // Opening it again from settings still goes back to where the user was.
    let button = cx.debug_bounds("identity-button").unwrap();
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let back = cx
        .debug_bounds("settings-back")
        .expect("back to app should be rendered");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cowork.read_with(cx, |cowork, _| cowork.main_stage),
        MainStage::Schedules
    );
    assert!(cx.debug_bounds("settings-back").is_none());
    assert!(cx.debug_bounds("search-chats").is_some());
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
            ..Default::default()
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
