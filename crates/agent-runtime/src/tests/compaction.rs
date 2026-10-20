use super::*;
use crate::compaction::encoded_bytes;

/// Instructions, three large old exchanges, and a small recent one.
pub(super) fn history() -> Vec<Message> {
    let mut messages = vec![Message::system("keep instructions")];
    for turn in 0..3 {
        messages.push(Message::user(format!("old request {turn} ").repeat(60)));
        let mut answer = reply(
            vec![Content::text(format!("old answer {turn} ").repeat(60))],
            "stop",
        );
        if turn == 0 {
            answer.replay.push(ReplayData {
                provider: "scripted".into(),
                model: None,
                format: "signed".into(),
                version: 1,
                payload: serde_json::json!({"signature":"unaltered"}),
            });
        }
        messages.push(answer);
    }
    messages.push(Message::user("recent request"));
    messages.push(reply(vec![Content::text("recent answer")], "stop"));
    messages
}

pub(super) fn policy(automatic: bool) -> CompactionPolicy {
    CompactionPolicy {
        trigger_bytes: NonZeroUsize::new(if automatic { 1 } else { usize::MAX }).unwrap(),
        ..CompactionPolicy::default()
    }
}

/// The size of `fraction` of `conversation`'s model context.
fn target(conversation: &Conversation, fraction: f64) -> usize {
    (conversation.context_bytes() as f64 * fraction) as usize
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

/// A compaction summarizes the context up to the last point within the
/// fraction, and the context goes on with the summary and the messages past
/// it, verbatim; the next compaction summarizes the previous summary too.
#[test]
fn compaction_summarizes_the_older_part_and_keeps_the_rest_across_revisions() {
    let original = history();
    let mut conversation = Conversation::new(original.clone()).unwrap();
    let target = target(&conversation, 0.7);
    let plan = conversation.plan_compaction(0.7).unwrap();
    assert_eq!(plan.context, original[..plan.range.end]);
    assert!(encoded_bytes(&plan.context) <= target);
    assert!(encoded_bytes(&original[..=plan.range.end]) > target);
    assert!(plan.range.end < original.len() - 1);

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
    let mut expected = vec![original[0].clone(), compacted.summary.clone()];
    expected.extend_from_slice(&original[plan.range.end..]);
    expected.push(late);
    assert_eq!(conversation.model_messages(), expected);
    assert_eq!(
        conversation.apply_compaction(compacted.clone()),
        Err(CompactionError::Stale)
    );

    conversation
        .append(vec![
            reply(vec![Content::text("large new answer ".repeat(200))], "stop"),
            Message::user("newest"),
        ])
        .unwrap();
    let next = conversation.plan_compaction(0.7).unwrap();
    assert_eq!(next.previous_revision, 1);
    assert_eq!(next.context[..2], [original[0].clone(), compacted.summary]);
    assert!(next.range.end > plan.range.end);
    let second = crate::compaction::finish_summary(
        &next,
        &reply(vec![Content::text("all earlier work summarized")], "stop"),
    )
    .unwrap();
    conversation.apply_compaction(second.clone()).unwrap();
    assert_eq!(conversation.compactions().len(), 2);
    let mut expected = vec![original[0].clone(), second.summary];
    expected.extend_from_slice(&conversation.messages()[next.range.end..]);
    assert_eq!(conversation.model_messages(), expected);
}

/// When even the first exchange is past the fraction, a compaction covers
/// it, so one large exchange doesn't block compaction.
#[test]
fn compaction_covers_a_first_exchange_past_the_fraction() {
    let original = history();
    let conversation = Conversation::new(original.clone()).unwrap();
    let plan = conversation.plan_compaction(0.01).unwrap();
    assert_eq!(plan.range, 1..2);
    assert_eq!(plan.context, original[..2]);
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
    messages.push(Message::user("after the tool"));
    let call_index = messages.len() - 3;
    let mut conversation = Conversation::new(messages.clone()).unwrap();
    // A fraction ending right after the call: the cut stays before it.
    let fraction =
        (encoded_bytes(&messages[..=call_index]) + 1) as f64 / conversation.context_bytes() as f64;
    let plan = conversation.plan_compaction(fraction).unwrap();
    assert_eq!(plan.range.end, call_index);
    let invalid = Compaction {
        previous_revision: 0,
        range: 1..call_index + 1,
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

/// A summary request is the turn's request with its messages cut short and
/// the instruction after them, so the provider serves them from its cache.
#[tokio::test]
async fn automatic_compaction_keeps_steering_during_summary() {
    let (provider, mut requests) = test_provider();
    let (executor, _, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = seeded(provider, executor, true);
    let initial = session.conversation().messages().to_vec();
    let mut harness = Harness::start(session, handle);
    harness.submit("new request", Delivery::Queue).await;
    let summary = recv(&mut requests).await;
    let summary_request = summary.request.clone();
    // The older part of the context, which begins it.
    let (instruction, context) = summary_request.messages.split_last().unwrap();
    assert!(context.len() > 1 && initial.starts_with(context));
    assert_eq!(instruction.role, Role::User);
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
    // Everything but the messages is the turn's.
    let mut turn = request.request.clone();
    turn.messages = summary_request.messages.clone();
    assert_eq!(summary_request, turn);
    assert!(!summary_request.tools.is_empty());
    // The instructions, the summary, and the rest verbatim.
    let messages = &request.request.messages;
    let [instructions, summary, rest @ .., new, steer] = messages.as_slice() else {
        panic!("unexpected context {messages:?}");
    };
    assert_eq!(instructions, &initial[0]);
    assert!(
        matches!(&summary.content[..], [Content::Text { text }] if text.contains("old work summarized"))
    );
    assert_eq!(rest, &initial[context.len()..]);
    assert_eq!(new.content, [Content::text("new request")]);
    assert_eq!(steer.content, [Content::text("steer during summary")]);
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
