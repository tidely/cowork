use std::{
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{FutureExt, StreamExt, future::BoxFuture, stream};
use language_model::{
    Completion, CompletionRequest, CompletionStream, Content, ContentDelta, FinishReason,
    GenerationMetadata, LanguageModel, LanguageModelProvider, Message, ReplayData, Role,
    StreamEvent, ToolArguments, ToolCall, ToolCallId, ToolResultContent,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tool_runtime::{
    Provides, Tool, ToolCapabilities, ToolProgress, ToolStream, ToolStreamItem, finished_only,
};

use crate::*;

#[path = "tests/agent.rs"]
mod agent;
#[path = "tests/compaction.rs"]
mod compaction;
#[path = "tests/persistence.rs"]
mod persistence;
#[path = "tests/state.rs"]
mod state;

const WAIT: Duration = Duration::from_secs(5);

pub(crate) struct SamplingRequest {
    pub model: LanguageModel,
    pub request: CompletionRequest,
    pub response: oneshot::Sender<CompletionStream>,
}

/// A provider whose every request waits for the test to answer it.
struct Scripted {
    requests: mpsc::UnboundedSender<SamplingRequest>,
}

impl LanguageModelProvider for Scripted {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, language_model::Error>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn stream<'a>(
        &'a self,
        model: &'a LanguageModel,
        request: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, language_model::Error>> {
        Box::pin(async move {
            let (response, reply) = oneshot::channel();
            self.requests
                .send(SamplingRequest {
                    model: model.clone(),
                    request,
                    response,
                })
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

/// The test host's access: the path a call works on.
#[derive(Clone, Debug, Default, PartialEq)]
struct Access(String);

impl From<()> for Access {
    fn from(_: ()) -> Self {
        Self("nothing".into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Allow,
    Ask,
    Deny,
}

struct Probe {
    calls: AtomicUsize,
    invocations: mpsc::UnboundedSender<Invocation>,
}

#[derive(Clone)]
struct Host {
    probe: Arc<Probe>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Host")
    }
}

impl Provides<Probe> for Host {
    fn provide(self: &Arc<Self>) -> Arc<Probe> {
        self.probe.clone()
    }
}

impl AgentHost for Host {
    type Source = String;
    type Input = String;
    type Access = Access;
    type Permissions = Mode;

    fn admit(&self, request: &Request<Self>) -> Result<(), String> {
        if request.source == "intruder" {
            Err("not a participant".into())
        } else {
            Ok(())
        }
    }

    fn authorize(&self, call: &CallAuthorization<'_, Self>) -> Decision {
        match call.permissions {
            Mode::Allow => Decision::Allow,
            Mode::Deny => Decision::Deny("denied by mode".into()),
            Mode::Ask
                if call.grants.iter().any(|grant| {
                    grant.tool == call.tool.name() && &grant.access == call.access
                }) =>
            {
                Decision::Allow
            }
            Mode::Ask => Decision::Ask,
        }
    }

    fn describe_permissions(
        &self,
        mode: &Mode,
        _: &[tool_runtime::ToolDescriptor],
    ) -> Option<String> {
        Some(format!("Mode: {mode:?}"))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkArgs {
    path: String,
}

struct Invocation {
    call: tool_runtime::ToolCall<Access>,
    args: WorkArgs,
    response: oneshot::Sender<ToolStream<Value>>,
}

/// Hands each invocation to the test, which answers with a stream.
struct Work;

impl Tool for Work {
    type Args = WorkArgs;
    type Output = Value;
    type Context = Probe;
    type Access = Access;

    fn name(&self) -> &str {
        "work"
    }
    fn description(&self) -> String {
        "Work on a path".into()
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            supports_cancel: true,
            supports_progress: true,
            ..Default::default()
        }
    }
    fn access(&self, args: &WorkArgs, _: &Probe) -> Access {
        Access(args.path.clone())
    }
    async fn execute(
        &self,
        call: tool_runtime::ToolCall<Access>,
        probe: Arc<Probe>,
        args: WorkArgs,
    ) -> ToolStream<Value> {
        probe.calls.fetch_add(1, Ordering::SeqCst);
        let (response, reply) = oneshot::channel();
        probe
            .invocations
            .send(Invocation {
                call,
                args,
                response,
            })
            .unwrap();
        reply.await.unwrap()
    }
}

fn definition() -> AgentDefinition {
    AgentDefinition::new(LanguageModel::new("scripted"))
        .instructions("system")
        .tool("work")
}

struct Fixture {
    session: AgentSession<Host>,
    handle: SessionHandle<Host>,
    requests: mpsc::UnboundedReceiver<SamplingRequest>,
    invocations: mpsc::UnboundedReceiver<Invocation>,
    probe: Arc<Probe>,
}

fn fixture(mode: Mode) -> Fixture {
    fixture_with(mode, definition())
}

fn fixture_with(mode: Mode, definition: AgentDefinition) -> Fixture {
    let (provider, requests) = test_provider();
    let (invocations, invocation_receiver) = mpsc::unbounded_channel();
    let probe = Arc::new(Probe {
        calls: AtomicUsize::new(0),
        invocations,
    });
    let mut registry = tools::HostRegistry::<Host>::default();
    registry.register(Work).unwrap();
    let configuration = Configuration {
        permissions: mode,
        sampling: Sampling::default(),
        max_steps: NonZeroUsize::new(64).unwrap(),
        compaction: CompactionPolicy::default(),
    };
    let (session, handle) = AgentSession::new(
        Arc::new(Host {
            probe: probe.clone(),
        }),
        provider,
        definition,
        configuration,
        registry,
    )
    .unwrap();
    Fixture {
        session,
        handle,
        requests,
        invocations: invocation_receiver,
        probe,
    }
}

struct Harness {
    handle: SessionHandle<Host>,
    task: tokio::task::JoinHandle<SessionResult<Host>>,
    events: mpsc::UnboundedReceiver<AgentEvent<Host>>,
    log: Arc<Mutex<Vec<AgentEvent<Host>>>>,
}

impl Harness {
    fn start(session: AgentSession<Host>, handle: SessionHandle<Host>) -> Self {
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

    async fn event(&mut self, predicate: impl Fn(&EventKind<Host>) -> bool) -> AgentEvent<Host> {
        loop {
            let event = recv(&mut self.events).await;
            if predicate(&event.kind) {
                return event;
            }
        }
    }

    async fn request(&self, kind: RequestKind<Host>) -> Result<Response<Host>, RequestError> {
        self.handle.send(Request::new("human".into(), kind)).await
    }

    async fn submit(&self, text: &str, delivery: Delivery) {
        self.request(input(text, delivery)).await.unwrap();
    }

    async fn run(&self) -> RunId {
        self.handle.snapshot().await.unwrap().run.unwrap()
    }

    async fn finish(self) -> (SessionResult<Host>, Vec<AgentEvent<Host>>) {
        let _ = self.request(RequestKind::Shutdown).await;
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

fn policy() -> Policy {
    Policy {
        access: QueueAccess::Editable,
        fallback: Fallback::Queue,
        lifetime: Lifetime::Retain,
    }
}

fn input(text: &str, delivery: Delivery) -> RequestKind<Host> {
    RequestKind::Input {
        id: InputId::new(),
        payload: text.into(),
        delivery,
        policy: policy(),
    }
}

fn reply(content: Vec<Content>, reason: FinishReason) -> Message {
    let mut message = Message::new(Role::Assistant, content);
    message.generation = Some(GenerationMetadata {
        provider: "scripted".into(),
        model: None,
        finish_reason: Some(reason),
        usage: None,
        extra: Default::default(),
    });
    message
}

fn text(text: &str) -> Message {
    reply(vec![Content::text(text)], FinishReason::Stop)
}

fn tool_call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        provider_id: Some(format!("native-{}", ToolCallId::new().0)),
        name: name.into(),
        arguments: ToolArguments::from_raw(arguments.to_string()),
    }
}

fn call() -> ToolCall {
    tool_call("work", json!({"path": "project"}))
}

fn calls(calls: Vec<ToolCall>) -> Message {
    reply(
        calls.into_iter().map(Content::ToolCall).collect(),
        FinishReason::ToolCalls,
    )
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

fn done() -> ToolStream<Value> {
    finished_only(Ok(json!({"done": true})))
}

fn result_of(message: &Message) -> &language_model::ToolResult {
    let Content::ToolResult(result) = &message.content[0] else {
        panic!("not a tool result: {message:?}")
    };
    result
}

fn error_kind(message: &Message) -> String {
    let result = result_of(message);
    assert!(result.is_error);
    let ToolResultContent::Json { value } = &result.content[0] else {
        panic!("tool errors are JSON")
    };
    value["kind"].as_str().unwrap().to_owned()
}

fn count(events: &[AgentEvent<Host>], predicate: impl Fn(&EventKind<Host>) -> bool) -> usize {
    events.iter().filter(|event| predicate(&event.kind)).count()
}

fn replay(events: &[AgentEvent<Host>]) -> Conversation<String> {
    let mut replica = Conversation::new();
    for event in events {
        match &event.kind {
            EventKind::MessageCommitted { index, entry } => {
                replica.apply(*index, entry.clone()).unwrap()
            }
            EventKind::Compacted(compaction) => {
                replica.apply_compaction(compaction.clone()).unwrap()
            }
            _ => {}
        }
    }
    replica
}

#[tokio::test]
async fn tools_steering_and_replay_share_one_ordered_conversation() {
    let Fixture {
        session,
        handle,
        mut requests,
        mut invocations,
        ..
    } = fixture(Mode::Allow);
    let mut harness = Harness::start(session, handle);
    harness.submit("begin", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    let call = call();
    let mut assistant = calls(vec![call.clone()]);
    assistant.replay.push(ReplayData {
        provider: "scripted".into(),
        model: None,
        format: "signed".into(),
        version: 1,
        payload: json!({"signature":"opaque"}),
    });
    respond(request, assistant.clone());
    let invocation = recv(&mut invocations).await;
    assert_eq!(invocation.args.path, "project");
    assert_eq!(invocation.call.id, call.id);
    // The tool receives exactly the access it computed and authorization allowed.
    assert_eq!(invocation.call.access, Access("project".into()));
    let obsolete = RequestKind::Input {
        id: InputId::new(),
        payload: "obsolete".into(),
        delivery: Delivery::Steer,
        policy: Policy {
            lifetime: Lifetime::ProducerBound,
            ..policy()
        },
    };
    harness
        .handle
        .send(Request::new("timer".into(), obsolete))
        .await
        .unwrap();
    let withdrawn = harness
        .handle
        .send(Request::new("timer".into(), RequestKind::SourceClosed))
        .await
        .unwrap();
    assert!(matches!(withdrawn, Response::Withdrawn(withdrawn) if withdrawn.inputs.len() == 1));
    harness
        .handle
        .send(Request::new(
            "parent".into(),
            input("steer", Delivery::Interject),
        ))
        .await
        .unwrap();
    let control = invocation.call.control.clone();
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
                    ToolStreamItem::Finished(Ok(json!({"done": true}))),
                ])
                .boxed()
            )
            .is_ok()
    );
    let request = recv(&mut requests).await;
    let messages = &request.request.messages;
    assert_eq!(messages[2], assistant);
    assert!(result_of(&messages[3]).call == call.id && !result_of(&messages[3]).is_error);
    assert_eq!(messages[4].content, vec![Content::text("steer")]);
    respond(request, text("done"));
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
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, sequence as u64);
    }
    // A replica folding the events matches, provenance included.
    let replica = replay(&events);
    assert_eq!(replica.entries(), result.conversation.entries());
    let origins: Vec<_> = replica
        .entries()
        .iter()
        .map(|entry| match &entry.origin {
            Origin::Context { .. } => "context",
            Origin::Input { source, .. } => source.as_str(),
            Origin::Model { .. } => "model",
            Origin::Tool => "tool",
        })
        .collect();
    assert_eq!(
        origins,
        ["context", "human", "model", "tool", "parent", "model"]
    );
    assert_eq!(
        count(&events, |kind| matches!(
            kind,
            EventKind::ToolProgress { .. }
        )),
        1
    );
    assert_eq!(
        count(&events, |kind| matches!(
            kind,
            EventKind::RunFinished { .. }
        )),
        1
    );
}

#[tokio::test]
async fn host_admission_refuses_requests_before_the_session_acts_on_them() {
    let Fixture {
        session, handle, ..
    } = fixture(Mode::Allow);
    let harness = Harness::start(session, handle);
    let refused = harness
        .handle
        .send(Request::new(
            "intruder".into(),
            input("take over", Delivery::Interrupt),
        ))
        .await;
    assert_eq!(
        refused.unwrap_err(),
        RequestError::Rejected("not a participant".into())
    );
    assert_eq!(harness.handle.snapshot().await.unwrap().pending_inputs, 0);
    harness.finish().await;
}

#[tokio::test]
async fn stop_during_startup_drops_local_work_and_preserves_queued_inputs() {
    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = fixture(Mode::Allow);
    let mut harness = Harness::start(session, handle);
    harness.submit("first", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    let run = harness.run().await;
    harness.submit("later", Delivery::Queue).await;
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
    let snapshot = harness.handle.snapshot().await.unwrap();
    assert!(snapshot.paused);
    assert_eq!(snapshot.pending_inputs, 1);
    assert_eq!(
        harness
            .request(RequestKind::Stop { run })
            .await
            .unwrap_err(),
        RequestError::State(agent_input::StateError::StaleRun)
    );
    let (result, events) = harness.finish().await;
    assert_eq!(result.pending.len(), 1);
    // The context note and the first input.
    assert_eq!(result.conversation.entries().len(), 2);
    assert_eq!(
        count(&events, |kind| matches!(
            kind,
            EventKind::RunFinished { .. }
        )),
        1
    );
}

#[tokio::test]
async fn interrupt_settles_running_and_undispatched_calls_before_successor() {
    let Fixture {
        session,
        handle,
        mut requests,
        mut invocations,
        probe,
    } = fixture(Mode::Allow);
    let mut harness = Harness::start(session, handle);
    harness.submit("first", Delivery::Queue).await;
    respond(recv(&mut requests).await, calls(vec![call(), call()]));
    let invocation = recv(&mut invocations).await;
    let control = invocation.call.control.clone();
    let old_run = harness.run().await;
    harness.submit("urgent", Delivery::Interrupt).await;
    let request = recv(&mut requests).await;
    assert!(control.cancellation.is_cancelled());
    assert!(invocation.response.is_closed());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    let messages = &request.request.messages;
    for (index, started) in [(3, true), (4, false)] {
        assert_eq!(error_kind(&messages[index]), "cancelled");
        let ToolResultContent::Json { value } = &result_of(&messages[index]).content[0] else {
            panic!()
        };
        assert_eq!(value["details"]["execution_may_have_started"], started);
    }
    assert_eq!(messages[5].content, vec![Content::text("urgent")]);
    assert_eq!(
        harness
            .request(RequestKind::Stop { run: old_run })
            .await
            .unwrap_err(),
        RequestError::State(agent_input::StateError::StaleRun)
    );
    respond(request, text("done"));
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
        count(&events, |kind| matches!(
            kind,
            EventKind::ToolFinished { .. }
        )),
        2
    );
    assert_eq!(
        count(&events, |kind| matches!(
            kind,
            EventKind::RunFinished { .. }
        )),
        2
    );
}

/// Approvals are requests: the call parks until `ResolveApproval` answers,
/// and a run grant lets the same access through for the rest of the run.
#[tokio::test]
async fn approvals_park_calls_until_resolved_and_run_grants_cover_repeats() {
    let Fixture {
        session,
        handle,
        mut requests,
        mut invocations,
        probe,
    } = fixture(Mode::Ask);
    let mut harness = Harness::start(session, handle);
    harness.submit("work", Delivery::Queue).await;
    let other = tool_call("work", json!({"path": "elsewhere"}));
    respond(
        recv(&mut requests).await,
        calls(vec![call(), call(), other.clone()]),
    );
    let asked = harness
        .event(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested {
        call: first,
        tool,
        access,
    } = asked.kind
    else {
        unreachable!()
    };
    assert_eq!((tool.as_str(), access), ("work", Access("project".into())));
    assert_eq!(
        harness.handle.snapshot().await.unwrap().phase,
        Phase::AwaitingApproval
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness
            .request(RequestKind::ResolveApproval {
                call: other.id.clone(),
                decision: ApprovalDecision::AllowOnce,
            })
            .await
            .unwrap_err(),
        RequestError::NoApproval(other.id.clone())
    );
    harness
        .request(RequestKind::ResolveApproval {
            call: first.id,
            decision: ApprovalDecision::AllowForRun,
        })
        .await
        .unwrap();
    assert!(recv(&mut invocations).await.response.send(done()).is_ok());
    // The same tool and access are granted for this run: no second question.
    assert!(recv(&mut invocations).await.response.send(done()).is_ok());
    let asked = harness
        .event(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested { call: third, .. } = asked.kind else {
        unreachable!()
    };
    assert_eq!(third.id, other.id);
    harness
        .request(RequestKind::ResolveApproval {
            call: third.id,
            decision: ApprovalDecision::Deny("not that folder".into()),
        })
        .await
        .unwrap();
    let request = recv(&mut requests).await;
    assert_eq!(
        error_kind(request.request.messages.last().unwrap()),
        "permission_denied"
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    respond(request, text("done"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    harness.finish().await;
}

#[tokio::test]
async fn stop_while_approval_pending_never_dispatches_and_refuses_late_answers() {
    let Fixture {
        session,
        handle,
        mut requests,
        probe,
        ..
    } = fixture(Mode::Ask);
    let mut harness = Harness::start(session, handle);
    harness.submit("work", Delivery::Queue).await;
    let pending = call();
    respond(recv(&mut requests).await, calls(vec![pending.clone()]));
    harness
        .event(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    let run = harness.run().await;
    harness.request(RequestKind::Stop { run }).await.unwrap();
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    let late = harness
        .request(RequestKind::ResolveApproval {
            call: pending.id.clone(),
            decision: ApprovalDecision::AllowOnce,
        })
        .await;
    assert_eq!(late.unwrap_err(), RequestError::NoApproval(pending.id));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
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
        let Fixture {
            session,
            handle,
            mut requests,
            probe,
            ..
        } = fixture(Mode::Allow);
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
        let reason = if scenario == "truncated" {
            FinishReason::Length
        } else {
            FinishReason::ToolCalls
        };
        let mut message = reply(content, reason);
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
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0, "{scenario}");
        let (result, events) = harness.finish().await;
        assert_eq!(result.conversation.entries().len(), 2, "{scenario}");
        let billable = !matches!(
            scenario,
            "missing final" | "trailing event" | "trailing error"
        );
        assert_eq!(
            count(&events, |kind| matches!(kind, EventKind::UsageReported(_))),
            usize::from(billable),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn invalid_tool_stream_is_settled_as_error_without_committing_its_provisional_result() {
    let Fixture {
        session,
        handle,
        mut requests,
        mut invocations,
        ..
    } = fixture(Mode::Allow);
    let mut harness = Harness::start(session, handle);
    harness.submit("work", Delivery::Queue).await;
    respond(recv(&mut requests).await, calls(vec![call()]));
    let invocation = recv(&mut invocations).await;
    assert!(
        invocation
            .response
            .send(
                stream::iter([
                    ToolStreamItem::Finished(Ok(json!({"done": true}))),
                    ToolStreamItem::Progress(ToolProgress::Text {
                        text: "invalid late progress".into()
                    }),
                ])
                .boxed()
            )
            .is_ok()
    );
    let request = recv(&mut requests).await;
    assert_eq!(error_kind(&request.request.messages[3]), "invalid_stream");
    respond(request, text("done"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
    harness.finish().await;
}

#[tokio::test]
async fn queue_drains_after_handles_close_and_step_limit_pauses_remaining_work() {
    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = fixture(Mode::Allow);
    let task = tokio::spawn(session.run(|_| {}));
    for text in ["one", "two"] {
        handle
            .send(Request::new("human".into(), input(text, Delivery::Queue)))
            .await
            .unwrap();
    }
    drop(handle);
    respond(recv(&mut requests).await, text("one"));
    respond(recv(&mut requests).await, text("two"));
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert!(result.pending.is_empty());
    // The context note, then two prompts with their answers.
    assert_eq!(result.conversation.entries().len(), 5);

    let Fixture {
        session,
        handle,
        mut requests,
        mut invocations,
        ..
    } = fixture(Mode::Allow);
    let mut harness = Harness::start(session, handle);
    harness
        .request(RequestKind::Configure(ConfigurationChange::MaxSteps(
            NonZeroUsize::new(1).unwrap(),
        )))
        .await
        .unwrap();
    harness.submit("one", Delivery::Queue).await;
    let request = recv(&mut requests).await;
    harness.submit("two", Delivery::Queue).await;
    respond(request, calls(vec![call()]));
    assert!(recv(&mut invocations).await.response.send(done()).is_ok());
    harness
        .event(|kind| {
            matches!(
                kind,
                EventKind::RunFinished {
                    outcome: RunOutcome::Failed,
                    error: Some(RunError::StepLimit)
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
    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = fixture(Mode::Allow);
    let commands = handle.clone();
    let mut injected = false;
    let task = tokio::spawn(session.run(move |event| {
        if let EventKind::MessageCommitted { entry, .. } = &event.kind
            && entry.message.role == Role::Assistant
            && !injected
        {
            injected = true;
            commands
                .post(Request::new(
                    "parent".into(),
                    input("one more", Delivery::Steer),
                ))
                .unwrap();
        }
        if matches!(event.kind, EventKind::RunFinished { .. }) {
            commands
                .post(Request::new("human".into(), RequestKind::Shutdown))
                .unwrap();
        }
    }));
    handle
        .send(Request::new(
            "human".into(),
            input("begin", Delivery::Queue),
        ))
        .await
        .unwrap();
    respond(recv(&mut requests).await, text("first answer"));
    let next = recv(&mut requests).await;
    assert_eq!(
        next.request.messages.last().unwrap().content,
        vec![Content::text("one more")]
    );
    respond(next, text("second answer"));
    let result = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(result.conversation.entries().len(), 5);
}

#[test]
fn conversation_batches_are_atomic_and_replay_rejects_gaps_and_duplicates() {
    let entry = |message: Message| Entry {
        message,
        origin: Origin::Tool,
    };
    let mut conversation = Conversation::<&str>::new();
    let instructions = Message::new(Role::System, vec![Content::text("rules")]);
    assert!(matches!(
        conversation.append(vec![entry(instructions)]),
        Err(ConversationError::InvalidMessage(_))
    ));
    let initial = entry(Message::user("initial"));
    conversation.append(vec![initial.clone()]).unwrap();
    let call = call();
    let assistant = entry(calls(vec![call.clone()]));
    assert_eq!(
        conversation.append(vec![assistant.clone(), entry(Message::user("too soon"))]),
        Err(ConversationError::UnsettledCalls)
    );
    assert_eq!(conversation.entries(), [initial]);
    conversation.append(vec![assistant]).unwrap();
    let wrong = Message::new(
        Role::Tool,
        vec![Content::ToolResult(language_model::ToolResult {
            call: call.id,
            provider_id: Some("wrong".into()),
            name: Some(call.name),
            content: vec![ToolResultContent::Text {
                text: "done".into(),
            }],
            is_error: false,
        })],
    );
    assert_eq!(
        conversation.append(vec![entry(wrong)]),
        Err(ConversationError::UncorrelatedResult)
    );
    assert!(!conversation.is_settled());
    assert_eq!(
        conversation.apply(9, entry(Message::user("gap"))),
        Err(ConversationError::OutOfOrder)
    );
    assert_eq!(
        conversation.apply(0, entry(Message::user("duplicate"))),
        Err(ConversationError::OutOfOrder)
    );
}
