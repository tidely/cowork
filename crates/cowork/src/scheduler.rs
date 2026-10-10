//! Running scheduled tasks: looking at the clock, the queue that runs them
//! one at a time, submitting each run's prompt, and saving tasks to disk.
//! See `docs/schedule.md`; the policy for each task's occurrences is in
//! `scheduled_task.rs`.
//!
//! Only the host runs tasks, in its own threads, through the same
//! submission path as anything else it submits. Runs start in order of the
//! time they were due, ties going to the task created first, and the next
//! starts once the agent run of the last one ends.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{Local, NaiveDateTime};
use gpui::{AnyWindowHandle, App, Context, Entity, Task};
use gpui_component::{WindowExt as _, notification::Notification};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Cowork, protocol,
    scheduled_task::{RunResult, ScheduledTask, TaskSettings, ThreadMode},
    thread::Thread,
    thread_draft::ThreadDraft,
};

/// How often the scheduler looks at the clock. Short enough that a run
/// starts within seconds of its time; occurrences found much later than
/// that were missed (see [`crate::scheduled_task::MISSED_AFTER`]).
const TICK: Duration = Duration::from_secs(15);

/// How often to ask for models again while runs wait for them, as when
/// Ollama isn't running yet.
const MODEL_RETRY: Duration = Duration::from_secs(60);

/// The saved file's format. Older or newer files aren't read, nor
/// overwritten until a task changes.
const FILE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct ScheduleFile {
    version: u32,
    tasks: Vec<ScheduledTask>,
}

/// The run going on now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScheduledRun {
    pub(crate) task_id: Uuid,
    record_id: Uuid,
    pub(crate) thread_id: Uuid,
    message_id: Uuid,
}

/// What a task is doing, as its row shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Running,
    /// Its run waits for someone to allow a tool call.
    WaitingForApproval,
    /// Next in the queue, but its thread is busy with someone else's run.
    WaitingForThread,
    /// Next in the queue, but no models are known yet.
    WaitingForModels,
    Queued,
    Due(NaiveDateTime),
    NoMoreRuns,
    Paused,
}

/// Why the next run can't start yet.
enum Blocked {
    Thread,
    Models,
}

#[derive(Default)]
pub(crate) struct Scheduler {
    /// In the order they were created, which breaks ties in the queue.
    pub(crate) tasks: Vec<ScheduledTask>,
    pub(crate) run: Option<ScheduledRun>,
    /// Where tasks are saved; `None` keeps them in memory, as in tests.
    file: Option<PathBuf>,
    /// Shows notifications; `None` in tests.
    window: Option<AnyWindowHandle>,
    last_model_request: Option<Instant>,
    _ticking: Option<Task<()>>,
}

impl Scheduler {
    pub(crate) fn task(&self, task_id: Uuid) -> Option<&ScheduledTask> {
        self.tasks.iter().find(|task| task.id == task_id)
    }

    fn task_mut(&mut self, task_id: Uuid) -> Option<&mut ScheduledTask> {
        self.tasks.iter_mut().find(|task| task.id == task_id)
    }

    /// The task whose queued run is next.
    fn next_queued(&self) -> Option<usize> {
        self.tasks
            .iter()
            .enumerate()
            .filter_map(|(index, task)| Some((task.queued()?.scheduled_for, index)))
            .min()
            .map(|(_, index)| index)
    }

    /// The tasks continuing `thread_id`, which deleting it would make start
    /// a new thread.
    pub(crate) fn tasks_continuing(&self, thread_id: Uuid) -> Vec<&ScheduledTask> {
        self.tasks
            .iter()
            .filter(|task| {
                task.settings().thread == ThreadMode::ContinueLast
                    && task.thread_id == Some(thread_id)
            })
            .collect()
    }

    fn save(&self) {
        let Some(path) = &self.file else {
            return;
        };
        let file = ScheduleFile {
            version: FILE_VERSION,
            tasks: self.tasks.clone(),
        };
        if let Err(error) = save_tasks(path, &file) {
            eprintln!(
                "could not save scheduled tasks to {}: {error:#}",
                path.display()
            );
        }
    }
}

/// Where tasks are saved: next to Cowork's other files.
pub(crate) fn schedule_file() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".cowork").join("schedules.json"))
}

/// Writes `file` whole, replacing the old one only once the new one is
/// written, so a crash never leaves half a file. Small and only written when
/// a task changes, so it is written in place of waiting on another thread.
fn save_tasks(path: &Path, file: &ScheduleFile) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(file)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Reads the saved tasks. A file that can't be read is moved aside rather
/// than overwritten by the next save.
pub(crate) fn load_tasks(path: &Path) -> Vec<ScheduledTask> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            eprintln!(
                "could not read scheduled tasks from {}: {error}",
                path.display()
            );
            return Vec::new();
        }
    };
    match serde_json::from_slice::<ScheduleFile>(&bytes) {
        Ok(file) if file.version == FILE_VERSION => {
            file.tasks.into_iter().map(ScheduledTask::loaded).collect()
        }
        result => {
            let aside = path.with_extension("json.unreadable");
            match result {
                Ok(file) => eprintln!(
                    "scheduled tasks in {} are format {}, not {FILE_VERSION}; moved to {}",
                    path.display(),
                    file.version,
                    aside.display()
                ),
                Err(error) => eprintln!(
                    "could not read scheduled tasks from {}: {error}; moved to {}",
                    path.display(),
                    aside.display()
                ),
            }
            _ = std::fs::rename(path, aside);
            Vec::new()
        }
    }
}

fn now() -> NaiveDateTime {
    Local::now().naive_local()
}

impl Cowork {
    /// Loads the saved tasks and starts looking at the clock.
    pub(crate) fn start_scheduler(
        &mut self,
        file: Option<PathBuf>,
        window: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = &file {
            self.scheduler.tasks = load_tasks(path);
        }
        self.scheduler.file = file;
        self.scheduler.window = window;
        self.scheduler._ticking = Some(cx.spawn(async move |this, cx| {
            loop {
                if this
                    .update(cx, |this, cx| this.tick_schedules(now(), cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(TICK).await;
            }
        }));
    }

    /// Queues the runs due by `now`, records missed and skipped ones, and
    /// starts the next run if none is going on.
    pub(crate) fn tick_schedules(&mut self, now: NaiveDateTime, cx: &mut Context<Self>) {
        let mut changed = false;
        for task in &mut self.scheduler.tasks {
            changed |= task.advance(now);
        }
        if changed {
            self.scheduler.save();
            cx.notify();
        }
        self.request_models_for_queued_runs(cx);
        self.run_next_scheduled(now, cx);
    }

    /// Asks for models again while queued runs wait for them.
    fn request_models_for_queued_runs(&mut self, cx: &mut Context<Self>) {
        if self.models.providers().next().is_some() || self.scheduler.next_queued().is_none() {
            return;
        }
        if self
            .scheduler
            .last_model_request
            .is_some_and(|asked| asked.elapsed() < MODEL_RETRY)
        {
            return;
        }
        self.scheduler.last_model_request = Some(Instant::now());
        self.refresh_models(cx);
    }

    /// Starts queued runs until one is going on or the next must wait.
    pub(crate) fn run_next_scheduled(&mut self, now: NaiveDateTime, cx: &mut Context<Self>) {
        while self.scheduler.run.is_none() && self.start_next_scheduled_run(now, cx) {}
    }

    /// Why the next queued run, `index`'s, can't start yet, if it can't.
    fn blocked(&self, index: usize, cx: &App) -> Option<Blocked> {
        let task = &self.scheduler.tasks[index];
        let thread_busy = task.settings().thread == ThreadMode::ContinueLast
            && task.thread_id.is_some_and(|thread_id| {
                let store = self.thread_store.read(cx);
                store
                    .thread(thread_id, cx)
                    .or_else(|| store.archived_thread(thread_id, cx))
                    .is_some_and(|thread| thread.read(cx).generating)
            });
        if thread_busy {
            Some(Blocked::Thread)
        } else if self.models.providers().next().is_none() {
            Some(Blocked::Models)
        } else {
            None
        }
    }

    /// Starts the next queued run, or records why it failed to. Returns
    /// whether the queue moved on.
    fn start_next_scheduled_run(&mut self, now: NaiveDateTime, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.scheduler.next_queued() else {
            return false;
        };
        if self.blocked(index, cx).is_some() {
            return false;
        }
        let task = &self.scheduler.tasks[index];
        let task_id = task.id;
        let settings = task.settings().clone();
        let continued = task
            .thread_id
            .filter(|_| settings.thread == ThreadMode::ContinueLast);
        let run = self.scheduler.tasks[index]
            .take_queued()
            .expect("the next task has a queued run");
        let started = if !self.models.contains(&settings.model) {
            Err(format!("The model {} isn't available.", settings.model.id))
        } else if let Some(folder) = settings.missing_folder() {
            Err(format!("The folder {} is gone.", folder.display()))
        } else {
            let thread = continued
                .and_then(|thread_id| self.restore_for_scheduled_run(thread_id, cx))
                .unwrap_or_else(|| self.new_scheduled_thread(&settings, cx));
            let thread_id = thread.read(cx).instance_id;
            // Before submitting, so the run's sandbox mounts it from its
            // first command; nothing runs in the thread now.
            self.set_thread_project(&thread, &settings.folders, settings.project_mode, cx);
            self.submit_scheduled_prompt(
                &thread,
                &settings.prompt,
                &settings.title,
                &settings.model,
                cx,
            )
            .map(|message_id| (thread_id, message_id))
        };
        let task = &mut self.scheduler.tasks[index];
        match started {
            Ok((thread_id, message_id)) => {
                let record_id = task.start(run, now, thread_id);
                self.scheduler.run = Some(ScheduledRun {
                    task_id,
                    record_id,
                    thread_id,
                    message_id,
                });
            }
            Err(error) => task.fail(run, now, error),
        }
        self.scheduler.save();
        cx.notify();
        true
    }

    /// The thread a **continue the last thread** run goes on in, brought
    /// back from the archive if it was put there; `None` once it is deleted.
    fn restore_for_scheduled_run(
        &mut self,
        thread_id: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Thread>> {
        if let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) {
            return Some(thread);
        }
        self.thread_store.read(cx).archived_thread(thread_id, cx)?;
        self.restore_thread(thread_id, cx);
        self.thread_store.read(cx).thread(thread_id, cx)
    }

    /// A new thread for a run, at the top of the sidebar's recents. The
    /// user's own new-thread draft and project stay as they are.
    fn new_scheduled_thread(
        &mut self,
        settings: &TaskSettings,
        cx: &mut Context<Self>,
    ) -> Entity<Thread> {
        let thread = Self::new_empty_local_thread(
            ThreadDraft::new(self.local_participant_id),
            self.local_participant_id,
            self.models.clone(),
            Some(settings.model.clone()),
            cx,
        );
        self.thread_store.update(cx, |store, _| {
            store.threads.push_front(thread.clone());
        });
        thread
    }

    /// Records the end of a scheduled run, if the run producing `message_id`
    /// in `thread_id` was one, and starts the next: a run waiting for its
    /// thread may start now too.
    pub(crate) fn scheduled_generation_ended(
        &mut self,
        thread_id: Uuid,
        message_id: Uuid,
        outcome: &protocol::RunOutcome,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self
            .scheduler
            .run
            .take_if(|run| run.thread_id == thread_id && run.message_id == message_id)
        {
            // A task deleted during its run has nothing to record.
            if let Some(record) = self
                .scheduler
                .task_mut(run.task_id)
                .and_then(|task| task.record_mut(run.record_id))
            {
                record.result = match outcome {
                    protocol::RunOutcome::Completed => RunResult::Completed,
                    protocol::RunOutcome::Stopped => RunResult::Stopped,
                    protocol::RunOutcome::Failed(error) => RunResult::Failed(error.clone()),
                };
            }
            self.scheduler.save();
            cx.notify();
        }
        self.run_next_scheduled(now(), cx);
    }

    /// Tells the user a scheduled run waits for them to allow `tool`, which
    /// its model asked for twice; see `tool_approval.rs`.
    pub(crate) fn scheduled_run_waits(
        &mut self,
        thread_id: Uuid,
        message_id: Uuid,
        tool: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = self
            .scheduler
            .run
            .filter(|run| run.thread_id == thread_id && run.message_id == message_id)
        else {
            return;
        };
        cx.notify();
        let Some(window) = self.scheduler.window else {
            return;
        };
        let title = self.scheduler.task(run.task_id).map_or_else(
            || "A scheduled task".to_owned(),
            |task| task.settings().title.clone(),
        );
        let message =
            format!("“{title}” wants to use {tool}. Open the thread to allow or deny it.");
        let cowork = cx.entity().downgrade();
        // Deferred, as showing it updates the window this update is part of.
        cx.defer(move |cx| {
            _ = window.update(cx, |_, window, cx| {
                window.push_notification(
                    Notification::warning(message)
                        .id1::<ScheduledRun>(run.task_id)
                        .title("A scheduled run needs approval")
                        .autohide(false)
                        .in_app_and_system()
                        .on_click(move |_, window, cx| {
                            _ = cowork.update(cx, |cowork, cx| {
                                cowork.open_thread(thread_id, window, cx);
                            });
                        }),
                    cx,
                );
            });
        });
    }

    /// What `task` is doing.
    pub(crate) fn task_status(&self, task: &ScheduledTask, cx: &App) -> TaskStatus {
        if let Some(run) = self.scheduler.run.filter(|run| run.task_id == task.id) {
            let waiting = self
                .active_generations
                .get(&run.thread_id)
                .is_some_and(|generation| {
                    generation.message_id() == run.message_id
                        && generation.is_waiting_for_approval()
                });
            return if waiting {
                TaskStatus::WaitingForApproval
            } else {
                TaskStatus::Running
            };
        }
        if task.queued().is_some() {
            let index = self.scheduler.tasks.iter().position(|t| t.id == task.id);
            if self.scheduler.run.is_none() && index == self.scheduler.next_queued() {
                match index.and_then(|index| self.blocked(index, cx)) {
                    Some(Blocked::Thread) => return TaskStatus::WaitingForThread,
                    Some(Blocked::Models) => return TaskStatus::WaitingForModels,
                    None => {}
                }
            }
            return TaskStatus::Queued;
        }
        if !task.is_active() {
            return TaskStatus::Paused;
        }
        task.next_due()
            .map_or(TaskStatus::NoMoreRuns, TaskStatus::Due)
    }

    /// Adds a task, or with `task_id` changes one.
    pub(crate) fn save_scheduled_task(
        &mut self,
        task_id: Option<Uuid>,
        settings: TaskSettings,
        cx: &mut Context<Self>,
    ) {
        let now = now();
        match task_id.and_then(|task_id| self.scheduler.task_mut(task_id)) {
            Some(task) => task.set_settings(settings, now),
            None => self.scheduler.tasks.push(ScheduledTask::new(settings, now)),
        }
        self.scheduler.save();
        cx.notify();
    }

    pub(crate) fn set_scheduled_task_active(
        &mut self,
        task_id: Uuid,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(task) = self.scheduler.task_mut(task_id) {
            task.set_active(active, now());
            self.scheduler.save();
            cx.notify();
        }
    }

    /// Deletes a task. A run of it going on finishes; the threads it created
    /// are kept.
    pub(crate) fn delete_scheduled_task(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        self.scheduler.tasks.retain(|task| task.id != task_id);
        self.scheduler.save();
        cx.notify();
    }

    /// Queues a run of `task_id` now, behind the runs already waiting.
    pub(crate) fn run_scheduled_task_now(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        let now = now();
        if self
            .scheduler
            .task_mut(task_id)
            .is_some_and(|task| task.queue_now(now))
        {
            self.scheduler.save();
            cx.notify();
            self.run_next_scheduled(now, cx);
        }
    }

    /// Forgets that tasks continue `thread_id`, which is being deleted, so
    /// their next runs start a new thread.
    pub(crate) fn forget_scheduled_thread(&mut self, thread_id: Uuid) {
        let mut changed = false;
        for task in &mut self.scheduler.tasks {
            if task.thread_id == Some(thread_id) {
                task.thread_id = None;
                changed = true;
            }
        }
        if changed {
            self.scheduler.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_task::{
        MissedRuns, When,
        tests::{SATURDAY, at, settings, time},
    };

    #[test]
    fn tasks_are_saved_and_loaded() {
        let dir = std::env::temp_dir().join(format!("cowork-schedules-{}", Uuid::new_v4()));
        let path = dir.join("schedules.json");
        assert!(load_tasks(&path).is_empty(), "no file yet");

        let mut task = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at(SATURDAY, 8, 0),
        );
        task.queue_now(at(SATURDAY, 8, 0));
        let scheduler = Scheduler {
            tasks: vec![task.clone()],
            file: Some(path.clone()),
            ..Default::default()
        };
        scheduler.save();
        assert_eq!(load_tasks(&path), vec![task]);

        // An unreadable file is kept aside, not replaced by nothing.
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_tasks(&path).is_empty());
        assert!(!path.exists());
        assert!(path.with_extension("json.unreadable").exists());
        _ = std::fs::remove_dir_all(dir);
    }
}
