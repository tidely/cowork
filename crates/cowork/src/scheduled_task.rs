//! A scheduled task: its settings, when it is due, which of its occurrences
//! run or are skipped, and what happened on its runs. Pure data, so the
//! policy in `docs/schedule.md` is tested here without an app; running the
//! queue is `scheduler.rs`.
//!
//! Times are local wall-clock times (`NaiveDateTime` from `Local::now()`),
//! so a daily 09:00 task stays at 09:00 across daylight saving changes.

use std::path::{Path, PathBuf};

use chrono::{
    Datelike as _, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeZone as _, Weekday,
};
use rrule::{RRule, RRuleSet, Tz, Unvalidated};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{models::ModelRef, project_mode::ProjectMode};

/// How late an occurrence may be noticed and still run as scheduled. The
/// scheduler looks every few seconds while the app runs, so anything older
/// was due while the app was closed or the computer asleep: a missed run.
pub(crate) const MISSED_AFTER: Duration = Duration::minutes(2);

/// Runs kept in a task's history, oldest dropped first.
pub(crate) const MAX_HISTORY: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum IntervalUnit {
    Minutes,
    Hours,
    Days,
}

impl IntervalUnit {
    fn seconds(self) -> i64 {
        match self {
            Self::Minutes => 60,
            Self::Hours => 60 * 60,
            Self::Days => 24 * 60 * 60,
        }
    }

    pub(crate) fn name(self, count: u32) -> &'static str {
        match (self, count == 1) {
            (Self::Minutes, true) => "minute",
            (Self::Minutes, false) => "minutes",
            (Self::Hours, true) => "hour",
            (Self::Hours, false) => "hours",
            (Self::Days, true) => "day",
            (Self::Days, false) => "days",
        }
    }
}

/// When a task runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum When {
    /// Every `every` units, counted from `start`, not from when the last run
    /// finished, so a slow run doesn't push the later ones back.
    Interval {
        every: u32,
        unit: IntervalUnit,
        start: NaiveDateTime,
    },
    Once(NaiveDateTime),
    Daily(NaiveTime),
    Weekdays(NaiveTime),
    /// On each of `days`, kept in week order from Monday.
    Weekly {
        days: Vec<Weekday>,
        time: NaiveTime,
    },
    /// On day `day` of each month. Months without that day are skipped, as
    /// RFC 5545 does.
    Monthly {
        day: u32,
        time: NaiveTime,
    },
    /// An RFC 5545 `RRULE`, for what the presets can't express. `start` is
    /// its `DTSTART`: when the rule was written, which gives the dates and
    /// times of day the rule leaves out.
    Custom {
        rule: String,
        start: NaiveDateTime,
    },
}

/// The occurrences of a schedule within a span of time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Occurrences {
    pub(crate) count: u64,
    pub(crate) latest: Option<NaiveDateTime>,
}

impl Occurrences {
    fn add(&mut self, at: NaiveDateTime) {
        self.count += 1;
        self.latest = Some(at);
    }
}

/// `rule` as `rrule` reads it, counted from `start`. Accepts the rule with
/// or without its `RRULE:` prefix.
fn custom_rule(rule: &str, start: NaiveDateTime) -> Result<RRuleSet, String> {
    let rule = rule.trim();
    let rule = rule.strip_prefix("RRULE:").unwrap_or(rule);
    if rule.is_empty() {
        return Err("Enter a recurrence rule.".into());
    }
    let start = Tz::LOCAL
        .from_local_datetime(&start)
        .earliest()
        .ok_or("The rule's start time doesn't exist in this time zone.")?;
    rule.parse::<RRule<Unvalidated>>()
        .map_err(|error| error.to_string())?
        .build(start)
        .map(RRuleSet::limit)
        .map_err(|error| error.to_string())
}

impl When {
    /// Why the schedule can't run, if it can't.
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self {
            Self::Interval { every: 0, .. } => Err("Enter a whole number above zero.".into()),
            Self::Weekly { days, .. } if days.is_empty() => Err("Pick at least one day.".into()),
            Self::Monthly { day, .. } if !(1..=31).contains(day) => {
                Err("Enter a day from 1 to 31.".into())
            }
            Self::Custom { rule, start } => custom_rule(rule, *start).map(|_| ()),
            _ => Ok(()),
        }
    }

    /// The schedule in a few words, as the list shows it.
    pub(crate) fn describe(&self) -> String {
        let time = |time: &NaiveTime| time.format("%H:%M").to_string();
        match self {
            Self::Interval { every: 1, unit, .. } => format!("Every {}", unit.name(1)),
            Self::Interval { every, unit, .. } => format!("Every {every} {}", unit.name(*every)),
            Self::Once(at) => format!("Once, {} at {}", at.format("%b %-d, %Y"), time(&at.time())),
            Self::Daily(at) => format!("Every day at {}", time(at)),
            Self::Weekdays(at) => format!("Weekdays at {}", time(at)),
            Self::Weekly { days, time: at } => {
                let days = days
                    .iter()
                    .map(Weekday::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Every {days} at {}", time(at))
            }
            Self::Monthly { day, time: at } => {
                format!("Monthly on the {} at {}", ordinal(*day), time(at))
            }
            Self::Custom { rule, .. } => {
                let rule = rule.trim();
                format!("Custom: {}", rule.strip_prefix("RRULE:").unwrap_or(rule))
            }
        }
    }

    /// The first time after `after` the task is due, if it ever is again.
    pub(crate) fn next_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let first_day_after = |time: NaiveTime, runs_on: &dyn Fn(NaiveDate) -> bool| {
            after
                .date()
                .iter_days()
                .take(8)
                .filter(|date| runs_on(*date))
                .map(|date| date.and_time(time))
                .find(|at| *at > after)
        };
        match self {
            Self::Interval { every, unit, start } => {
                let period = i64::from((*every).max(1)) * unit.seconds();
                if *start > after {
                    return Some(*start);
                }
                let periods = (after - *start).num_seconds() / period + 1;
                Some(*start + Duration::seconds(periods * period))
            }
            Self::Once(at) => (*at > after).then_some(*at),
            Self::Daily(time) => first_day_after(*time, &|_| true),
            Self::Weekdays(time) => first_day_after(*time, &|date| {
                !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
            }),
            Self::Weekly { days, time } => {
                first_day_after(*time, &|date| days.contains(&date.weekday()))
            }
            Self::Monthly { day, time } => {
                let month = after.date().with_day(1)?;
                (0..=48)
                    .filter_map(|ahead| month.checked_add_months(Months::new(ahead)))
                    .filter_map(|month| month.with_day(*day))
                    .map(|date| date.and_time(*time))
                    .find(|at| *at > after)
            }
            Self::Custom { rule, start } => {
                let rule = custom_rule(rule, *start).ok()?;
                (&rule)
                    .into_iter()
                    .map(|at| at.naive_local())
                    .find(|at| *at > after)
            }
        }
    }

    /// The occurrences after `after`, up to and including `until`.
    pub(crate) fn occurrences(&self, after: NaiveDateTime, until: NaiveDateTime) -> Occurrences {
        let mut found = Occurrences::default();
        if until <= after {
            return found;
        }
        match self {
            // Counted, as a long sleep may span very many short periods.
            Self::Interval { every, unit, start } => {
                let period = i64::from((*every).max(1)) * unit.seconds();
                if until < *start {
                    return found;
                }
                let first = if after < *start {
                    0
                } else {
                    (after - *start).num_seconds() / period + 1
                };
                let last = (until - *start).num_seconds() / period;
                if last >= first {
                    found.count = (last - first + 1) as u64;
                    found.latest = Some(*start + Duration::seconds(last * period));
                }
            }
            // One pass, as each lookup would start over from `start`.
            Self::Custom { rule, start } => {
                let Ok(rule) = custom_rule(rule, *start) else {
                    return found;
                };
                (&rule)
                    .into_iter()
                    .map(|at| at.naive_local())
                    .skip_while(|at| *at <= after)
                    .take_while(|at| *at <= until)
                    .for_each(|at| found.add(at));
            }
            // At most one a day, so walking them is cheap.
            _ => {
                let mut at = after;
                while let Some(next) = self.next_after(at).filter(|next| *next <= until) {
                    found.add(next);
                    at = next;
                }
            }
        }
        found
    }
}

pub(crate) fn ordinal(day: u32) -> String {
    let suffix = match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{day}{suffix}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ThreadMode {
    /// Every run starts a fresh thread containing only the prompt.
    NewEachRun,
    /// Every run submits its prompt to the thread the first run created.
    ContinueLast,
}

impl ThreadMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NewEachRun => "New thread each run",
            Self::ContinueLast => "Continues one thread",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MissedRuns {
    /// One catch-up run for the most recent missed occurrence.
    RunOnce,
    Skip,
}

/// What the user sets in a task's dialog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskSettings {
    pub(crate) title: String,
    pub(crate) prompt: String,
    pub(crate) when: When,
    pub(crate) model: ModelRef,
    pub(crate) thread: ThreadMode,
    pub(crate) missed: MissedRuns,
    /// The folders on this machine each run's thread has as its project, in
    /// order. Every run gives its thread exactly these, so an edit applies
    /// from the next run, like the model.
    #[serde(default)]
    pub(crate) folders: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) project_mode: ProjectMode,
}

impl TaskSettings {
    /// A folder of the project that is gone, which keeps a run from
    /// starting.
    pub(crate) fn missing_folder(&self) -> Option<&Path> {
        self.folders
            .iter()
            .map(PathBuf::as_path)
            .find(|folder| !folder.is_dir())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Trigger {
    Scheduled,
    /// For the most recent occurrence missed while the app was closed or
    /// the computer asleep.
    CatchUp,
    /// Run now, outside the schedule.
    Manual,
}

/// A run waiting its turn in the queue. Waiting isn't missing, however
/// long it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QueuedRun {
    pub(crate) scheduled_for: NaiveDateTime,
    pub(crate) trigger: Trigger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SkipReason {
    /// Due while the app was closed or the computer asleep.
    Missed,
    /// Due while the task already had a run queued.
    AlreadyQueued,
    /// Queued when the task was paused.
    Paused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RunResult {
    Running,
    Completed,
    Stopped,
    Failed(String),
    /// Cowork quit during the run.
    Interrupted,
    /// `count` occurrences didn't run, the latest being the record's.
    Skipped {
        count: u64,
        reason: SkipReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub(crate) id: Uuid,
    pub(crate) scheduled_for: NaiveDateTime,
    pub(crate) started_at: Option<NaiveDateTime>,
    pub(crate) trigger: Trigger,
    pub(crate) result: RunResult,
    pub(crate) thread_id: Option<Uuid>,
}

impl RunRecord {
    fn skipped(
        scheduled_for: NaiveDateTime,
        trigger: Trigger,
        count: u64,
        reason: SkipReason,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            scheduled_for,
            started_at: None,
            trigger,
            result: RunResult::Skipped { count, reason },
            thread_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScheduledTask {
    pub(crate) id: Uuid,
    settings: TaskSettings,
    active: bool,
    /// Every occurrence up to this time has been handled: queued, run, or
    /// recorded as skipped. Saved, so occurrences due while the app was
    /// closed are found when it starts.
    checked_until: NaiveDateTime,
    /// The thread **continue the last thread** runs submit to.
    pub(crate) thread_id: Option<Uuid>,
    queued: Option<QueuedRun>,
    /// Oldest first.
    history: Vec<RunRecord>,
    /// The next occurrence after `checked_until`, kept so that neither each
    /// look at the clock nor each frame evaluates the schedule.
    #[serde(skip)]
    next_due: Option<NaiveDateTime>,
}

impl ScheduledTask {
    /// A task that runs from `now` on: earlier occurrences aren't missed.
    pub(crate) fn new(settings: TaskSettings, now: NaiveDateTime) -> Self {
        let mut task = Self {
            id: Uuid::new_v4(),
            settings,
            active: true,
            checked_until: now,
            thread_id: None,
            queued: None,
            history: Vec::new(),
            next_due: None,
        };
        task.refresh();
        task
    }

    /// Finishes a task read from disk: a run that was going on when Cowork
    /// quit didn't finish.
    pub(crate) fn loaded(mut self) -> Self {
        for record in &mut self.history {
            if record.result == RunResult::Running {
                record.result = RunResult::Interrupted;
            }
        }
        self.refresh();
        self
    }

    fn refresh(&mut self) {
        self.next_due = self.settings.when.next_after(self.checked_until);
    }

    pub(crate) fn settings(&self) -> &TaskSettings {
        &self.settings
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn queued(&self) -> Option<QueuedRun> {
        self.queued
    }

    pub(crate) fn history(&self) -> &[RunRecord] {
        &self.history
    }

    /// When the next occurrence is due. It may have passed if the scheduler
    /// hasn't looked yet. `None` for a paused task, or one with no more runs.
    pub(crate) fn next_due(&self) -> Option<NaiveDateTime> {
        self.next_due.filter(|_| self.active)
    }

    /// Applies edited settings from their next occurrence on. A changed
    /// schedule only counts from `now`, so editing never finds missed runs.
    pub(crate) fn set_settings(&mut self, settings: TaskSettings, now: NaiveDateTime) {
        if settings.when != self.settings.when {
            self.checked_until = now;
        }
        self.settings = settings;
        self.refresh();
    }

    /// Pauses or resumes. Occurrences passing while paused aren't missed
    /// runs, so they never run; a run already queued is dropped.
    pub(crate) fn set_active(&mut self, active: bool, now: NaiveDateTime) {
        if active == self.active {
            return;
        }
        self.active = active;
        if active {
            self.checked_until = now;
            self.refresh();
        } else if let Some(queued) = self.queued.take() {
            self.record(RunRecord::skipped(
                queued.scheduled_for,
                queued.trigger,
                1,
                SkipReason::Paused,
            ));
        }
    }

    /// Queues a run now, outside the schedule, unless one is queued already.
    pub(crate) fn queue_now(&mut self, now: NaiveDateTime) -> bool {
        if self.queued.is_some() {
            return false;
        }
        self.queued = Some(QueuedRun {
            scheduled_for: now,
            trigger: Trigger::Manual,
        });
        true
    }

    /// Handles the occurrences due by `now`, and returns whether anything
    /// changed. An occurrence noticed within [`MISSED_AFTER`] is queued as
    /// scheduled, and earlier missed ones aren't caught up as well, since a
    /// run happens now anyway. Otherwise **run once when available** queues
    /// a catch-up for the latest missed occurrence. Either way the rest are
    /// recorded as skipped, and a task keeps at most one run queued: one due
    /// while another waits is skipped.
    pub(crate) fn advance(&mut self, now: NaiveDateTime) -> bool {
        if !self.active || self.next_due.is_none_or(|due| due > now) {
            return false;
        }
        let cutoff = now - MISSED_AFTER;
        let when = &self.settings.when;
        let missed = when.occurrences(self.checked_until, cutoff.min(now));
        let recent = when.occurrences(self.checked_until.max(cutoff), now);
        self.checked_until = now;
        self.refresh();

        let (run, unrun_missed) = match (recent.latest, self.settings.missed) {
            (Some(at), _) => (Some((at, Trigger::Scheduled)), missed.count),
            (None, MissedRuns::RunOnce) => (
                missed.latest.map(|at| (at, Trigger::CatchUp)),
                missed.count.saturating_sub(1),
            ),
            (None, MissedRuns::Skip) => (None, missed.count),
        };
        if unrun_missed > 0
            && let Some(latest) = missed.latest
        {
            self.record(RunRecord::skipped(
                latest,
                Trigger::Scheduled,
                unrun_missed,
                SkipReason::Missed,
            ));
        }
        // Ticks are seconds apart, so only very short intervals see more
        // than one recent occurrence at once.
        let superseded = recent.count.saturating_sub(1);
        if let Some((at, trigger)) = run {
            if self.queued.is_some() {
                self.record(RunRecord::skipped(
                    at,
                    trigger,
                    superseded + 1,
                    SkipReason::AlreadyQueued,
                ));
            } else {
                if superseded > 0 {
                    self.record(RunRecord::skipped(
                        at,
                        trigger,
                        superseded,
                        SkipReason::AlreadyQueued,
                    ));
                }
                self.queued = Some(QueuedRun {
                    scheduled_for: at,
                    trigger,
                });
            }
        }
        true
    }

    /// Takes the queued run to start it.
    pub(crate) fn take_queued(&mut self) -> Option<QueuedRun> {
        self.queued.take()
    }

    pub(crate) fn record(&mut self, record: RunRecord) {
        self.history.push(record);
        let excess = self.history.len().saturating_sub(MAX_HISTORY);
        self.history.drain(..excess);
    }

    pub(crate) fn record_mut(&mut self, record_id: Uuid) -> Option<&mut RunRecord> {
        self.history
            .iter_mut()
            .find(|record| record.id == record_id)
    }

    /// Records `run` as started in thread `thread_id`, returning the
    /// record's id.
    pub(crate) fn start(&mut self, run: QueuedRun, now: NaiveDateTime, thread_id: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        self.record(RunRecord {
            id,
            scheduled_for: run.scheduled_for,
            started_at: Some(now),
            trigger: run.trigger,
            result: RunResult::Running,
            thread_id: Some(thread_id),
        });
        if self.settings.thread == ThreadMode::ContinueLast {
            self.thread_id = Some(thread_id);
        }
        id
    }

    /// Records `run` as failed before it could start.
    pub(crate) fn fail(&mut self, run: QueuedRun, now: NaiveDateTime, error: String) {
        self.record(RunRecord {
            id: Uuid::new_v4(),
            scheduled_for: run.scheduled_for,
            started_at: Some(now),
            trigger: run.trigger,
            result: RunResult::Failed(error),
            thread_id: None,
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::models::ModelProvider;

    pub(crate) fn at(date: (i32, u32, u32), hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(date.0, date.1, date.2)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    pub(crate) fn time(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    // 2026-10-10 is a Saturday.
    pub(crate) const SATURDAY: (i32, u32, u32) = (2026, 10, 10);

    pub(crate) fn settings(when: When, missed: MissedRuns) -> TaskSettings {
        TaskSettings {
            title: "Task".into(),
            prompt: "Do the thing".into(),
            when,
            model: ModelRef {
                provider: ModelProvider::Ollama,
                id: "test-other".into(),
            },
            thread: ThreadMode::NewEachRun,
            missed,
            folders: Vec::new(),
            project_mode: ProjectMode::Read,
        }
    }

    #[test]
    fn tasks_saved_before_projects_load_without_one() {
        let mut saved =
            serde_json::to_value(settings(When::Daily(time(9, 0)), MissedRuns::RunOnce)).unwrap();
        let fields = saved.as_object_mut().unwrap();
        fields.remove("folders");
        fields.remove("project_mode");
        let loaded: TaskSettings = serde_json::from_value(saved).unwrap();
        assert!(loaded.folders.is_empty());
        assert_eq!(loaded.project_mode, ProjectMode::Read);
        assert_eq!(loaded.missing_folder(), None);

        let gone = TaskSettings {
            folders: vec![
                std::env::temp_dir(),
                PathBuf::from("/no/such/cowork/folder"),
            ],
            ..loaded
        };
        assert_eq!(
            gone.missing_folder(),
            Some(Path::new("/no/such/cowork/folder"))
        );
    }

    #[test]
    fn schedules_are_described_in_a_few_words() {
        let cases = [
            (
                When::Interval {
                    every: 1,
                    unit: IntervalUnit::Hours,
                    start: at(SATURDAY, 8, 0),
                },
                "Every hour",
            ),
            (
                When::Interval {
                    every: 30,
                    unit: IntervalUnit::Minutes,
                    start: at(SATURDAY, 8, 0),
                },
                "Every 30 minutes",
            ),
            (
                When::Once(at((2026, 10, 12), 9, 5)),
                "Once, Oct 12, 2026 at 09:05",
            ),
            (When::Daily(time(9, 0)), "Every day at 09:00"),
            (When::Weekdays(time(9, 0)), "Weekdays at 09:00"),
            (
                When::Weekly {
                    days: vec![Weekday::Mon, Weekday::Fri],
                    time: time(16, 30),
                },
                "Every Mon, Fri at 16:30",
            ),
            (
                When::Monthly {
                    day: 22,
                    time: time(10, 0),
                },
                "Monthly on the 22nd at 10:00",
            ),
            (
                When::Custom {
                    rule: "RRULE:FREQ=YEARLY".into(),
                    start: at(SATURDAY, 9, 0),
                },
                "Custom: FREQ=YEARLY",
            ),
        ];
        for (when, described) in cases {
            assert_eq!(when.describe(), described);
        }
        assert_eq!(
            [1, 2, 3, 4, 11, 12, 13, 21, 31].map(ordinal),
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "31st"
            ]
        );
    }

    #[test]
    fn the_next_run_follows_each_kind_of_schedule() {
        let now = at(SATURDAY, 10, 0);
        assert_eq!(
            When::Daily(time(9, 0)).next_after(now),
            Some(at((2026, 10, 11), 9, 0))
        );
        assert_eq!(
            When::Daily(time(11, 0)).next_after(now),
            Some(at(SATURDAY, 11, 0))
        );
        // Over the weekend to Monday.
        assert_eq!(
            When::Weekdays(time(9, 0)).next_after(now),
            Some(at((2026, 10, 12), 9, 0))
        );
        // Later today, or a week from now when today's time has passed.
        let saturdays = |hour| When::Weekly {
            days: vec![Weekday::Sat],
            time: time(hour, 0),
        };
        assert_eq!(saturdays(12).next_after(now), Some(at(SATURDAY, 12, 0)));
        assert_eq!(saturdays(9).next_after(now), Some(at((2026, 10, 17), 9, 0)));
        assert_eq!(When::Once(at(SATURDAY, 9, 0)).next_after(now), None);
        assert_eq!(
            When::Once(at(SATURDAY, 11, 0)).next_after(now),
            Some(at(SATURDAY, 11, 0))
        );
    }

    #[test]
    fn intervals_count_from_their_start() {
        let every_45_minutes = When::Interval {
            every: 45,
            unit: IntervalUnit::Minutes,
            start: at(SATURDAY, 8, 0),
        };
        // 08:00, 08:45, 09:30, 10:15.
        assert_eq!(
            every_45_minutes.next_after(at(SATURDAY, 10, 0)),
            Some(at(SATURDAY, 10, 15))
        );
        // Exactly on an occurrence, the next one.
        assert_eq!(
            every_45_minutes.next_after(at(SATURDAY, 9, 30)),
            Some(at(SATURDAY, 10, 15))
        );
        // Not started yet.
        assert_eq!(
            every_45_minutes.next_after(at(SATURDAY, 7, 0)),
            Some(at(SATURDAY, 8, 0))
        );
        assert_eq!(
            every_45_minutes.occurrences(at(SATURDAY, 7, 0), at(SATURDAY, 10, 0)),
            Occurrences {
                count: 3,
                latest: Some(at(SATURDAY, 9, 30))
            }
        );
        // Counted rather than walked, across a year of minutes.
        let every_minute = When::Interval {
            every: 1,
            unit: IntervalUnit::Minutes,
            start: at(SATURDAY, 0, 0),
        };
        let found = every_minute.occurrences(at(SATURDAY, 0, 0), at((2027, 10, 10), 0, 0));
        assert_eq!(found.count, 365 * 24 * 60);
        assert_eq!(found.latest, Some(at((2027, 10, 10), 0, 0)));
    }

    #[test]
    fn months_without_the_day_are_skipped() {
        let on_the_31st = When::Monthly {
            day: 31,
            time: time(9, 0),
        };
        // November has 30 days.
        assert_eq!(
            on_the_31st.next_after(at((2026, 10, 31), 10, 0)),
            Some(at((2026, 12, 31), 9, 0))
        );
        let on_the_29th = When::Monthly {
            day: 29,
            time: time(9, 0),
        };
        // 2027 isn't a leap year, so February is skipped.
        assert_eq!(
            on_the_29th.next_after(at((2027, 1, 30), 9, 0)),
            Some(at((2027, 3, 29), 9, 0))
        );
    }

    #[test]
    fn custom_rules_follow_rfc_5545() {
        let first_monday = When::Custom {
            rule: "RRULE:FREQ=MONTHLY;BYDAY=1MO;BYHOUR=9;BYMINUTE=0;BYSECOND=0".into(),
            start: at(SATURDAY, 10, 0),
        };
        assert_eq!(first_monday.validate(), Ok(()));
        assert_eq!(
            first_monday.next_after(at(SATURDAY, 10, 0)),
            Some(at((2026, 11, 2), 9, 0))
        );
        assert_eq!(
            first_monday.occurrences(at(SATURDAY, 10, 0), at((2027, 1, 31), 0, 0)),
            Occurrences {
                count: 3,
                latest: Some(at((2027, 1, 4), 9, 0))
            }
        );
        // Without a time, the rule's start gives it; COUNT ends it.
        let three_days = When::Custom {
            rule: "FREQ=DAILY;COUNT=3".into(),
            start: at(SATURDAY, 7, 30),
        };
        assert_eq!(
            three_days.next_after(at(SATURDAY, 8, 0)),
            Some(at((2026, 10, 11), 7, 30))
        );
        assert_eq!(three_days.next_after(at((2026, 10, 12), 8, 0)), None);

        for rule in ["", "FREQ=SOMETIMES", "BYHOUR=9"] {
            let when = When::Custom {
                rule: rule.into(),
                start: at(SATURDAY, 10, 0),
            };
            assert!(when.validate().is_err(), "{rule:?} should be refused");
        }
    }

    #[test]
    fn an_on_time_occurrence_is_queued_as_scheduled() {
        let mut task = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at(SATURDAY, 8, 0),
        );
        assert_eq!(task.next_due(), Some(at(SATURDAY, 9, 0)));
        assert!(!task.advance(at(SATURDAY, 8, 59)));

        assert!(task.advance(at(SATURDAY, 9, 0)));
        assert_eq!(
            task.queued(),
            Some(QueuedRun {
                scheduled_for: at(SATURDAY, 9, 0),
                trigger: Trigger::Scheduled
            })
        );
        assert!(task.history().is_empty());
        assert_eq!(task.next_due(), Some(at((2026, 10, 11), 9, 0)));
    }

    #[test]
    fn missed_runs_are_caught_up_once_or_skipped() {
        // Closed from Saturday morning until Tuesday at noon.
        let reopened = at((2026, 10, 13), 12, 0);
        let mut catch_up = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at(SATURDAY, 8, 0),
        );
        assert!(catch_up.advance(reopened));
        assert_eq!(
            catch_up.queued(),
            Some(QueuedRun {
                scheduled_for: at((2026, 10, 13), 9, 0),
                trigger: Trigger::CatchUp
            })
        );
        assert_eq!(
            catch_up.history()[0].result,
            RunResult::Skipped {
                count: 3,
                reason: SkipReason::Missed
            }
        );

        let mut skip = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::Skip),
            at(SATURDAY, 8, 0),
        );
        assert!(skip.advance(reopened));
        assert_eq!(skip.queued(), None);
        assert_eq!(
            skip.history()[0].result,
            RunResult::Skipped {
                count: 4,
                reason: SkipReason::Missed
            }
        );
        // There is no maximum age.
        let mut long_ago = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at((2025, 1, 1), 8, 0),
        );
        assert!(long_ago.advance(reopened));
        assert_eq!(
            long_ago.queued().map(|run| run.trigger),
            Some(Trigger::CatchUp)
        );
    }

    #[test]
    fn a_run_due_on_reopening_replaces_the_catch_up() {
        let mut task = ScheduledTask::new(
            settings(
                When::Interval {
                    every: 1,
                    unit: IntervalUnit::Hours,
                    start: at(SATURDAY, 0, 0),
                },
                MissedRuns::RunOnce,
            ),
            at(SATURDAY, 8, 30),
        );
        // Asleep from 08:30, woken a minute after noon.
        assert!(task.advance(at(SATURDAY, 12, 1)));
        assert_eq!(
            task.queued(),
            Some(QueuedRun {
                scheduled_for: at(SATURDAY, 12, 0),
                trigger: Trigger::Scheduled
            })
        );
        assert_eq!(
            task.history()[0].result,
            RunResult::Skipped {
                count: 3,
                reason: SkipReason::Missed
            }
        );
    }

    #[test]
    fn a_task_keeps_one_run_queued() {
        let mut task = ScheduledTask::new(
            settings(
                When::Interval {
                    every: 1,
                    unit: IntervalUnit::Minutes,
                    start: at(SATURDAY, 9, 0),
                },
                MissedRuns::RunOnce,
            ),
            at(SATURDAY, 8, 59),
        );
        assert!(task.advance(at(SATURDAY, 9, 0)));
        assert!(task.advance(at(SATURDAY, 9, 1)));
        assert_eq!(
            task.queued().map(|run| run.scheduled_for),
            Some(at(SATURDAY, 9, 0))
        );
        assert_eq!(
            task.history()[0].result,
            RunResult::Skipped {
                count: 1,
                reason: SkipReason::AlreadyQueued
            }
        );
        assert!(!task.queue_now(at(SATURDAY, 9, 1)));
    }

    #[test]
    fn paused_time_is_not_missed() {
        let mut task = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at(SATURDAY, 8, 0),
        );
        assert!(task.queue_now(at(SATURDAY, 8, 0)));
        task.set_active(false, at(SATURDAY, 8, 1));
        assert_eq!(task.queued(), None);
        assert_eq!(
            task.history()[0].result,
            RunResult::Skipped {
                count: 1,
                reason: SkipReason::Paused
            }
        );
        assert_eq!(task.next_due(), None);
        assert!(!task.advance(at((2026, 10, 12), 12, 0)));

        task.set_active(true, at((2026, 10, 12), 12, 0));
        assert_eq!(task.next_due(), Some(at((2026, 10, 13), 9, 0)));
        assert!(!task.advance(at((2026, 10, 12), 12, 0)));
        assert_eq!(task.history().len(), 1);
    }

    #[test]
    fn a_changed_schedule_counts_from_the_edit() {
        let mut task = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at(SATURDAY, 8, 0),
        );
        let mut edited = task.settings().clone();
        edited.when = When::Daily(time(7, 0));
        task.set_settings(edited, at((2026, 10, 12), 8, 0));
        assert_eq!(task.next_due(), Some(at((2026, 10, 13), 7, 0)));
        assert!(!task.advance(at((2026, 10, 12), 8, 0)));
        assert!(task.history().is_empty());
    }

    #[test]
    fn history_keeps_the_latest_runs_and_survives_a_restart() {
        let mut task = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at(SATURDAY, 8, 0),
        );
        for _ in 0..MAX_HISTORY + 5 {
            task.queue_now(at(SATURDAY, 8, 0));
            let run = task.take_queued().unwrap();
            task.start(run, at(SATURDAY, 8, 0), Uuid::new_v4());
        }
        assert_eq!(task.history().len(), MAX_HISTORY);

        let saved = serde_json::to_string(&task).unwrap();
        let loaded = serde_json::from_str::<ScheduledTask>(&saved)
            .unwrap()
            .loaded();
        assert!(
            loaded
                .history()
                .iter()
                .all(|record| record.result == RunResult::Interrupted)
        );
        assert_eq!(loaded.next_due(), task.next_due());
    }
}
