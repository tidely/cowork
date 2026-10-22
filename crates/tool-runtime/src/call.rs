use std::sync::Arc;

use language_model::ToolCallId;
use tokio_util::sync::CancellationToken;

/// Cooperative controls for one call. Cancellation never promises rollback or
/// that external work stopped; tools choose whether they observe either token.
#[derive(Clone, Debug, Default)]
pub struct CallControl {
    /// The run is ending; stop as soon as it is safe.
    pub cancellation: CancellationToken,
    /// New input arrived that should wake a blocking wait, without stopping
    /// whatever the wait is observing.
    pub yield_wait: CancellationToken,
}

/// Runtime-owned facts about one invocation, beside the tool's own context.
///
/// `access` is exactly what authorization allowed: the value the tool's
/// `access` returned for these arguments when the call was prepared. A tool
/// can rely on it rather than recomputing what it may do.
pub struct ToolCall<A> {
    pub id: ToolCallId,
    pub access: A,
    pub control: CallControl,
}

/// A host context that can hand a tool the services it declares as its
/// `Tool::Context`. Registering a tool requires this, so a tool's dependencies
/// are checked when it is registered rather than looked up on every call.
/// Tools that need nothing use `()`, which every context provides.
pub trait Provides<T: ?Sized> {
    fn provide(self: &Arc<Self>) -> Arc<T>;
}

impl<C: ?Sized> Provides<()> for C {
    fn provide(self: &Arc<Self>) -> Arc<()> {
        Arc::new(())
    }
}
