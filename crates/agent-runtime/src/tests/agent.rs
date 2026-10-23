//! The managed `Agent` with `BasicHost`: prompts, permissions, compaction,
//! storage, and shutdown.

use super::persistence::Directory;
use super::state::Echo;
use super::*;

fn agent_builder() -> (
    AgentBuilder<BasicHost>,
    mpsc::UnboundedReceiver<SamplingRequest>,
) {
    let (provider, requests) = test_provider();
    let definition = AgentDefinition::new(LanguageModel::new("scripted"))
        .instructions("Use echo.")
        .tool("echo");
    let builder = AgentBuilder::new(Arc::new(BasicHost), provider, definition, "user".into())
        .tool(Echo)
        .unwrap();
    (builder, requests)
}

async fn prompt_and_answer(
    agent: &Agent,
    requests: &mut mpsc::UnboundedReceiver<SamplingRequest>,
    prompt: &str,
    answer: &str,
) -> Reply {
    let model = async {
        respond(recv(requests).await, text(answer));
    };
    let (reply, ()) =
        tokio::time::timeout(WAIT, async { futures::join!(agent.prompt(prompt), model) })
            .await
            .unwrap();
    reply.unwrap()
}

#[tokio::test]
async fn tools_run_only_when_permitted_and_the_model_is_told_which() {
    for allowed in [false, true] {
        let (builder, mut requests) = agent_builder();
        let builder = if allowed {
            builder.permissions(ToolPermissions::allow(["echo"]))
        } else {
            builder
        };
        let agent = builder.build().await.unwrap();
        let model = async move {
            let first = recv(&mut requests).await;
            let Content::Text { text: note } = &first.request.messages[0].content[0] else {
                panic!()
            };
            let expected = if allowed {
                "All tools run without approval."
            } else {
                "Calls to any tool are refused."
            };
            assert!(note.contains(expected), "{note}");
            respond(first, calls(vec![tool_call("echo", json!({"text": "hi"}))]));
            let next = recv(&mut requests).await;
            let result = result_of(next.request.messages.last().unwrap());
            assert_eq!(result.is_error, !allowed);
            respond(next, text("done"));
        };
        let (reply, ()) = tokio::time::timeout(WAIT, async {
            futures::join!(agent.prompt("echo hi"), model)
        })
        .await
        .unwrap();
        assert_eq!(reply.unwrap().text(), "done");
        agent.close().await.unwrap();
    }
}

#[tokio::test]
async fn concurrent_prompts_each_receive_their_own_reply() {
    let (builder, mut requests) = agent_builder();
    let agent = builder.build().await.unwrap();
    let model = async move {
        respond(recv(&mut requests).await, text("first reply"));
        respond(recv(&mut requests).await, text("second reply"));
    };
    let (first, second, ()) = tokio::time::timeout(WAIT, async {
        futures::join!(agent.prompt("first"), agent.prompt("second"), model)
    })
    .await
    .unwrap();
    assert_eq!(first.unwrap().text(), "first reply");
    assert_eq!(second.unwrap().text(), "second reply");
    agent.close().await.unwrap();
}

#[tokio::test]
async fn stopped_prompts_pause_the_queue_until_resumed() {
    let (builder, mut requests) = agent_builder();
    let agent = builder.build().await.unwrap();
    {
        let prompt = agent.prompt("work");
        futures::pin_mut!(prompt);
        assert!(prompt.as_mut().now_or_never().is_none());
        let request = recv(&mut requests).await;
        let run = agent.handle().snapshot().await.unwrap().run.unwrap();
        agent.send(RequestKind::Stop { run }).await.unwrap();
        assert!(matches!(
            prompt.await,
            Err(AgentError::Prompt(PromptError::Stopped(id))) if id == run
        ));
        assert!(request.response.is_closed());
    }
    assert!(matches!(
        agent.prompt("paused").await,
        Err(AgentError::Prompt(PromptError::Request(
            RequestError::Rejected(_)
        )))
    ));
    agent.send(RequestKind::Resume).await.unwrap();
    let reply = prompt_and_answer(&agent, &mut requests, "resume", "resumed").await;
    assert_eq!(reply.text(), "resumed");
    agent.close().await.unwrap();
}

#[tokio::test]
async fn compaction_distinguishes_unchanged_from_operational_failure() {
    let (builder, mut requests) = agent_builder();
    let agent = builder.build().await.unwrap();
    assert!(matches!(
        agent.compact().await.unwrap(),
        CompactionOutcome::Unchanged(CompactionError::NothingToCompact)
    ));
    prompt_and_answer(&agent, &mut requests, "first", "answer").await;
    let model = async move {
        let request = recv(&mut requests).await;
        let error = Err(language_model::Error::Transport("offline".into()));
        assert!(request.response.send(stream::iter([error]).boxed()).is_ok());
    };
    let (result, ()) = tokio::time::timeout(WAIT, async { futures::join!(agent.compact(), model) })
        .await
        .unwrap();
    assert!(matches!(
        result,
        Err(AgentError::Request(RequestError::Compaction(
            CompactionError::Model(_)
        )))
    ));
    agent.close().await.unwrap();
}

#[tokio::test]
async fn storage_refuses_overwrite_and_resumes_with_provenance() {
    let directory = Directory::new();
    let (builder, mut requests) = agent_builder();
    let observed = Arc::new(Mutex::new(0));
    let finished = observed.clone();
    let agent = builder
        .create_at(&directory.0)
        .on_event(move |event| {
            if matches!(event.kind, EventKind::RunFinished { .. }) {
                *finished.lock() += 1;
            }
        })
        .build()
        .await
        .unwrap();
    prompt_and_answer(&agent, &mut requests, "first", "saved answer").await;
    agent.close().await.unwrap();
    assert_eq!(*observed.lock(), 1);
    let path = directory.0.join("updates.jsonl");
    let before = tokio::fs::read(&path).await.unwrap();
    let (builder, _) = agent_builder();
    assert!(matches!(
        builder.create_at(&directory.0).build().await,
        Err(AgentError::Log(_))
    ));
    assert_eq!(tokio::fs::read(&path).await.unwrap(), before);

    let (builder, mut requests) = agent_builder();
    let agent = builder.open_or_create(&directory.0).build().await.unwrap();
    let model = async move {
        let request = recv(&mut requests).await;
        assert_eq!(
            request.request.messages[2].content,
            vec![Content::text("saved answer")]
        );
        respond(request, text("resumed answer"));
    };
    let (reply, ()) = tokio::time::timeout(WAIT, async {
        futures::join!(agent.prompt("second"), model)
    })
    .await
    .unwrap();
    assert_eq!(reply.unwrap().text(), "resumed answer");
    let result = agent.close().await.unwrap();
    let loaded = crate::persistence::load::<String>(&directory.0)
        .await
        .unwrap();
    assert_eq!(loaded.conversation.entries(), result.conversation.entries());
    assert!(matches!(
        &loaded.conversation.entries()[1].origin,
        Origin::Input { source, .. } if source == "user"
    ));
}

/// After a crash, a call the log never saw start (waiting for approval, or
/// behind another call) had no effect: resuming settles it as not run. A call
/// that may have started needs explicit recovery.
#[tokio::test]
async fn resuming_after_a_crash_settles_only_calls_that_never_started() {
    for started in [false, true] {
        let directory = Directory::new();
        let log =
            crate::persistence::JsonlLog::create(&directory.0, &Conversation::<String>::new(), 0)
                .await
                .unwrap();
        let pending = tool_call("echo", json!({"text": "hi"}));
        let entries = [
            Entry {
                message: Message::user("echo hi"),
                origin: Origin::Input {
                    id: InputId::new(),
                    source: "user".to_owned(),
                },
            },
            Entry {
                message: calls(vec![pending.clone()]),
                origin: Origin::Model {
                    revision: DefinitionRevision(0),
                },
            },
        ];
        let mut events: Vec<EventKind<BasicHost>> = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| EventKind::MessageCommitted { index, entry })
            .collect();
        if started {
            events.push(EventKind::ToolStarted {
                call: pending.clone(),
                tool: "echo".into(),
            });
        }
        for (sequence, kind) in events.into_iter().enumerate() {
            let event = AgentEvent::<BasicHost> {
                sequence: sequence as u64,
                run: None,
                kind,
            };
            log.record(&event).unwrap();
        }
        log.finish().await.unwrap();

        let (builder, mut requests) = agent_builder();
        let built = builder.resume_from(&directory.0).build().await;
        if started {
            assert!(matches!(built, Err(AgentError::Session(_))));
            continue;
        }
        let agent = built.unwrap();
        let model = async move {
            let request = recv(&mut requests).await;
            assert_eq!(error_kind(&request.request.messages[2]), "cancelled");
            respond(request, text("ok"));
        };
        let (reply, ()) =
            tokio::time::timeout(WAIT, async { futures::join!(agent.prompt("again"), model) })
                .await
                .unwrap();
        assert_eq!(reply.unwrap().text(), "ok");
        let result = agent.close().await.unwrap();
        // The settled result was logged like any other entry.
        let loaded = crate::persistence::load::<String>(&directory.0)
            .await
            .unwrap();
        assert_eq!(loaded.conversation.entries(), result.conversation.entries());
    }
}

#[tokio::test]
async fn close_reports_background_write_failure_without_blocking_agent_work() {
    let directory = Directory::new();
    let (builder, mut requests) = agent_builder();
    let agent = builder.create_at(&directory.0).build().await.unwrap();
    let path = directory.0.join("chat_history.jsonl");
    tokio::fs::remove_file(&path).await.unwrap();
    tokio::fs::create_dir(&path).await.unwrap();
    let reply = prompt_and_answer(&agent, &mut requests, "work", "still ran").await;
    assert_eq!(reply.text(), "still ran");
    assert!(matches!(agent.close().await, Err(AgentError::Log(_))));
}
