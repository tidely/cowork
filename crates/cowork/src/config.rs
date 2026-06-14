use std::time::Duration;

/// Delay before each prompt attempt. The first attempt starts immediately;
/// later entries are retry backoffs. A failure on the final entry gives up.
pub(crate) const PROMPT_RETRY_DELAYS: [Duration; 5] = [
    Duration::ZERO,
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

pub const MAIN_AGENT_PREAMBLE: &str = include_str!("../prompts/main-agent.md");
