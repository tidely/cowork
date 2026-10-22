use super::*;
use crate::compaction::{encoded_bytes, finish_summary};

fn entry(message: Message) -> Entry<String> {
    Entry {
        message,
        origin: Origin::Tool,
    }
}

/// Three large old exchanges and a small recent one.
pub(super) fn history() -> Vec<Message> {
    let mut messages = Vec::new();
    for turn in 0..3 {
        messages.push(Message::user(format!("old request {turn} ").repeat(60)));
        let mut answer = text(&format!("old answer {turn} ").repeat(60));
        if turn == 0 {
            answer.replay.push(ReplayData {
                provider: "scripted".into(),
                model: None,
                format: "signed".into(),
                version: 1,
                payload: json!({"signature":"unaltered"}),
            });
        }
        messages.push(answer);
    }
    messages.push(Message::user("recent request"));
    messages.push(text("recent answer"));
    messages
}

pub(super) fn conversation(messages: &[Message]) -> Conversation<String> {
    let mut conversation = Conversation::new();
    conversation
        .append(messages.iter().cloned().map(entry).collect())
        .unwrap();
    conversation
}

fn messages(conversation: &Conversation<String>) -> Vec<Message> {
    conversation
        .entries()
        .iter()
        .map(|entry| entry.message.clone())
        .collect()
}

fn summary(reason: FinishReason, text_: &str) -> Message {
    reply(vec![Content::text(text_)], reason)
}

/// A session continuing `history`, compacting automatically or on request.
pub(super) fn seeded(automatic: bool) -> Fixture {
    let mut fixture = fixture(Mode::Allow);
    fixture.session = fixture
        .session
        .with_conversation(conversation(&history()), 0)
        .unwrap();
    if automatic {
        fixture.session.configuration_mut().compaction.trigger_bytes = NonZeroUsize::new(1);
    }
    fixture
}

fn target(conversation: &Conversation<String>, fraction: f64) -> usize {
    (conversation.context_bytes() as f64 * fraction) as usize
}

/// A compaction summarizes the context up to the last point within the
/// fraction, and the context goes on with the summary and the entries past
/// it, verbatim; the next compaction summarizes the previous summary too.
#[test]
fn compaction_summarizes_the_older_part_and_keeps_the_rest_across_revisions() {
    let original = history();
    let mut conversation = conversation(&original);
    let target = target(&conversation, 0.7);
    let plan = conversation.plan_compaction(0.7).unwrap();
    assert_eq!(plan.context, original[..plan.end]);
    assert!(encoded_bytes(&plan.context) <= target);
    assert!(encoded_bytes(&original[..=plan.end]) > target);
    assert!(plan.end < original.len() - 1);

    let late = Message::user("arrived during summary");
    conversation.append(vec![entry(late.clone())]).unwrap();
    let compacted =
        finish_summary(&plan, &summary(FinishReason::Stop, "older work summarized")).unwrap();
    conversation.apply_compaction(compacted.clone()).unwrap();
    assert_eq!(messages(&conversation)[..original.len()], original);
    let mut expected = vec![compacted.summary.clone()];
    expected.extend_from_slice(&original[plan.end..]);
    expected.push(late);
    assert_eq!(conversation.model_messages(), expected);
    assert_eq!(
        conversation.apply_compaction(compacted.clone()),
        Err(CompactionError::Stale)
    );

    conversation
        .append(vec![
            entry(text(&"large new answer ".repeat(200))),
            entry(Message::user("newest")),
        ])
        .unwrap();
    let next = conversation.plan_compaction(0.7).unwrap();
    assert_eq!(next.previous_revision, 1);
    assert_eq!(next.context[0], compacted.summary);
    assert!(next.end > plan.end);
    let second = finish_summary(
        &next,
        &summary(FinishReason::Stop, "all earlier work summarized"),
    )
    .unwrap();
    conversation.apply_compaction(second.clone()).unwrap();
    assert_eq!(conversation.compactions().len(), 2);
    let mut expected = vec![second.summary];
    expected.extend_from_slice(&messages(&conversation)[next.end..]);
    assert_eq!(conversation.model_messages(), expected);
}

/// When even the first exchange is past the fraction, a compaction covers
/// it, so one large exchange doesn't block compaction.
#[test]
fn compaction_covers_a_first_exchange_past_the_fraction() {
    let original = history();
    let plan = conversation(&original).plan_compaction(0.01).unwrap();
    assert_eq!(plan.end, 1);
    assert_eq!(plan.context, original[..1]);
}

#[test]
fn compaction_never_splits_tool_batches_and_rejects_non_reducing_or_stale_replacements() {
    let mut original = history();
    let call = call();
    original.push(calls(vec![call.clone()]));
    original.push(Message::new(
        Role::Tool,
        vec![Content::ToolResult(language_model::ToolResult {
            call: call.id,
            provider_id: call.provider_id,
            name: Some(call.name),
            content: vec![ToolResultContent::Text {
                text: "actual output".into(),
            }],
            is_error: false,
        })],
    ));
    original.push(Message::user("after the tool"));
    let call_index = original.len() - 3;
    let mut conversation = conversation(&original);
    // A fraction ending right after the call: the cut stays before it.
    let fraction =
        (encoded_bytes(&original[..=call_index]) + 1) as f64 / conversation.context_bytes() as f64;
    let plan = conversation.plan_compaction(fraction).unwrap();
    assert_eq!(plan.end, call_index);
    let unsettled = Compaction {
        previous_revision: 0,
        end: call_index + 1,
        summary: Message::user("small"),
    };
    assert!(matches!(
        conversation.apply_compaction(unsettled),
        Err(CompactionError::Conversation(
            ConversationError::UnsettledCalls
        ))
    ));
    let large = Compaction {
        previous_revision: 0,
        end: plan.end,
        summary: Message::user("too large ".repeat(5000)),
    };
    assert_eq!(
        conversation.apply_compaction(large),
        Err(CompactionError::NotSmaller)
    );
    assert_eq!(messages(&conversation), original);
    assert!(conversation.compactions().is_empty());
}

/// What the model was told is forgotten when a compaction summarizes the
/// entry that told it, so the section renders again after the summary.
#[test]
fn compaction_forgets_context_delivered_before_its_cut() {
    let mut conversation = conversation(&history());
    conversation.deliver("early".into(), json!("old"), 0);
    let late = conversation.entries().len() - 1;
    conversation.deliver("late".into(), json!("recent"), late);
    let plan = conversation.plan_compaction(0.7).unwrap();
    assert!(plan.end > 0 && plan.end <= late);
    let compaction = finish_summary(&plan, &summary(FinishReason::Stop, "summary")).unwrap();
    conversation.apply_compaction(compaction).unwrap();
    assert_eq!(conversation.delivered("early"), None);
    assert_eq!(conversation.delivered("late"), Some(&json!("recent")));
}

/// A summary request is the step's request with its messages cut short and
/// the instruction after them, so the provider serves them from its cache.
#[tokio::test]
async fn automatic_compaction_keeps_steering_during_summary() {
    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = seeded(true);
    let initial = messages(session.conversation());
    let mut harness = Harness::start(session, handle);
    harness.submit("new request", Delivery::Queue).await;
    let summary_request = recv(&mut requests).await;
    let sent = summary_request.request.clone();
    let (instruction, context) = sent.messages.split_last().unwrap();
    assert!(context.len() > 1 && initial.starts_with(context));
    assert_eq!(instruction.role, Role::User);
    assert_eq!(
        harness.handle.snapshot().await.unwrap().phase,
        Phase::Compacting
    );
    harness
        .handle
        .send(Request::new(
            "parent".into(),
            input("steer during summary", Delivery::Steer),
        ))
        .await
        .unwrap();
    respond(
        summary_request,
        summary(FinishReason::Stop, "old work summarized"),
    );
    let request = recv(&mut requests).await;
    // Everything but the messages and their cache point is the step's.
    let mut step = request.request.clone();
    step.messages = sent.messages.clone();
    step.cache = sent.cache.clone();
    assert_eq!(sent, step);
    assert!(!sent.tools.is_empty());
    // The summary, the rest verbatim, then the new input with its context.
    let messages = &request.request.messages;
    let [summary, rest @ .., _context, new, steer] = messages.as_slice() else {
        panic!("unexpected context {messages:?}");
    };
    assert!(
        matches!(&summary.content[..], [Content::Text { text }] if text.contains("old work summarized"))
    );
    assert_eq!(rest, &initial[context.len()..]);
    assert_eq!(new.content, [Content::text("new request")]);
    assert_eq!(steer.content, [Content::text("steer during summary")]);
    respond(request, text("done"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    let (result, events) = harness.finish().await;
    assert_eq!(messages_of(&result)[..initial.len()], initial);
    let mut replica = conversation(&initial);
    for event in events {
        match event.kind {
            EventKind::MessageCommitted { index, entry } => replica.apply(index, entry).unwrap(),
            EventKind::Compacted(compaction) => replica.apply_compaction(compaction).unwrap(),
            _ => {}
        }
    }
    assert_eq!(replica.entries(), result.conversation.entries());
    assert_eq!(
        replica.model_messages(),
        result.conversation.model_messages()
    );
}

fn messages_of(result: &SessionResult<Host>) -> Vec<Message> {
    messages(&result.conversation)
}

#[tokio::test]
async fn manual_compaction_failures_preserve_context_and_do_not_fail_the_session() {
    for reason in ["length", "empty", "larger", "transport"] {
        let Fixture {
            session,
            handle,
            mut requests,
            ..
        } = seeded(false);
        let initial = session.conversation().model_messages();
        let harness = Harness::start(session, handle);
        let commands = harness.handle.clone();
        let compact = tokio::spawn(async move {
            commands
                .send(Request::new("human".into(), RequestKind::Compact))
                .await
        });
        let request = recv(&mut requests).await;
        if reason == "transport" {
            assert!(
                request
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
            let finish = if reason == "length" {
                FinishReason::Length
            } else {
                FinishReason::Stop
            };
            respond(request, summary(finish, &text));
        }
        assert!(matches!(
            tokio::time::timeout(WAIT, compact).await.unwrap().unwrap(),
            Err(RequestError::Compaction(_))
        ));
        assert_eq!(harness.handle.snapshot().await.unwrap().phase, Phase::Idle);
        let (result, _) = harness.finish().await;
        assert_eq!(result.conversation.model_messages(), initial);
        assert!(result.conversation.compactions().is_empty());
    }
}

#[tokio::test]
async fn stop_during_summary_cancels_it_without_a_context_transition() {
    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = seeded(true);
    let mut harness = Harness::start(session, handle);
    harness.submit("new request", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    let run = harness.run().await;
    harness.request(RequestKind::Stop { run }).await.unwrap();
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
    assert!(request.response.is_closed());
    let (result, events) = harness.finish().await;
    assert!(result.conversation.compactions().is_empty());
    assert_eq!(
        count(&events, |kind| matches!(kind, EventKind::Compacted(_))),
        0
    );
}
