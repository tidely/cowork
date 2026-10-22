use std::sync::Arc;

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    future,
    stream::{self, BoxStream},
};
use language_model::ToolCall;
use tool_runtime::{
    CallControl, ToolError, ToolErrorKind, ToolProgress, ToolStreamItem, TypedToolOutput,
};

use crate::{
    AgentHost, ApprovalDecision, CallAuthorization, Decision, Grant,
    tools::{Resolved, SEARCH_TOOLS, ToolSnapshot},
};

#[derive(Debug)]
pub(crate) enum ExecutionEvent<A> {
    /// The real tool this call runs, also when reached through `call_tool`.
    Resolved(String),
    /// The host must decide; the session keeps `respond` until a
    /// `ResolveApproval` request answers.
    AwaitingApproval {
        access: A,
        respond: oneshot::Sender<ApprovalDecision>,
    },
    /// Authorized and dispatched; external effects may happen from here.
    Started,
    Progress(ToolProgress),
    Finished(Result<TypedToolOutput, ToolError>),
}

/// Authorization state captured when a call starts, so a configuration
/// change applies to calls not yet authorized and never to a running one.
pub(crate) struct CallScope<H: AgentHost> {
    pub host: Arc<H>,
    pub tools: Arc<ToolSnapshot<H>>,
    pub origin: H::Source,
    pub permissions: H::Permissions,
    pub grants: Vec<Grant<H::Access>>,
}

/// Runs one call: resolve it, decode its arguments and compute its access,
/// authorize it (possibly waiting for an approval), then execute it. The
/// stream ends with exactly one `Finished`. Dropping it drops the call.
pub(crate) fn execute<H: AgentHost>(
    scope: CallScope<H>,
    call: ToolCall,
    control: CallControl,
) -> BoxStream<'static, ExecutionEvent<H::Access>> {
    let (events, receiver) = mpsc::unbounded();
    let work = async move {
        let result = run(&scope, &call, control, &events).await;
        let _ = events.unbounded_send(ExecutionEvent::Finished(result));
    };
    // The work future sends its events through the channel; polling both
    // together delivers them in order and ends after `Finished`.
    stream::select(
        receiver,
        stream::once(work).filter_map(|()| future::ready(None)),
    )
    .boxed()
}

async fn run<H: AgentHost>(
    scope: &CallScope<H>,
    call: &ToolCall,
    control: CallControl,
    events: &mpsc::UnboundedSender<ExecutionEvent<H::Access>>,
) -> Result<TypedToolOutput, ToolError> {
    let send = |event| {
        let _ = events.unbounded_send(event);
    };
    let (tool, args) = match scope.tools.resolve(call)? {
        Resolved::Search { query } => {
            // The runtime's own read-only built-in needs no authorization.
            send(ExecutionEvent::Resolved(SEARCH_TOOLS.into()));
            send(ExecutionEvent::Started);
            return scope.tools.search(&query);
        }
        Resolved::Tool { tool, args } => (tool, args),
    };
    let name = tool.descriptor.name().to_owned();
    send(ExecutionEvent::Resolved(name.clone()));
    let prepared = tool.prepare(args.clone(), &scope.host)?;
    let decision = scope.host.authorize(&CallAuthorization {
        tool: &tool.descriptor,
        arguments: &args,
        access: prepared.access(),
        origin: &scope.origin,
        permissions: &scope.permissions,
        grants: &scope.grants,
    });
    match decision {
        Decision::Allow => {}
        Decision::Deny(reason) => {
            return Err(ToolError::new(ToolErrorKind::PermissionDenied, reason));
        }
        Decision::Ask => {
            let (respond, decision) = oneshot::channel();
            send(ExecutionEvent::AwaitingApproval {
                access: prepared.access().clone(),
                respond,
            });
            match decision.await {
                Ok(ApprovalDecision::AllowOnce | ApprovalDecision::AllowForRun) => {}
                Ok(ApprovalDecision::Deny(reason)) => {
                    return Err(ToolError::new(ToolErrorKind::PermissionDenied, reason));
                }
                Err(_) => {
                    return Err(ToolError::new(
                        ToolErrorKind::Cancelled,
                        "the approval request was abandoned",
                    ));
                }
            }
        }
    }
    send(ExecutionEvent::Started);
    let mut stream = prepared.execute(call.id.clone(), control).await;
    let mut result = None;
    while let Some(item) = stream.next().await {
        match item {
            _ if result.is_some() => {
                return Err(ToolError::new(
                    ToolErrorKind::InvalidStream,
                    "event after final tool result",
                ));
            }
            ToolStreamItem::Progress(progress) => send(ExecutionEvent::Progress(progress)),
            ToolStreamItem::Finished(value) => result = Some(value),
        }
    }
    let output = result.unwrap_or_else(|| {
        Err(ToolError::new(
            ToolErrorKind::InvalidStream,
            "tool stream ended without a final result",
        ))
    })?;
    if output.tool_name != name || output.model_output.is_empty() {
        return Err(ToolError::new(
            ToolErrorKind::InvalidOutput,
            "tool output identity or content is invalid",
        ));
    }
    Ok(output)
}
