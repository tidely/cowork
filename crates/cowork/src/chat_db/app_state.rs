//! What Cowork saves besides chats, in the same database: projects,
//! settings, what statistics count of deleted chats, and scheduled tasks
//! (schema in `migrations/`). Each is read whole at
//! startup. Changes become a [`StateJob`] on the app's side, where the
//! values are, so only plain rows cross to the writer, which applies each in
//! a transaction of its own, in order with the chat saves around it.
//!
//! Projects and the deleted chats' counts change by steps (a project made
//! or removed, a chat's usage added); settings and tasks are small and
//! written whole.

use std::{collections::HashMap, path::PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::{Row, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{columns, rows::ActivityRow};
use crate::{
    context_settings::ContextSettings,
    models::ModelRef,
    projects::{ProjectId, ProjectRecord},
    saved_settings::Settings,
    scheduled_task::{QueuedRun, RunRecord, SavedTask, ScheduledTask, TaskSettings},
    statistics::Statistics,
};

/// Everything besides chats, as the database holds it at startup. Rows that
/// can't be read are left out, and logged.
#[derive(Default)]
pub(crate) struct AppState {
    /// Newest first.
    pub(crate) projects: Vec<ProjectRecord>,
    pub(crate) settings: Settings,
    pub(crate) deleted: Statistics,
    /// In the order they were created.
    pub(crate) tasks: Vec<ScheduledTask>,
}

/// A change to what is saved besides chats.
pub(super) enum StateJob {
    CreateProject(ProjectRow),
    RemoveProject(Uuid),
    Settings(SettingsRow),
    Tasks(Vec<TaskRow>),
    CountDeleted(UsageRow),
}

impl StateJob {
    /// What failed, for the log.
    pub(super) fn what(&self) -> &'static str {
        match self {
            Self::CreateProject(_) => "the new project",
            Self::RemoveProject(_) => "the project's removal",
            Self::Settings(_) => "settings",
            Self::Tasks(_) => "scheduled tasks",
            Self::CountDeleted(_) => "deleted chats' statistics",
        }
    }
}

fn path_column(path: &std::path::Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{} isn't UTF-8", path.display()))
}

fn path_columns(paths: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    paths.iter().map(|path| path_column(path)).collect()
}

pub(super) struct ProjectRow {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    folders: Vec<String>,
}

impl ProjectRow {
    pub(super) fn of(record: &ProjectRecord) -> anyhow::Result<Self> {
        Ok(Self {
            id: record.id.as_uuid(),
            name: record.name.clone(),
            created_at: columns::time(record.created_at),
            folders: path_columns(&record.folders)?,
        })
    }

    fn into_record(self) -> ProjectRecord {
        ProjectRecord {
            id: ProjectId::from_uuid(self.id),
            name: self.name,
            folders: self.folders.into_iter().map(PathBuf::from).collect(),
            created_at: columns::system_time(self.created_at),
        }
    }
}

pub(super) struct SettingsRow {
    model: Option<ModelRef>,
    window_tokens: i64,
    threshold_percent: i64,
}

impl SettingsRow {
    pub(super) fn of(settings: &Settings) -> anyhow::Result<Self> {
        Ok(Self {
            model: settings.model.clone(),
            window_tokens: columns::int(settings.context.window_tokens, "context window")?,
            threshold_percent: settings.context.threshold_percent.into(),
        })
    }

    /// Brought into range, as values once saved may be out of it.
    fn into_settings(self) -> anyhow::Result<Settings> {
        Ok(Settings {
            model: self.model,
            context: ContextSettings {
                window_tokens: columns::uint(self.window_tokens, "context window")?,
                threshold_percent: u8::try_from(self.threshold_percent)
                    .context("invalid compaction threshold")?,
            }
            .in_range(),
        })
    }
}

/// Deleted chats' counts, or what to add to them.
pub(super) struct UsageRow {
    chats: i64,
    tokens_used: i64,
    longest_chat_ns: i64,
    activity: Vec<ActivityRow>,
}

impl UsageRow {
    pub(super) fn of(statistics: &Statistics) -> anyhow::Result<Self> {
        Ok(Self {
            chats: columns::index(statistics.chats, "chats")?,
            tokens_used: columns::int(statistics.tokens_used, "tokens used")?,
            longest_chat_ns: columns::nanos(statistics.longest_chat, "longest chat")?,
            activity: statistics
                .token_activity
                .iter()
                .map(ActivityRow::of)
                .collect::<anyhow::Result<_>>()?,
        })
    }

    fn into_statistics(self) -> anyhow::Result<Statistics> {
        Ok(Statistics {
            chats: columns::count(self.chats, "chats")?,
            tokens_used: columns::uint(self.tokens_used, "tokens used")?,
            longest_chat: columns::duration(self.longest_chat_ns, "longest chat")?,
            token_activity: self
                .activity
                .into_iter()
                .map(ActivityRow::into_activity)
                .collect::<anyhow::Result<_>>()?,
        })
    }
}

pub(super) struct TaskRow {
    id: Uuid,
    title: String,
    prompt: String,
    schedule: String,
    model: ModelRef,
    thread_mode: &'static str,
    missed_runs: &'static str,
    project_mode: &'static str,
    active: bool,
    checked_until: NaiveDateTime,
    thread_id: Option<Uuid>,
    queued: Option<(NaiveDateTime, &'static str)>,
    folders: Vec<String>,
    runs: Vec<RunRow>,
}

struct RunRow {
    id: Uuid,
    scheduled_for: NaiveDateTime,
    started_at: Option<NaiveDateTime>,
    trigger: &'static str,
    result: &'static str,
    error: Option<String>,
    skipped_count: Option<i64>,
    skip_reason: Option<&'static str>,
    thread_id: Option<Uuid>,
}

impl RunRow {
    fn of(record: &RunRecord) -> anyhow::Result<Self> {
        let result = columns::run_result(&record.result)?;
        Ok(Self {
            id: record.id,
            scheduled_for: record.scheduled_for,
            started_at: record.started_at,
            trigger: columns::trigger(record.trigger),
            result: result.result,
            error: result.error.map(str::to_owned),
            skipped_count: result.skipped_count,
            skip_reason: result.skip_reason,
            thread_id: record.thread_id,
        })
    }
}

impl TaskRow {
    pub(super) fn of(task: &ScheduledTask) -> anyhow::Result<Self> {
        let SavedTask {
            id,
            settings,
            active,
            checked_until,
            thread_id,
            queued,
            history,
        } = task.to_saved();
        Ok(Self {
            id,
            schedule: serde_json::to_string(&settings.when)?,
            thread_mode: columns::thread_mode(settings.thread),
            missed_runs: columns::missed_runs(settings.missed),
            project_mode: columns::mode(settings.project_mode),
            folders: path_columns(&settings.folders)?,
            title: settings.title,
            prompt: settings.prompt,
            model: settings.model,
            active,
            checked_until,
            thread_id,
            queued: queued.map(|queued| (queued.scheduled_for, columns::trigger(queued.trigger))),
            runs: history
                .iter()
                .map(RunRow::of)
                .collect::<anyhow::Result<_>>()?,
        })
    }
}

/// Applies `job`. The caller commits.
pub(super) async fn write(conn: &mut SqliteConnection, job: &StateJob) -> anyhow::Result<()> {
    match job {
        StateJob::CreateProject(project) => insert_project(conn, project).await,
        StateJob::RemoveProject(id) => {
            // Its folders cascade; its chats are unfiled (`ON DELETE SET NULL`).
            sqlx::query("DELETE FROM projects WHERE id = ?")
                .bind(id)
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        StateJob::Settings(settings) => write_settings(conn, settings).await,
        StateJob::Tasks(tasks) => replace_tasks(conn, tasks).await,
        StateJob::CountDeleted(usage) => count_deleted(conn, usage).await,
    }
}

async fn insert_project(conn: &mut SqliteConnection, project: &ProjectRow) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES (?, ?, ?)")
        .bind(project.id)
        .bind(&project.name)
        .bind(project.created_at)
        .execute(&mut *conn)
        .await?;
    for (seq, path) in project.folders.iter().enumerate() {
        sqlx::query("INSERT INTO project_folders (project_id, seq, path) VALUES (?, ?, ?)")
            .bind(project.id)
            .bind(columns::index(seq, "folder")?)
            .bind(path)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

async fn write_settings(conn: &mut SqliteConnection, settings: &SettingsRow) -> anyhow::Result<()> {
    let (provider, model) = match &settings.model {
        Some(model) => (Some(columns::provider(model.provider)), Some(&model.id)),
        None => (None, None),
    };
    sqlx::query(
        "INSERT INTO settings (id, model_provider, model_id, context_window_tokens,
             compact_threshold_percent)
         VALUES (1, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
             model_provider = excluded.model_provider, model_id = excluded.model_id,
             context_window_tokens = excluded.context_window_tokens,
             compact_threshold_percent = excluded.compact_threshold_percent",
    )
    .bind(provider)
    .bind(model)
    .bind(settings.window_tokens)
    .bind(settings.threshold_percent)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Adds `usage` to the deleted chats' counts: sums, but the longest chat is
/// the longer of the two.
pub(super) async fn count_deleted(
    conn: &mut SqliteConnection,
    usage: &UsageRow,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE deleted_usage SET chats = chats + ?, tokens_used = tokens_used + ?,
             longest_chat_ns = max(longest_chat_ns, ?)
         WHERE id = 1",
    )
    .bind(usage.chats)
    .bind(usage.tokens_used)
    .bind(usage.longest_chat_ns)
    .execute(&mut *conn)
    .await?;
    for activity in &usage.activity {
        sqlx::query(
            "INSERT INTO deleted_token_activity (at, duration_ns, tokens) VALUES (?, ?, ?)",
        )
        .bind(activity.at)
        .bind(activity.duration_ns)
        .bind(activity.tokens)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Replaces every task with `tasks`; their folders and runs cascade.
async fn replace_tasks(conn: &mut SqliteConnection, tasks: &[TaskRow]) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM scheduled_tasks")
        .execute(&mut *conn)
        .await?;
    for (seq, task) in tasks.iter().enumerate() {
        let (queued_for, queued_trigger) = task.queued.unzip();
        sqlx::query(
            "INSERT INTO scheduled_tasks (id, seq, title, prompt, schedule, model_provider,
                 model_id, thread_mode, missed_runs, project_mode, active, checked_until,
                 thread_id, queued_for, queued_trigger)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task.id)
        .bind(columns::index(seq, "task")?)
        .bind(&task.title)
        .bind(&task.prompt)
        .bind(&task.schedule)
        .bind(columns::provider(task.model.provider))
        .bind(&task.model.id)
        .bind(task.thread_mode)
        .bind(task.missed_runs)
        .bind(task.project_mode)
        .bind(task.active)
        .bind(task.checked_until)
        .bind(task.thread_id)
        .bind(queued_for)
        .bind(queued_trigger)
        .execute(&mut *conn)
        .await?;
        for (seq, path) in task.folders.iter().enumerate() {
            sqlx::query("INSERT INTO scheduled_task_folders (task_id, seq, path) VALUES (?, ?, ?)")
                .bind(task.id)
                .bind(columns::index(seq, "folder")?)
                .bind(path)
                .execute(&mut *conn)
                .await?;
        }
        for (seq, run) in task.runs.iter().enumerate() {
            sqlx::query(
                "INSERT INTO scheduled_runs (task_id, seq, id, scheduled_for, started_at,
                     run_trigger, result, error, skipped_count, skip_reason, thread_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(task.id)
            .bind(columns::index(seq, "run")?)
            .bind(run.id)
            .bind(run.scheduled_for)
            .bind(run.started_at)
            .bind(run.trigger)
            .bind(run.result)
            .bind(&run.error)
            .bind(run.skipped_count)
            .bind(run.skip_reason)
            .bind(run.thread_id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Reads everything besides chats. Fails only if the database can't be
/// read; a row that can't be understood is logged and left out. (A task
/// left out is gone at the next save of the tasks, as a schedules file that
/// couldn't be read was set aside.)
pub(super) async fn read(pool: &SqlitePool) -> anyhow::Result<AppState> {
    Ok(AppState {
        projects: read_projects(pool).await?,
        settings: read_settings(pool).await?,
        deleted: read_deleted(pool).await?,
        tasks: read_tasks(pool).await?,
    })
}

async fn read_projects(pool: &SqlitePool) -> anyhow::Result<Vec<ProjectRecord>> {
    let mut folders = HashMap::<Uuid, Vec<String>>::new();
    for (project, path) in sqlx::query_as::<_, (Uuid, String)>(
        "SELECT project_id, path FROM project_folders ORDER BY project_id, seq",
    )
    .fetch_all(pool)
    .await?
    {
        folders.entry(project).or_default().push(path);
    }
    Ok(sqlx::query_as::<_, (Uuid, String, DateTime<Utc>)>(
        "SELECT id, name, created_at FROM projects
         ORDER BY unixepoch(created_at, 'subsec') DESC",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(id, name, created_at)| {
        ProjectRow {
            id,
            name,
            created_at,
            folders: folders.remove(&id).unwrap_or_default(),
        }
        .into_record()
    })
    .collect())
}

async fn read_settings(pool: &SqlitePool) -> anyhow::Result<Settings> {
    let Some(row) = sqlx::query(
        "SELECT model_provider, model_id, context_window_tokens, compact_threshold_percent
         FROM settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Settings::default());
    };
    let settings = (|| {
        let model = match (
            row.get::<Option<&str>, _>(0),
            row.get::<Option<String>, _>(1),
        ) {
            (Some(provider), Some(id)) => Some(ModelRef {
                provider: columns::to_provider(provider)?,
                id,
            }),
            _ => None,
        };
        SettingsRow {
            model,
            window_tokens: row.get(2),
            threshold_percent: row.get(3),
        }
        .into_settings()
    })();
    Ok(settings.unwrap_or_else(|error| {
        eprintln!("could not read settings: {error:#}; using the defaults");
        Settings::default()
    }))
}

async fn read_deleted(pool: &SqlitePool) -> anyhow::Result<Statistics> {
    let (chats, tokens_used, longest_chat_ns) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT chats, tokens_used, longest_chat_ns FROM deleted_usage WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    let activity = sqlx::query_as::<_, (DateTime<Utc>, i64, i64)>(
        "SELECT at, duration_ns, tokens FROM deleted_token_activity ORDER BY seq",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(at, duration_ns, tokens)| ActivityRow {
        at,
        duration_ns,
        tokens,
    })
    .collect();
    let usage = UsageRow {
        chats,
        tokens_used,
        longest_chat_ns,
        activity,
    };
    Ok(usage.into_statistics().unwrap_or_else(|error| {
        eprintln!("could not read deleted chats' statistics: {error:#}");
        Statistics::default()
    }))
}

async fn read_tasks(pool: &SqlitePool) -> anyhow::Result<Vec<ScheduledTask>> {
    let mut folders = HashMap::<Uuid, Vec<PathBuf>>::new();
    for (task, path) in sqlx::query_as::<_, (Uuid, String)>(
        "SELECT task_id, path FROM scheduled_task_folders ORDER BY task_id, seq",
    )
    .fetch_all(pool)
    .await?
    {
        folders.entry(task).or_default().push(path.into());
    }
    let mut runs = HashMap::<Uuid, Vec<anyhow::Result<RunRecord>>>::new();
    for row in sqlx::query(
        "SELECT task_id, id, scheduled_for, started_at, run_trigger, result, error,
                skipped_count, skip_reason, thread_id
         FROM scheduled_runs ORDER BY task_id, seq",
    )
    .fetch_all(pool)
    .await?
    {
        let record = (|| {
            Ok(RunRecord {
                id: row.get(1),
                scheduled_for: row.get(2),
                started_at: row.get(3),
                trigger: columns::to_trigger(row.get::<&str, _>(4))?,
                result: columns::to_run_result(
                    row.get::<&str, _>(5),
                    row.get(6),
                    row.get(7),
                    row.get(8),
                )?,
                thread_id: row.get(9),
            })
        })();
        runs.entry(row.get(0)).or_default().push(record);
    }
    let mut tasks = Vec::new();
    for row in sqlx::query(
        "SELECT id, title, prompt, schedule, model_provider, model_id, thread_mode,
                missed_runs, project_mode, active, checked_until, thread_id, queued_for,
                queued_trigger
         FROM scheduled_tasks ORDER BY seq",
    )
    .fetch_all(pool)
    .await?
    {
        let id: Uuid = row.get(0);
        let task = (|| {
            let queued = match (
                row.get::<Option<NaiveDateTime>, _>(12),
                row.get::<Option<&str>, _>(13),
            ) {
                (Some(scheduled_for), Some(trigger)) => Some(QueuedRun {
                    scheduled_for,
                    trigger: columns::to_trigger(trigger)?,
                }),
                _ => None,
            };
            anyhow::Ok(SavedTask {
                id,
                settings: TaskSettings {
                    title: row.get(1),
                    prompt: row.get(2),
                    when: serde_json::from_str(row.get(3))?,
                    model: ModelRef {
                        provider: columns::to_provider(row.get(4))?,
                        id: row.get(5),
                    },
                    thread: columns::to_thread_mode(row.get(6))?,
                    missed: columns::to_missed_runs(row.get(7))?,
                    folders: folders.remove(&id).unwrap_or_default(),
                    project_mode: columns::to_mode(row.get(8))?,
                },
                active: row.get(9),
                checked_until: row.get(10),
                thread_id: row.get(11),
                queued,
                history: runs
                    .remove(&id)
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<anyhow::Result<_>>()?,
            })
        })();
        match task {
            Ok(task) => tasks.push(ScheduledTask::loaded(task)),
            Err(error) => eprintln!("could not read scheduled task {id}: {error:#}; left out"),
        }
    }
    Ok(tasks)
}
