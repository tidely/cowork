use super::*;

pub(super) fn history() -> Vec<Message> {
    let mut assistant = reply(
        vec![Content::text("important old answer ".repeat(250))],
        "stop",
    );
    assistant.replay.push(ReplayData {
        provider: "scripted".into(),
        model: None,
        format: "signed".into(),
        version: 1,
        payload: serde_json::json!({"signature":"unaltered"}),
    });
    vec![
        Message::system("keep instructions"),
        Message::user("important old request ".repeat(250)),
        assistant,
        Message::user("recent request"),
        reply(vec![Content::text("recent answer")], "stop"),
    ]
}

pub(super) fn policy(automatic: bool) -> CompactionPolicy {
    CompactionPolicy {
        trigger_bytes: NonZeroUsize::new(if automatic { 1 } else { usize::MAX }).unwrap(),
        keep_recent_messages: NonZeroUsize::new(1).unwrap(),
        max_summary_tokens: std::num::NonZeroU64::new(1024).unwrap(),
    }
}

pub(super) fn seeded(
    provider: Arc<dyn LanguageModelProvider>,
    executor: ToolExecutor,
    automatic: bool,
) -> (AgentSession<&'static str>, SessionHandle<&'static str>) {
    let (session, handle) = AgentSession::new(
        provider,
        LanguageModel::new("scripted"),
        executor,
        CompletionRequest::new(history()),
    )
    .unwrap();
    (session.with_compaction(policy(automatic)), handle)
}

#[test]
fn compaction_preserves_full_history_prefix_and_concurrent_tail_across_revisions() {
    let original = history();
    let mut conversation = Conversation::new(original.clone()).unwrap();
    let plan = conversation.plan_compaction(1).unwrap();
    let late = Message::user("arrived during summary");
    conversation.append(vec![late.clone()]).unwrap();
    let compacted = crate::compaction::finish_summary(
        &plan,
        &reply(vec![Content::text("older work summarized")], "stop"),
    )
    .unwrap();
    conversation.apply_compaction(compacted.clone()).unwrap();
    assert_eq!(&conversation.messages()[..original.len()], &original);
    assert_eq!(conversation.messages().last(), Some(&late));
    let context = conversation.model_messages();
    assert_eq!(context[0], original[0]);
    assert_eq!(context[1], compacted.summary);
    assert_eq!(&context[2..], &[original[4].clone(), late]);
    assert_eq!(
        conversation.apply_compaction(compacted),
        Err(CompactionError::Stale)
    );

    conversation
        .append(vec![
            reply(vec![Content::text("large new answer ".repeat(200))], "stop"),
            Message::user("newest"),
        ])
        .unwrap();
    let next = conversation.plan_compaction(1).unwrap();
    assert_eq!(next.previous_revision, 1);
    assert!(
        next.messages
            .iter()
            .any(|message| message.id == context[1].id)
    );
    let second = crate::compaction::finish_summary(
        &next,
        &reply(vec![Content::text("all earlier work summarized")], "stop"),
    )
    .unwrap();
    conversation.apply_compaction(second).unwrap();
    assert_eq!(conversation.compactions().len(), 2);
    assert_eq!(conversation.messages().len(), original.len() + 3);
    assert_eq!(conversation.model_messages().len(), 3);
}

#[test]
fn compaction_never_splits_tool_batches_and_rejects_non_reducing_or_stale_replacements() {
    let mut messages = history();
    let call = call();
    messages.push(reply(vec![Content::ToolCall(call.clone())], "tool_calls"));
    messages.push(Message::new(
        Role::Tool,
        vec![Content::ToolResult(language_model::ToolResult {
            call: call.id,
            provider_id: call.provider_id,
            name: Some(call.name),
            content: vec![language_model::ToolResultContent::Text {
                text: "actual output".into(),
            }],
            is_error: false,
        })],
    ));
    let mut conversation = Conversation::new(messages.clone()).unwrap();
    let plan = conversation.plan_compaction(1).unwrap();
    assert_eq!(plan.range.end, 5); // Both the call and result remain in the tail.
    let invalid = Compaction {
        previous_revision: 0,
        range: 1..6,
        summary: Message::user("small"),
    };
    assert!(matches!(
        conversation.apply_compaction(invalid),
        Err(CompactionError::Conversation(
            ConversationError::UnsettledCalls
        ))
    ));
    let large = Compaction {
        previous_revision: 0,
        range: plan.range.clone(),
        summary: Message::user("too large ".repeat(5000)),
    };
    assert_eq!(
        conversation.apply_compaction(large),
        Err(CompactionError::NotSmaller)
    );
    assert_eq!(conversation.messages(), messages);
    assert!(conversation.compactions().is_empty());
}

#[tokio::test]
async fn automatic_compaction_uses_a_tool_free_request_and_keeps_steering_during_summary() {
    let (provider, mut requests) = test_provider();
    let (executor, _, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = seeded(provider, executor, true);
    let initial = session.conversation().messages().to_vec();
    let mut harness = Harness::start(session, handle);
    harness.submit("new request", Delivery::Queue).await;
    let summary = recv(&mut requests).await;
    assert!(summary.request.tools.is_empty());
    assert!(
        summary
            .request
            .messages
            .iter()
            .all(|message| message.replay.is_empty())
    );
    assert_eq!(
        harness.handle.snapshot().await.unwrap().phase,
        Phase::Compacting
    );
    harness
        .handle
        .submit(input("parent", "steer during summary"), Delivery::Steer)
        .await
        .unwrap();
    respond(
        summary,
        reply(vec![Content::text("old work summarized")], "stop"),
    );
    let request = recv(&mut requests).await;
    assert_eq!(request.request.messages.len(), 4);
    assert_eq!(request.request.messages[0], initial[0]);
    assert_eq!(
        request.request.messages[2].content,
        vec![Content::text("new request")]
    );
    assert_eq!(
        request.request.messages[3].content,
        vec![Content::text("steer during summary")]
    );
    respond(request, reply(vec![Content::text("done")], "stop"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    let summary_id = harness
        .log
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Compacted(compaction) => Some(compaction.summary.id.clone()),
            _ => None,
        })
        .unwrap();
    let mut duplicate = input("human", "reused summary identity");
    duplicate.payload.id = summary_id;
    assert!(
        harness
            .handle
            .submit(duplicate, Delivery::Queue)
            .await
            .is_err()
    );
    let (result, events) = harness.finish().await;
    assert_eq!(&result.conversation.messages()[..initial.len()], &initial);
    let mut replica = Conversation::new(initial).unwrap();
    for event in events {
        match event.kind {
            EventKind::MessageCommitted { index, message, .. } => {
                replica.apply(index, message).unwrap()
            }
            EventKind::Compacted(compaction) => replica.apply_compaction(compaction).unwrap(),
            _ => {}
        }
    }
    assert_eq!(replica.messages(), result.conversation.messages());
    assert_eq!(
        replica.model_messages(),
        result.conversation.model_messages()
    );
}

#[tokio::test]
async fn manual_compaction_failures_preserve_context_and_do_not_fail_the_session() {
    for reason in ["length", "empty", "larger", "transport"] {
        let (provider, mut requests) = test_provider();
        let (executor, _, _) = test_executor(Arc::new(AllowAll));
        let (session, handle) = seeded(provider, executor, false);
        let initial = session.conversation().messages().to_vec();
        let harness = Harness::start(session, handle);
        let commands = harness.handle.clone();
        let compact = tokio::spawn(async move { commands.compact().await });
        let summary = recv(&mut requests).await;
        if reason == "transport" {
            assert!(
                summary
                    .response
                    .send(
                        stream::iter([Err(language_model::Error::Transport("unavailable".into()))])
                            .boxed()
                    )
                    .is_ok()
            );
        } else {
            let text = match reason {
                "empty" => String::new(),
                "larger" => "large".repeat(10000),
                _ => "summary".into(),
            };
            respond(
                summary,
                reply(
                    vec![Content::text(text)],
                    if reason == "length" { "length" } else { "stop" },
                ),
            );
        }
        assert!(
            tokio::time::timeout(WAIT, compact)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(harness.handle.snapshot().await.unwrap().phase, Phase::Idle);
        let (result, _) = harness.finish().await;
        assert_eq!(result.conversation.model_messages(), initial);
        assert!(result.conversation.compactions().is_empty());
    }
}

#[tokio::test]
async fn stop_during_summary_cancels_it_without_a_context_transition() {
    let (provider, mut requests) = test_provider();
    let (executor, _, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = seeded(provider, executor, true);
    let mut harness = Harness::start(session, handle);
    harness.submit("new request", Delivery::Queue).await;
    let summary = recv(&mut requests).await;
    let run = harness.handle.snapshot().await.unwrap().run.unwrap();
    harness.handle.stop(run).await.unwrap();
    harness
        .event(|kind| {
            matches!(
                kind,
                EventKind::RunFinished {
                    outcome: RunOutcome::Stopped,
                    ..
                }
            )
        })
        .await;
    assert!(summary.response.is_closed());
    let (result, events) = harness.finish().await;
    assert!(result.conversation.compactions().is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::Compacted(_)))
    );
}
