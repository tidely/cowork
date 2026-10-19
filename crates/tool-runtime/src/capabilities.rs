use std::{num::NonZeroU32, time::Duration};

use serde::{Deserialize, Serialize};

/// A tool's execution declarations, separate from its model-facing schema.
/// Hosts use these to choose scheduling, cancellation, and permission policy;
/// the declarations do not grant access or enforce limits by themselves.
/// They describe the registered implementation across calls, rather than one
/// invocation's arguments or the caller's current permission grants.
/// Unknown side effects are treated as potentially mutating, and optional
/// limits left unspecified are governed by the host's policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapabilities {
    /// The implementation cooperates with the cancellation service supplied
    /// by its host through ToolCallContext. This does not promise rollback or
    /// that dropping the future stops external work. The host may stop waiting
    /// for any call, independently of cooperative cancellation support.
    #[serde(default)]
    pub supports_cancel: bool,

    /// The implementation may emit ToolProgress before its final result.
    #[serde(default)]
    pub supports_progress: bool,

    /// The tool declares that it does not mutate files or external state.
    /// This describes side effects, rather than whether the caller may access
    /// the data. Hosts must still authorize reads and validate paths/arguments.
    #[serde(default)]
    pub is_read_only: bool,

    /// Maximum simultaneous calls supported by this registered implementation.
    /// None declares no tool-specific limit; the host can impose a stricter one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<NonZeroU32>,

    /// Suggested call timeout. None leaves the timeout to the host. The
    /// runtime does not introduce a timeout or interrupt execution implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}
