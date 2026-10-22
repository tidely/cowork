use std::num::NonZeroUsize;

use crate::CompactionPolicy;

/// Generation controls that don't change the prompt, so changing them keeps
/// the prompt cache. `None` (or an empty `stop`) keeps the provider's default.
#[derive(Clone, Debug, PartialEq)]
pub struct Sampling {
    /// Defaults to 4096: some providers (Anthropic) require an explicit cap.
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub seed: Option<i64>,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            max_output_tokens: Some(4096),
            temperature: None,
            top_p: None,
            stop: Vec::new(),
            seed: None,
        }
    }
}

/// Session state that doesn't change the prompt prefix. A host seeds it from
/// any reusable value; `Configure` requests change one field at a time, so
/// changes from independent sources never undo each other.
#[derive(Clone, Debug)]
pub struct Configuration<P> {
    /// The host's live permission state. Model-relevant changes reach the
    /// model through the tool-policy context section.
    pub permissions: P,
    pub sampling: Sampling,
    /// Model requests one run may make before it fails.
    pub max_steps: NonZeroUsize,
    pub compaction: CompactionPolicy,
}

impl<P: Default> Default for Configuration<P> {
    fn default() -> Self {
        Self {
            permissions: P::default(),
            sampling: Sampling::default(),
            max_steps: NonZeroUsize::new(16).expect("positive"),
            compaction: CompactionPolicy::default(),
        }
    }
}

/// Replaces one configuration field. Applies to calls not yet authorized and
/// to the next model request.
#[derive(Clone, Debug)]
pub enum ConfigurationChange<P> {
    Permissions(P),
    Sampling(Sampling),
    MaxSteps(NonZeroUsize),
    Compaction(CompactionPolicy),
}

impl<P> Configuration<P> {
    pub fn apply(&mut self, change: ConfigurationChange<P>) {
        match change {
            ConfigurationChange::Permissions(permissions) => self.permissions = permissions,
            ConfigurationChange::Sampling(sampling) => self.sampling = sampling,
            ConfigurationChange::MaxSteps(limit) => self.max_steps = limit,
            ConfigurationChange::Compaction(policy) => self.compaction = policy,
        }
    }
}
