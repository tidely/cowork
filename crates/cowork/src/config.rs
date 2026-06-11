use std::time::Duration;

pub const MODEL: &str = "gemma4:12b-it-qat";
pub(crate) const AGENT_MAX_TURNS: usize = 1000;

/// Backoffs between successive retries, slowest last. We make one more attempt
/// than there are backoffs — the final attempt has nothing waiting after it — so
/// a failure on 1-based attempt `n` waits `PROMPT_RETRY_BACKOFFS[n - 1]`, and a
/// failure on the last attempt finds no entry and gives up.
pub(crate) const PROMPT_RETRY_BACKOFFS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
pub(crate) const PROMPT_RETRY_ATTEMPTS: usize = PROMPT_RETRY_BACKOFFS.len() + 1;

pub const MAIN_AGENT_PREAMBLE: &str = include_str!("../prompts/main-agent.md");
