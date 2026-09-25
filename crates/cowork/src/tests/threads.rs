//! Thread state: titles, ownership, applying host events, context usage,
//! and requests a collaborator sends the host.

use super::*;

#[test]
fn collaborator_threads_are_writable_and_removed_on_disconnect() {
    assert!(ThreadOwnership::Remote.can_write());
    assert!(ThreadOwnership::Remote.remove_on_disconnect());
    assert!(ThreadOwnership::Local.can_write());
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
        assert_eq!(thread.draft.id, view.draft_id);
        assert_eq!(thread.summary.title, "New thread");
        assert_eq!(thread.ownership, ThreadOwnership::Local);
        assert_eq!(thread.model, Some(ollama_qwen()));
        assert_eq!(*thread.models, test_catalog());
        assert!(thread.participants.is_empty());
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
            failure: None,
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
    cx.update(gpui_component::init);
    let message_id = Uuid::new_v4();
    let events = agent_stream_events(message_id);
    // The collaborator joins once the agent has started reasoning, so it has
    // to fold the rest of that turn from what it missed.
    let joined_after = 4;

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
                thread.participants = vec![thread.participant_id];
                thread.apply(joined(collaborator_participant), cx);
            });
            for event in events.iter().take(joined_after) {
                view.host
                    .update(cx, |thread, cx| thread.apply(event.clone(), cx));
            }

            let welcome = protocol::Welcome {
                participant_id: collaborator_participant.into_bytes(),
                thread: view.host.read(cx).to_protocol(),
                draft: view.host.read(cx).draft.doc.encode_state(),
                presence: Vec::new(),
                stored_attachments: Vec::new(),
            };
            let draft = ThreadDraft::new(ParticipantId::new());
            let collaborator =
                cx.new(|cx| Thread::from_welcome(welcome, draft, ThreadSharing::NotShared, cx));

            for event in events.iter().skip(joined_after) {
                view.host
                    .update(cx, |thread, cx| thread.apply(event.clone(), cx));
                collaborator.update(cx, |thread, cx| thread.apply(event.clone(), cx));
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

        assert_eq!(collaborator.to_protocol(), host.to_protocol());
        assert_eq!(collaborator.summary.id, host.summary.id);
        assert_ne!(collaborator.instance_id, host.instance_id);
        assert_eq!(collaborator.participant_id, collaborator_participant);
        assert_eq!(collaborator.draft.author, collaborator_participant);
        assert_eq!(collaborator.participants, host.participants);
        assert_eq!(collaborator.participants.len(), 3);
        assert_eq!(
            collaborator.participants[..2],
            [host_participant, collaborator_participant]
        );
        assert_eq!(collaborator.model, Some(ollama_qwen()));
        assert_eq!(collaborator.max_tokens(), 65_536);
        assert_eq!(collaborator.context_tokens, Some(2_048));
        assert!(!host.generating);
        assert!(!collaborator.generating);

        let TimelineMessage::Agent(message) = &collaborator.timeline[1] else {
            panic!("expected the agent's reply");
        };
        assert_eq!(message.thinking, "Weighing options.");
        assert_eq!(message.text, "Here is the answer. Done.");
        assert!(message.thinking_complete);
        assert!(message.complete);
        assert!(!message.failed);
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
                thread.apply(event, cx);
            }
            assert_eq!(thread.participants, [first, second]);
            assert_eq!(thread.model, Some(ollama_model("no-such-model")));
            assert!(thread.runnable_model().is_none());
            assert_eq!(thread.max_tokens(), 0);

            thread.apply(
                protocol::HostMessage::ParticipantLeft(first.into_bytes()),
                cx,
            );
            thread.apply(
                protocol::HostMessage::ParticipantLeft(first.into_bytes()),
                cx,
            );
            assert_eq!(thread.participants, [second]);
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
        let joined = cx.new(|_| {
            let mut thread = test_thread(
                Uuid::new_v4(),
                Vec::new(),
                ThreadDraft::new(ParticipantId::new()),
            );
            thread.ownership = ThreadOwnership::Remote;
            thread
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
                thread.apply(agent_started(Uuid::from_bytes(id), None, "prompt"), cx);
                thread.apply(
                    protocol::HostMessage::AgentEnded {
                        id,
                        failure: None,
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
            thread.apply(agent_started(Uuid::from_bytes(id), None, "prompt"), cx);

            // Before any count, streamed output is all there is.
            thread.apply(text("12345"), cx);
            assert_eq!(thread.live_context_tokens(), Some(2));

            // A measurement replaces the estimate, which then grows on.
            thread.apply(agent_event(Uuid::from_bytes(id), turn_ended(100)), cx);
            assert_eq!(thread.live_context_tokens(), Some(100));
            thread.apply(text("12345678"), cx);
            assert_eq!(thread.live_context_tokens(), Some(102));
        });

        // Someone joining mid-stream sees the same count.
        let welcome = protocol::Welcome {
            participant_id: ParticipantId::new().into_bytes(),
            thread: thread.read(cx).to_protocol(),
            draft: thread.read(cx).draft.doc.encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let draft = ThreadDraft::new(ParticipantId::new());
        let mirror =
            cx.new(|cx| Thread::from_welcome(welcome, draft, ThreadSharing::NotShared, cx));
        assert_eq!(mirror.read(cx).live_context_tokens(), Some(102));

        // Output of a stopped request never reaches the transcript.
        thread.update(cx, |thread, cx| {
            thread.apply(
                protocol::HostMessage::AgentEnded {
                    id,
                    failure: None,
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
                    failure: None,
                    duration: Duration::from_secs(3),
                },
                agent_started(running, None, "prompt"),
            ] {
                thread.apply(event, cx);
            }
            assert_eq!(thread.running_agent_message_id(), Some(running));
            // A running message has no duration yet.
            assert_eq!(thread.generation_time(), Duration::from_secs(3));

            thread.apply(
                protocol::HostMessage::AgentEnded {
                    id: running.into_bytes(),
                    failure: None,
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
        cowork.active_generations.insert(
            thread_id,
            ActiveGeneration {
                message_id: running_message_id,
                abort_handle: task.abort_handle(),
                cancelled: cancelled.clone(),
            },
        );

        for request in [
            protocol::CollaboratorMessage::SelectModel(ollama_model("no-such-model")),
            protocol::CollaboratorMessage::SelectModel(recommended_qwen()),
            protocol::CollaboratorMessage::Stop {
                message_id: Uuid::new_v4().into_bytes(),
            },
        ] {
            cowork
                .collaborator_request(&thread, ParticipantId::new(), request, cx)
                .expect("valid request");
        }
        assert_eq!(thread.read(cx).model, None);
        assert!(!cancelled.load(Ordering::Acquire));

        for request in [
            protocol::CollaboratorMessage::SelectModel(ollama_qwen()),
            protocol::CollaboratorMessage::Stop {
                message_id: running_message_id.into_bytes(),
            },
        ] {
            cowork
                .collaborator_request(&thread, ParticipantId::new(), request, cx)
                .expect("valid request");
        }
        assert_eq!(thread.read(cx).model, Some(ollama_qwen()));
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
        thread
            .draft
            .doc
            .create_prompt(thread.draft.author.as_uuid(), "second");
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
