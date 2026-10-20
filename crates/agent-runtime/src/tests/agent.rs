use super::*;

struct ReadOnlyDispatch(Dispatch);
impl ToolDispatch for ReadOnlyDispatch {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut descriptors = self.0.descriptors();
        descriptors[0].capabilities.is_read_only = true;
        descriptors
    }
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: serde_json::Value,
        context: ToolCallContext,
    ) -> BoxFuture<'a, ToolStream<TypedToolOutput>> {
        self.0.call(name, args, context)
    }
}

fn session_builder() -> (
    AgentBuilder<&'static str>,
    mpsc::UnboundedReceiver<SamplingRequest>,
    mpsc::UnboundedReceiver<Invocation>,
    Arc<AtomicUsize>,
) {
    let (provider, requests) = test_provider();
    let (invocations, calls_rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatch = Arc::new(ReadOnlyDispatch(Dispatch {
        calls: calls.clone(),
        invocations,
    }));
    (
        AgentBuilder::new(provider, LanguageModel::new("scripted"), "local")
            .tool_dispatch(dispatch),
        requests,
        calls_rx,
        calls,
    )
}

#[tokio::test]
async fn reusable_definitions_keep_history_isolated_and_allow_instance_overrides() {
    let definition = AgentDefinition {
        name: "worker".into(),
        system_prompt: "Use work.".into(),
        tools: Some(vec!["work".into()]),
        permissions: ToolPermissions::AllowListed(vec!["work".into()]),
        max_output_tokens: 256,
        thinking: Some(language_model::Thinking::Disabled),
        ..Default::default()
    };
    for selected in [true, false] {
        let (builder, mut requests, mut invocations, calls) = session_builder();
        let builder = builder.definition(&definition);
        let builder = if selected {
            builder
        } else {
            // Even allow-all cannot dispatch a tool excluded from this instance.
            builder
                .tools(Vec::<String>::new())
                .allow_all_tools()
                .max_output_tokens(512)
        };
        let agent = builder.build().await.unwrap();
        assert_eq!(agent.definition().name, "worker");
        let model = async move {
            let first = recv(&mut requests).await;
            assert_eq!(first.request.messages.len(), 2);
            assert_eq!(
                first.request.messages[0].content,
                vec![Content::text("Use work.")]
            );
            assert_eq!(
                first.request.max_output_tokens,
                Some(if selected { 256 } else { 512 })
            );
            assert_eq!(
                first.request.thinking,
                Some(language_model::Thinking::Disabled)
            );
            assert_eq!(first.request.tools.len(), usize::from(selected));
            respond(first, reply(vec![Content::ToolCall(call())], "tool_calls"));
            if selected {
                assert!(
                    recv(&mut invocations)
                        .await
                        .response
                        .send(finished_only(Ok(output())))
                        .is_ok()
                );
            }
            let next = recv(&mut requests).await;
            let Content::ToolResult(result) = &next.request.messages[3].content[0] else {
                panic!()
            };
            assert_eq!(result.is_error, !selected);
            if !selected {
                assert!(
                    matches!(&result.content[0], language_model::ToolResultContent::Json { value } if value["kind"] == "not_found")
                );
            }
            respond(next, reply(vec![Content::text("done")], "stop"));
        };
        let (reply, ()) =
            tokio::time::timeout(WAIT, async { futures::join!(agent.prompt("work"), model) })
                .await
                .unwrap();
        assert_eq!(reply.unwrap().text(), "done");
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(selected));
        agent.close().await.unwrap();
    }
    assert_eq!(definition.max_output_tokens, 256);
    let (builder, _, _, _) = session_builder();
    assert!(matches!(
        builder
            .definition(&definition)
            .tools(["missing"])
            .build()
            .await,
        Err(AgentError::Tool(ToolError {
            kind: ToolErrorKind::NotFound,
            ..
        }))
    ));
}

#[tokio::test]
async fn managed_prompts_bind_their_own_replies_without_event_watchers() {
    let (builder, mut requests, _, _) = session_builder();
    let session = builder.build().await.unwrap();
    let prompts = async { futures::join!(session.prompt("first"), session.prompt("second")) };
    let model = async move {
        let first = recv(&mut requests).await;
        assert_eq!(
            first.request.messages.last().unwrap().content,
            vec![Content::text("first")]
        );
        respond(first, reply(vec![Content::text("first answer")], "stop"));
        let second = recv(&mut requests).await;
        assert_eq!(
            second.request.messages.last().unwrap().content,
            vec![Content::text("second")]
        );
        respond(second, reply(vec![Content::text("second answer")], "stop"));
    };
    let ((first, second), ()) =
        tokio::time::timeout(WAIT, async { futures::join!(prompts, model) })
            .await
            .unwrap();
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(first.run, second.run);
    assert_eq!(first.text(), "first answer");
    assert_eq!(second.text(), "second answer");
    assert_eq!(
        session.close().await.unwrap().conversation.messages().len(),
        4
    );
}

#[tokio::test]
async fn managed_tools_require_explicit_permission_even_when_read_only() {
    for allow in [false, true] {
        let (builder, mut requests, mut invocations, calls) = session_builder();
        let session = if allow {
            builder.allow_tools(["work"])
        } else {
            builder
        }
        .build()
        .await
        .unwrap();
        let prompt = session.prompt("work");
        let model = async move {
            respond(
                recv(&mut requests).await,
                reply(vec![Content::ToolCall(call())], "tool_calls"),
            );
            if allow {
                let invocation = recv(&mut invocations).await;
                assert!(
                    invocation
                        .response
                        .send(finished_only(Ok(output())))
                        .is_ok()
                );
            }
            let next = recv(&mut requests).await;
            let Content::ToolResult(result) = &next.request.messages[2].content[0] else {
                panic!()
            };
            assert_eq!(result.is_error, !allow);
            if !allow {
                assert!(
                    matches!(&result.content[0], language_model::ToolResultContent::Json { value } if value["kind"] == "permission_denied")
                );
            }
            respond(next, reply(vec![Content::text("done")], "stop"));
        };
        let (result, ()) = tokio::time::timeout(WAIT, async { futures::join!(prompt, model) })
            .await
            .unwrap();
        assert_eq!(result.unwrap().text(), "done");
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(allow));
        session.close().await.unwrap();
    }
}

#[tokio::test]
async fn managed_prompt_returns_stop_and_rejects_a_paused_queue_until_resumed() {
    let (builder, mut requests, _, _) = session_builder();
    let session = builder.build().await.unwrap();
    {
        let prompt = session.prompt("work");
        futures::pin_mut!(prompt);
        assert!(prompt.as_mut().now_or_never().is_none());
        let request = recv(&mut requests).await;
        let handle = session.handle();
        let run = handle.snapshot().await.unwrap().run.unwrap();
        handle.stop(run).await.unwrap();
        assert!(
            matches!(prompt.await, Err(AgentError::Prompt(PromptError::Stopped(id))) if id == run)
        );
        assert!(request.response.is_closed());
    }
    assert!(matches!(
        session.prompt("paused").await,
        Err(AgentError::Prompt(PromptError::Rejected(_)))
    ));
    session.handle().resume().unwrap();
    let model = async move {
        respond(
            recv(&mut requests).await,
            reply(vec![Content::text("resumed")], "stop"),
        );
    };
    let (result, ()) = tokio::time::timeout(WAIT, async {
        futures::join!(session.prompt("resume"), model)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().text(), "resumed");
    session.close().await.unwrap();
}

#[tokio::test]
async fn managed_compaction_distinguishes_unchanged_from_operational_failure() {
    let (builder, _, _, _) = session_builder();
    let session = builder.build().await.unwrap();
    assert!(matches!(
        session.compact().await.unwrap(),
        CompactionOutcome::Unchanged(CompactionError::NothingToCompact)
    ));
    session.close().await.unwrap();

    let (builder, mut requests, _, _) = session_builder();
    let session = builder
        .request(CompletionRequest::new(compaction::history()))
        .build()
        .await
        .unwrap();
    let model = async move {
        let request = recv(&mut requests).await;
        assert!(
            request
                .response
                .send(
                    stream::iter([Err(language_model::Error::Transport("offline".into()))]).boxed()
                )
                .is_ok()
        );
    };
    let (result, ()) =
        tokio::time::timeout(WAIT, async { futures::join!(session.compact(), model) })
            .await
            .unwrap();
    assert!(matches!(
        result,
        Err(AgentError::Compaction(CompactionError::Model(_)))
    ));
    session.close().await.unwrap();
}

#[tokio::test]
async fn managed_storage_refuses_overwrite_or_instruction_drift_and_can_explicitly_resume() {
    let directory = super::persistence::Directory::new();
    let (builder, mut requests, _, _) = session_builder();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let events = observations.clone();
    let session = builder
        .system_prompt("original instructions")
        .create_at(&directory.0)
        .on_event(move |event| events.lock().unwrap().push(event))
        .build()
        .await
        .unwrap();
    let model = async move {
        respond(
            recv(&mut requests).await,
            reply(vec![Content::text("saved answer")], "stop"),
        );
    };
    let (result, ()) = tokio::time::timeout(WAIT, async {
        futures::join!(session.prompt("first"), model)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().text(), "saved answer");
    session.close().await.unwrap();
    assert_eq!(
        observations
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event.kind, EventKind::RunFinished { .. }))
            .count(),
        1
    );
    let path = directory.0.join("updates.jsonl");
    let before = tokio::fs::read(&path).await.unwrap();
    let (builder, _, _, _) = session_builder();
    assert!(matches!(
        builder.create_at(&directory.0).build().await,
        Err(AgentError::Log(_))
    ));
    let (builder, _, _, _) = session_builder();
    assert!(matches!(
        builder
            .system_prompt("changed instructions")
            .resume_from(&directory.0)
            .build()
            .await,
        Err(AgentError::Configuration(_))
    ));
    assert_eq!(tokio::fs::read(&path).await.unwrap(), before);

    let (builder, mut requests, _, _) = session_builder();
    let session = builder
        .system_prompt("changed instructions")
        .use_saved_instructions()
        .open_or_create(&directory.0)
        .build()
        .await
        .unwrap();
    let model = async move {
        let request = recv(&mut requests).await;
        assert_eq!(
            request.request.messages[0].content,
            vec![Content::text("original instructions")]
        );
        assert_eq!(
            request.request.messages[2].content,
            vec![Content::text("saved answer")]
        );
        respond(
            request,
            reply(vec![Content::text("resumed answer")], "stop"),
        );
    };
    let (result, ()) = tokio::time::timeout(WAIT, async {
        futures::join!(session.prompt("second"), model)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().text(), "resumed answer");
    let result = session.close().await.unwrap();
    assert_eq!(
        crate::persistence::load(&directory.0)
            .await
            .unwrap()
            .conversation
            .messages(),
        result.conversation.messages()
    );
}

#[tokio::test]
async fn managed_close_reports_background_write_failure_without_blocking_agent_work() {
    let directory = super::persistence::Directory::new();
    let (builder, mut requests, _, _) = session_builder();
    let session = builder.create_at(&directory.0).build().await.unwrap();
    let path = directory.0.join("chat_history.jsonl");
    tokio::fs::remove_file(&path).await.unwrap();
    tokio::fs::create_dir(&path).await.unwrap();
    let model = async move {
        respond(
            recv(&mut requests).await,
            reply(vec![Content::text("still ran")], "stop"),
        );
    };
    let (result, ()) = tokio::time::timeout(WAIT, async {
        futures::join!(session.prompt("work"), model)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().text(), "still ran");
    assert!(matches!(session.close().await, Err(AgentError::Log(_))));
}
