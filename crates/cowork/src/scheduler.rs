//! Running scheduled tasks: looking at the clock, the queue that runs them
//! one at a time, submitting each run's prompt, and saving tasks (in the
//! chats' database, `chat_db/app_state.rs`).
//! See `docs/schedule.md`; the policy for each task's occurrences is in
//! `scheduled_task.rs`.
//!
//! Only the host runs tasks, in its own threads, through the same
//! submission path as anything else it submits. Runs start in order of the
//! time they were due, ties going to the task created first, and the next
//! starts once the agent run of the last one ends.

use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, App, Context, Entity, Task};
use gpui_component::{WindowExt as _, notification::Notification};
use jiff::civil::DateTime;
use uuid::Uuid;

use crate::chat_db::StateSaver;
use crate::chat_store::{ListedChat, Place};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScheduledRun {
    pub(crate) task_id: Uuid,
    record_id: Uuid,
    pub(crate) thread_id: Uuid,
    message_id: Uuid,
}

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
    Due(DateTime),
    NoMoreRuns,
    Paused,
}

enum Blocked {
    Thread,
    /// The chat it continues is loading, or must be loaded first.
    Loading(Uuid),
    Models,
}

#[derive(Default)]
pub(crate) struct Scheduler {
    /// In the order they were created, which breaks ties in the queue.
    pub(crate) tasks: Vec<ScheduledTask>,
    pub(crate) run: Option<ScheduledRun>,
    /// Saves the tasks; `None` keeps them in memory, as in tests and
    /// ephemeral launches.
    saver: Option<StateSaver>,
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

    pub(crate) fn start_saving(&mut self, tasks: Vec<ScheduledTask>, saver: StateSaver) {
        self.tasks = tasks;
        self.saver = Some(saver);
    }

    /// Saves every task as it is now. Called on every change: there are
    /// few, each with a bounded history.
    fn save(&self) {
        if let Some(saver) = &self.saver {
            saver.save_tasks(&self.tasks);
        }
    }
}

/// The local wall-clock time schedules run by.
pub(crate) fn now() -> DateTime {
    jiff::ZonedDateTime::now().datetime()
}

impl Cowork {
    pub(crate) fn start_scheduler(
        &mut self,
        window: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
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
    pub(crate) fn tick_schedules(&mut self, now: DateTime, cx: &mut Context<Self>) {
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

    pub(crate) fn run_next_scheduled(&mut self, now: DateTime, cx: &mut Context<Self>) {
        while self.scheduler.run.is_none() && self.start_next_scheduled_run(now, cx) {}
    }

    fn blocked(&self, index: usize, cx: &App) -> Option<Blocked> {
        self.ready(index, cx).err()
    }

    /// The chat the next queued run, `index`'s, continues (`None` for a new
    /// one), or why it can't start yet. Only a loaded chat is returned, so
    /// a chat that isn't loaded can't be taken for deleted.
    fn ready(&self, index: usize, cx: &App) -> Result<Option<Entity<Thread>>, Blocked> {
        let task = &self.scheduler.tasks[index];
        let continued = task
            .thread_id
            .filter(|_| task.settings().thread == ThreadMode::ContinueLast);
        let store = self.thread_store.read(cx);
        let thread = match continued.and_then(|thread_id| store.listed(thread_id, cx)) {
            Some((_, ListedChat::Loaded(thread))) if thread.read(cx).is_generating() => {
                return Err(Blocked::Thread);
            }
            Some((_, ListedChat::Loaded(thread))) => Some(thread.clone()),
            Some((_, ListedChat::Unloaded(meta))) => return Err(Blocked::Loading(meta.id)),
            // Deleted: the run starts a new chat.
            None => None,
        };
        if self.models.providers().next().is_none() {
            return Err(Blocked::Models);
        }
        Ok(thread)
    }

    /// Starts the next queued run, or records why it failed to. Returns
    /// whether the queue moved on.
    fn start_next_scheduled_run(&mut self, now: DateTime, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.scheduler.next_queued() else {
            return false;
        };
        let continued = match self.ready(index, cx) {
            Ok(continued) => continued,
            Err(Blocked::Loading(thread_id)) => {
                // Retried when it has loaded; see `ChatStoreEvent::Loaded`.
                let models = self.models.clone();
                self.thread_store
                    .update(cx, |store, cx| store.load(thread_id, models, cx));
                return false;
            }
            Err(_) => return false,
        };
        let task = &self.scheduler.tasks[index];
        let task_id = task.id;
        let settings = task.settings().clone();
        let run = self.scheduler.tasks[index]
            .take_queued()
            .expect("the next task has a queued run");
        let started = if !self.models.contains(&settings.model) {
            Err(format!("The model {} isn't available.", settings.model.id))
        } else if let Some(folder) = settings.missing_folder() {
            Err(format!("The folder {} is gone.", folder.display()))
        } else {
            let thread = match continued {
                Some(thread) => self.restore_for_scheduled_run(thread, cx),
                None => self.new_scheduled_thread(&settings, cx),
            };
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
    /// back from the archive if it was put there.
    fn restore_for_scheduled_run(
        &mut self,
        thread: Entity<Thread>,
        cx: &mut Context<Self>,
    ) -> Entity<Thread> {
        let thread_id = thread.read(cx).instance_id;
        if let Some((Place::Archived(_), _)) = self.thread_store.read(cx).listed(thread_id, cx) {
            self.restore_thread(thread_id, cx);
        }
        thread
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
        self.thread_store.update(cx, |store, cx| {
            store.push_front(thread.clone(), cx);
        });
        self.profile_statistics = None;
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
        let message = format!("“{title}” wants to use {tool}. Open the chat to allow or deny it.");
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
                    Some(Blocked::Thread | Blocked::Loading(_)) => {
                        return TaskStatus::WaitingForThread;
                    }
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
            changed |= task.thread_id.take_if(|id| *id == thread_id).is_some();
        }
        if changed {
            self.scheduler.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_db::TestDb;
    use crate::scheduled_task::{
        MissedRuns, When,
        tests::{SATURDAY, at, settings, time},
    };

    #[test]
    fn tasks_are_saved_and_loaded() {
        let (test_db, db) = TestDb::open();
        let mut queued = ScheduledTask::new(
            settings(When::Daily(time(9, 0)), MissedRuns::RunOnce),
            at(SATURDAY, 8, 0),
        );
        queued.queue_now(at(SATURDAY, 8, 0));
        let mut ran = ScheduledTask::new(
            crate::scheduled_task::TaskSettings {
                folders: vec![std::env::temp_dir()],
                ..settings(When::Once(at(SATURDAY, 9, 0)), MissedRuns::Skip)
            },
            at(SATURDAY, 8, 0),
        );
        ran.queue_now(at(SATURDAY, 8, 0));
        let run = ran.take_queued().unwrap();
        ran.start(run, at(SATURDAY, 8, 1), Uuid::new_v4());
        ran.advance(at(SATURDAY, 9, 30));
        let mut scheduler = Scheduler::default();
        scheduler.start_saving(vec![queued.clone(), ran.clone()], db.saver());
        scheduler.save();
        futures::executor::block_on(db.quit());

        let (db, saved) = test_db.reopen();
        // The run going on when Cowork quit didn't finish.
        let ran = ScheduledTask::loaded(ran.to_saved());
        assert_eq!(saved.state.tasks, vec![queued, ran]);
        futures::executor::block_on(db.quit());
    }
}
