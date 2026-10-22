use std::{future::Future, sync::Arc};

use futures::{Stream, StreamExt as _, future::BoxFuture, stream, stream::BoxStream};
use language_model::{ToolCallId, ToolDefinition, ToolResultContent};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    CallControl, Provides, ToolCall, ToolCapabilities, ToolError, ToolErrorKind, ToolOutput,
    TypedToolOutput,
};

/// One typed tool. Simple tools implement `run`; streaming tools override
/// `execute` to return progress before their result. Associated types stay
/// concrete until the blanket `ToolDyn` adapter erases them for a registry.
pub trait Tool: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Send + 'static;
    type Output: ToolOutput + Send + 'static;
    /// Host services this tool needs; `()` when it needs none. A registry's
    /// context must provide it (see [`Provides`]).
    type Context: ?Sized + Send + Sync + 'static;
    /// What one call needs to be allowed, in this tool's terms; `()` when
    /// calls differ only by tool. A registry converts it into the host's
    /// access type, and authorization decides on that.
    type Access: Clone + Default + Send + Sync + 'static;

    fn name(&self) -> &str;
    fn description(&self) -> String;

    /// Execution declarations captured when the tool is registered. These are
    /// host metadata and are not part of the model's definition.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::default()
    }

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

    /// What these arguments need. Computed once, before authorization; the
    /// call then receives this exact value in [`ToolCall::access`].
    fn access(&self, _args: &Self::Args, _context: &Self::Context) -> Self::Access {
        Self::Access::default()
    }

    fn run(
        &self,
        _call: ToolCall<Self::Access>,
        _context: Arc<Self::Context>,
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
        call: ToolCall<Self::Access>,
        context: Arc<Self::Context>,
        args: Self::Args,
    ) -> impl Future<Output = ToolStream<Self::Output>> + Send {
        async move { finished_only(self.run(call, context, args).await) }
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

type Run = Box<
    dyn FnOnce(ToolCallId, CallControl) -> BoxFuture<'static, ToolStream<TypedToolOutput>> + Send,
>;

/// A call whose arguments decoded and whose access is known, but which has
/// not started. Authorization inspects `access`; only `execute` runs the tool.
pub struct PreparedCall<A> {
    access: A,
    run: Run,
}

impl<A> PreparedCall<A> {
    pub fn new(
        access: A,
        run: impl FnOnce(ToolCallId, CallControl) -> BoxFuture<'static, ToolStream<TypedToolOutput>>
        + Send
        + 'static,
    ) -> Self {
        Self {
            access,
            run: Box::new(run),
        }
    }

    pub fn access(&self) -> &A {
        &self.access
    }

    /// Awaiting the returned future may already run a simple tool.
    pub fn execute(
        self,
        id: ToolCallId,
        control: CallControl,
    ) -> BoxFuture<'static, ToolStream<TypedToolOutput>> {
        (self.run)(id, control)
    }
}

/// Object-safe surface for heterogeneous registries, over a host context `C`
/// and the host's access type `A`. Typed tools gain this automatically when
/// `C` provides their context and `A` converts from their access.
pub trait ToolDyn<C: ?Sized, A>: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::default()
    }

    /// Decodes arguments and computes the call's access without running it.
    fn prepare(
        self: Arc<Self>,
        args: Value,
        context: &Arc<C>,
    ) -> Result<PreparedCall<A>, ToolError>;
}

impl<T, C, A> ToolDyn<C, A> for T
where
    T: Tool,
    C: Provides<T::Context> + ?Sized,
    A: From<T::Access>,
{
    fn definition(&self) -> ToolDefinition {
        Tool::definition(self)
    }

    fn capabilities(&self) -> ToolCapabilities {
        Tool::capabilities(self)
    }

    fn prepare(
        self: Arc<Self>,
        args: Value,
        context: &Arc<C>,
    ) -> Result<PreparedCall<A>, ToolError> {
        let args = serde_json::from_value::<T::Args>(args)
            .map_err(|error| ToolError::new(ToolErrorKind::InvalidArguments, error.to_string()))?;
        let context = context.provide();
        let access = self.access(&args, &context);
        Ok(PreparedCall::new(
            A::from(access.clone()),
            move |id, control| {
                Box::pin(async move {
                    let name = self.name().to_owned();
                    let call = ToolCall {
                        id,
                        access,
                        control,
                    };
                    Tool::execute(&*self, call, context, args)
                        .await
                        .map(move |item| match item {
                            ToolStreamItem::Progress(progress) => {
                                ToolStreamItem::Progress(progress)
                            }
                            ToolStreamItem::Finished(result) => ToolStreamItem::Finished(
                                result.and_then(|output| TypedToolOutput::new(&name, output)),
                            ),
                        })
                        .boxed()
                })
            },
        ))
    }
}
