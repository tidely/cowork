use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::{StreamExt as _, executor::block_on, stream};
use language_model::{ToolCallId, ToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::*;

/// The host-side context: a counter service plus the host's own access type.
struct Host {
    counter: Arc<Counter>,
}

#[derive(Default)]
struct Counter(AtomicUsize);

impl Provides<Counter> for Host {
    fn provide(self: &Arc<Self>) -> Arc<Counter> {
        self.counter.clone()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Scope {
    #[default]
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq)]
enum HostAccess {
    Basic,
    Scoped(Scope),
}

impl From<()> for HostAccess {
    fn from(_: ()) -> Self {
        Self::Basic
    }
}

impl From<Scope> for HostAccess {
    fn from(scope: Scope) -> Self {
        Self::Scoped(scope)
    }
}

type Registry = ToolRegistry<Host, HostAccess>;

fn host() -> Arc<Host> {
    Arc::new(Host {
        counter: Arc::default(),
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    value: usize,
    #[serde(default)]
    store: bool,
}

#[derive(Serialize)]
struct Answer {
    value: usize,
    call: ToolCallId,
}

impl ToolOutput for Answer {
    fn model_output(&self) -> Option<Vec<ToolResultContent>> {
        Some(vec![ToolResultContent::Text {
            text: format!("The answer is {}", self.value),
        }])
    }
}

/// Needs the counter service, and write access only when asked to store.
struct Calculate;

impl Tool for Calculate {
    type Args = Args;
    type Output = Answer;
    type Context = Counter;
    type Access = Scope;

    fn name(&self) -> &str {
        "calculate"
    }
    fn description(&self) -> String {
        "Double a value".into()
    }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }
    fn access(&self, args: &Args, _: &Counter) -> Scope {
        if args.store {
            Scope::Write
        } else {
            Scope::Read
        }
    }
    async fn run(
        &self,
        call: ToolCall<Scope>,
        counter: Arc<Counter>,
        args: Args,
    ) -> Result<Answer, ToolError> {
        assert_eq!(call.access == Scope::Write, args.store);
        counter.0.fetch_add(1, Ordering::SeqCst);
        Ok(Answer {
            value: args.value * 2,
            call: call.id,
        })
    }
}

fn run(
    registry: &Registry,
    host: &Arc<Host>,
    name: &str,
    args: serde_json::Value,
) -> Result<TypedToolOutput, ToolError> {
    block_on(async {
        let prepared = registry.prepare(name, args, host)?;
        collect_output(
            prepared
                .execute(ToolCallId::new(), CallControl::default())
                .await,
        )
        .await
    })
}

#[test]
fn registry_provides_typed_context_and_computes_access_before_running() {
    let mut registry = Registry::default();
    registry.register(Calculate).unwrap();
    let host = host();
    assert!(
        registry
            .get("calculate")
            .unwrap()
            .descriptor
            .capabilities
            .is_read_only
    );

    // Preparing decodes arguments and computes access, but runs nothing.
    let prepared = registry
        .prepare("calculate", json!({"value": 3, "store": true}), &host)
        .unwrap();
    assert_eq!(prepared.access(), &HostAccess::Scoped(Scope::Write));
    assert_eq!(host.counter.0.load(Ordering::SeqCst), 0);
    let id = ToolCallId::new();
    let output = block_on(async {
        collect_output(prepared.execute(id.clone(), CallControl::default()).await).await
    })
    .unwrap();
    assert_eq!(
        output.output,
        BuiltinToolOutput::Dynamic(json!({"value": 6, "call": id}))
    );
    assert_eq!(
        output.model_output,
        vec![ToolResultContent::Text {
            text: "The answer is 6".into()
        }]
    );

    for (name, args, kind) in [
        (
            "calculate",
            json!({"value": "wrong type"}),
            ToolErrorKind::InvalidArguments,
        ),
        (
            "calculate",
            json!({"value": 3, "invented": true}),
            ToolErrorKind::InvalidArguments,
        ),
        ("missing", json!({"value": 3}), ToolErrorKind::NotFound),
    ] {
        assert_eq!(run(&registry, &host, name, args).unwrap_err().kind, kind);
    }
    assert_eq!(host.counter.0.load(Ordering::SeqCst), 1);
}

/// Registration order is the order a session offers its tools in.
#[test]
fn registry_keeps_registration_order_and_refuses_duplicates() {
    let mut registry = Registry::default();
    registry.register(Streaming).unwrap();
    registry.register(Calculate).unwrap();
    assert!(registry.register(Streaming).is_err());
    let names: Vec<_> = registry.descriptors().map(ToolDescriptor::name).collect();
    assert_eq!(names, ["streaming", "calculate"]);
}

/// Needs nothing from the host: `()` context and `()` access.
struct Streaming;

impl Tool for Streaming {
    type Args = Args;
    type Output = serde_json::Value;
    type Context = ();
    type Access = ();

    fn name(&self) -> &str {
        "streaming"
    }
    fn description(&self) -> String {
        "Report progress before returning a result".into()
    }
    async fn execute(
        &self,
        _: ToolCall<()>,
        _: Arc<()>,
        args: Args,
    ) -> ToolStream<serde_json::Value> {
        with_progress(
            stream::iter([ToolProgress::Text {
                text: "working".into(),
            }]),
            async move { Ok(json!({"value": args.value})) },
        )
    }
}

#[test]
fn streaming_tools_report_progress_before_their_result() {
    let mut registry = Registry::default();
    registry.register(Streaming).unwrap();
    let host = host();
    let prepared = registry
        .prepare("streaming", json!({"value": 9}), &host)
        .unwrap();
    assert_eq!(prepared.access(), &HostAccess::Basic);
    block_on(async {
        let mut stream = prepared
            .execute(ToolCallId::new(), CallControl::default())
            .await;
        assert!(matches!(
            stream.next().await,
            Some(ToolStreamItem::Progress(ToolProgress::Text { text })) if text == "working"
        ));
        let output = collect_output(stream).await.unwrap();
        assert_eq!(
            output.model_output,
            vec![ToolResultContent::Json {
                value: json!({"value": 9})
            }]
        );
    });
}

#[test]
fn collecting_requires_one_final_result_and_rejects_late_events() {
    block_on(async {
        fn progress() -> ToolStreamItem<usize> {
            ToolStreamItem::Progress(ToolProgress::Text {
                text: "partial".into(),
            })
        }
        let failure = ToolError::new(ToolErrorKind::PermissionDenied, "host declined")
            .with_details(json!({"path":"private"}));
        for items in [
            vec![],
            vec![progress()],
            vec![ToolStreamItem::Finished(Ok(1)), progress()],
            vec![
                ToolStreamItem::Finished(Ok(1)),
                ToolStreamItem::Finished(Ok(2)),
            ],
            vec![ToolStreamItem::Finished(Err(failure.clone())), progress()],
        ] {
            let error = collect_output(stream::iter(items).boxed())
                .await
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidStream);
        }
        assert_eq!(
            collect_output(finished_only::<usize>(Err(failure.clone())))
                .await
                .unwrap_err(),
            failure
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let task_calls = calls.clone();
        let stream = deferred_result(async move {
            task_calls.fetch_add(1, Ordering::SeqCst);
            Ok(7)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(collect_output(stream).await.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

#[derive(Serialize)]
struct EmptyContent;

impl ToolOutput for EmptyContent {
    fn model_output(&self) -> Option<Vec<ToolResultContent>> {
        Some(vec![])
    }
}

struct BrokenOutput;

impl Serialize for BrokenOutput {
    fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("output could not be encoded"))
    }
}

impl ToolOutput for BrokenOutput {}

#[test]
fn output_projection_rejects_invalid_results_and_keeps_dynamic_values_unwrapped() {
    assert_eq!(
        TypedToolOutput::new("broken", BrokenOutput)
            .unwrap_err()
            .kind,
        ToolErrorKind::InvalidOutput
    );
    assert_eq!(
        TypedToolOutput::new("empty", EmptyContent)
            .unwrap_err()
            .kind,
        ToolErrorKind::InvalidOutput
    );
    let output = TypedToolOutput::new(
        "custom",
        BuiltinToolOutput::Dynamic(json!({"data": [1, 2]})),
    )
    .unwrap();
    assert_eq!(
        output.model_output,
        vec![ToolResultContent::Json {
            value: json!({"data": [1, 2]})
        }]
    );
}

/// A hand-written dynamic tool whose metadata changes on every read.
struct DynamicMetadata {
    reads: Arc<AtomicUsize>,
}

impl ToolDyn<Host, HostAccess> for DynamicMetadata {
    fn definition(&self) -> language_model::ToolDefinition {
        let revision = self.reads.fetch_add(1, Ordering::SeqCst);
        language_model::ToolDefinition {
            name: "dynamic".into(),
            description: Some(format!("revision {revision}")),
            parameters: Default::default(),
        }
    }
    fn prepare(
        self: Arc<Self>,
        args: serde_json::Value,
        _: &Arc<Host>,
    ) -> Result<PreparedCall<HostAccess>, ToolError> {
        Ok(PreparedCall::new(HostAccess::Basic, move |_, _| {
            Box::pin(async move { finished_only(TypedToolOutput::new("dynamic", args)) })
        }))
    }
}

#[test]
fn registry_snapshots_metadata_once_at_registration() {
    let reads = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry
        .register_dyn(Arc::new(DynamicMetadata {
            reads: reads.clone(),
        }))
        .unwrap();
    let snapshot = registry.get("dynamic").unwrap().descriptor.clone();
    assert_eq!(
        registry.descriptors().cloned().collect::<Vec<_>>(),
        vec![snapshot.clone()]
    );
    assert_eq!(registry.get("dynamic").unwrap().descriptor, snapshot);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(run(&registry, &host(), "dynamic", json!({"x": 1})).is_ok());
}
