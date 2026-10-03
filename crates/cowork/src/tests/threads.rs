//! Thread state: titles, ownership, applying host events, context usage,
//! and requests a collaborator sends the host.

use super::*;

#[test]
fn collaborator_threads_are_removed_on_disconnect_and_default_to_admin() {
    assert_eq!(PeerPermissions::default().default_mode(), PeerMode::Admin);
    assert!(PeerMode::Admin.can_edit_draft());
    assert!(ThreadOwnership::Remote.remove_on_disconnect());
    assert!(!ThreadOwnership::Local.remove_on_disconnect());
}

#[test]
fn thread_titles_normalize_whitespace_and_truncate_by_character() {
    assert_eq!(
        Cowork::thread_title("  Collaborate\non\tthis prompt  "),
        "Collaborate on this prompt"
    );
    assert_eq!(
        Cowork::thread_title("12345678901234567890123456789012"),
        "12345678901234567890123456789012"
    );
    assert_eq!(
        Cowork::thread_title("12345678901234567890123456789012 more"),
        "12345678901234567890123456789012…"
    );
    assert_eq!(
        Cowork::thread_title("🦀".repeat(33).as_str()),
        format!("{}…", "🦀".repeat(32))
    );
}

#[test]
fn first_message_titles_an_empty_pre_shared_thread() {
    assert_eq!(
        Cowork::title_for_first_message(&[], "  Collaborate on this prompt  "),
        Some("Collaborate on this prompt".into())
    );

    let existing_timeline = vec![TimelineMessage::User(UserMessageGroup {
        id: Uuid::new_v4(),
        comments: Vec::new(),
        blocks: vec![PromptBlock {
            id: Uuid::new_v4(),
            author: ParticipantId::new(),
            text: "Existing message".into(),
            attachments: Vec::new(),
        }],
        comments_folded: false,
    })];
    assert_eq!(
        Cowork::title_for_first_message(&existing_timeline, "Later message"),
        None
    );
}

#[gpui::test]
fn sharing_before_first_message_materializes_an_empty_owned_thread(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|_, cx| {
        let draft = ThreadDraft::new(ParticipantId::new());
        let draft_id = draft.id;
        let thread = Cowork::new_empty_local_thread(
            draft,
            ParticipantId::new(),
            Arc::new(test_catalog()),
            Some(ollama_qwen()),
            cx,
        );
        EmptyThreadTestView { thread, draft_id }
    });

    view.read_with(cx, |view, cx| {
        let thread = view.thread.read(cx);
        assert!(thread.timeline.is_empty());
        assert_eq!(thread.draft().id, view.draft_id);
        assert_eq!(thread.summary.title, "New thread");
        assert_eq!(thread.ownership(), ThreadOwnership::Local);
        assert_eq!(thread.model().cloned(), Some(ollama_qwen()));
        assert_eq!(**thread.models(), test_catalog());
        assert!(thread.participants().is_empty());
        assert!(matches!(thread.sharing, ThreadSharing::NotShared));
    });
}

struct ThreadMirrorTestView {
    host: Entity<Thread>,
    collaborator: Option<Entity<Thread>>,
}

impl Render for ThreadMirrorTestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The events a host broadcasts while answering one prompt: a turn that
/// thinks, answers, and replies to a comment with a tool call, then the
/// tool's result and a second turn.
fn agent_stream_events(message_id: Uuid) -> Vec<protocol::HostMessage> {
    use rig::streaming::{BlockClose, BlockKind};

    let user_message_id = Uuid::new_v4().into_bytes();
    let comment_id = Uuid::new_v4().into_bytes();
    let mut first_turn = vec![
        streamed_text("thinking", "Weighing ", true),
        streamed_text("thinking", "options.", true),
        // No explicit thinking end, so the first answer token closes it.
        streamed_text("answer", "Here is ", false),
    ];
    first_turn.extend(streamed_tool_call(
        "call",
        "respond_to_comment",
        serde_json::json!({"comment_id": "comment_1", "response": "Because of this."}),
    ));
    first_turn.extend([
        streamed_text("answer", "the answer.", false),
        turn_ended(1_024),
    ]);
    let first_turn = agent::test_support::canonical(first_turn);
    let results = tool_results(&first_turn, "Recorded");
    let mut second_turn = streamed_block(
        "second",
        BlockKind::Text {
            additional_params: None,
        },
        [rig::streaming::Delta::Text {
            text: " Done.".into(),
        }],
        BlockClose::Text,
    );
    second_turn.push(turn_ended(2_048));
    let second_turn = agent::test_support::canonical(second_turn);
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

/// A collaborator that joins midway through a generation has to end up with
/// the host's timeline: its snapshot covers what it missed, and the events
/// it replays afterwards cover the rest.
#[gpui::test]
fn collaborators_joining_mid_stream_converge_on_the_host_timeline(cx: &mut gpui::TestAppContext) {
    assert_joining_agent_timeline_converges(cx, JoinAt::Reasoning);
}

#[gpui::test]
fn collaborators_joining_after_a_tool_call_receive_its_later_result(cx: &mut gpui::TestAppContext) {
    assert_joining_agent_timeline_converges(cx, JoinAt::Call);
}

#[gpui::test]
fn collaborators_joining_after_a_tool_result_read_it_from_rig_history(
    cx: &mut gpui::TestAppContext,
) {
    assert_joining_agent_timeline_converges(cx, JoinAt::Result);
}

#[gpui::test]
fn collaborators_joining_during_a_later_turn_restore_committed_and_streamed_text(
    cx: &mut gpui::TestAppContext,
) {
    assert_joining_agent_timeline_converges(cx, JoinAt::SecondTurn);
}

#[gpui::test]
fn collaborators_joining_after_a_run_read_tool_calls_from_rig_history(
    cx: &mut gpui::TestAppContext,
) {
    assert_joining_agent_timeline_converges(cx, JoinAt::Finished);
}

#[derive(Clone, Copy)]
enum JoinAt {
    Reasoning,
    Call,
    Result,
    SecondTurn,
    Finished,
}

fn assert_joining_agent_timeline_converges(cx: &mut gpui::TestAppContext, join_at: JoinAt) {
    use rig::streaming::{BlockClose, StreamEvent};

    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let events = agent_stream_events(message_id);
    let joined_after = match join_at {
        JoinAt::Reasoning => 4,
        JoinAt::Call => {
            events
                .iter()
                .position(|event| {
                    matches!(event, protocol::HostMessage::AgentEvent { event, .. }
                    if matches!(event.to_agent(), Ok(agent::AgentEvent::Model(
                        StreamEvent::BlockEnd { end: BlockClose::ToolCall(_), .. }
                    ))))
                })
                .expect("completed tool call")
                + 1
        }
        JoinAt::Result => {
            events
                .iter()
                .position(|event| {
                    matches!(event, protocol::HostMessage::AgentEvent { event, .. }
                if matches!(event.to_agent(), Ok(agent::AgentEvent::ToolResult { .. })))
                })
                .expect("tool result")
                + 1
        }
        JoinAt::SecondTurn => events
            .iter()
            .position(|event| {
                matches!(event,
                    protocol::HostMessage::AgentEvent { event, .. }
                    if matches!(event.to_agent(), Ok(agent::AgentEvent::Model(
                        StreamEvent::BlockDelta { delta: rig::streaming::Delta::Text { text }, .. }
                    )) if text == " Done.")
                )
            })
            .expect("later turn's text")
            + 1,
        JoinAt::Finished => {
            events
                .iter()
                .position(|event| matches!(event, protocol::HostMessage::AgentEnded { .. }))
                .expect("agent ended")
                + 1
        }
    };

    let host_participant = ParticipantId::new();
    let collaborator_participant = ParticipantId::new();
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            host_participant,
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });

    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.host.update(cx, |thread, cx| {
                thread.apply_for_test(joined(thread.participant_id()), cx);
                thread.apply_for_test(joined(collaborator_participant), cx);
            });
            for event in events.iter().take(joined_after) {
                view.host
                    .update(cx, |thread, cx| thread.apply_for_test(event.clone(), cx));
            }

            if matches!(
                join_at,
                JoinAt::Call | JoinAt::Result | JoinAt::SecondTurn | JoinAt::Finished
            ) {
                let host = view.host.read(cx);
                let TimelineMessage::Agent(message) = &host.timeline[1] else {
                    panic!("expected the agent message");
                };
                let calls = message.output.tool_calls().collect::<Vec<_>>();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].result.is_some(), !matches!(join_at, JoinAt::Call));
            }
            let welcome = protocol::Welcome {
                draft_generation: 0,
                participant_id: collaborator_participant.into_bytes(),
                thread: view.host.read(cx).to_protocol(),
                draft: view.host.read(cx).draft().encode_state(),
                presence: Vec::new(),
                stored_attachments: Vec::new(),
            };
            let draft = ThreadDraft::new(ParticipantId::new());
            let collaborator =
                cx.new(|cx| Thread::from_welcome(welcome, draft, ThreadSharing::NotShared, cx));
            // What the host showed live, the collaborator derives from the
            // snapshot's transcript and pending events.
            assert_eq!(
                collaborator.read(cx).conversation().agent_output,
                view.host.read(cx).conversation().agent_output
            );

            for event in events.iter().skip(joined_after) {
                view.host
                    .update(cx, |thread, cx| thread.apply_for_test(event.clone(), cx));
                collaborator.update(cx, |thread, cx| thread.apply_for_test(event.clone(), cx));
            }
            view.collaborator = Some(collaborator);
        });
    });

    view.read_with(cx, |view, cx| {
        let host = view.host.read(cx);
        let collaborator = view
            .collaborator
            .as_ref()
            .expect("collaborator should have joined")
            .read(cx);

        assert_eq!(collaborator.conversation(), host.conversation());
        assert_eq!(collaborator.summary.id, host.summary.id);
        assert_ne!(collaborator.instance_id, host.instance_id);
        assert_eq!(collaborator.participant_id(), collaborator_participant);
        assert_eq!(collaborator.draft().author, collaborator_participant);
        assert_eq!(collaborator.participants(), host.participants());
        assert_eq!(collaborator.participants().len(), 3);
        assert_eq!(
            collaborator.participants()[..2],
            [host_participant, collaborator_participant]
        );
        assert_eq!(collaborator.model().cloned(), Some(ollama_qwen()));
        assert_eq!(collaborator.max_tokens(), 65_536);
        assert_eq!(collaborator.context_tokens, Some(2_048));
        assert!(!host.generating);
        assert!(!collaborator.generating);

        let TimelineMessage::Agent(message) = &collaborator.timeline[1] else {
            panic!("expected the agent's reply");
        };
        assert_eq!(
            message.output.thinking().collect::<Vec<_>>(),
            ["Weighing options."]
        );
        // Text before the tool call is work; the response is what follows.
        let [
            AgentStep::Thinking(_),
            AgentStep::Text(work),
            AgentStep::ToolCall(call),
            AgentStep::Text(_),
        ] = &message.output.steps[..]
        else {
            panic!("expected thinking, text, a call, and the response");
        };
        assert_eq!(work, "Here is the answer.");
        assert_eq!(message.output.text, " Done.");
        assert!(!message.work_expanded);
        assert_eq!(call.call.function.name, "respond_to_comment");
        assert!(call.arguments_text().contains("comment_1"));
        assert_eq!(call.result_text().as_deref(), Some("Recorded"));
        assert!(!message.step_views.iter().any(StepView::expanded));
        assert!(message.output.thinking_complete);
        assert!(!message.is_generating());
        assert_eq!(message.run.failure(), None);
        assert!(message.pending_events.is_empty());
        let [response] = message.comment_responses.as_slice() else {
            panic!("expected the reply to the comment");
        };
        assert_eq!(response.response, "Because of this.");

        // The transcript, thinking and tool call included, is the host's,
        // though the collaborator missed the start of the turn.
        assert_eq!(collaborator.transcript, host.transcript);
        let [
            RigMessage::User { .. },
            RigMessage::Assistant { content, .. },
            RigMessage::User { .. },
            RigMessage::Assistant { .. },
        ] = collaborator.transcript.as_slice()
        else {
            panic!("expected prompt, reply, tool result, reply");
        };
        assert!(matches!(
            content.as_slice(),
            [
                rig::completion::AssistantContent::Reasoning(_),
                rig::completion::AssistantContent::Text(_),
                rig::completion::AssistantContent::ToolCall(_),
            ]
        ));
    });
}

#[gpui::test]
fn failed_run_without_rig_output_shows_its_failure(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
            thread.apply_for_test(agent_started(message_id, None, "Try this"), cx);
            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id: message_id.into_bytes(),
                    outcome: crate::protocol::RunOutcome::Failed("Model unavailable".into()),
                    duration: Duration::from_secs(1),
                },
                cx,
            );
        });
        let host = view.host.read(cx);
        let snapshot = host.to_protocol();
        let protocol::TimelineMessage::Agent(message) = &snapshot.messages[0] else {
            panic!("expected agent message");
        };
        assert!(message.pending_events.is_empty());
        assert_eq!(message.run.failure(), Some("Model unavailable"));
        let restored = Thread::from_welcome(
            protocol::Welcome {
                draft_generation: 0,
                participant_id: ParticipantId::new().into_bytes(),
                thread: snapshot,
                draft: host.draft().encode_state(),
                presence: Vec::new(),
                stored_attachments: Vec::new(),
            },
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        );
        let TimelineMessage::Agent(message) = &restored.timeline[0] else {
            panic!("expected agent message");
        };
        // The failure is not agent output, so it stays out of the text.
        assert!(message.output.text.is_empty());
        assert_eq!(message.run.failure(), Some("Model unavailable"));
        assert_eq!(restored.conversation(), view.host.read(cx).conversation());
    });
}

#[gpui::test]
fn separate_runs_reconstruct_only_their_own_tool_calls(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
            for event in agent_stream_events(first_id) {
                thread.apply_for_test(event, cx);
            }
            thread.apply_for_test(agent_started(second_id, None, "Calculate this"), cx);
            let mut events = streamed_tool_call(
                "call",
                "calculate",
                serde_json::json!({
                    "operation": "add", "a": 2, "b": 3
                }),
            );
            events.push(turn_ended(128));
            let events = agent::test_support::canonical(events);
            let results = tool_results(&events, "5");
            for event in events.into_iter().chain(results).chain([turn_ended(256)]) {
                thread.apply_for_test(agent_event(second_id, event), cx);
            }
            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id: second_id.into_bytes(),
                    outcome: crate::protocol::RunOutcome::Completed,
                    duration: Duration::from_secs(1),
                },
                cx,
            );
        });
        let host = view.host.read(cx);
        let snapshot = host.to_protocol();
        for entry in &snapshot.messages {
            if let protocol::TimelineMessage::Agent(message) = entry {
                assert!(message.pending_events.is_empty());
            }
        }
        let welcome = protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: snapshot,
            draft: host.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let restored = Thread::from_welcome(
            welcome,
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        );
        let calls = restored
            .timeline
            .iter()
            .filter_map(|entry| match entry {
                TimelineMessage::Agent(message) => {
                    Some(message.output.tool_calls().collect::<Vec<_>>())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].len(), 1);
        assert_eq!(calls[0][0].call.function.name, "respond_to_comment");
        assert_eq!(calls[1].len(), 1);
        assert_eq!(calls[1][0].call.function.name, "calculate");
        assert_eq!(calls[1][0].result_text().as_deref(), Some("5"));
        assert_eq!(restored.conversation(), view.host.read(cx).conversation());
    });
}

#[gpui::test]
fn failed_run_keeps_output_the_transcript_never_got_for_joiners(cx: &mut gpui::TestAppContext) {
    use rig::streaming::{BlockClose, StreamEvent};

    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let events = agent_stream_events(message_id);
    let through_call = events
        .iter()
        .position(|event| {
            matches!(event, protocol::HostMessage::AgentEvent { event, .. }
            if matches!(event.to_agent(), Ok(agent::AgentEvent::Model(
                StreamEvent::BlockEnd { end: BlockClose::ToolCall(_), .. }
            ))))
        })
        .unwrap()
        + 1;
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
            for event in events.iter().take(through_call) {
                thread.apply_for_test(event.clone(), cx);
            }
            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id: message_id.into_bytes(),
                    outcome: crate::protocol::RunOutcome::Failed("stream failed".into()),
                    duration: Duration::from_secs(1),
                },
                cx,
            );
        });
        let host = view.host.read(cx);
        let snapshot = host.to_protocol();
        let protocol::TimelineMessage::Agent(message) = &snapshot.messages[1] else {
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
        let welcome = protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: snapshot,
            draft: host.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let restored = Thread::from_welcome(
            welcome,
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        );
        assert_eq!(restored.conversation(), view.host.read(cx).conversation());
        let TimelineMessage::Agent(restored) = &restored.timeline[1] else {
            panic!("expected agent message");
        };
        let calls = restored.output.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls[0].call.function.name, "respond_to_comment");
        assert!(calls[0].result.is_none());
        assert_eq!(
            restored.output.thinking().collect::<Vec<_>>(),
            ["Weighing options."]
        );
        // The run stopped at a tool call, so all its text is work.
        assert_eq!(restored.output.text, "");
        assert!(matches!(
            &restored.output.steps[1],
            AgentStep::Text(text) if text == "Here is "
        ));
        assert_eq!(restored.comment_responses.len(), 1);
        assert_eq!(restored.comment_responses[0].response, "Because of this.");
    });
}

/// The host shows each run's output incrementally, redoing only what follows
/// the committed part. After every event, that is exactly what deriving it
/// from scratch gives, which is what someone joining then does.
#[gpui::test]
fn incremental_output_matches_a_fresh_derivation_after_every_event(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let mut events = agent_stream_events(first_id);
    // A second run whose turn calls two tools, answered one at a time, so
    // results arrive for committed calls before they join the transcript.
    events.push(agent_started(second_id, None, "Calculate both"));
    let mut turn = vec![streamed_text("thinking", "Two sums.", true)];
    turn.extend(streamed_tool_call(
        "first",
        "calculate",
        serde_json::json!({"operation": "add", "a": 1, "b": 2}),
    ));
    turn.extend(streamed_tool_call(
        "second",
        "calculate",
        serde_json::json!({"operation": "add", "a": 3, "b": 4}),
    ));
    turn.push(turn_ended(300));
    let mut turn = agent::test_support::canonical(turn);
    let results = tool_results(&turn, "sum");
    assert_eq!(results.len(), 2);
    turn.extend(results);
    turn.extend(agent::test_support::canonical([
        streamed_text("answer", "3 and ", false),
        streamed_text("answer", "7.", false),
        turn_ended(400),
    ]));
    events.extend(turn.into_iter().map(|event| agent_event(second_id, event)));
    events.push(protocol::HostMessage::AgentEnded {
        id: second_id.into_bytes(),
        outcome: crate::protocol::RunOutcome::Completed,
        duration: Duration::from_secs(2),
    });

    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        for (index, event) in events.into_iter().enumerate() {
            view.host
                .update(cx, |thread, cx| thread.apply_for_test(event, cx));
            let host = view.host.read(cx);
            let welcome = protocol::Welcome {
                draft_generation: 0,
                participant_id: ParticipantId::new().into_bytes(),
                thread: host.to_protocol(),
                draft: host.draft().encode_state(),
                presence: Vec::new(),
                stored_attachments: Vec::new(),
            };
            let restored = Thread::from_welcome(
                welcome,
                ThreadDraft::new(ParticipantId::new()),
                ThreadSharing::NotShared,
                cx,
            );
            assert_eq!(
                restored.conversation().agent_output,
                view.host.read(cx).conversation().agent_output,
                "after event {index}"
            );
        }
        let host = view.host.read(cx);
        let TimelineMessage::Agent(message) = &host.timeline[2] else {
            panic!("expected the second run's message");
        };
        assert_eq!(message.output.thinking().collect::<Vec<_>>(), ["Two sums."]);
        assert_eq!(message.output.text, "3 and 7.");
        assert!(
            message
                .output
                .tool_calls()
                .all(|call| call.result_text().as_deref() == Some("sum"))
        );
    });
}

/// A block's end may restate the whole block, superseding what streamed.
/// What the message shows follows Rig's accumulation, so it shows the
/// restatement, as the transcript records it, and so does someone joining.
#[gpui::test]
fn a_restated_block_shows_as_rig_accumulates_it(cx: &mut gpui::TestAppContext) {
    use rig::streaming::{BlockClose, BlockKind, Delta};

    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let mut turn = streamed_block(
        "thinking",
        BlockKind::Reasoning { provider_id: None },
        [Delta::Reasoning { text: "Hm".into() }],
        BlockClose::Reasoning {
            reasoning: Some(rig::message::Reasoning::new("Considered.")),
            signature: None,
            wire_sent: true,
        },
    );
    turn.extend([streamed_text("answer", "Yes.", false), turn_ended(64)]);
    let mut turn = agent::test_support::canonical(turn);
    let ended = turn.pop().expect("the turn's end");
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
            thread.apply_for_test(agent_started(message_id, None, "Is it?"), cx);
            for event in turn {
                thread.apply_for_test(agent_event(message_id, event), cx);
            }
        });
        let host = view.host.read(cx);
        let TimelineMessage::Agent(message) = &host.timeline[0] else {
            panic!("expected the agent message");
        };
        assert_eq!(
            message.output.thinking().collect::<Vec<_>>(),
            ["Considered."]
        );
        assert_eq!(message.output.text, "Yes.");
        let welcome = protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: host.to_protocol(),
            draft: host.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let restored = Thread::from_welcome(
            welcome,
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        );
        assert_eq!(restored.conversation(), view.host.read(cx).conversation());

        view.host.update(cx, |thread, cx| {
            thread.apply_for_test(agent_event(message_id, ended), cx);
        });
        let host = view.host.read(cx);
        let TimelineMessage::Agent(message) = &host.timeline[0] else {
            panic!("expected the agent message");
        };
        assert_eq!(
            message.output.thinking().collect::<Vec<_>>(),
            ["Considered."]
        );
        let [_, RigMessage::Assistant { content, .. }] = host.transcript.as_slice() else {
            panic!("expected the prompt and the reply");
        };
        assert!(matches!(
            content.first(),
            Some(rig::completion::AssistantContent::Reasoning(reasoning))
                if reasoning.display_text() == "Considered."
        ));
    });
}

/// A snapshot whose agent messages do not fit its transcript is rejected
/// before anything is derived from it.
#[gpui::test]
fn snapshots_with_agent_runs_the_transcript_cannot_hold_are_rejected(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
            for event in agent_stream_events(message_id) {
                thread.apply_for_test(event, cx);
            }
        });
        let host = view.host.read(cx);
        let welcome = protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: host.to_protocol(),
            draft: host.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
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
        assert!(with_agent_message(&|message| message.prompt = 1).is_err());
        assert!(with_agent_message(&|message| message.prompt = 99).is_err());
        // Events completing a turn would have joined the transcript.
        assert!(
            with_agent_message(&|message| {
                message.pending_events =
                    vec![protocol::Json::shared(turn_ended(1)).expect("a folded event")];
            })
            .is_err()
        );
        assert!(
            with_agent_message(&|message| {
                message.pending_events =
                    vec![protocol::Json::shared(streamed_text("answer", "More", false)).unwrap()];
            })
            .is_ok()
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
        two_generating
            .thread
            .transcript
            .push(protocol::Json::from_rig(&RigMessage::user("Next")));
        let protocol::TimelineMessage::Agent(mut second) = first.clone() else {
            unreachable!();
        };
        second.id = Uuid::new_v4().into_bytes();
        second.prompt = two_generating.thread.transcript.len() - 1;
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
        invalid_json.thread.transcript[0] = postcard::from_bytes(
            &postcard::to_stdvec("not json").expect("encode invalid JSON string"),
        )
        .expect("decode an untrusted transcript message");
        view.host.update(cx, |thread, cx| {
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
    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let events = agent_stream_events(message_id);
    // Through the first answer token, which completes the thinking.
    let thinking_done = events
        .iter()
        .position(|event| {
            matches!(event, protocol::HostMessage::AgentEvent { event, .. }
            if matches!(event.to_agent(), Ok(agent::AgentEvent::Model(
                rig::streaming::StreamEvent::BlockDelta {
                    delta: rig::streaming::Delta::Text { .. }, ..
                }
            ))))
        })
        .expect("an answer token")
        + 1;
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
            for event in events.iter().take(thinking_done) {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            assert!(message.output.thinking_complete);
            assert!(matches!(message.output.steps[0], AgentStep::Thinking(_)));
            assert!(!message.step_views[0].expanded());
            message.step_views[0].set_expanded(true);
            for event in events.iter().skip(thinking_done) {
                thread.apply_for_test(event.clone(), cx);
            }
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            assert!(message.step_views[0].expanded());
        });
    });
}

/// Thinking after a tool call is a step of its own, after the call. The
/// earlier thinking stays as it was, collapsed, rather than reopening to
/// take the new reasoning.
#[gpui::test]
fn thinking_after_a_tool_call_is_a_new_step(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let mut turn = vec![streamed_text("thinking", "Add first.", true)];
    turn.extend(streamed_tool_call(
        "sum",
        "calculate",
        serde_json::json!({"operation": "add", "a": 1, "b": 2}),
    ));
    turn.push(turn_ended(100));
    let mut turn = agent::test_support::canonical(turn);
    turn.extend(tool_results(&turn, "3"));
    // Through the first fragment of the second thinking.
    let second_thinking_started = turn.len() + 1;
    turn.extend(agent::test_support::canonical([
        streamed_text("second-thinking", "Now ", true),
        streamed_text("second-thinking", "answer.", true),
        streamed_text("answer", "It is 3.", false),
        turn_ended(200),
    ]));
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
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
            ] = &message.output.steps[..]
            else {
                panic!("expected thinking, a call, and thinking");
            };
            assert_eq!((first.as_str(), second.as_str()), ("Add first.", "Now "));
            assert!(!message.output.thinking_in_progress(0));
            assert!(message.output.thinking_in_progress(2));
            assert_eq!(message.step_views.len(), 3);
            assert!(!message.step_views[0].expanded());
            assert!(message.work_expanded, "work shows while the run goes");

            for event in turn.iter().skip(second_thinking_started) {
                thread.apply_for_test(agent_event(message_id, event.clone()), cx);
            }
            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id: message_id.into_bytes(),
                    outcome: crate::protocol::RunOutcome::Completed,
                    duration: Duration::from_secs(1),
                },
                cx,
            );
        });
        let host = view.host.read(cx);
        let TimelineMessage::Agent(message) = &host.timeline[0] else {
            panic!("expected the agent message");
        };
        assert_eq!(
            message.output.thinking().collect::<Vec<_>>(),
            ["Add first.", "Now answer."]
        );
        assert!(matches!(message.output.steps[1], AgentStep::ToolCall(_)));
        assert_eq!(message.output.text, "It is 3.");
        assert!(message.output.thinking_complete);
        assert!(!message.work_expanded, "work collapses when the run ends");

        let welcome = protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: host.to_protocol(),
            draft: host.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let restored = Thread::from_welcome(
            welcome,
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        );
        assert_eq!(restored.conversation(), view.host.read(cx).conversation());
    });
}

#[gpui::test]
fn membership_events_are_idempotent_and_uncatalogued_models_cannot_run(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|_, cx| {
        let draft = ThreadDraft::new(ParticipantId::new());
        EmptyThreadTestView {
            thread: Cowork::new_empty_local_thread(
                draft,
                ParticipantId::new(),
                Arc::new(test_catalog()),
                None,
                cx,
            ),
            draft_id: Uuid::nil(),
        }
    });
    let first = ParticipantId::new();
    let second = ParticipantId::new();

    cx.update(|_, cx| {
        let thread = view.read(cx).thread.clone();
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
    let turn = |total| Usage {
        total_tokens: Some(total),
        ..Default::default()
    };

    cowork.update(cx, |cowork, cx| {
        let local = cowork.active_thread(cx).expect("active thread");
        let joined = cx.new(|cx| {
            Thread::from_welcome(
                protocol::Welcome {
                    participant_id: ParticipantId::new().into_bytes(),
                    thread: local.read(cx).to_protocol(),
                    draft_generation: 0,
                    draft: local.read(cx).draft().encode_state(),
                    presence: Vec::new(),
                    stored_attachments: Vec::new(),
                },
                ThreadDraft::new(ParticipantId::new()),
                ThreadSharing::NotShared,
                cx,
            )
        });

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

        for (thread, seconds) in [(&local, 20), (&joined, 90)] {
            let id = Uuid::new_v4().into_bytes();
            thread.update(cx, |thread, cx| {
                thread.apply_for_test(agent_started(Uuid::from_bytes(id), None, "prompt"), cx);
                thread.apply_for_test(
                    protocol::HostMessage::AgentEnded {
                        id,
                        outcome: crate::protocol::RunOutcome::Completed,
                        duration: Duration::from_secs(seconds),
                    },
                    cx,
                );
            });
        }
        assert_eq!(cowork.longest_chat(cx), Duration::from_secs(20));
    });
}

#[gpui::test]
fn context_tokens_are_estimated_while_streaming(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|_, cx| {
        let draft = ThreadDraft::new(ParticipantId::new());
        EmptyThreadTestView {
            thread: Cowork::new_empty_local_thread(
                draft,
                ParticipantId::new(),
                Arc::default(),
                None,
                cx,
            ),
            draft_id: Uuid::nil(),
        }
    });
    let id = Uuid::new_v4().into_bytes();
    let text = |text: &str| agent_event(Uuid::from_bytes(id), streamed_text("answer", text, false));

    cx.update(|_, cx| {
        let thread = view.read(cx).thread.clone();
        thread.update(cx, |thread, cx| {
            assert_eq!(thread.live_context_tokens(), None);
            thread.apply_for_test(agent_started(Uuid::from_bytes(id), None, "prompt"), cx);

            // Before any count, streamed output is all there is.
            thread.apply_for_test(text("12345"), cx);
            assert_eq!(thread.live_context_tokens(), Some(2));

            // A measurement replaces the estimate, which then grows on.
            thread.apply_for_test(agent_event(Uuid::from_bytes(id), turn_ended(100)), cx);
            assert_eq!(thread.live_context_tokens(), Some(100));
            thread.apply_for_test(text("12345678"), cx);
            assert_eq!(thread.live_context_tokens(), Some(102));
        });

        // Someone joining mid-stream sees the same count.
        let welcome = protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: thread.read(cx).to_protocol(),
            draft: thread.read(cx).draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let draft = ThreadDraft::new(ParticipantId::new());
        let mirror =
            cx.new(|cx| Thread::from_welcome(welcome, draft, ThreadSharing::NotShared, cx));
        assert_eq!(mirror.read(cx).live_context_tokens(), Some(102));

        // Output of a stopped request never reaches the transcript.
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(
                protocol::HostMessage::AgentEnded {
                    id,
                    outcome: crate::protocol::RunOutcome::Completed,
                    duration: Duration::from_secs(1),
                },
                cx,
            );
            assert_eq!(thread.live_context_tokens(), Some(100));
        });
    });
}

#[gpui::test]
fn running_agent_message_is_the_incomplete_one(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let (view, cx) = cx.add_window_view(|_, cx| {
        let draft = ThreadDraft::new(ParticipantId::new());
        EmptyThreadTestView {
            thread: Cowork::new_empty_local_thread(
                draft,
                ParticipantId::new(),
                Arc::default(),
                None,
                cx,
            ),
            draft_id: Uuid::nil(),
        }
    });
    let finished = Uuid::new_v4();
    let running = Uuid::new_v4();

    cx.update(|_, cx| {
        let thread = view.read(cx).thread.clone();
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
            thread.apply_for_test(joined(collaborator), cx)
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
        assert_eq!(thread.transcript.len(), 1);
        thread.transcript[0].clone()
    });
    assert_eq!(
        prompt_texts(&first_prompt),
        [format!("{derived_name}:\nfirst")]
    );

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
    thread.update(cx, |thread, _| {
        thread.generating = false;
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
        assert_eq!(thread.transcript.len(), 2);
        assert_eq!(
            prompt_texts(&thread.transcript[0]),
            prompt_texts(&first_prompt)
        );
        assert_eq!(
            prompt_texts(&thread.transcript[1]),
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
    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let mut turn = vec![streamed_text("answer", "Let me check.", false)];
    turn.extend(streamed_tool_call(
        "sum",
        "calculate",
        serde_json::json!({"operation": "add", "a": 1, "b": 2}),
    ));
    let turn = agent::test_support::canonical(turn);
    let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
        host: Cowork::new_empty_local_thread(
            ThreadDraft::new(ParticipantId::new()),
            ParticipantId::new(),
            Arc::default(),
            None,
            cx,
        ),
        collaborator: None,
    });
    view.update(cx, |view, cx| {
        view.host.update(cx, |thread, cx| {
            thread.apply_for_test(agent_started(message_id, None, "Add 1 and 2"), cx);
            thread.apply_for_test(
                agent_event(message_id, streamed_text("answer", "Let me check.", false)),
                cx,
            );
            let message = thread
                .agent_message_mut(message_id.into_bytes())
                .expect("the agent message");
            // Until a tool call follows, the text may be the response.
            assert_eq!(message.output.text, "Let me check.");
            for event in turn.iter().skip(1) {
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
        let host = view.host.read(cx);
        let TimelineMessage::Agent(message) = &host.timeline[0] else {
            panic!("expected the agent message");
        };
        assert_eq!(message.run.outcome(), Some(&protocol::RunOutcome::Stopped));
        assert_eq!(message.run.failure(), None);
        assert!(!message.work_expanded);
        assert_eq!(message.output.text, "");
        assert_eq!(message.output.work().collect::<Vec<_>>(), [0, 1]);

        let welcome = protocol::Welcome {
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: host.to_protocol(),
            draft: host.draft().encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let restored = Thread::from_welcome(
            welcome,
            ThreadDraft::new(ParticipantId::new()),
            ThreadSharing::NotShared,
            cx,
        );
        assert_eq!(restored.conversation(), view.host.read(cx).conversation());
        let TimelineMessage::Agent(restored) = &restored.timeline[0] else {
            panic!("expected the agent message");
        };
        assert!(
            !restored.work_expanded,
            "a joiner sees ended runs collapsed"
        );
    });
}
