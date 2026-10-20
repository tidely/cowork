use std::{
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_input::{Delivery, Fallback, Input, InputId, Lifetime, Policy, QueueAccess, RunOutcome};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream};
use language_model::{
    Completion, CompletionRequest, CompletionStream, Content, ContentDelta, GenerationMetadata,
    LanguageModel, LanguageModelProvider, Message, ReplayData, Role, StreamEvent, ToolArguments,
    ToolCall, ToolCallId, ToolDefinition,
};
use tokio::sync::{mpsc, oneshot};
use tool_runtime::{
    ToolCallContext, ToolCapabilities, ToolDescriptor, ToolDispatch, ToolError, ToolErrorKind,
    ToolProgress, ToolStream, ToolStreamItem, TypedExtensions, TypedToolOutput, finished_only,
};

use crate::*;

#[path = "tests/agent.rs"]
mod agent;
#[path = "tests/compaction.rs"]
mod compaction;
#[path = "tests/persistence.rs"]
mod persistence;

const WAIT: Duration = Duration::from_secs(5);

struct SamplingRequest {
    request: CompletionRequest,
    response: oneshot::Sender<CompletionStream>,
}

struct Scripted {
    requests: mpsc::UnboundedSender<SamplingRequest>,
}

impl LanguageModelProvider for Scripted {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, language_model::Error>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn stream<'a>(
        &'a self,
        _: &'a LanguageModel,
        request: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, language_model::Error>> {
        Box::pin(async move {
            let (response, reply) = oneshot::channel();
            self.requests
                .send(SamplingRequest { request, response })
                .map_err(|_| language_model::Error::Cancelled)?;
            reply.await.map_err(|_| language_model::Error::Cancelled)
        })
    }
}

fn test_provider() -> (
    Arc<dyn LanguageModelProvider>,
    mpsc::UnboundedReceiver<SamplingRequest>,
) {
    let (requests, receiver) = mpsc::unbounded_channel();
    (Arc::new(Scripted { requests }), receiver)
}

struct Invocation {
    context: ToolCallContext,
    args: serde_json::Value,
    response: oneshot::Sender<ToolStream<TypedToolOutput>>,
}

struct Dispatch {
    calls: Arc<AtomicUsize>,
    invocations: mpsc::UnboundedSender<Invocation>,
}

impl ToolDispatch for Dispatch {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            definition: ToolDefinition {
                name: "work".into(),
                description: None,
                parameters: Default::default(),
            },
            capabilities: ToolCapabilities {
                supports_cancel: true,
                supports_progress: true,
                ..Default::default()
            },
        }]
    }

    fn call<'a>(
        &'a self,
        _: &'a str,
        args: serde_json::Value,
        context: ToolCallContext,
    ) -> BoxFuture<'a, ToolStream<TypedToolOutput>> {
        // Deliberately eager: authorization must happen before even calling this.
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let (response, reply) = oneshot::channel();
            self.invocations
                .send(Invocation {
                    context,
                    args,
                    response,
                })
                .unwrap();
            reply.await.unwrap()
        })
    }
}

fn test_executor(
    policy: Arc<dyn ToolPolicy>,
) -> (
    ToolExecutor,
    mpsc::UnboundedReceiver<Invocation>,
    Arc<AtomicUsize>,
) {
    let (invocations, receiver) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatch = Arc::new(Dispatch {
        calls: calls.clone(),
        invocations,
    });
    (
        ToolExecutor::new(dispatch, policy).unwrap(),
        receiver,
        calls,
    )
}

struct Harness {
    handle: SessionHandle<&'static str>,
    task: tokio::task::JoinHandle<SessionResult<&'static str>>,
    events: mpsc::UnboundedReceiver<AgentEvent<&'static str>>,
    log: Arc<Mutex<Vec<AgentEvent<&'static str>>>>,
}

impl Harness {
    fn start(session: AgentSession<&'static str>, handle: SessionHandle<&'static str>) -> Self {
        let (sender, events) = mpsc::unbounded_channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let records = log.clone();
        let task = tokio::spawn(session.run(move |event| {
            records.lock().unwrap().push(event.clone());
            let _ = sender.send(event);
        }));
        Self {
            handle,
            task,
            events,
            log,
        }
    }

    async fn event(
        &mut self,
        predicate: impl Fn(&EventKind<&'static str>) -> bool,
    ) -> AgentEvent<&'static str> {
        loop {
            let event = recv(&mut self.events).await;
            if predicate(&event.kind) {
                return event;
            }
        }
    }

    async fn submit(&self, text: &str, delivery: Delivery) {
        self.handle
            .submit(input("human", text), delivery)
            .await
            .unwrap();
    }

    async fn finish(self) -> (SessionResult<&'static str>, Vec<AgentEvent<&'static str>>) {
        self.handle.shutdown().unwrap();
        let result = tokio::time::timeout(WAIT, self.task)
            .await
            .unwrap()
            .unwrap();
        let log = self.log.lock().unwrap().clone();
        (result, log)
    }
}

async fn recv<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(WAIT, receiver.recv())
        .await
        .expect("test driver stalled")
        .expect("sender closed")
}

fn input(source: &'static str, text: &str) -> Input<&'static str, Message> {
    Input {
        id: InputId::new(),
        source,
        payload: Message::user(text),
        policy: Policy {
            access: QueueAccess::Editable,
            fallback: Fallback::Queue,
            lifetime: Lifetime::Retain,
        },
    }
}

fn reply(content: Vec<Content>, reason: &str) -> Message {
    let mut message = Message::new(Role::Assistant, content);
    message.generation = Some(GenerationMetadata {
        provider: "scripted".into(),
        model: None,
        finish_reason: Some(reason.into()),
        usage: None,
        extra: Default::default(),
    });
    message
}

fn call() -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        provider_id: Some("native-call".into()),
        name: "work".into(),
        arguments: ToolArguments::from_raw(r#"{"path":"project"}"#),
    }
}

fn respond(request: SamplingRequest, message: Message) {
    assert!(
        request
            .response
            .send(
                stream::iter([Ok(StreamEvent::Finished(Box::new(Completion { message })))]).boxed()
            )
            .is_ok()
    );
}

fn output() -> TypedToolOutput {
    TypedToolOutput::new("work", serde_json::json!({"done":true})).unwrap()
}

fn test_session(
    provider: Arc<dyn LanguageModelProvider>,
    executor: ToolExecutor,
) -> (AgentSession<&'static str>, SessionHandle<&'static str>) {
    AgentSession::new(
        provider,
        LanguageModel::new("scripted"),
        executor,
        CompletionRequest::new(vec![Message::system("system")]),
    )
    .unwrap()
}

#[tokio::test]
async fn tools_steering_and_replay_share_one_ordered_conversation() {
    let (provider, mut requests) = test_provider();
    let (executor, mut invocations, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let initial = session.conversation().messages().to_vec();
    let mut harness = Harness::start(session, handle);
    harness.submit("begin", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    let call = call();
    let mut assistant = reply(vec![Content::ToolCall(call.clone())], "tool_calls");
    assistant.replay.push(ReplayData {
        provider: "scripted".into(),
        model: None,
        format: "signed".into(),
        version: 1,
        payload: serde_json::json!({"signature":"opaque"}),
    });
    respond(request, assistant.clone());
    let invocation = recv(&mut invocations).await;
    assert_eq!(invocation.args["path"], "project");
    assert_eq!(invocation.context.call_id, call.id);
    let mut obsolete = input("timer", "obsolete");
    obsolete.policy.lifetime = Lifetime::ProducerBound;
    harness
        .handle
        .submit(obsolete, Delivery::Steer)
        .await
        .unwrap();
    assert_eq!(
        harness
            .handle
            .producer_closed("timer")
            .await
            .unwrap()
            .inputs
            .len(),
        1
    );
    harness
        .handle
        .submit(input("parent", "steer"), Delivery::Interject)
        .await
        .unwrap();
    let control = invocation.context.extensions.get::<CallControl>().unwrap();
    tokio::time::timeout(WAIT, control.yield_wait.cancelled())
        .await
        .unwrap();
    assert!(!control.cancellation.is_cancelled());
    assert!(
        invocation
            .response
            .send(
                stream::iter([
                    ToolStreamItem::Progress(ToolProgress::Text {
                        text: "working".into()
                    }),
                    ToolStreamItem::Finished(Ok(output())),
                ])
                .boxed()
            )
            .is_ok()
    );
    let request = recv(&mut requests).await;
    assert_eq!(request.request.messages[2], assistant);
    assert!(
        matches!(&request.request.messages[3].content[0], Content::ToolResult(result) if result.call == call.id && !result.is_error)
    );
    assert_eq!(
        request.request.messages[4].content,
        vec![Content::text("steer")]
    );
    respond(request, reply(vec![Content::text("done")], "stop"));
    harness
        .event(|kind| {
            matches!(
                kind,
                EventKind::RunFinished {
                    outcome: RunOutcome::Completed,
                    ..
                }
            )
        })
        .await;
    let (result, events) = harness.finish().await;
    let mut replica = Conversation::new(initial).unwrap();
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, sequence as u64);
        if let EventKind::MessageCommitted { index, message, .. } = &event.kind {
            replica.apply(*index, message.clone()).unwrap();
        }
    }
    assert_eq!(replica.messages(), result.conversation.messages());
    assert!(replica.is_settled());
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolProgress { .. }))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::RunFinished { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn stop_during_startup_drops_local_work_and_preserves_queued_inputs() {
    let (provider, mut requests) = test_provider();
    let (executor, _, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let mut harness = Harness::start(session, handle);
    harness.submit("first", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    let run = harness.handle.snapshot().await.unwrap().run.unwrap();
    harness.submit("later", Delivery::Queue).await;
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
    assert!(request.response.is_closed());
    let snapshot = harness.handle.snapshot().await.unwrap();
    assert!(snapshot.paused);
    assert_eq!(snapshot.pending_inputs, 1);
    assert_eq!(
        harness.handle.stop(run).await,
        Err(ControlError::State(agent_input::StateError::StaleRun))
    );
    let (result, events) = harness.finish().await;
    assert_eq!(result.pending.len(), 1);
    assert_eq!(result.conversation.messages().len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::RunFinished { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn interrupt_settles_running_and_undispatched_calls_before_successor() {
    let (provider, mut requests) = test_provider();
    let (executor, mut invocations, calls) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let mut harness = Harness::start(session, handle);
    harness.submit("first", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    let first = call();
    let mut second = call();
    second.provider_id = Some("second-native-call".into());
    respond(
        request,
        reply(
            vec![
                Content::ToolCall(first.clone()),
                Content::ToolCall(second.clone()),
            ],
            "tool_calls",
        ),
    );
    let invocation = recv(&mut invocations).await;
    let control = invocation.context.extensions.get::<CallControl>().unwrap();
    let old_run = harness.handle.snapshot().await.unwrap().run.unwrap();
    harness.submit("urgent", Delivery::Interrupt).await;
    let request = recv(&mut requests).await;
    assert!(control.cancellation.is_cancelled());
    assert!(invocation.response.is_closed());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for (index, expected_started) in [(3, true), (4, false)] {
        let Content::ToolResult(result) = &request.request.messages[index].content[0] else {
            panic!("unsettled call")
        };
        assert!(result.is_error);
        let language_model::ToolResultContent::Json { value } = &result.content[0] else {
            panic!()
        };
        assert_eq!(value["kind"], "cancelled");
        assert_eq!(
            value["details"]["execution_may_have_started"],
            expected_started
        );
    }
    assert_eq!(
        request.request.messages[5].content,
        vec![Content::text("urgent")]
    );
    assert_eq!(
        harness.handle.stop(old_run).await,
        Err(ControlError::State(agent_input::StateError::StaleRun))
    );
    respond(request, reply(vec![Content::text("done")], "stop"));
    harness
        .event(|kind| {
            matches!(
                kind,
                EventKind::RunFinished {
                    outcome: RunOutcome::Completed,
                    ..
                }
            )
        })
        .await;
    let (result, events) = harness.finish().await;
    assert!(result.conversation.is_settled());
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::ToolFinished { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::RunFinished { .. }))
            .count(),
        2
    );
}

struct Approval {
    requests: mpsc::UnboundedSender<oneshot::Sender<Result<TypedExtensions, ToolError>>>,
}

impl ToolPolicy for Approval {
    fn authorize(&self, _: &ToolRequest) -> Authorization {
        let (sender, receiver) = oneshot::channel();
        self.requests.send(sender).unwrap();
        Authorization::Pending(Box::pin(async move {
            receiver.await.unwrap_or_else(|_| {
                Err(ToolError::new(
                    ToolErrorKind::Cancelled,
                    "approval owner closed",
                ))
            })
        }))
    }
}

#[derive(Debug)]
struct Grant(&'static str);

#[tokio::test]
async fn authorization_precedes_eager_dispatch_and_carries_call_scoped_grants() {
    let (approvals, mut decisions) = mpsc::unbounded_channel();
    let (provider, mut requests) = test_provider();
    let (executor, mut invocations, calls) = test_executor(Arc::new(Approval {
        requests: approvals,
    }));
    let (session, handle) = test_session(provider, executor);
    let mut harness = Harness::start(session, handle);
    harness.submit("work", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    respond(
        request,
        reply(
            vec![Content::ToolCall(call()), Content::ToolCall(call())],
            "tool_calls",
        ),
    );
    let approve = recv(&mut decisions).await;
    harness
        .event(|kind| matches!(kind, EventKind::PhaseChanged(Phase::AwaitingApproval)))
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut extensions = TypedExtensions::default();
    extensions.insert(Grant("exact folders"));
    assert!(approve.send(Ok(extensions)).is_ok());
    let invocation = recv(&mut invocations).await;
    assert_eq!(
        invocation.context.extensions.get::<Grant>().unwrap().0,
        "exact folders"
    );
    assert!(
        invocation
            .response
            .send(finished_only(Ok(output())))
            .is_ok()
    );
    let deny = recv(&mut decisions).await;
    assert!(
        deny.send(Err(ToolError::new(
            ToolErrorKind::PermissionDenied,
            "denied"
        )))
        .is_ok()
    );
    let request = recv(&mut requests).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let Content::ToolResult(result) = &request.request.messages[4].content[0] else {
        panic!()
    };
    assert!(result.is_error);
    respond(request, reply(vec![Content::text("done")], "stop"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    harness.finish().await;
}

#[tokio::test]
async fn stop_while_approval_pending_never_dispatches_even_if_approval_arrives_late() {
    let (approvals, mut decisions) = mpsc::unbounded_channel();
    let (provider, mut requests) = test_provider();
    let (executor, _, calls) = test_executor(Arc::new(Approval {
        requests: approvals,
    }));
    let (session, handle) = test_session(provider, executor);
    let mut harness = Harness::start(session, handle);
    harness.submit("work", Delivery::Queue).await;
    respond(
        recv(&mut requests).await,
        reply(vec![Content::ToolCall(call())], "tool_calls"),
    );
    let approve = recv(&mut decisions).await;
    let run = harness.handle.snapshot().await.unwrap().run.unwrap();
    harness.handle.stop(run).await.unwrap();
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    assert!(approve.send(Ok(TypedExtensions::default())).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (result, _) = harness.finish().await;
    assert!(result.conversation.is_settled());
}

#[tokio::test]
async fn provisional_or_unusable_model_responses_never_dispatch_tools() {
    for scenario in [
        "missing final",
        "trailing event",
        "trailing error",
        "truncated",
        "invalid arguments",
        "duplicate calls",
    ] {
        let (provider, mut requests) = test_provider();
        let (executor, _, calls) = test_executor(Arc::new(AllowAll));
        let (session, handle) = test_session(provider, executor);
        let mut harness = Harness::start(session, handle);
        harness.submit("work", Delivery::Queue).await;
        let request = recv(&mut requests).await;
        let mut call = call();
        if scenario == "invalid arguments" {
            call.arguments = ToolArguments::Invalid { raw: "{bad".into() };
        }
        let content = if scenario == "duplicate calls" {
            vec![Content::ToolCall(call.clone()), Content::ToolCall(call)]
        } else {
            vec![Content::ToolCall(call)]
        };
        let mut message = reply(
            content,
            if scenario == "truncated" {
                "length"
            } else {
                "tool_calls"
            },
        );
        message.generation.as_mut().unwrap().usage = Some(language_model::Usage {
            input_tokens: Some(1),
            ..Default::default()
        });
        let delta = || {
            Ok(StreamEvent::Delta {
                index: 0,
                delta: ContentDelta::Text("preview".into()),
            })
        };
        let mut events = vec![delta()];
        if scenario != "missing final" {
            events.push(Ok(StreamEvent::Finished(Box::new(Completion { message }))));
        }
        if scenario == "trailing event" {
            events.push(delta());
        }
        if scenario == "trailing error" {
            events.push(Err(language_model::Error::Provider("late failure".into())));
        }
        assert!(request.response.send(stream::iter(events).boxed()).is_ok());
        harness
            .event(|kind| {
                matches!(
                    kind,
                    EventKind::RunFinished {
                        outcome: RunOutcome::Failed,
                        error: Some(_)
                    }
                )
            })
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{scenario}");
        let (result, events) = harness.finish().await;
        assert_eq!(result.conversation.messages().len(), 2, "{scenario}");
        let billable = !matches!(
            scenario,
            "missing final" | "trailing event" | "trailing error"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::UsageReported(_)))
                .count(),
            usize::from(billable),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn invalid_tool_stream_is_settled_as_error_without_committing_its_provisional_result() {
    let (provider, mut requests) = test_provider();
    let (executor, mut invocations, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let mut harness = Harness::start(session, handle);
    harness.submit("work", Delivery::Queue).await;
    respond(
        recv(&mut requests).await,
        reply(vec![Content::ToolCall(call())], "tool_calls"),
    );
    let invocation = recv(&mut invocations).await;
    assert!(
        invocation
            .response
            .send(
                stream::iter([
                    ToolStreamItem::Finished(Ok(output())),
                    ToolStreamItem::Progress(ToolProgress::Text {
                        text: "invalid late progress".into()
                    }),
                ])
                .boxed()
            )
            .is_ok()
    );
    let request = recv(&mut requests).await;
    let Content::ToolResult(result) = &request.request.messages[3].content[0] else {
        panic!()
    };
    assert!(result.is_error);
    assert!(
        matches!(&result.content[0], language_model::ToolResultContent::Json { value } if value["kind"] == "invalid_stream")
    );
    respond(request, reply(vec![Content::text("done")], "stop"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    harness.finish().await;
}

#[tokio::test]
async fn queue_drains_after_handles_close_and_turn_limit_pauses_remaining_work() {
    let (provider, mut requests) = test_provider();
    let (executor, _, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let task = tokio::spawn(session.run(|_| {}));
    handle
        .submit(input("human", "one"), Delivery::Queue)
        .await
        .unwrap();
    let first = recv(&mut requests).await;
    handle
        .submit(input("human", "two"), Delivery::Queue)
        .await
        .unwrap();
    drop(handle);
    respond(first, reply(vec![Content::text("one")], "stop"));
    respond(
        recv(&mut requests).await,
        reply(vec![Content::text("two")], "stop"),
    );
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(result.pending.is_empty());
    assert_eq!(result.conversation.messages().len(), 5);

    let (provider, mut requests) = test_provider();
    let (executor, mut invocations, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let mut harness = Harness::start(
        session.with_max_model_turns(NonZeroUsize::new(1).unwrap()),
        handle,
    );
    harness.submit("one", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    harness.submit("two", Delivery::Queue).await;
    respond(
        request,
        reply(vec![Content::ToolCall(call())], "tool_calls"),
    );
    let invocation = recv(&mut invocations).await;
    assert!(
        invocation
            .response
            .send(finished_only(Ok(output())))
            .is_ok()
    );
    harness
        .event(|kind| {
            matches!(
                kind,
                EventKind::RunFinished {
                    outcome: RunOutcome::Failed,
                    error: Some(RunError::TurnLimit)
                }
            )
        })
        .await;
    assert!(harness.handle.snapshot().await.unwrap().paused);
    let (result, _) = harness.finish().await;
    assert_eq!(result.pending.len(), 1);
    assert!(result.conversation.is_settled());
}

#[tokio::test]
async fn final_message_callback_can_admit_steering_before_terminal_commit() {
    let (provider, mut requests) = test_provider();
    let (executor, _, _) = test_executor(Arc::new(AllowAll));
    let (session, handle) = test_session(provider, executor);
    let commands = handle.clone();
    let mut injected = false;
    let task = tokio::spawn(session.run(move |event| {
        if let EventKind::MessageCommitted { message, .. } = &event.kind
            && message.role == Role::Assistant
            && !injected
        {
            injected = true;
            // Sending happens on the first poll. Dropping the receipt future
            // does not withdraw accepted work.
            let _ = commands
                .submit(input("parent", "one more"), Delivery::Steer)
                .now_or_never();
        }
        if matches!(event.kind, EventKind::RunFinished { .. }) {
            commands.shutdown().unwrap();
        }
    }));
    handle
        .submit(input("human", "begin"), Delivery::Queue)
        .await
        .unwrap();
    respond(
        recv(&mut requests).await,
        reply(vec![Content::text("first answer")], "stop"),
    );
    let next = recv(&mut requests).await;
    assert_eq!(
        next.request.messages.last().unwrap().content,
        vec![Content::text("one more")]
    );
    respond(next, reply(vec![Content::text("second answer")], "stop"));
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(result.conversation.messages().len(), 5);
}

#[test]
fn conversation_batches_are_atomic_and_replay_rejects_gaps_and_duplicates() {
    let initial = Message::user("initial");
    let mut conversation = Conversation::new(vec![initial.clone()]).unwrap();
    let call = call();
    let assistant = reply(vec![Content::ToolCall(call.clone())], "tool_calls");
    assert_eq!(
        conversation.append(vec![assistant.clone(), Message::user("too soon")]),
        Err(ConversationError::UnsettledCalls)
    );
    assert_eq!(conversation.messages(), [initial]);
    conversation.append(vec![assistant]).unwrap();
    let wrong = Message::new(
        Role::Tool,
        vec![Content::ToolResult(language_model::ToolResult {
            call: call.id,
            provider_id: Some("wrong".into()),
            name: Some(call.name),
            content: vec![language_model::ToolResultContent::Text {
                text: "done".into(),
            }],
            is_error: false,
        })],
    );
    assert_eq!(
        conversation.append(vec![wrong]),
        Err(ConversationError::UncorrelatedResult)
    );
    assert!(!conversation.is_settled());
    assert_eq!(
        conversation.apply(9, Message::user("gap")),
        Err(ConversationError::OutOfOrder)
    );
    assert_eq!(
        conversation.apply(0, Message::user("duplicate")),
        Err(ConversationError::OutOfOrder)
    );
}

#[tokio::test]
async fn executor_serializes_shared_sessions_until_full_stream_settlement() {
    let (executor, mut invocations, calls) = test_executor(Arc::new(AllowAll));
    let mut first = executor.execute(agent_input::RunId::new(), call(), CallControl::default());
    assert!(matches!(
        first.next().await,
        Some(executor::ExecutionEvent::WaitingForCapacity)
    ));
    assert!(matches!(
        first.next().await,
        Some(executor::ExecutionEvent::Started)
    ));
    let mut first_poll = Box::pin(first.next());
    assert!(first_poll.as_mut().now_or_never().is_none());
    let invocation = recv(&mut invocations).await;
    let mut second = executor.execute(agent_input::RunId::new(), call(), CallControl::default());
    assert!(matches!(
        second.next().await,
        Some(executor::ExecutionEvent::WaitingForCapacity)
    ));
    assert!(second.next().now_or_never().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        invocation
            .response
            .send(finished_only(Ok(output())))
            .is_ok()
    );
    assert!(matches!(
        first_poll.await,
        Some(executor::ExecutionEvent::Finished(Ok(_)))
    ));
    assert!(matches!(
        second.next().await,
        Some(executor::ExecutionEvent::Started)
    ));
}
