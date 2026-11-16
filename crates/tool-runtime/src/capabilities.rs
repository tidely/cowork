use std::{num::NonZeroU32, time::Duration};

use serde::{Deserialize, Serialize};

/// A tool's execution declarations, separate from its model-facing schema.
/// Hosts use these to choose scheduling, cancellation, and permission policy;
/// they do not grant access or enforce limits by themselves. They describe the
/// implementation across calls, not one invocation or the current grants.
/// Unknown side effects are treated as potentially mutating.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapabilities {
    /// The implementation cooperates with the cancellation service its host
    /// supplies through ToolCallContext. This does not promise rollback or that
    /// dropping the future stops external work; the host may stop waiting for
    /// any call regardless.
    #[serde(default)]
    pub supports_cancel: bool,

    /// The implementation may emit ToolProgress before its final result.
    #[serde(default)]
    pub supports_progress: bool,

    /// The tool declares that it does not mutate files or external state. This
    /// is about side effects, not access: hosts must still authorize reads.
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
