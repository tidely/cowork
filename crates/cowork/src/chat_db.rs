//! Saving the user's own chats in one SQLite database, `~/.cowork/cowork.db`,
//! and reading them back: every chat's row at startup, and the rest of a chat
//! when it loads. The schema is in `migrations/` and the design in
//! `docs/sqlite-storage.md`. The same database and writer keep the rest of
//! what Cowork saves; see `app_state`.
//!
//! Only the host saves a chat; a joined one is its host's. A chat is saved
//! at turn boundaries, when it is archived or restored, before it is
//! unloaded, and when Cowork quits, never during a run, so every tool call
//! in its transcript has its result. Its draft, pending submissions and
//! sharing are not saved.
//!
//! **The database is the truth about what is saved.** A save carries the
//! extent it was built from (`from`: messages, transcript entries,
//! compactions, all of which only grow) and the chat's whole extent (`end`).
//! In its transaction the writer reads what the database holds and refuses
//! the save if that is less than `from` (an earlier save failed: the next
//! one starts from what is there) or more than `end` (the chat in memory
//! isn't the whole chat). Otherwise it writes the rows past what is stored.
//! What this side remembers per chat is only a hint of where to start
//! encoding, plus what the writer last confirmed, from which
//! [`ChatDb::is_saved`] tells whether a chat may be unloaded.
//!
//! One Tokio task writes, taking jobs in the order they were queued, so a
//! load sees every save before it and quit waits for all of them. Only
//! plain rows cross into it. Searches read on their own connection.
//!
//! One Cowork instance keeps the chats, holding `.lock` next to the database.
//! An explicitly ephemeral instance has no database and saves nothing; its
//! scratch folders are in a temporary folder removed when it quits. Each
//! chat's scratch folder is `threads/<id>/scratch/` either way; see
//! `sandbox::settings`.

mod app_state;
mod columns;
mod rows;
mod sql;

use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::{App, Context, Entity};
use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

pub(crate) use app_state::AppState;
pub(crate) use rows::ChatRows;
pub(crate) use sql::SearchHit;

use crate::{
    Cowork,
    chat_meta::{ChatLoader, ChatMeta},
    chat_store::{ListedChat, Place},
    projects::{ProjectId, ProjectRecord},
    saved_settings::Settings,
    scheduled_task::ScheduledTask,
    statistics::Statistics,
    thread::{Thread, ThreadOwnership},
};
use app_state::{ProjectRow, SettingsRow, StateJob, TaskRow, UsageRow};
use rows::{Extent, Head, Placement, Tail};

const DB_FILE: &str = "cowork.db";
const LOCK_FILE: &str = ".lock";
const THREADS_DIR: &str = "threads";
/// The most texts one search reads; enough for a palette's worth of chats.
const SEARCH_LIMIT: i64 = 2000;

/// Cowork's folder: the database, its lock, and the chats' folders.
pub(crate) fn cowork_dir() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".cowork"))
}

/// The scratch folders' root for an ephemeral instance, removed at quit.
pub(crate) fn ephemeral_root() -> PathBuf {
    std::env::temp_dir().join(format!("cowork-threads-{}", Uuid::new_v4().simple()))
}

pub(crate) struct ListedRow {
    pub(crate) meta: ChatMeta,
    pub(crate) archived: bool,
}

pub(crate) struct Saved {
    /// Archived ones last, each list in its saved order.
    pub(crate) chats: Vec<ListedRow>,
    pub(crate) state: AppState,
}

pub(crate) struct ChatDb {
    root: PathBuf,
    jobs: mpsc::UnboundedSender<Job>,
    pool: SqlitePool,
    runtime: tokio::runtime::Handle,
    tracking: Rc<RefCell<Tracking>>,
    _lock: File,
}

struct Tracking {
    /// Where the next save starts encoding: what the database is believed
    /// to hold. Too low costs re-encoding; too high is refused and
    /// corrected by the writer's report.
    hints: HashMap<Uuid, Extent>,
    /// The extent and head the writer last confirmed writing.
    confirmed: HashMap<Uuid, (Extent, Head)>,
    reports: mpsc::UnboundedReceiver<Report>,
}

impl Tracking {
    fn pump(&mut self) {
        while let Ok(report) = self.reports.try_recv() {
            match report {
                Report::Saved { id, end, head } => {
                    // A chat forgotten meanwhile (unloaded or deleted) is
                    // no longer tracked.
                    if self.hints.contains_key(&id) {
                        self.confirmed.insert(id, (end, *head));
                    }
                }
                Report::Failed { id, actual } => {
                    if let Some(hint) = self.hints.get_mut(&id) {
                        *hint = actual.unwrap_or_default();
                    }
                }
            }
        }
    }
}

enum Job {
    Save(Box<SaveJob>),
    Meta(Uuid, Placement),
    /// Deletes a chat, adding what its statistics count to the deleted
    /// chats' in the same transaction.
    Delete(Uuid, UsageRow),
    State(StateJob),
    Load(Uuid, oneshot::Sender<anyhow::Result<Option<ChatRows>>>),
    Quit(oneshot::Sender<()>),
    #[cfg(test)]
    Flush(oneshot::Sender<()>),
    /// Makes the next save fail after writing some of its rows.
    #[cfg(test)]
    FailNextSave,
}

struct SaveJob {
    from: Extent,
    end: Extent,
    head: Head,
    placement: Placement,
    tail: Tail,
}

enum Report {
    Saved {
        id: Uuid,
        end: Extent,
        head: Box<Head>,
    },
    /// With what the database holds, when it could tell.
    Failed { id: Uuid, actual: Option<Extent> },
}

/// Why the writer refused a save: what the database holds doesn't fit it.
#[derive(Debug, thiserror::Error)]
enum Mismatch {
    #[error("an earlier save of the chat failed")]
    Behind(Extent),
    #[error("the chat in memory is shorter than the saved one")]
    Ahead(Extent),
}

impl ChatDb {
    /// Keeps what Cowork saves in `dir`, reading every chat's row and the
    /// rest of the app's state, or fails if another instance keeps them or
    /// the database can't be opened. Blocks on `runtime` until they are
    /// read.
    pub(crate) fn open(
        dir: PathBuf,
        runtime: &tokio::runtime::Handle,
    ) -> anyhow::Result<(Self, Saved)> {
        use anyhow::Context as _;
        let lock = lock(&dir)
            .with_context(|| format!("could not lock {}", dir.display()))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cowork is already running. Use --ephemeral to open a separate instance."
                )
            })?;
        let path = dir.join(DB_FILE);
        let root = dir.join(THREADS_DIR);
        let (pool, rows, state) = runtime.block_on(async {
            let pool = sql::open(&path).await?;
            let rows = sql::read_metas(&pool).await?;
            let state = app_state::read(&pool).await?;
            anyhow::Ok((pool, rows, state))
        })?;
        let mut listed = Vec::new();
        for (row, activity) in rows {
            let id = row.id;
            let archived = row.archived;
            match row.into_meta(activity) {
                Ok(meta) => listed.push(ListedRow { meta, archived }),
                Err(error) => eprintln!("could not read chat {id}: {error:#}; left as it is"),
            }
        }
        let (jobs, received) = mpsc::unbounded_channel();
        let (reported, reports) = mpsc::unbounded_channel();
        runtime.spawn(write(pool.clone(), root.clone(), received, reported));
        let db = Self {
            root,
            jobs,
            pool,
            runtime: runtime.clone(),
            tracking: Rc::new(RefCell::new(Tracking {
                hints: HashMap::new(),
                confirmed: HashMap::new(),
                reports,
            })),
            _lock: lock,
        };
        Ok((
            db,
            Saved {
                chats: listed,
                state,
            },
        ))
    }

    pub(crate) fn saver(&self) -> StateSaver {
        StateSaver {
            jobs: self.jobs.clone(),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Queues a save of `thread`, which must be loaded whole, listed at
    /// `placement`.
    fn save(&self, thread: &Thread, placement: Placement) -> anyhow::Result<()> {
        let id = thread.instance_id;
        let mut tracking = self.tracking.borrow_mut();
        tracking.pump();
        let end = Extent::of(thread);
        let mut from = tracking.hints.get(&id).copied().unwrap_or_default();
        if !from.within(end) {
            // Can't be: a chat in memory only grows. Encode it whole; the
            // writer refuses it if the database holds more.
            from = Extent::default();
        }
        let job = SaveJob {
            from,
            end,
            head: Head::of(thread)?,
            placement,
            tail: Tail::after(thread, from)?,
        };
        tracking.hints.insert(id, end);
        _ = self.jobs.send(Job::Save(Box::new(job)));
        Ok(())
    }

    pub(crate) fn is_saved(&self, thread: &Thread) -> bool {
        let mut tracking = self.tracking.borrow_mut();
        tracking.pump();
        let Some((extent, head)) = tracking.confirmed.get(&thread.instance_id) else {
            return false;
        };
        *extent == Extent::of(thread) && Head::of(thread).is_ok_and(|now| now == *head)
    }

    /// Queues saving where a chat is listed, loaded or not. A chat with no
    /// row yet doesn't get one.
    fn save_meta(&self, id: Uuid, placement: Placement) {
        _ = self.jobs.send(Job::Meta(id, placement));
    }

    pub(crate) fn forget(&self, id: Uuid) {
        let mut tracking = self.tracking.borrow_mut();
        tracking.hints.remove(&id);
        tracking.confirmed.remove(&id);
    }

    /// Queues deleting a chat's rows and folder, adding `counted` to what
    /// statistics count of deleted chats.
    fn delete(&self, id: Uuid, counted: &Statistics) {
        self.forget(id);
        match UsageRow::of(counted) {
            Ok(counted) => _ = self.jobs.send(Job::Delete(id, counted)),
            Err(error) => eprintln!("could not delete chat {id}: {error:#}"),
        }
    }

    pub(crate) fn loader(&self) -> ChatLoader {
        let jobs = self.jobs.clone();
        let tracking = self.tracking.clone();
        Rc::new(move |meta: ChatMeta| {
            let (reply, rows) = oneshot::channel();
            _ = jobs.send(Job::Load(meta.id, reply));
            let tracking = tracking.clone();
            Box::pin(async move {
                match rows.await {
                    Ok(Ok(Some(rows))) => {
                        tracking.borrow_mut().hints.insert(meta.id, rows.extent());
                        Some(rows)
                    }
                    Ok(Ok(None)) => None,
                    Ok(Err(error)) => {
                        eprintln!("could not load chat {}: {error:#}", meta.id);
                        None
                    }
                    Err(_) => None,
                }
            })
        })
    }

    /// The saved texts containing `phrase`; see [`SearchHit`]. Runs on
    /// Tokio, on its own connection alongside saves, from when this is
    /// called; await the answer anywhere.
    pub(crate) fn search(
        &self,
        phrase: &str,
    ) -> impl Future<Output = anyhow::Result<Vec<SearchHit>>> + 'static {
        let pool = self.pool.clone();
        let phrase = phrase.to_owned();
        let search = self
            .runtime
            .spawn(async move { sql::search(&pool, &phrase, SEARCH_LIMIT).await });
        async move { search.await? }
    }

    pub(crate) fn quit(self) -> impl Future<Output = ()> + 'static {
        let (done, finished) = oneshot::channel();
        _ = self.jobs.send(Job::Quit(done));
        async move {
            _ = finished.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_save(&self) {
        _ = self.jobs.send(Job::FailNextSave);
    }

    #[cfg(test)]
    pub(crate) async fn flush(&self) {
        let (done, finished) = oneshot::channel();
        _ = self.jobs.send(Job::Flush(done));
        _ = finished.await;
        self.tracking.borrow_mut().pump();
    }
}

/// Queues saving the app's state besides chats, behind every save queued
/// before; held by what keeps that state. What can't be saved is logged.
#[derive(Clone)]
pub(crate) struct StateSaver {
    jobs: mpsc::UnboundedSender<Job>,
}

impl StateSaver {
    fn send(&self, job: anyhow::Result<StateJob>, what: &str) {
        match job {
            Ok(job) => _ = self.jobs.send(Job::State(job)),
            Err(error) => eprintln!("could not save {what}: {error:#}"),
        }
    }

    pub(crate) fn create_project(&self, record: &ProjectRecord) {
        self.send(
            ProjectRow::of(record).map(StateJob::CreateProject),
            "the new project",
        );
    }

    /// Removes the project and, in the same transaction, unfiles its chats.
    pub(crate) fn remove_project(&self, id: ProjectId) {
        self.send(Ok(StateJob::RemoveProject(id.as_uuid())), "");
    }

    pub(crate) fn save_settings(&self, settings: &Settings) {
        self.send(
            SettingsRow::of(settings).map(StateJob::Settings),
            "settings",
        );
    }

    pub(crate) fn save_tasks(&self, tasks: &[ScheduledTask]) {
        let rows = tasks.iter().map(TaskRow::of).collect::<anyhow::Result<_>>();
        self.send(rows.map(StateJob::Tasks), "scheduled tasks");
    }

    /// Adds `counted` to what statistics count of deleted chats.
    pub(crate) fn count_deleted(&self, counted: &Statistics) {
        self.send(
            UsageRow::of(counted).map(StateJob::CountDeleted),
            "deleted chats' statistics",
        );
    }
}

fn lock(dir: &Path) -> anyhow::Result<Option<File>> {
    std::fs::create_dir_all(dir)?;
    let file = File::create(dir.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

async fn write(
    pool: SqlitePool,
    root: PathBuf,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    reports: mpsc::UnboundedSender<Report>,
) {
    // Set only by a test's `Job::FailNextSave`.
    let fail_next_save = std::cell::Cell::new(false);
    while let Some(job) = jobs.recv().await {
        match job {
            Job::Save(job) => {
                let id = job.head.id;
                let report = match save(&pool, &job, fail_next_save.take()).await {
                    Ok(()) => Report::Saved {
                        id,
                        end: job.end,
                        head: Box::new(job.head),
                    },
                    Err(error) => {
                        eprintln!("could not save chat {id}: {error:#}");
                        let actual = match error.downcast_ref::<Mismatch>() {
                            Some(Mismatch::Behind(actual) | Mismatch::Ahead(actual)) => {
                                Some(*actual)
                            }
                            None => match pool.acquire().await {
                                Ok(mut conn) => sql::extent(&mut conn, id).await.ok(),
                                Err(_) => None,
                            },
                        };
                        Report::Failed { id, actual }
                    }
                };
                _ = reports.send(report);
            }
            Job::Meta(id, placement) => {
                let result = async {
                    let mut conn = pool.acquire().await?;
                    sql::update_placement(&mut conn, id, &placement).await
                }
                .await;
                if let Err(error) = result {
                    eprintln!("could not save where chat {id} is listed: {error:#}");
                }
            }
            Job::Delete(id, counted) => {
                let result = async {
                    let mut tx = pool.begin().await?;
                    sql::delete(&mut tx, id).await?;
                    app_state::count_deleted(&mut tx, &counted).await?;
                    tx.commit().await?;
                    anyhow::Ok(())
                }
                .await;
                if let Err(error) = result {
                    eprintln!("could not delete chat {id}: {error:#}");
                }
                remove_dir(&root.join(id.to_string()));
            }
            Job::State(job) => {
                let result = async {
                    let mut tx = pool.begin().await?;
                    app_state::write(&mut tx, &job).await?;
                    tx.commit().await?;
                    anyhow::Ok(())
                }
                .await;
                if let Err(error) = result {
                    eprintln!("could not save {}: {error:#}", job.what());
                }
            }
            Job::Load(id, reply) => {
                let result = async {
                    let mut tx = pool.begin().await?;
                    sql::load(&mut tx, id).await
                }
                .await;
                _ = reply.send(result);
            }
            Job::Quit(done) => {
                _ = done.send(());
                break;
            }
            #[cfg(test)]
            Job::Flush(done) => _ = done.send(()),
            #[cfg(test)]
            Job::FailNextSave => fail_next_save.set(true),
        }
    }
}

/// Writes `job`'s rows past what the database holds, or refuses it; see the
/// module documentation. `fail` fails it midway, for tests.
async fn save(pool: &SqlitePool, job: &SaveJob, fail: bool) -> anyhow::Result<()> {
    let id = job.head.id;
    let mut tx = pool.begin().await?;
    let actual = sql::extent(&mut tx, id).await?;
    if !job.from.within(actual) {
        return Err(Mismatch::Behind(actual).into());
    }
    if !actual.within(job.end) {
        return Err(Mismatch::Ahead(actual).into());
    }
    sql::write_head(&mut tx, &job.head, &job.placement).await?;
    let skip = actual.messages - job.from.messages;
    for (index, message) in job.tail.messages.iter().enumerate().skip(skip) {
        sql::insert_message(&mut tx, id, job.from.messages + index, message).await?;
    }
    anyhow::ensure!(!fail, "failed for a test");
    let from = job.from.transcript;
    let skip = actual.transcript.entries - from.entries;
    for (index, entry) in job.tail.entries.iter().enumerate().skip(skip) {
        sql::insert_entry(&mut tx, id, from.entries + index, entry).await?;
    }
    let skip = actual.transcript.compactions - from.compactions;
    for (index, (at, compaction)) in job.tail.compactions.iter().enumerate().skip(skip) {
        sql::insert_compaction(&mut tx, id, from.compactions + index, *at, compaction).await?;
    }
    for (file, bytes) in &job.tail.files {
        sql::insert_file(&mut tx, id, *file, bytes).await?;
    }
    tx.commit().await?;
    Ok(())
}

fn remove_dir(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("could not remove {}: {error}", path.display());
    }
}

impl Cowork {
    /// Saves with `db` from now on, starting from what it held: lists its
    /// chats unloaded in their saved order and takes up its projects,
    /// settings, deleted chats' statistics and scheduled tasks.
    pub(crate) fn start_chat_db(&mut self, db: ChatDb, saved: Saved, cx: &mut Context<Self>) {
        let Saved { chats, state } = saved;
        self.profile_statistics = None;
        let loader = db.loader();
        self.thread_store.update(cx, |store, cx| {
            store.set_loader(loader);
            for row in chats {
                store.push_back_unloaded(row.meta, row.archived, cx);
            }
        });
        let saver = db.saver();
        self.projects = crate::projects::Projects::new(state.projects, Some(saver.clone()));
        self.deleted = state.deleted;
        self.scheduler.start_saving(state.tasks, saver);
        self.chat_db = Some(db);
        self.start_settings(state.settings, cx);
        cx.on_app_quit(|this, cx| {
            this.save_all_threads(cx);
            let finished = this.chat_db.take().map(ChatDb::quit);
            async move {
                if let Some(finished) = finished {
                    finished.await;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Saves `thread` as it is now, if it is one of the user's own and not
    /// running; see the module documentation for when.
    pub(crate) fn save_thread(&mut self, thread: &Entity<Thread>, cx: &App) {
        self.save_chat(thread.read(cx).instance_id, cx);
    }

    /// Saves a listed chat: whole if loaded, one of the user's own, and not
    /// running; only where it is listed if it isn't loaded.
    pub(crate) fn save_chat(&mut self, id: Uuid, cx: &App) {
        let Some(db) = &self.chat_db else {
            return;
        };
        let store = self.thread_store.read(cx);
        // Not listed: deleted.
        let Some((place, chat)) = store.listed(id, cx) else {
            return;
        };
        let (archived, position) = match place {
            Place::Recents(position) => (false, position),
            Place::Archived(position) => (true, position),
        };
        let placement = match Placement::new(archived, position, chat.project(cx)) {
            Ok(placement) => placement,
            Err(error) => {
                eprintln!("could not save chat {id}: {error:#}");
                return;
            }
        };
        match chat {
            ListedChat::Loaded(thread) => {
                let state = thread.read(cx);
                if state.ownership() != ThreadOwnership::Local || state.is_generating() {
                    return;
                }
                if let Err(error) = db.save(state, placement) {
                    eprintln!("could not save chat {id}: {error:#}");
                }
            }
            ListedChat::Unloaded(_) => db.save_meta(id, placement),
        }
    }

    /// Saves every chat of the user's own not running, as when Cowork quits,
    /// which also keeps their order.
    pub(crate) fn save_all_threads(&mut self, cx: &App) {
        let store = self.thread_store.read(cx);
        let ids = store.all(cx).map(|chat| chat.id(cx)).collect::<Vec<_>>();
        for id in ids {
            self.save_chat(id, cx);
        }
    }

    pub(crate) fn delete_saved_chat(&mut self, thread_id: Uuid, counted: &Statistics) {
        if let Some(db) = &self.chat_db {
            db.delete(thread_id, counted);
        }
    }
}

#[cfg(test)]
pub(crate) struct TestDb {
    root: PathBuf,
    runtime: tokio::runtime::Runtime,
}

#[cfg(test)]
impl TestDb {
    pub(crate) fn open() -> (Self, ChatDb) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let root = std::env::temp_dir().join(format!("cowork-{}", Uuid::new_v4()));
        let (db, _) = ChatDb::open(root.clone(), runtime.handle()).expect("a test database");
        (Self { root, runtime }, db)
    }

    pub(crate) fn reopen(&self) -> (ChatDb, Saved) {
        ChatDb::open(self.root.clone(), self.runtime.handle()).expect("a test database")
    }
}

#[cfg(test)]
impl Drop for TestDb {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests;
