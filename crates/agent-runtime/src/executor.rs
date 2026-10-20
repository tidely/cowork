use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use agent_input::RunId;
use futures::{
    StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use language_model::{ToolArguments, ToolCall, ToolDefinition};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tool_runtime::{
    ToolCallContext, ToolDescriptor, ToolDispatch, ToolError, ToolErrorKind, ToolProgress,
    ToolStream, ToolStreamItem, TypedExtensions, TypedToolOutput,
};

/// Per-call host service, installed after authorization. Cancellation is
/// cooperative and never promises rollback or termination of external work.
#[derive(Clone, Debug, Default)]
pub struct CallControl {
    pub cancellation: CancellationToken,
    /// Separate from cancellation: a wait may yield while its producer keeps
    /// running. Tools choose whether they implement this service.
    pub yield_wait: CancellationToken,
}

pub struct ToolRequest {
    pub run: RunId,
    pub call: ToolCall,
    pub descriptor: ToolDescriptor,
}

/// An explicit host decision. Approved extensions can carry exact permission
/// grants. Pending authorization never invokes the underlying dispatcher.
pub enum Authorization {
    Allow(TypedExtensions),
    Deny(ToolError),
    Pending(BoxFuture<'static, Result<TypedExtensions, ToolError>>),
}

pub trait ToolPolicy: Send + Sync {
    fn authorize(&self, request: &ToolRequest) -> Authorization;
}

/// Opt-in unrestricted policy, useful for trusted tools and examples. The
/// executor deliberately has no implicit allow policy.
pub struct AllowAll;

/// An empty allowlist denies every call, including read-only tools. Hosts can
/// opt individual names in without replacing a custom permission service.
#[derive(Default)]
pub struct AllowTools {
    names: HashSet<String>,
}

impl AllowTools {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }
}

impl ToolPolicy for AllowTools {
    fn authorize(&self, request: &ToolRequest) -> Authorization {
        if self.names.contains(&request.call.name) {
            Authorization::Allow(TypedExtensions::default())
        } else {
            Authorization::Deny(ToolError::new(
                ToolErrorKind::PermissionDenied,
                format!("tool {} is not allowed", request.call.name),
            ))
        }
    }
}

impl ToolPolicy for AllowAll {
    fn authorize(&self, _: &ToolRequest) -> Authorization {
        Authorization::Allow(TypedExtensions::default())
    }
}

#[derive(Debug)]
pub(crate) enum ExecutionEvent {
    AwaitingApproval,
    WaitingForCapacity,
    Started,
    Progress(ToolProgress),
    Finished(Result<TypedToolOutput, ToolError>),
}

/// A conservative executor shared by one or more sessions: one call at a time,
/// including its full result stream. This respects positive per-tool concurrency
/// limits and serializes conflicting operations without guessing paths from JSON.
/// Suggested timeouts remain host policy; no tool call is automatically retried.
#[derive(Clone)]
pub struct ToolExecutor {
    dispatch: Arc<dyn ToolDispatch>,
    policy: Arc<dyn ToolPolicy>,
    descriptors: Arc<BTreeMap<String, ToolDescriptor>>,
    serial: Arc<Semaphore>,
}

enum Stage {
    Authorize,
    Approval(BoxFuture<'static, Result<TypedExtensions, ToolError>>),
    Permit(TypedExtensions),
    Acquire(ToolCallContext),
    Dispatch(ToolCallContext, OwnedSemaphorePermit),
    Read(
        ToolStream<TypedToolOutput>,
        OwnedSemaphorePermit,
        Option<Result<TypedToolOutput, ToolError>>,
    ),
    Done,
}

struct Execution {
    executor: ToolExecutor,
    run: RunId,
    call: ToolCall,
    control: CallControl,
    stage: Stage,
}

impl ToolExecutor {
    pub fn new(
        dispatch: Arc<dyn ToolDispatch>,
        policy: Arc<dyn ToolPolicy>,
    ) -> Result<Self, ToolError> {
        let mut descriptors = BTreeMap::new();
        for descriptor in dispatch.descriptors() {
            let name = descriptor.definition.name.clone();
            if name.trim().is_empty() || descriptors.insert(name, descriptor).is_some() {
                return Err(ToolError::new(
                    ToolErrorKind::InvalidArguments,
                    "tool definitions need unique nonempty names",
                ));
            }
        }
        Ok(Self {
            dispatch,
            policy,
            descriptors: Arc::new(descriptors),
            serial: Arc::new(Semaphore::new(1)),
        })
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.descriptors
            .values()
            .map(|descriptor| descriptor.definition.clone())
            .collect()
    }

    /// Restrict both model visibility and dispatch admission to selected tools.
    /// Resolve every name before changing the snapshot so typos fail at build.
    pub(crate) fn select_tools(mut self, names: &[String]) -> Result<Self, ToolError> {
        let mut selected = BTreeMap::new();
        for name in names {
            let descriptor = self.descriptors.get(name).ok_or_else(|| {
                ToolError::new(
                    ToolErrorKind::NotFound,
                    format!("unregistered tool: {name}"),
                )
            })?;
            selected.insert(name.clone(), descriptor.clone());
        }
        self.descriptors = Arc::new(selected);
        Ok(self)
    }

    pub(crate) fn execute(
        &self,
        run: RunId,
        call: ToolCall,
        control: CallControl,
    ) -> BoxStream<'static, ExecutionEvent> {
        let state = Execution {
            executor: self.clone(),
            run,
            call,
            control,
            stage: Stage::Authorize,
        };
        stream::unfold(state, |mut state| async move {
            loop {
                let stage = std::mem::replace(&mut state.stage, Stage::Done);
                match stage {
                    Stage::Done => return None,
                    Stage::Authorize => {
                        let Some(descriptor) =
                            state.executor.descriptors.get(&state.call.name).cloned()
                        else {
                            return Some((
                                ExecutionEvent::Finished(Err(ToolError::new(
                                    ToolErrorKind::NotFound,
                                    "unknown tool",
                                ))),
                                state,
                            ));
                        };
                        if !matches!(state.call.arguments, ToolArguments::Object { .. }) {
                            return Some((
                                ExecutionEvent::Finished(Err(ToolError::new(
                                    ToolErrorKind::InvalidArguments,
                                    "tool arguments must be a JSON object",
                                ))),
                                state,
                            ));
                        }
                        let request = ToolRequest {
                            run: state.run,
                            call: state.call.clone(),
                            descriptor,
                        };
                        match state.executor.policy.authorize(&request) {
                            Authorization::Allow(extensions) => {
                                state.stage = Stage::Permit(extensions)
                            }
                            Authorization::Deny(error) => {
                                return Some((ExecutionEvent::Finished(Err(error)), state));
                            }
                            Authorization::Pending(future) => {
                                state.stage = Stage::Approval(future);
                                return Some((ExecutionEvent::AwaitingApproval, state));
                            }
                        }
                    }
                    Stage::Approval(future) => match future.await {
                        Ok(extensions) => state.stage = Stage::Permit(extensions),
                        Err(error) => return Some((ExecutionEvent::Finished(Err(error)), state)),
                    },
                    Stage::Permit(mut extensions) => {
                        extensions.insert(state.control.clone());
                        let context = ToolCallContext {
                            call_id: state.call.id.clone(),
                            extensions,
                        };
                        state.stage = Stage::Acquire(context);
                        return Some((ExecutionEvent::WaitingForCapacity, state));
                    }
                    Stage::Acquire(context) => {
                        state.stage = Stage::Dispatch(
                            context,
                            state
                                .executor
                                .serial
                                .clone()
                                .acquire_owned()
                                .await
                                .expect("executor semaphore stays open"),
                        );
                    }
                    Stage::Dispatch(context, permit) => {
                        state.stage = Stage::Read(
                            // Dispatch happens on the next poll after Started;
                            // see the startup stream below for eager tools.
                            dispatch_stream(
                                state.executor.dispatch.clone(),
                                state.call.clone(),
                                context,
                            ),
                            permit,
                            None,
                        );
                        return Some((ExecutionEvent::Started, state));
                    }
                    Stage::Read(mut stream, permit, mut result) => match stream.next().await {
                        Some(_) if result.is_some() => {
                            return Some((
                                ExecutionEvent::Finished(Err(ToolError::new(
                                    ToolErrorKind::InvalidStream,
                                    "event after final tool result",
                                ))),
                                state,
                            ));
                        }
                        Some(ToolStreamItem::Progress(progress)) => {
                            state.stage = Stage::Read(stream, permit, result);
                            return Some((ExecutionEvent::Progress(progress), state));
                        }
                        Some(ToolStreamItem::Finished(value)) => {
                            result = Some(value);
                            state.stage = Stage::Read(stream, permit, result);
                        }
                        None => {
                            let result = result
                                .unwrap_or_else(|| {
                                    Err(ToolError::new(
                                        ToolErrorKind::InvalidStream,
                                        "tool stream ended without a final result",
                                    ))
                                })
                                .and_then(|output| {
                                    if output.tool_name != state.call.name
                                        || output.model_output.is_empty()
                                    {
                                        Err(ToolError::new(
                                            ToolErrorKind::InvalidOutput,
                                            "tool output identity or content is invalid",
                                        ))
                                    } else {
                                        Ok(output)
                                    }
                                });
                            return Some((ExecutionEvent::Finished(result), state));
                        }
                    },
                }
            }
        })
        .boxed()
    }
}

fn dispatch_stream(
    dispatch: Arc<dyn ToolDispatch>,
    call: ToolCall,
    context: ToolCallContext,
) -> ToolStream<TypedToolOutput> {
    stream::once(async move {
        let ToolArguments::Object { value } = call.arguments else {
            unreachable!("validated tool arguments")
        };
        dispatch.call(&call.name, value.into(), context).await
    })
    .flatten()
    .boxed()
}
