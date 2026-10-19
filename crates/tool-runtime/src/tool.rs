use std::{future::Future, sync::Arc};

use futures::{Stream, StreamExt as _, future::BoxFuture, stream, stream::BoxStream};
use language_model::{ToolDefinition, ToolResultContent};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{ToolCallContext, ToolError, ToolErrorKind, ToolOutput, TypedToolOutput};

/// One typed tool. The host invokes `execute`; simple tools implement `run`,
/// while streaming tools override `execute` to return progress and a final result.
/// Associated types stay concrete until the blanket `ToolDyn` adapter is used.
pub trait Tool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + Send + 'static;
    type Output: Serialize + ToolOutput + Send + 'static;

    fn name(&self) -> &str;
    fn description(&self) -> String;

    fn definition(&self) -> ToolDefinition {
        let schema: Value = schemars::schema_for!(Self::Args).into();
        let parameters = match schema {
            Value::Object(properties) => properties,
            // Custom JsonSchema implementations can produce boolean schemas.
            // ToolDefinition requires an object; allOf preserves their meaning.
            value => serde_json::Map::from_iter([("allOf".into(), Value::Array(vec![value]))]),
        };
        ToolDefinition {
            name: self.name().into(),
            description: Some(self.description()),
            parameters,
        }
    }

    fn run(
        &self,
        _context: ToolCallContext,
        _args: Self::Args,
    ) -> impl Future<Output = Result<Self::Output, ToolError>> + Send {
        async {
            Err(ToolError::new(
                ToolErrorKind::NotImplemented,
                "tool must implement run or execute",
            ))
        }
    }

    fn execute(
        &self,
        context: ToolCallContext,
        args: Self::Args,
    ) -> impl Future<Output = ToolStream<Self::Output>> + Send {
        async move { finished_only(self.run(context, args).await) }
    }
}

pub type ToolStream<T> = BoxStream<'static, ToolStreamItem<T>>;

/// A call emits progress followed by exactly one final result. An ended stream
/// without `Finished` is incomplete, even if useful progress was already shown.
#[derive(Debug)]
pub enum ToolStreamItem<T> {
    Progress(ToolProgress),
    Finished(Result<T, ToolError>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolProgress {
    Text { text: String },
    Content { content: Vec<ToolResultContent> },
    Custom { kind: String, value: Value },
}

pub fn finished_only<T: Send + 'static>(result: Result<T, ToolError>) -> ToolStream<T> {
    stream::once(async move { ToolStreamItem::Finished(result) }).boxed()
}

/// Defers the operation until the returned stream is polled.
pub fn deferred_result<T, F>(result: F) -> ToolStream<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T, ToolError>> + Send + 'static,
{
    stream::once(async move { ToolStreamItem::Finished(result.await) }).boxed()
}

/// Drains progress before polling the final-result future. The progress stream
/// and final future must therefore be able to make progress in that order.
pub fn with_progress<T, P, F>(progress: P, result: F) -> ToolStream<T>
where
    T: Send + 'static,
    P: Stream<Item = ToolProgress> + Send + 'static,
    F: Future<Output = Result<T, ToolError>> + Send + 'static,
{
    progress
        .map(ToolStreamItem::Progress)
        .chain(deferred_result(result))
        .boxed()
}

/// Drains a call without keeping previews. Validates missing or repeated final
/// results and progress after completion, including after a tool error.
pub async fn collect_output<T>(mut stream: ToolStream<T>) -> Result<T, ToolError> {
    let mut result = None;
    while let Some(item) = stream.next().await {
        if result.is_some() {
            return Err(ToolError::new(
                ToolErrorKind::InvalidStream,
                "tool emitted an event after its final result",
            ));
        }
        if let ToolStreamItem::Finished(value) = item {
            result = Some(value);
        }
    }
    result.ok_or_else(|| {
        ToolError::new(
            ToolErrorKind::InvalidStream,
            "tool stream ended without a final result",
        )
    })?
}

/// Object-safe surface for heterogeneous registries. Typed implementations gain
/// this adapter automatically; input decoding failures are final tool errors.
pub trait ToolDyn: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn execute(
        &self,
        context: ToolCallContext,
        args: Value,
    ) -> BoxFuture<'_, ToolStream<TypedToolOutput>>;
}

impl<T: Tool> ToolDyn for T {
    fn definition(&self) -> ToolDefinition {
        Tool::definition(self)
    }

    fn execute(
        &self,
        context: ToolCallContext,
        args: Value,
    ) -> BoxFuture<'_, ToolStream<TypedToolOutput>> {
        Box::pin(async move {
            let args = match serde_json::from_value::<T::Args>(args) {
                Ok(args) => args,
                Err(error) => {
                    return finished_only(Err(ToolError::new(
                        ToolErrorKind::InvalidArguments,
                        error.to_string(),
                    )));
                }
            };
            let name = self.name().to_owned();
            Tool::execute(self, context, args)
                .await
                .map(move |item| match item {
                    ToolStreamItem::Progress(progress) => ToolStreamItem::Progress(progress),
                    ToolStreamItem::Finished(result) => ToolStreamItem::Finished(
                        result.and_then(|output| TypedToolOutput::new(name.clone(), output)),
                    ),
                })
                .boxed()
        })
    }
}

pub type ArcTool = Arc<dyn ToolDyn>;
