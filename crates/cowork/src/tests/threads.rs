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
    protocol::HostMessage::AgentEnded {
        id: id.into_bytes(),
        outcome,
        duration: Duration::from_secs(1),
    }
}

/// The events a host broadcasts while answering one prompt: a turn that
/// thinks, answers, and replies to a comment with a tool call, then the
/// tool's result and a second turn.
fn agent_stream_events(message_id: Uuid) -> Vec<protocol::HostMessage> {
    let user_message_id = Uuid::new_v4().into_bytes();
    let comment_id = Uuid::new_v4().into_bytes();
    let first_turn = model_turn(
        [
            streamed_text("thinking", "Weighing ", true),
            streamed_text("thinking", "options.", true),
            // The thinking stays open until the turn ends, but the answer
            // follows it from its first token.
            streamed_text("answer", "Here is ", false),
            streamed_text("answer", "the answer.", false),
            streamed_tool_call(
                "call",
                "respond_to_comment",
                serde_json::json!({"comment_id": "comment_1", "response": "Because of this."}),
            ),
        ],
        1_024,
    );
    let results = tool_results(&first_turn, "Recorded");
    let second_turn = model_turn([streamed_text("second", " Done.", false)], 2_048);
    let agent = first_turn
        .into_iter()
        .chain(results)
        .chain(second_turn)
        .map(|event| agent_event(message_id, event));

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
        agent_started(message_id, Some(user_message_id), "Explain this"),
    ];
    events.extend(agent);
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
/// the transcript and pending events where the host showed it
/// incrementally, and the events that follow bring them to where the host
/// ends up. The second run's turn calls two tools, answered one at a time,
/// so results arrive for committed calls before they join the transcript.
#[gpui::test]
fn joining_after_any_event_converges_on_the_host(cx: &mut gpui::TestAppContext) {
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let mut events = agent_stream_events(first_id);
    events.push(agent_started(second_id, None, "Calculate both"));
    let mut turn = model_turn(
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
    );
    let results = tool_results(&turn, "sum");
    turn.extend(results);
    turn.extend(model_turn(
        [
            streamed_text("answer", "3 and ", false),
            streamed_text("answer", "7.", false),
        ],
        400,
    ));
    events.extend(turn.into_iter().map(|event| agent_event(second_id, event)));
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
        assert_eq!(calls[0].call.function.name, "respond_to_comment");
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

#[gpui::test]
fn failed_run_keeps_output_the_transcript_never_got_for_joiners(cx: &mut gpui::TestAppContext) {
    use rig::{completion::AssistantContent, streaming::StreamEvent};

    let message_id = Uuid::new_v4();
    let events = agent_stream_events(message_id);
    let through_call = events
        .iter()
        .position(|event| {
            matches!(event, protocol::HostMessage::AgentEvent { event, .. }
            if matches!(event.to_agent(), Ok(agent::AgentEvent::Model(
                StreamEvent::End { content: AssistantContent::ToolCall(_), .. }
            ))))
        })
        .unwrap()
        + 1;
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            for event in events.iter().take(through_call) {
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
        // The turn never ended, so all of it is pending.
        assert_eq!(
            message.pending_events.len(),
            events[..through_call]
                .iter()
                .filter(|event| matches!(event, protocol::HostMessage::AgentEvent { .. }))
                .count()
        );
        let restored = Thread::join_for_test(welcome, cx);
        assert_eq!(restored.conversation(), thread.read(cx).conversation());
        let TimelineMessage::Agent(restored) = &restored.timeline[1] else {
            panic!("expected agent message");
        };
        assert_eq!(restored.record.run.failure(), Some("stream failed"));
        let calls = restored.ui.output.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls[0].call.function.name, "respond_to_comment");
        assert!(calls[0].result.is_none());
    });
}

/// A part's end may state other content than its fragments did, superseding
/// what streamed. What the message shows follows Rig's accumulation, so it
/// shows the content the end states, as the transcript records it.
#[gpui::test]
fn a_restated_part_shows_as_rig_accumulates_it(cx: &mut gpui::TestAppContext) {
    let message_id = Uuid::new_v4();
    let event = |value: serde_json::Value| {
        agent::AgentEvent::Model(serde_json::from_value(value).expect("a stream event"))
    };
    let turn = [
        event(serde_json::json!({"event": "start", "part": 0, "kind": "reasoning"})),
        event(serde_json::json!({"event": "reasoning", "part": 0, "text": "Hm"})),
        event(serde_json::json!({"event": "end", "part": 0,
            "content": {"type": "reasoning", "text": "Considered."}})),
        event(serde_json::json!({"event": "start", "part": 1, "kind": "text"})),
        event(serde_json::json!({"event": "text", "part": 1, "text": "Yes."})),
        event(serde_json::json!({"event": "end", "part": 1,
            "content": {"type": "text", "text": "Yes."}})),
    ];
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(agent_started(message_id, None, "Is it?"), cx);
            for event in turn {
                thread.apply_for_test(agent_event(message_id, event), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            assert_eq!(
                message.ui.output.thinking().collect::<Vec<_>>(),
                ["Considered."]
            );
            assert_eq!(message.ui.output.text, "Yes.");
        });
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
            for event in agent_stream_events(message_id) {
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

        // The prompt must be a user message: here it is the agent's reply.
        assert!(
            with_agent_message(&|message| message.start = crate::transcript::TranscriptIndex(1))
                .is_err()
        );
        assert!(
            with_agent_message(&|message| message.start = crate::transcript::TranscriptIndex(99))
                .is_err()
        );
        // Events completing a turn would have joined the transcript.
        let more = model_turn([streamed_text("answer", "More", false)], 1)
            .into_iter()
            .map(protocol::Json::shared)
            .collect::<Vec<_>>();
        assert!(with_agent_message(&|message| message.pending_events = more.clone()).is_err());
        assert!(with_agent_message(&|message| message.pending_events = more[..2].to_vec()).is_ok());
        // Events Rig's stream could not have produced: text before its part
        // started.
        assert!(
            with_agent_message(&|message| message.pending_events = more[1..2].to_vec()).is_err()
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
        let mut transcript = two_generating.thread.transcript.to_transcript().unwrap();
        let second_prompt = transcript.end();
        transcript.append(RigMessage::user("Next"));
        two_generating.thread.transcript = transcript.to_protocol();
        let protocol::TimelineMessage::Agent(mut second) = first.clone() else {
            unreachable!();
        };
        second.id = Uuid::new_v4().into_bytes();
        second.start = second_prompt;
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
/// later turns and the end of the run leave it open.
#[gpui::test]
fn expanded_thinking_stays_open_as_the_run_continues(cx: &mut gpui::TestAppContext) {
    let message_id = Uuid::new_v4();
    let events = agent_stream_events(message_id);
    // Through the first answer token, which completes the thinking.
    let thinking_done = events
        .iter()
        .position(|event| {
            matches!(event, protocol::HostMessage::AgentEvent { event, .. }
            if matches!(event.to_agent(), Ok(agent::AgentEvent::Model(
                rig::streaming::StreamEvent::Text { .. }
            ))))
        })
        .expect("an answer token")
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
    let mut turn = model_turn(
        [
            streamed_text("thinking", "Add first.", true),
            streamed_tool_call(
                "sum",
                "calculate",
                serde_json::json!({"operation": "add", "a": 1, "b": 2}),
            ),
        ],
        100,
    );
    turn.extend(tool_results(&turn, "3"));
    // Through the first fragment of the second thinking: its part's start,
    // then the fragment.
    let second_thinking_started = turn.len() + 2;
    turn.extend(model_turn(
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
            thread.apply_for_test(agent_started(message_id, None, "Add 1 and 2"), cx);
            for event in turn.iter().take(second_thinking_started) {
                thread.apply_for_test(agent_event(message_id, event.clone()), cx);
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

            for event in turn.iter().skip(second_thinking_started) {
                thread.apply_for_test(agent_event(message_id, event.clone()), cx);
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

#[gpui::test]
fn turn_usage_counts_globally_only_for_local_threads(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    let turn = |total: u64| Usage::new().total_tokens(total);

    cowork.update(cx, |cowork, cx| {
        let local = cowork.active_thread(cx).expect("active thread");
        let joined = cx.new(|cx| Thread::join_for_test(local.read(cx).welcome_for_test(), cx));

        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        let took = Duration::from_secs;
        cowork.record_turn_usage(&local, turn(100), at(10), took(3), cx);
        cowork.record_turn_usage(&local, turn(0), at(15), took(1), cx);
        cowork.record_turn_usage(&local, turn(50), at(20), took(2), cx);
        cowork.record_turn_usage(&joined, turn(30), at(30), took(4), cx);

        assert_eq!(local.read(cx).tokens_used, 150);
        assert_eq!(joined.read(cx).tokens_used, 30);
        assert_eq!(cowork.tokens_used, 150);
        // Only the user's own turns that used tokens are charted.
        assert_eq!(
            cowork.token_activity,
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
    let apply = |thread: &mut Thread, events: &[agent::AgentEvent], cx: &mut Context<Thread>| {
        for event in events {
            thread.apply_for_test(agent_event(id, event.clone()), cx);
        }
    };
    // Each turn streams its text's start, then the text.
    let first = model_turn([streamed_text("answer", "12345", false)], 100);
    let (first_text, first_end) = first.split_at(2);
    let second = model_turn([streamed_text("answer", "12345678", false)], 200);

    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            assert_eq!(thread.live_context_tokens(), None);
            thread.apply_for_test(agent_started(id, None, "prompt"), cx);

            // Before any count, streamed output is all there is.
            apply(thread, first_text, cx);
            assert_eq!(thread.live_context_tokens(), Some(2));

            // A measurement replaces the estimate, which then grows on.
            apply(thread, first_end, cx);
            assert_eq!(thread.live_context_tokens(), Some(100));
            apply(thread, &second[..2], cx);
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
                agent_started(finished, None, "prompt"),
                protocol::HostMessage::AgentEnded {
                    id: finished.into_bytes(),
                    outcome: crate::protocol::RunOutcome::Completed,
                    duration: Duration::from_secs(3),
                },
                agent_started(running, None, "prompt"),
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
    let task = runtime.spawn(std::future::pending::<()>());
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
            ActiveGeneration::for_test(running_message_id, task.abort_handle(), cancelled.clone()),
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
    let aborted = runtime
        .block_on(task)
        .expect_err("generation task was aborted");
    assert!(aborted.is_cancelled());
}

/// The text parts of a user message sent to the agent.
fn prompt_texts(message: &RigMessage) -> Vec<String> {
    let RigMessage::User { content } = message else {
        panic!("expected a user message, got {message:?}");
    };
    content
        .iter()
        .filter_map(|part| match part {
            UserContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn renaming_never_changes_what_the_agent_was_sent(cx: &mut gpui::TestAppContext) {
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
    // The prompt is recorded before the run starts, and this test's run
    // never does.
    let first_prompt = thread.read_with(cx, |thread, _| {
        assert_eq!(thread.transcript.messages().len(), 1);
        thread.transcript.messages()[0].clone()
    });
    // The first prompt tells the agent where its commands run.
    let [note, first] = prompt_texts(&first_prompt).try_into().expect("two parts");
    assert!(note.starts_with(sandbox::ENVIRONMENT_NOTE_START), "{note}");
    assert_eq!(first, format!("{derived_name}:\nfirst"));

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
        assert_eq!(thread.transcript.messages().len(), 2);
        assert_eq!(
            prompt_texts(&thread.transcript.messages()[0]),
            prompt_texts(&first_prompt)
        );
        // The project did not change, so the note is not repeated.
        assert_eq!(
            prompt_texts(&thread.transcript.messages()[1]),
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
    let mut turn = model_turn(
        [
            streamed_text("answer", "Let me check.", false),
            streamed_tool_call(
                "sum",
                "calculate",
                serde_json::json!({"operation": "add", "a": 1, "b": 2}),
            ),
        ],
        100,
    );
    // Stopped before the turn ended.
    turn.pop();
    let (thread, cx) = empty_thread(cx, Arc::default());
    cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(agent_started(message_id, None, "Add 1 and 2"), cx);
            // The text's start, then the text.
            for event in &turn[..2] {
                thread.apply_for_test(agent_event(message_id, event.clone()), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            // Until a tool call follows, the text may be the response.
            assert_eq!(message.ui.output.text, "Let me check.");
            for event in turn.iter().skip(2) {
                thread.apply_for_test(agent_event(message_id, event.clone()), cx);
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
