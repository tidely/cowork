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

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    value: usize,
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

struct Calculate {
    multiplier: usize,
}

impl Tool for Calculate {
    type Args = Args;
    type Output = Answer;

    fn name(&self) -> &str {
        "calculate"
    }

    fn description(&self) -> String {
        "Multiply a value".into()
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }

    async fn run(&self, context: ToolCallContext, args: Args) -> Result<Answer, ToolError> {
        let calls = context.extensions.get::<AtomicUsize>().unwrap();
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Answer {
            value: args.value * self.multiplier,
            call: context.call_id,
        })
    }
}

#[test]
fn dynamic_dispatch_retains_structured_output_model_content_and_context() {
    block_on(async {
        let mut registry = ToolRegistry::default();
        let tool = Calculate { multiplier: 2 };
        let capabilities = Tool::capabilities(&tool);
        registry.register(tool).unwrap();
        let registry: Arc<dyn ToolDispatch> = Arc::new(registry);
        assert_eq!(
            registry.descriptor("calculate").unwrap().capabilities,
            capabilities
        );
        assert!(registry.descriptor("missing").is_none());
        let calls = Arc::new(AtomicUsize::new(0));
        let id = ToolCallId::new();
        let mut context = ToolCallContext::new(id.clone());
        context.extensions.insert_arc(calls.clone());
        let stream = registry
            .call("calculate", json!({"value": 3}), context.clone())
            .await;
        // A returned stream owns its work independently of the registry borrow.
        let output = collect_output(stream).await.unwrap();
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
            let error = collect_output(registry.call(name, args, context.clone()).await)
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn duplicate_registration_does_not_replace_the_existing_tool() {
    block_on(async {
        let mut registry = ToolRegistry::default();
        registry.register(Calculate { multiplier: 2 }).unwrap();
        assert!(registry.register(Calculate { multiplier: 100 }).is_err());
        let mut context = ToolCallContext::new(ToolCallId::new());
        context.extensions.insert(AtomicUsize::new(0));
        let output = collect_output(
            registry
                .call("calculate", json!({"value": 3}), context)
                .await,
        )
        .await
        .unwrap();
        assert!(matches!(output.output, BuiltinToolOutput::Dynamic(value) if value["value"] == 6));
    });
}

struct Streaming;
impl Tool for Streaming {
    type Args = Args;
    type Output = serde_json::Value;

    fn name(&self) -> &str {
        "streaming"
    }
    fn description(&self) -> String {
        "Report progress before returning a result".into()
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            supports_progress: true,
            ..Default::default()
        }
    }

    async fn execute(&self, context: ToolCallContext, args: Args) -> ToolStream<Self::Output> {
        let calls = context.extensions.get::<AtomicUsize>().unwrap();
        with_progress(
            stream::iter([ToolProgress::Text {
                text: "working".into(),
            }]),
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"value": args.value}))
            },
        )
    }
}

#[test]
fn streaming_dispatch_preserves_progress_and_defers_final_work() {
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut context = ToolCallContext::new(ToolCallId::new());
        context.extensions.insert_arc(calls.clone());
        let mut stream = {
            let mut registry = ToolRegistry::default();
            registry.register(Streaming).unwrap();
            registry
                .call("streaming", json!({"value": 9}), context)
                .await
        };
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            matches!(stream.next().await, Some(ToolStreamItem::Progress(ToolProgress::Text { text })) if text == "working")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let output = collect_output(stream).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
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
        output.output,
        BuiltinToolOutput::Dynamic(json!({"data": [1, 2]}))
    );
    assert_eq!(
        output.model_output,
        vec![ToolResultContent::Json {
            value: json!({"data": [1, 2]})
        }]
    );
}

struct DynamicMetadata {
    definition_reads: Arc<AtomicUsize>,
    capability_reads: Arc<AtomicUsize>,
}

impl ToolDyn for DynamicMetadata {
    fn definition(&self) -> language_model::ToolDefinition {
        // Deliberately changing source metadata checks that the registry's
        // description and policy view remain the same registration snapshot.
        let revision = self.definition_reads.fetch_add(1, Ordering::SeqCst);
        language_model::ToolDefinition {
            name: "dynamic".into(),
            description: Some(format!("revision {revision}")),
            parameters: Default::default(),
        }
    }

    fn capabilities(&self) -> ToolCapabilities {
        let limit = self.capability_reads.fetch_add(1, Ordering::SeqCst) + 1;
        ToolCapabilities {
            supports_cancel: true,
            supports_progress: true,
            is_read_only: true,
            max_concurrency: std::num::NonZeroU32::new(limit as u32),
            timeout: Some(std::time::Duration::from_secs(30)),
        }
    }

    fn execute(
        &self,
        _context: ToolCallContext,
        args: serde_json::Value,
    ) -> futures::future::BoxFuture<'_, ToolStream<TypedToolOutput>> {
        Box::pin(async move { finished_only(TypedToolOutput::new("dynamic", args)) })
    }
}

#[test]
fn registry_snapshots_dynamic_metadata_and_keeps_model_definitions_separate() {
    let definition_reads = Arc::new(AtomicUsize::new(0));
    let capability_reads = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::default();
    registry
        .register_dyn(Arc::new(DynamicMetadata {
            definition_reads: definition_reads.clone(),
            capability_reads: capability_reads.clone(),
        }))
        .unwrap();
    let registry: Arc<dyn ToolDispatch> = Arc::new(registry);
    let snapshot = registry.descriptor("dynamic").unwrap();
    assert_eq!(registry.descriptors(), vec![snapshot.clone()]);
    assert_eq!(registry.definitions(), vec![snapshot.definition.clone()]);
    assert_eq!(registry.descriptor("dynamic"), Some(snapshot));
    assert_eq!(definition_reads.load(Ordering::SeqCst), 1);
    assert_eq!(capability_reads.load(Ordering::SeqCst), 1);
}

/// A minimal host policy used to exercise metadata through a dispatch wrapper.
/// A production permission gate would also inspect arguments and caller grants.
struct ReadOnlyDispatch {
    inner: Arc<dyn ToolDispatch>,
}

impl ToolDispatch for ReadOnlyDispatch {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.inner.descriptors()
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: serde_json::Value,
        context: ToolCallContext,
    ) -> futures::future::BoxFuture<'a, ToolStream<TypedToolOutput>> {
        Box::pin(async move {
            if self
                .descriptor(name)
                .is_some_and(|tool| !tool.capabilities.is_read_only)
            {
                return finished_only(Err(ToolError::new(
                    ToolErrorKind::PermissionDenied,
                    "mutating calls need approval",
                )));
            }
            self.inner.call(name, args, context).await
        })
    }
}

#[test]
fn dispatch_wrappers_can_use_metadata_before_execution_starts() {
    block_on(async {
        let mut registry = ToolRegistry::default();
        registry.register(Calculate { multiplier: 2 }).unwrap();
        registry.register(Streaming).unwrap();
        let dispatch: Arc<dyn ToolDispatch> = Arc::new(ReadOnlyDispatch {
            inner: Arc::new(registry),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let mut context = ToolCallContext::new(ToolCallId::new());
        context.extensions.insert_arc(calls.clone());
        let denied = collect_output(
            dispatch
                .call("streaming", json!({"value": 3}), context.clone())
                .await,
        )
        .await
        .unwrap_err();
        assert_eq!(denied.kind, ToolErrorKind::PermissionDenied);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        collect_output(
            dispatch
                .call("calculate", json!({"value": 3}), context)
                .await,
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}
