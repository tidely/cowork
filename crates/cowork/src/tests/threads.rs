//! Thread state: titles, ownership, applying host events, context usage,
//! and requests a collaborator sends the host.

use super::*;

#[test]
fn thread_titles_normalize_whitespace_and_truncate_by_character() {
    assert_eq!(
        Cowork::thread_title("  Collaborate\non\tthis prompt  "),
        "Collaborate on this prompt"
    );
    let hundred = "1234567890".repeat(10);
    assert_eq!(Cowork::thread_title(&hundred), hundred);
    // Cut without an ellipsis: the sidebar fades the title out instead.
    assert_eq!(Cowork::thread_title(&format!("{hundred} more")), hundred);
    assert_eq!(
        Cowork::thread_title("🦀".repeat(101).as_str()),
        "🦀".repeat(100)
    );
    // A cut right after a word doesn't leave a trailing space.
    let ninety_nine = "x".repeat(99);
    assert_eq!(
        Cowork::thread_title(&format!("{ninety_nine} more")),
        ninety_nine
    );
}

/// An empty local thread, in a window as the app's are, to apply host
/// events to.
fn empty_thread(
    cx: &mut gpui::TestAppContext,
    models: Arc<ModelCatalog>,
) -> (Entity<Thread>, &mut gpui::VisualTestContext) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|_, cx| EmptyThreadTestView {
        thread: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            models,
            None,
            cx,
        ),
    });
    let thread = view.read_with(cx, |view, _| view.thread.clone());
    (thread, cx)
}

fn agent_ended(id: Uuid, outcome: protocol::RunOutcome) -> protocol::HostMessage {
    ScriptedHost::default().ended(id, outcome)
}

/// Whether `event` previews text of the answer.
fn previews_text(event: &protocol::HostMessage) -> bool {
    matches!(
        event,
        protocol::HostMessage::AgentEvent {
            event: protocol::RunEvent::Preview(protocol::Preview {
                delta: protocol::PreviewDelta::Text(_),
                ..
            }),
            ..
        }
    )
}

/// The messages a host sends while answering one prompt: a reply that
/// thinks, answers, and replies to a comment with a tool call, then the
/// tool's result and a second reply.
fn agent_stream_events(host: &mut ScriptedHost, message_id: Uuid) -> Vec<protocol::HostMessage> {
    let user_message_id = Uuid::new_v4().into_bytes();
    let comment_id = Uuid::new_v4().into_bytes();
    let mut events = vec![
        protocol::HostMessage::ThreadTitled("Explain this".into()),
        protocol::HostMessage::UserMessage(protocol::UserMessage {
            id: user_message_id,
            blocks: vec![protocol::PromptBlock {
                id: Uuid::new_v4().into_bytes(),
                author: ParticipantId::new().into_bytes(),
                text: "Explain this".into(),
                attachments: Vec::new(),
            }],
            comments: vec![protocol::UserComment {
                id: comment_id,
                author: ParticipantId::new().into_bytes(),
                reference: protocol::CommentReference {
                    message_id: Uuid::new_v4().into_bytes(),
                    range: 0..10,
                    quote: "an excerpt".into(),
                },
                body: "why?".into(),
            }],
        }),
        host.started(message_id, Some(user_message_id)),
        host.prompt(message_id, "Explain this"),
    ];
    events.extend(host.turn(
        message_id,
        [
            streamed_text("thinking", "Weighing ", true),
            streamed_text("thinking", "options.", true),
            streamed_text("answer", "Here is ", false),
            streamed_text("answer", "the answer.", false),
            streamed_tool_call(
                "call",
                "respond_to_comment",
                serde_json::json!({"comment_id": "comment_1", "response": "Because of this."}),
            ),
        ],
        1_024,
    ));
    events.extend(host.results(message_id, "Recorded"));
    events.extend(host.turn(
        message_id,
        [streamed_text("second", " Done.", false)],
        2_048,
    ));
    events.extend([
        protocol::HostMessage::AgentEnded {
            id: message_id.into_bytes(),
            outcome: crate::protocol::RunOutcome::Completed,
            duration: Duration::from_secs(5),
        },
        joined(ParticipantId::new()),
        protocol::HostMessage::ModelCatalogChanged({
            let mut catalog = ModelCatalog::default();
            catalog.set_provider(
                ModelProvider::Ollama,
                [(
                    ollama_qwen().id,
                    ModelInfo {
                        name: "Other".into(),
                        max_tokens: 65_536,
                        max_output_tokens: None,
                        image_input: None,
                        reasoning: crate::models::Reasoning::Default,
                    },
                )],
            );
            catalog
        }),
        protocol::HostMessage::ModelSelected(ollama_qwen()),
    ]);
    events
}

/// Someone joining after any event of two runs ends up with the host's
/// thread: the snapshot shows what the host showed then, deriving it from
/// the transcript and pending previews where the host showed it
/// incrementally, and the events that follow bring them to where the host
/// ends up. The second run's reply calls two tools, answered one at a time.
#[gpui::test]
fn joining_after_any_event_converges_on_the_host(cx: &mut gpui::TestAppContext) {
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let mut host_script = ScriptedHost::default();
    let mut events = agent_stream_events(&mut host_script, first_id);
    events.extend(host_script.start(second_id, "Calculate both"));
    events.extend(host_script.turn(
        second_id,
        [
            streamed_text("thinking", "Two sums.", true),
            streamed_tool_call(
                "first",
                "calculate",
                serde_json::json!({"operation": "add", "a": 1, "b": 2}),
            ),
            streamed_tool_call(
                "second",
                "calculate",
                serde_json::json!({"operation": "add", "a": 3, "b": 4}),
            ),
        ],
        300,
    ));
    events.extend(host_script.results(second_id, "sum"));
    events.extend(host_script.turn(
        second_id,
        [
            streamed_text("answer", "3 and ", false),
            streamed_text("answer", "7.", false),
        ],
        400,
    ));
    events.push(agent_ended(second_id, protocol::RunOutcome::Completed));

    let collaborator = ParticipantId::new();
    let (host, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        host.update(cx, |thread, cx| {
            thread.apply_for_test(joined(thread.participant_id()), cx);
            thread.apply_for_test(joined(collaborator), cx);
        });
        let mut joiners = Vec::new();
        for index in 0..=events.len() {
            let mut welcome = host.read(cx).welcome_for_test();
            welcome.participant_id = collaborator.into_bytes();
            let joiner = cx.new(|cx| Thread::join_for_test(welcome, cx));
            assert_eq!(
                joiner.read(cx).conversation().agent_output,
                host.read(cx).conversation().agent_output,
                "joined before event {index}"
            );
            joiners.push(joiner);
            let Some(event) = events.get(index) else {
                break;
            };
            host.update(cx, |thread, cx| thread.apply_for_test(event.clone(), cx));
            for joiner in &joiners {
                joiner.update(cx, |thread, cx| thread.apply_for_test(event.clone(), cx));
            }
        }

        let host = host.read(cx);
        for (index, joiner) in joiners.iter().enumerate() {
            let joiner = joiner.read(cx);
            assert_eq!(
                joiner.conversation(),
                host.conversation(),
                "joined at {index}"
            );
            assert_eq!(joiner.transcript, host.transcript, "joined at {index}");
            assert_eq!(joiner.participants(), host.participants());
            assert_eq!(joiner.max_tokens(), 65_536);
        }

        // What everyone converged on is the runs' output.
        let TimelineMessage::Agent(first) = &host.timeline[1] else {
            panic!("expected the first run's message");
        };
        let calls = first.ui.output.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call.name, "respond_to_comment");
        assert_eq!(calls[0].result_text().as_deref(), Some("Recorded"));
        assert_eq!(first.ui.comment_responses.len(), 1);
        let TimelineMessage::Agent(second) = &host.timeline[2] else {
            panic!("expected the second run's message");
        };
        assert_eq!(
            second.ui.output.thinking().collect::<Vec<_>>(),
            ["Two sums."]
        );
        assert_eq!(second.ui.output.text, "3 and 7.");
        let calls = second.ui.output.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|call| call.result_text().as_deref() == Some("sum"))
        );
    });
}

/// A run that fails mid-reply keeps what it previewed, which the transcript
/// never got, for joiners too.
#[gpui::test]
fn failed_run_keeps_output_the_transcript_never_got_for_joiners(cx: &mut gpui::TestAppContext) {
    let message_id = Uuid::new_v4();
    let mut host = ScriptedHost::default();
    let events = agent_stream_events(&mut host, message_id);
    // Through the first reply's previews, before it joins the transcript.
    let previewed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                protocol::HostMessage::AgentEvent {
                    event: protocol::RunEvent::Usage { .. },
                    ..
                }
            )
        })
        .unwrap();
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in events.iter().take(previewed) {
                thread.apply_for_test(event.clone(), cx);
            }
            thread.apply_for_test(
                agent_ended(
                    message_id,
                    protocol::RunOutcome::Failed("stream failed".into()),
                ),
                cx,
            );
        });
        let welcome = thread.read(cx).welcome_for_test();
        let protocol::TimelineMessage::Agent(message) = &welcome.thread.messages[1] else {
            panic!("expected agent message");
        };
        // The reply never completed, so all of it is pending.
        assert_eq!(
            message.pending_events.len(),
            events[..previewed]
                .iter()
                .filter(|event| matches!(
                    event,
                    protocol::HostMessage::AgentEvent {
                        event: protocol::RunEvent::Preview(_),
                        ..
                    }
                ))
                .count()
        );
        let restored = Thread::join_for_test(welcome, cx);
        assert_eq!(restored.conversation(), thread.read(cx).conversation());
        let TimelineMessage::Agent(restored) = &restored.timeline[1] else {
            panic!("expected agent message");
        };
        assert_eq!(restored.record.run.failure(), Some("stream failed"));
        assert_eq!(restored.ui.output.text, "Here is the answer.");
        assert_eq!(restored.ui.output.tool_calls().count(), 0);
    });
}

/// A snapshot whose agent messages do not fit its transcript is rejected
/// before anything is derived from it.
#[gpui::test]
fn snapshots_with_agent_runs_the_transcript_cannot_hold_are_rejected(
    cx: &mut gpui::TestAppContext,
) {
    let message_id = Uuid::new_v4();
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in agent_stream_events(&mut ScriptedHost::default(), message_id) {
                thread.apply_for_test(event, cx);
            }
        });
        let welcome = thread.read(cx).welcome_for_test();
        Thread::validate_welcome(&welcome).expect("the host's own snapshot");
        let with_agent_message = |change: &dyn Fn(&mut protocol::AgentMessage)| {
            let mut welcome = welcome.clone();
            for message in &mut welcome.thread.messages {
                if let protocol::TimelineMessage::Agent(message) = message {
                    change(message);
                }
            }
            Thread::validate_welcome(&welcome)
        };

        assert!(
            with_agent_message(&|message| message.start = crate::transcript::TranscriptIndex(99))
                .is_err()
        );
        // An ended run may keep previews; a call it never made can't wait.
        let preview = protocol::Preview {
            index: 0,
            delta: protocol::PreviewDelta::Text("More".into()),
        };
        assert!(
            with_agent_message(&|message| message.pending_events = vec![preview.clone()]).is_ok()
        );
        assert!(
            with_agent_message(&|message| {
                message.run = protocol::AgentRun::Generating;
                message.awaiting_approval = Some(agent::model::ToolCallId("missing".into()));
            })
            .is_err()
        );

        // Only the last run can still be generating.
        let mut two_generating = welcome.clone();
        let first = two_generating
            .thread
            .messages
            .iter()
            .find(|message| matches!(message, protocol::TimelineMessage::Agent(_)))
            .cloned()
            .expect("an agent message");
        let protocol::TimelineMessage::Agent(mut second) = first.clone() else {
            unreachable!();
        };
        second.id = Uuid::new_v4().into_bytes();
        second.start = thread.read(cx).transcript.end();
        second.pending_events.clear();
        let protocol::TimelineMessage::Agent(mut first) = first else {
            unreachable!();
        };
        first.run = protocol::AgentRun::Generating;
        two_generating
            .thread
            .messages
            .retain(|message| !matches!(message, protocol::TimelineMessage::Agent(_)));
        two_generating
            .thread
            .messages
            .push(protocol::TimelineMessage::Agent(first));
        two_generating
            .thread
            .messages
            .push(protocol::TimelineMessage::Agent(second));
        assert!(Thread::validate_welcome(&two_generating).is_err());

        let mut invalid_json = welcome;
        invalid_json.thread.transcript = postcard::from_bytes(
            &postcard::to_stdvec("not json").expect("encode invalid JSON string"),
        )
        .expect("decode an untrusted transcript");
        thread.update(cx, |thread, cx| {
            let snapshot = postcard::to_stdvec(&thread.to_protocol()).unwrap();
            let draft = thread.draft().encode_state();
            let participant = thread.participant_id();
            let stored = thread.draft().stored.clone();
            for invalid in [two_generating, invalid_json] {
                assert!(
                    thread
                        .try_apply_for_test(protocol::HostMessage::Welcome(Box::new(invalid)), cx)
                        .is_err()
                );
                assert_eq!(
                    postcard::to_stdvec(&thread.to_protocol()).unwrap(),
                    snapshot
                );
                assert_eq!(thread.draft().encode_state(), draft);
                assert_eq!(thread.participant_id(), participant);
                assert_eq!(thread.draft().stored, stored);
            }
        });
    });
}

/// Thinking collapses when it completes, but once someone expands it again,
/// later replies and the end of the run leave it open.
#[gpui::test]
fn expanded_thinking_stays_open_as_the_run_continues(cx: &mut gpui::TestAppContext) {
    let message_id = Uuid::new_v4();
    let events = agent_stream_events(&mut ScriptedHost::default(), message_id);
    // Through the first answer fragment, which completes the thinking.
    let thinking_done = events
        .iter()
        .position(previews_text)
        .expect("an answer fragment")
        + 1;
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in events.iter().take(thinking_done) {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            assert!(message.ui.output.thinking_complete);
            assert!(matches!(
                message.ui.output.steps()[0],
                AgentStep::Thinking(_)
            ));
            assert!(!message.ui.step_view(0).unwrap().expanded());
            message.ui.step_view_mut(0).unwrap().set_expanded(true);
            for event in events.iter().skip(thinking_done) {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            assert!(message.ui.step_view(0).unwrap().expanded());
        });
    });
}

/// Thinking after a tool call is a step of its own, after the call. The
/// earlier thinking stays as it was, collapsed, rather than reopening to
/// take the new reasoning.
#[gpui::test]
fn thinking_after_a_tool_call_is_a_new_step(cx: &mut gpui::TestAppContext) {
    let message_id = Uuid::new_v4();
    let mut host = ScriptedHost::default();
    let mut events = host.start(message_id, "Add 1 and 2");
    events.extend(host.turn(
        message_id,
        [
            streamed_text("thinking", "Add first.", true),
            streamed_tool_call(
                "sum",
                "calculate",
                serde_json::json!({"operation": "add", "a": 1, "b": 2}),
            ),
        ],
        100,
    ));
    events.extend(host.results(message_id, "3"));
    // Through the first fragment of the second thinking.
    let second_thinking_started = events.len() + 1;
    events.extend(host.turn(
        message_id,
        [
            streamed_text("second-thinking", "Now ", true),
            streamed_text("second-thinking", "answer.", true),
            streamed_text("answer", "It is 3.", false),
        ],
        200,
    ));
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in events.iter().take(second_thinking_started) {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            let [
                AgentStep::Thinking(first),
                AgentStep::ToolCall(_),
                AgentStep::Thinking(second),
            ] = message.ui.output.steps()
            else {
                panic!("expected thinking, a call, and thinking");
            };
            assert_eq!((first.as_str(), second.as_str()), ("Add first.", "Now "));
            assert!(!message.ui.output.thinking_in_progress(0));
            assert!(message.ui.output.thinking_in_progress(2));
            assert_eq!(message.ui.views().count(), 3);
            assert!(!message.ui.step_view(0).unwrap().expanded());
            assert!(message.ui.work_expanded, "work shows while the run goes");

            for event in events.iter().skip(second_thinking_started) {
                thread.apply_for_test(event.clone(), cx);
            }
            thread.apply_for_test(agent_ended(message_id, protocol::RunOutcome::Completed), cx);
        });
        let thread = thread.read(cx);
        let TimelineMessage::Agent(message) = &thread.timeline[0] else {
            panic!("expected the agent message");
        };
        assert_eq!(
            message.ui.output.thinking().collect::<Vec<_>>(),
            ["Add first.", "Now answer."]
        );
        assert!(matches!(
            message.ui.output.steps()[1],
            AgentStep::ToolCall(_)
        ));
    });
}

/// A file change, shown as its diff, starts open; a call shown as its raw
/// arguments starts folded. A change someone folds stays folded as the run
/// continues.
#[gpui::test]
fn file_changes_start_open_and_raw_calls_folded(cx: &mut gpui::TestAppContext) {
    let message_id = Uuid::new_v4();
    let mut host = ScriptedHost::default();
    let mut events = host.start(message_id, "Fix it");
    events.extend(host.turn(
        message_id,
        [streamed_tool_call(
            "edit",
            sandbox::EditFile::NAME,
            serde_json::json!({"path": "zed/a.rs", "old_string": "a", "new_string": "b"}),
        )],
        100,
    ));
    let edited = events.len();
    events.extend(host.results(message_id, "Edited"));
    events.extend(host.turn(
        message_id,
        [streamed_tool_call(
            "sum",
            "calculate",
            serde_json::json!({"operation": "add", "a": 1, "b": 2}),
        )],
        200,
    ));
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in events.iter().take(edited) {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            assert!(message.ui.step_view(0).unwrap().expanded());
            message.ui.step_view_mut(0).unwrap().set_expanded(false);
            for event in events.iter().skip(edited) {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            assert!(matches!(
                message.ui.output.steps(),
                [AgentStep::ToolCall(_), AgentStep::ToolCall(_)]
            ));
            assert!(!message.ui.step_view(0).unwrap().expanded());
            assert!(!message.ui.step_view(1).unwrap().expanded());
        });
    });
}

#[gpui::test]
fn membership_events_are_idempotent_and_uncatalogued_models_cannot_run(
    cx: &mut gpui::TestAppContext,
) {
    let (thread, cx) = empty_thread(cx, Arc::new(test_catalog()));
    let first = ParticipantId::new();
    let second = ParticipantId::new();

    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in [
                joined(first),
                joined(second),
                joined(first),
                // The host is authoritative, so its selection is kept
                // even when the catalog does not offer it.
                protocol::HostMessage::ModelSelected(ollama_model("no-such-model")),
            ] {
                thread.apply_for_test(event, cx);
            }
            assert_eq!(thread.participants(), [first, second]);
            assert_eq!(thread.model().cloned(), Some(ollama_model("no-such-model")));
            assert!(thread.runnable_model().is_none());
            assert_eq!(thread.max_tokens(), 0);

            thread.apply_for_test(
                protocol::HostMessage::ParticipantLeft(first.into_bytes()),
                cx,
            );
            thread.apply_for_test(
                protocol::HostMessage::ParticipantLeft(first.into_bytes()),
                cx,
            );
            assert_eq!(thread.participants(), [second]);
        });
    });
}

/// Only the host saves a thread: one joined from someone else is theirs.
#[gpui::test]
fn only_the_users_own_threads_are_saved(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, local_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let root = std::env::temp_dir().join(format!("cowork-threads-{}", Uuid::new_v4()));
    let finished = cowork.update(cx, |cowork, cx| {
        let local = cowork.active_thread(cx).expect("active thread");
        let joined = cx.new(|cx| Thread::join_for_test(local.read(cx).welcome_for_test(), cx));
        cowork
            .thread_store
            .update(cx, |store, _| store.threads.push_back(joined));
        let (files, saved) = crate::thread_files::ThreadFiles::open(Some(root.clone())).unwrap();
        assert!(saved.is_empty());
        cowork.thread_files = Some(files);
        cowork.save_all_threads(cx);
        cowork.thread_files.as_ref().unwrap().quit()
    });
    futures::executor::block_on(finished).unwrap();
    let saved = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(saved, [local_id.to_string()]);
    _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn turn_usage_counts_globally_only_for_local_threads(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let turn = |total: u64| Usage {
        total_tokens: Some(total),
        ..Usage::default()
    };

    cowork.update(cx, |cowork, cx| {
        let local = cowork.active_thread(cx).expect("active thread");
        let joined = cx.new(|cx| Thread::join_for_test(local.read(cx).welcome_for_test(), cx));

        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        let took = Duration::from_secs;
        cowork.record_turn_usage(&local, turn(100), at(10), took(3), cx);
        let now = chrono::Local::now();
        cowork.prepare_profile_statistics(now, cx);
        assert_eq!(
            cowork
                .profile_statistics
                .as_ref()
                .unwrap()
                .totals
                .tokens_used,
            100
        );
        let snapshot = cowork.profile_statistics.as_ref().unwrap();
        let activity = snapshot.totals.token_activity.as_ptr();
        let buckets = snapshot.chart.chart.buckets.as_ptr();
        // Streaming invalidates the view, not the statistics snapshot or chart.
        cowork.thread_updated(local.read(cx).instance_id, cx);
        cowork.prepare_profile_statistics(now, cx);
        let snapshot = cowork.profile_statistics.as_ref().unwrap();
        assert_eq!(snapshot.totals.token_activity.as_ptr(), activity);
        assert_eq!(snapshot.chart.chart.buckets.as_ptr(), buckets);

        cowork.record_turn_usage(&local, turn(0), at(15), took(1), cx);
        assert!(
            cowork.profile_statistics.is_none(),
            "zero-token runs can change duration"
        );
        cowork.record_turn_usage(&local, turn(50), at(20), took(2), cx);
        cowork.prepare_profile_statistics(now, cx);
        cowork.record_turn_usage(&joined, turn(30), at(30), took(4), cx);
        assert!(
            cowork.profile_statistics.is_some(),
            "joined usage does not affect the profile"
        );
        let totals = &cowork.profile_statistics.as_ref().unwrap().totals;
        assert_eq!(totals.tokens_used, cowork.tokens_used(cx));
        assert_eq!(totals.chats, cowork.total_chats(cx));
        assert_eq!(totals.longest_chat, cowork.longest_chat(cx));
        assert_eq!(totals.token_activity, cowork.token_activity(cx));

        let activity = totals.token_activity.as_ptr();
        cowork.activity_range = ActivityRange::Day;
        let later = now + chrono::TimeDelta::hours(2);
        cowork.prepare_profile_statistics(later, cx);
        let snapshot = cowork.profile_statistics.as_ref().unwrap();
        assert_eq!(snapshot.totals.token_activity.as_ptr(), activity);
        assert_eq!(
            snapshot.chart.chart,
            crate::usage::token_activity_chart(
                &snapshot.totals.token_activity,
                ActivityRange::Day,
                later,
            )
        );

        assert_eq!(local.read(cx).tokens_used, 150);
        assert_eq!(joined.read(cx).tokens_used, 30);
        assert_eq!(cowork.tokens_used(cx), 150);
        // Only the user's own turns that used tokens are charted.
        assert_eq!(
            cowork.token_activity(cx),
            [
                TokenActivity {
                    at: at(10),
                    duration: took(3),
                    tokens: 100,
                },
                TokenActivity {
                    at: at(20),
                    duration: took(2),
                    tokens: 50,
                },
            ]
        );

        // Joined chats are not the user's own.
        cowork
            .thread_store
            .update(cx, |store, _| store.threads.push_back(joined.clone()));
        assert_eq!(cowork.total_chats(cx), 1);
    });
}

#[gpui::test]
fn context_tokens_are_estimated_while_streaming(cx: &mut gpui::TestAppContext) {
    let (thread, cx) = empty_thread(cx, Arc::default());
    let id = Uuid::new_v4();
    let apply =
        |thread: &mut Thread, events: &[protocol::HostMessage], cx: &mut Context<Thread>| {
            for event in events {
                thread.apply_for_test(event.clone(), cx);
            }
        };
    // Each reply previews its text, then reports its usage and joins.
    let mut host = ScriptedHost::default();
    let start = host.start(id, "prompt");
    let first = host.turn(id, [streamed_text("answer", "12345", false)], 100);
    let (first_text, first_end) = first.split_at(1);
    let second = host.turn(id, [streamed_text("answer", "12345678", false)], 200);

    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            assert_eq!(thread.live_context_tokens(), None);
            apply(thread, &start, cx);

            // Before any count, streamed output is all there is.
            apply(thread, first_text, cx);
            assert_eq!(thread.live_context_tokens(), Some(2));

            // A measurement replaces the estimate, which then grows on.
            apply(thread, first_end, cx);
            assert_eq!(thread.live_context_tokens(), Some(100));
            apply(thread, &second[..1], cx);
            assert_eq!(thread.live_context_tokens(), Some(102));
        });

        // Someone joining mid-stream sees the same count.
        let mirror = cx.new(|cx| Thread::join_for_test(thread.read(cx).welcome_for_test(), cx));
        assert_eq!(mirror.read(cx).live_context_tokens(), Some(102));

        // Output of a stopped request never reaches the transcript.
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(agent_ended(id, protocol::RunOutcome::Completed), cx);
            assert_eq!(thread.live_context_tokens(), Some(100));
        });
    });
}

#[gpui::test]
fn running_agent_message_is_the_incomplete_one(cx: &mut gpui::TestAppContext) {
    let (thread, cx) = empty_thread(cx, Arc::default());
    let finished = Uuid::new_v4();
    let running = Uuid::new_v4();

    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            assert_eq!(thread.running_agent_message_id(), None);
            for event in [
                ScriptedHost::default().started(finished, None),
                protocol::HostMessage::AgentEnded {
                    id: finished.into_bytes(),
                    outcome: crate::protocol::RunOutcome::Completed,
                    duration: Duration::from_secs(3),
                },
                ScriptedHost::default().started(running, None),
            ] {
                thread.apply_for_test(event, cx);
            }
            assert_eq!(thread.running_agent_message_id(), Some(running));
            // A running message has no duration yet.
            assert_eq!(thread.generation_time(), Duration::from_secs(3));

            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id: running.into_bytes(),
                    outcome: crate::protocol::RunOutcome::Completed,
                    duration: Duration::from_millis(4_500),
                },
                cx,
            );
            assert_eq!(thread.running_agent_message_id(), None);
            assert_eq!(thread.generation_time(), Duration::from_millis(7_500));
        });
    });
}

#[gpui::test]
fn collaborator_requests_select_models_and_stop_only_the_running_generation(
    cx: &mut gpui::TestAppContext,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let running_message_id = Uuid::new_v4();
    let cancelled = Arc::new(AtomicBool::new(false));

    cowork.update(cx, |cowork, cx| {
        cowork.set_models(catalog_of(&[ollama_qwen()]), cx);
        let thread = cowork.active_thread(cx).expect("active thread");
        let collaborator = ParticipantId::new();
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(joined(collaborator), cx);
            thread
                .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                    auth.set_override(collaborator, Some(PeerMode::Admin), cx);
                })
                .expect("host manages access");
        });
        cowork.active_generations.insert(
            thread_id,
            ActiveGeneration::for_test(running_message_id, cancelled.clone()),
        );

        for request in [
            protocol::CollaboratorMessage::SelectModel(ollama_model("no-such-model")),
            protocol::CollaboratorMessage::SelectModel(recommended_qwen()),
            protocol::CollaboratorMessage::Stop {
                message_id: Uuid::new_v4().into_bytes(),
            },
        ] {
            cowork
                .collaborator_request(&thread, collaborator, request, cx)
                .expect("valid request");
        }
        assert_eq!(thread.read(cx).model().cloned(), None);
        assert!(!cancelled.load(Ordering::Acquire));

        for request in [
            protocol::CollaboratorMessage::SelectModel(ollama_qwen()),
            protocol::CollaboratorMessage::Stop {
                message_id: running_message_id.into_bytes(),
            },
        ] {
            cowork
                .collaborator_request(&thread, collaborator, request, cx)
                .expect("valid request");
        }
        assert_eq!(thread.read(cx).model().cloned(), Some(ollama_qwen()));
        assert!(cancelled.load(Ordering::Acquire));
        // Only the requesting peer picked it; new local threads keep the
        // local user's choice.
        assert_eq!(cowork.new_thread_model, None);
    });
}

/// The text parts of the prompt the agent is sent for `thread`'s last
/// submission.
fn last_prompt_texts(thread: &Thread) -> Vec<String> {
    let group = thread
        .timeline
        .iter()
        .rev()
        .find_map(|message| match message {
            TimelineMessage::User(group) => Some(group),
            TimelineMessage::Agent(_) => None,
        })
        .expect("a submission");
    crate::prompt::agent_message(
        None,
        &group.record.blocks,
        &thread.draft().files,
        &thread.prompt_names,
    )
    .into_iter()
    .filter_map(|part| match part {
        Content::Text { text } => Some(text),
        _ => None,
    })
    .collect()
}

#[gpui::test]
fn renaming_never_changes_what_the_agent_is_sent(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let derived_name = cowork.read_with(cx, |cowork, _| cowork.local_participant_id.display_name());
    cx.simulate_input("first");
    cx.run_until_parked();
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx));
    });
    cx.run_until_parked();
    let thread = cowork.read_with(cx, |cowork, cx| {
        cowork
            .active_thread(cx)
            .expect("the submission started a thread")
    });
    thread.read_with(cx, |thread, _| {
        assert_eq!(
            last_prompt_texts(thread),
            [format!("{derived_name}:\nfirst")]
        );
    });

    cowork.update(cx, |cowork, cx| {
        cowork.set_profile(
            Profile {
                name: Some("Grace".into()),
                picture: None,
                ..cowork.profile.clone()
            },
            cx,
        );
    });
    thread.update(cx, |thread, cx| {
        thread.set_generating_for_test(false, cx);
        let actor = thread.participant_id();
        thread
            .with_authorized::<EditDraft, _>(actor, |auth| {
                auth.edit(|draft| draft.create_prompt(draft.author.as_uuid(), "second"))
            })
            .expect("host edit");
    });
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx));
    });
    cx.run_until_parked();

    thread.read_with(cx, |thread, _| {
        assert_eq!(
            last_prompt_texts(thread),
            [format!("{derived_name}:\nsecond")]
        );
    });
    // The rename still shows everywhere else.
    assert_eq!(
        cowork.read_with(cx, |cowork, _| cowork.profile_name()),
        "Grace"
    );
}

/// A stopped run says so, and its work collapses like any other's. Text the
/// agent wrote before its last tool call is work, not the response.
#[gpui::test]
fn a_stopped_run_keeps_its_outcome_and_collapses_its_work(cx: &mut gpui::TestAppContext) {
    let message_id = Uuid::new_v4();
    let mut host = ScriptedHost::default();
    let mut events = host.start(message_id, "Add 1 and 2");
    // The reply previews its text, then joins with a call that never
    // returns: the run is stopped first.
    events.extend(host.turn(
        message_id,
        [
            streamed_text("answer", "Let me check.", false),
            streamed_tool_call(
                "sum",
                "calculate",
                serde_json::json!({"operation": "add", "a": 1, "b": 2}),
            ),
        ],
        100,
    ));
    let previewed = 3;
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in &events[..previewed] {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            // Until a tool call follows, the text may be the response.
            assert_eq!(message.ui.output.text, "Let me check.");
            for event in events.iter().skip(previewed) {
                thread.apply_for_test(event.clone(), cx);
            }
            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id: message_id.into_bytes(),
                    outcome: protocol::RunOutcome::Stopped,
                    duration: Duration::from_secs(2),
                },
                cx,
            );
        });
        let host = thread.read(cx);
        let TimelineMessage::Agent(message) = &host.timeline[0] else {
            panic!("expected the agent message");
        };
        assert_eq!(
            message.record.run.outcome(),
            Some(&protocol::RunOutcome::Stopped)
        );
        assert_eq!(message.record.run.failure(), None);
        assert!(!message.ui.work_expanded);
        assert_eq!(message.ui.output.text, "");
        assert_eq!(message.ui.output.work().collect::<Vec<_>>(), [0, 1]);

        let restored = Thread::join_for_test(host.welcome_for_test(), cx);
        assert_eq!(restored.conversation(), thread.read(cx).conversation());
        let TimelineMessage::Agent(restored) = &restored.timeline[0] else {
            panic!("expected the agent message");
        };
        assert!(
            !restored.ui.work_expanded,
            "a joiner sees ended runs collapsed"
        );
    });
}
