//! Every encoding between app values and SQLite columns, in one place:
//! checked integer conversions (sqlx can't bind `u64`, and `as` would wrap),
//! durations as nanoseconds, timestamps as `DateTime<Utc>`, and enums as
//! the snake_case names the schema's CHECKs list, by explicit `match`
//! rather than serde (`ModelProvider` and `ProjectMode` serialize
//! unrenamed).

use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use chrono::{DateTime, Utc};

use crate::{
    models::ModelProvider,
    project_mode::ProjectMode,
    protocol,
    scheduled_task::{MissedRuns, RunResult, SkipReason, ThreadMode, Trigger},
};

pub(super) fn int(value: u64, what: &'static str) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{what} out of range"))
}

pub(super) fn index(value: usize, what: &'static str) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{what} out of range"))
}

pub(super) fn uint(value: i64, what: &'static str) -> anyhow::Result<u64> {
    u64::try_from(value).with_context(|| format!("invalid {what}"))
}

pub(super) fn count(value: i64, what: &'static str) -> anyhow::Result<usize> {
    usize::try_from(value).with_context(|| format!("invalid {what}"))
}

pub(super) fn nanos(duration: Duration, what: &'static str) -> anyhow::Result<i64> {
    i64::try_from(duration.as_nanos()).with_context(|| format!("{what} out of range"))
}

pub(super) fn duration(nanos: i64, what: &'static str) -> anyhow::Result<Duration> {
    Ok(Duration::from_nanos(uint(nanos, what)?))
}

pub(super) fn time(time: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(time)
}

pub(super) fn system_time(time: DateTime<Utc>) -> SystemTime {
    SystemTime::from(time)
}

pub(super) fn provider(provider: ModelProvider) -> &'static str {
    match provider {
        ModelProvider::Ollama => "ollama",
        ModelProvider::Anthropic => "anthropic",
        ModelProvider::OpenAi => "open_ai",
    }
}

pub(super) fn to_provider(column: &str) -> anyhow::Result<ModelProvider> {
    Ok(match column {
        "ollama" => ModelProvider::Ollama,
        "anthropic" => ModelProvider::Anthropic,
        "open_ai" => ModelProvider::OpenAi,
        _ => anyhow::bail!("unknown model provider {column:?}"),
    })
}

pub(super) fn mode(mode: ProjectMode) -> &'static str {
    match mode {
        ProjectMode::Write => "write",
        ProjectMode::Read => "read",
        ProjectMode::Network => "network",
    }
}

pub(super) fn to_mode(column: &str) -> anyhow::Result<ProjectMode> {
    Ok(match column {
        "write" => ProjectMode::Write,
        "read" => ProjectMode::Read,
        "network" => ProjectMode::Network,
        _ => anyhow::bail!("unknown project mode {column:?}"),
    })
}

pub(super) fn attachment_kind(kind: protocol::AttachmentKind) -> &'static str {
    match kind {
        protocol::AttachmentKind::Text => "text",
        protocol::AttachmentKind::Png => "png",
        protocol::AttachmentKind::Jpeg => "jpeg",
    }
}

pub(super) fn to_attachment_kind(column: &str) -> anyhow::Result<protocol::AttachmentKind> {
    Ok(match column {
        "text" => protocol::AttachmentKind::Text,
        "png" => protocol::AttachmentKind::Png,
        "jpeg" => protocol::AttachmentKind::Jpeg,
        _ => anyhow::bail!("unknown attachment kind {column:?}"),
    })
}

/// The `outcome` and `error` columns.
pub(super) fn outcome(outcome: &protocol::RunOutcome) -> (&'static str, Option<&str>) {
    match outcome {
        protocol::RunOutcome::Completed => ("completed", None),
        protocol::RunOutcome::Stopped => ("stopped", None),
        protocol::RunOutcome::Failed(error) => ("failed", Some(error)),
    }
}

pub(super) fn to_outcome(
    column: &str,
    error: Option<String>,
) -> anyhow::Result<protocol::RunOutcome> {
    Ok(match (column, error) {
        ("completed", None) => protocol::RunOutcome::Completed,
        ("stopped", None) => protocol::RunOutcome::Stopped,
        ("failed", Some(error)) => protocol::RunOutcome::Failed(error),
        _ => anyhow::bail!("invalid run outcome {column:?}"),
    })
}

pub(super) fn thread_mode(mode: ThreadMode) -> &'static str {
    match mode {
        ThreadMode::NewEachRun => "new_each_run",
        ThreadMode::ContinueLast => "continue_last",
    }
}

pub(super) fn to_thread_mode(column: &str) -> anyhow::Result<ThreadMode> {
    Ok(match column {
        "new_each_run" => ThreadMode::NewEachRun,
        "continue_last" => ThreadMode::ContinueLast,
        _ => anyhow::bail!("unknown thread mode {column:?}"),
    })
}

pub(super) fn missed_runs(missed: MissedRuns) -> &'static str {
    match missed {
        MissedRuns::RunOnce => "run_once",
        MissedRuns::Skip => "skip",
    }
}

pub(super) fn to_missed_runs(column: &str) -> anyhow::Result<MissedRuns> {
    Ok(match column {
        "run_once" => MissedRuns::RunOnce,
        "skip" => MissedRuns::Skip,
        _ => anyhow::bail!("unknown missed runs {column:?}"),
    })
}

pub(super) fn trigger(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Scheduled => "scheduled",
        Trigger::CatchUp => "catch_up",
        Trigger::Manual => "manual",
    }
}

pub(super) fn to_trigger(column: &str) -> anyhow::Result<Trigger> {
    Ok(match column {
        "scheduled" => Trigger::Scheduled,
        "catch_up" => Trigger::CatchUp,
        "manual" => Trigger::Manual,
        _ => anyhow::bail!("unknown trigger {column:?}"),
    })
}

fn skip_reason(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::Missed => "missed",
        SkipReason::AlreadyQueued => "already_queued",
        SkipReason::Paused => "paused",
    }
}

fn to_skip_reason(column: &str) -> anyhow::Result<SkipReason> {
    Ok(match column {
        "missed" => SkipReason::Missed,
        "already_queued" => SkipReason::AlreadyQueued,
        "paused" => SkipReason::Paused,
        _ => anyhow::bail!("unknown skip reason {column:?}"),
    })
}

/// A scheduled run's `result`, `error`, `skipped_count` and `skip_reason`
/// columns.
pub(super) struct RunResultColumns<'a> {
    pub(super) result: &'static str,
    pub(super) error: Option<&'a str>,
    pub(super) skipped_count: Option<i64>,
    pub(super) skip_reason: Option<&'static str>,
}

pub(super) fn run_result(result: &RunResult) -> anyhow::Result<RunResultColumns<'_>> {
    let simple = |result| RunResultColumns {
        result,
        error: None,
        skipped_count: None,
        skip_reason: None,
    };
    Ok(match result {
        RunResult::Running => simple("running"),
        RunResult::Completed => simple("completed"),
        RunResult::Stopped => simple("stopped"),
        RunResult::Interrupted => simple("interrupted"),
        RunResult::Failed(error) => RunResultColumns {
            error: Some(error),
            ..simple("failed")
        },
        RunResult::Skipped { count, reason } => RunResultColumns {
            skipped_count: Some(int(*count, "skipped runs")?),
            skip_reason: Some(skip_reason(*reason)),
            ..simple("skipped")
        },
    })
}

pub(super) fn to_run_result(
    result: &str,
    error: Option<String>,
    skipped_count: Option<i64>,
    skip_reason: Option<&str>,
) -> anyhow::Result<RunResult> {
    Ok(match (result, error, skipped_count, skip_reason) {
        ("running", None, None, None) => RunResult::Running,
        ("completed", None, None, None) => RunResult::Completed,
        ("stopped", None, None, None) => RunResult::Stopped,
        ("interrupted", None, None, None) => RunResult::Interrupted,
        ("failed", Some(error), None, None) => RunResult::Failed(error),
        ("skipped", None, Some(count), Some(reason)) => RunResult::Skipped {
            count: uint(count, "skipped runs")?,
            reason: to_skip_reason(reason)?,
        },
        _ => anyhow::bail!("invalid run result {result:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_values_are_refused_not_wrapped() {
        assert!(int(u64::MAX, "x").is_err());
        assert!(uint(-1, "x").is_err());
        assert!(nanos(Duration::from_secs(u64::MAX), "x").is_err());
        let before_epoch = SystemTime::UNIX_EPOCH - Duration::new(3, 123_456_789);
        assert_eq!(system_time(time(before_epoch)), before_epoch);
    }
}
