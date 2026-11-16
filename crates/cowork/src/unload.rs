//! Unloading chats nobody has needed for a while: when a loaded chat may go
//! back to its saved row, and the only way to make it.
//!
//! A chat may be unloaded only when everything it holds that isn't saved can
//! be dropped: see "Unloading" in `docs/sqlite-storage.md`. The rule is
//! derived from existing state, never from flags kept beside it. The
//! thread's own fields are judged by [`Thread::unload_blocker`], whose
//! exhaustive destructure makes a new field fail to compile until it is
//! classified; the per-chat state on `Cowork` is judged here.
//!
//! "Idle" is measured by the sweep itself: a chat's clock starts the first
//! time a sweep finds nothing keeping it loaded, and any blocker resets it.
//! So no code path has to remember to touch a timestamp, and a chat is
//! unloaded after between [`UNLOAD_AFTER`] and one [`SWEEP_EVERY`] more of
//! being continuously idle.
//!
//! A chat goes back to its row only once everything it holds is confirmed
//! saved (`ChatDb::is_saved`). An eligible chat that isn't is saved by the
//! sweep and unloaded by a later one, if it is still eligible and nothing
//! changed meanwhile. A failed save just leaves it loaded. Without a
//! database nothing is unloaded.
//!
//! [`ThreadStore::unload`] takes an [`Idle`], which only this module makes,
//! and only from a check passed in the same synchronous update, so nothing
//! can unload a chat that isn't eligible.
//!
//! [`ThreadStore::unload`]: crate::chat_store::ThreadStore::unload

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use gpui::{App, Context, Entity, Task};
use uuid::Uuid;

use crate::{Cowork, thread::Thread};

/// How long a chat stays loaded once nothing needs it.
pub(crate) const UNLOAD_AFTER: Duration = Duration::from_secs(5 * 60);
/// How often loaded chats are checked.
pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Why a chat stays loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnloadBlocker {
    /// Showing.
    Active,
    /// Someone else's: never saved here.
    Joined,
    /// Being shared, or hosting: peers hold its live state.
    Shared,
    /// Running, compacting, awaiting approval, or a run still being stopped.
    Running,
    /// Submissions waiting for the run, which aren't saved.
    PendingSubmissions,
    /// A draft with something in it, which isn't saved.
    DraftNotEmpty,
    /// File bytes still being sent or received.
    Transferring,
    /// Files still being read into its draft.
    ReadingAttachments,
    /// A scheduled task's run continuing it is queued.
    QueuedScheduledRun,
    /// What it holds isn't confirmed saved yet; the sweep saves it.
    Unsaved,
    /// Nothing blocks it, but not for [`UNLOAD_AFTER`] yet.
    NotIdleLongEnough,
}

/// Proof that `thread` passed every check in this update. Only this module
/// makes one, and it borrows the thread, so it can't be kept for later.
pub(crate) struct Idle<'a> {
    thread: &'a Entity<Thread>,
}

impl Idle<'_> {
    pub(crate) fn thread(&self) -> &Entity<Thread> {
        self.thread
    }
}

/// When each loaded chat was first found idle, continuously since.
#[derive(Default)]
pub(crate) struct IdleClock {
    since: HashMap<Uuid, Instant>,
    _sweeping: Option<Task<()>>,
}

impl Cowork {
    /// Checks loaded chats once every [`SWEEP_EVERY`].
    pub(crate) fn start_unload_sweep(&mut self, cx: &mut Context<Self>) {
        self.idle_clock._sweeping = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SWEEP_EVERY).await;
                if this
                    .update(cx, |this, cx| this.sweep_idle_threads(Instant::now(), cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Why `thread` must stay loaded right now, however long it was idle.
    pub(crate) fn unload_blocker(
        &self,
        thread: &Entity<Thread>,
        cx: &App,
    ) -> Option<UnloadBlocker> {
        let state = thread.read(cx);
        let id = state.instance_id;
        if self.active_thread_id(cx) == Some(id) {
            return Some(UnloadBlocker::Active);
        }
        if let Some(blocker) = state.unload_blocker() {
            return Some(blocker);
        }
        // A stopped run clears `active_run` at once, but its generation
        // stays until the session reports the end, and then saves.
        if self.active_generations.contains_key(&id)
            || self.scheduler.run.is_some_and(|run| run.thread_id == id)
        {
            return Some(UnloadBlocker::Running);
        }
        let draft_id = state.draft().id;
        if self
            .pending_attachments
            .iter()
            .any(|pending| pending.draft_id == draft_id)
        {
            return Some(UnloadBlocker::ReadingAttachments);
        }
        if self
            .scheduler
            .tasks_continuing(id)
            .iter()
            .any(|task| task.queued().is_some())
        {
            return Some(UnloadBlocker::QueuedScheduledRun);
        }
        None
    }

    /// The proof that `thread` may be unloaded at `now`, advancing its idle
    /// clock, or why it may not.
    fn idle<'a>(
        &mut self,
        thread: &'a Entity<Thread>,
        now: Instant,
        cx: &App,
    ) -> Result<Idle<'a>, UnloadBlocker> {
        let id = thread.read(cx).instance_id;
        if let Some(blocker) = self.unload_blocker(thread, cx) {
            self.idle_clock.since.remove(&id);
            return Err(blocker);
        }
        let since = *self.idle_clock.since.entry(id).or_insert(now);
        if now.duration_since(since) < UNLOAD_AFTER {
            return Err(UnloadBlocker::NotIdleLongEnough);
        }
        let Some(db) = &self.chat_db else {
            return Err(UnloadBlocker::Unsaved);
        };
        if !db.is_saved(thread.read(cx)) {
            // Folder, mode and model changes aren't saved when made, and a
            // save may have failed. This one's outcome is known by the next
            // sweep.
            self.save_thread(thread, cx);
            return Err(UnloadBlocker::Unsaved);
        }
        Ok(Idle { thread })
    }

    /// Unloads every chat idle for [`UNLOAD_AFTER`] by `now`, returning
    /// their ids.
    pub(crate) fn sweep_idle_threads(&mut self, now: Instant, cx: &mut Context<Self>) -> Vec<Uuid> {
        let threads = self.thread_store.read(cx).loaded_all(cx);
        let listed: std::collections::HashSet<Uuid> = threads
            .iter()
            .map(|thread| thread.read(cx).instance_id)
            .collect();
        self.idle_clock.since.retain(|id, _| listed.contains(id));
        let mut unloaded = Vec::new();
        for thread in &threads {
            if let Ok(idle) = self.idle(thread, now, cx) {
                unloaded.push(self.unload(idle, cx));
            }
        }
        drop(threads);
        // Nothing may keep an unloaded chat alive: whatever did could change
        // it where no save would ever see. Checked once this update's
        // effects, which may hold entities, have run.
        let (ids, weaks): (Vec<_>, Vec<_>) = unloaded.into_iter().unzip();
        if !ids.is_empty() {
            cx.defer(move |_| {
                for weak in weaks {
                    debug_assert!(
                        weak.upgrade().is_none(),
                        "an unloaded chat is still held somewhere"
                    );
                }
            });
            cx.notify();
        }
        ids
    }

    /// Drops `idle`'s chat, confirmed saved, and everything kept for it.
    fn unload(
        &mut self,
        idle: Idle<'_>,
        cx: &mut Context<Self>,
    ) -> (Uuid, gpui::WeakEntity<Thread>) {
        let thread = idle.thread().clone();
        let (id, draft_id) = {
            let state = thread.read(cx);
            (state.instance_id, state.draft().id)
        };
        self.retire_agent(id);
        self.sandboxes.forget(&id.to_string());
        self.published_presence.remove(&id);
        self.segment_text_views
            .retain(|(message, _), _| message.thread_id != id);
        self.shown_segments
            .retain(|message, _| message.thread_id != id);
        self.attachment_errors
            .retain(|error| error.draft_id != draft_id);
        if self.copied_endpoint_id == Some(id) {
            self.copied_endpoint_id = None;
        }
        if let Some(db) = &self.chat_db {
            db.forget(id);
        }
        self.idle_clock.since.remove(&id);
        let weak = thread.downgrade();
        drop(thread);
        self.thread_store
            .update(cx, |store, cx| store.unload(idle, cx));
        (id, weak)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use draft::DraftItem;
    use gpui::{AppContext as _, TestAppContext};

    use super::*;
    use crate::{
        chat_db::TestDb,
        participant::ParticipantId,
        thread::{ThreadSharing, submission::ActiveGeneration},
        thread_draft::ThreadDraft,
    };

    fn local_thread(cx: &mut App) -> Entity<Thread> {
        let author = ParticipantId::new();
        cx.new(|_| {
            Thread::new_local(
                "Chat".into(),
                Vec::new(),
                ThreadDraft::new(author),
                author,
                Arc::default(),
                None,
            )
        })
    }

    #[gpui::test]
    fn an_idle_draft_blocks_only_while_it_has_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let thread = local_thread(cx);
            assert_eq!(thread.read(cx).unload_blocker(), None);
            let author = thread.read(cx).participant_id().as_uuid();
            let item = thread.update(cx, |thread, _| {
                thread.draft_mut_for_test().create_prompt(author, "  ")
            });
            // Whitespace alone is nothing to lose.
            assert_eq!(thread.read(cx).unload_blocker(), None);
            thread.update(cx, |thread, _| {
                thread.draft_mut_for_test().set_body(item, "hello");
            });
            assert!(
                !thread
                    .read(cx)
                    .draft()
                    .items()
                    .iter()
                    .all(DraftItem::is_empty)
            );
            assert_eq!(
                thread.read(cx).unload_blocker(),
                Some(UnloadBlocker::DraftNotEmpty)
            );
        });
    }

    #[gpui::test]
    fn sharing_blocks_until_it_fails(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let thread = local_thread(cx);
            thread.update(cx, |thread, _| thread.sharing = ThreadSharing::Sharing);
            assert_eq!(
                thread.read(cx).unload_blocker(),
                Some(UnloadBlocker::Shared)
            );
            thread.update(cx, |thread, _| thread.sharing = ThreadSharing::Failed);
            assert_eq!(thread.read(cx).unload_blocker(), None);
        });
    }

    /// The Cowork-side rules, and the sweep's clock, on one app.
    #[gpui::test]
    fn the_sweep_unloads_only_chats_idle_for_the_whole_wait(cx: &mut TestAppContext) {
        let (cowork, _runtime, cx) = crate::tests::composer_test_cowork(cx);
        let (_test_db, db) = TestDb::open();
        let start = Instant::now();
        let (active, running, idle) = cx.update(|_, cx| {
            let store = cowork.read(cx).thread_store.clone();
            let threads: Vec<_> = (0..3).map(|_| local_thread(cx)).collect();
            store.update(cx, |store, cx| {
                for thread in &threads {
                    store.push_back_loaded(thread.clone(), cx);
                }
            });
            let ids: Vec<Uuid> = threads.iter().map(|t| t.read(cx).instance_id).collect();
            store.update(cx, |store, cx| assert!(store.activate(ids[0], cx)));
            cowork.update(cx, |cowork, _| {
                cowork.chat_db = Some(db);
                cowork.active_generations.insert(
                    ids[1],
                    ActiveGeneration::for_test(Uuid::new_v4(), Default::default()),
                );
            });
            (ids[0], ids[1], ids[2])
        });
        let blocker = |id: Uuid, cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                let cowork = cowork.read(cx);
                let thread = cowork.thread_store.read(cx).loaded(id, cx)?;
                cowork.unload_blocker(&thread, cx)
            })
        };
        assert_eq!(blocker(active, cx), Some(UnloadBlocker::Active));
        assert_eq!(blocker(running, cx), Some(UnloadBlocker::Running));
        assert_eq!(blocker(idle, cx), None);

        let sweep = |at: Instant, cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| cowork.update(cx, |cowork, cx| cowork.sweep_idle_threads(at, cx)))
        };
        let flush = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                futures::executor::block_on(cowork.read(cx).chat_db.as_ref().unwrap().flush())
            })
        };
        // The first sweep starts the clock.
        assert_eq!(sweep(start, cx), Vec::<Uuid>::new());
        assert_eq!(
            sweep(start + UNLOAD_AFTER - SWEEP_EVERY, cx),
            Vec::<Uuid>::new()
        );
        // Opening the chat in between restarts its clock.
        let activate = |id: Uuid, cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                let store = cowork.read(cx).thread_store.clone();
                store.update(cx, |store, cx| assert!(store.activate(id, cx)));
            })
        };
        activate(idle, cx);
        assert_eq!(sweep(start + UNLOAD_AFTER, cx), Vec::<Uuid>::new());
        activate(active, cx);
        assert_eq!(
            sweep(start + UNLOAD_AFTER + SWEEP_EVERY, cx),
            Vec::<Uuid>::new()
        );
        // Idle long enough, but never saved: this sweep saves it.
        let later = start + 2 * UNLOAD_AFTER + SWEEP_EVERY;
        assert_eq!(sweep(later, cx), Vec::<Uuid>::new());
        flush(cx);
        assert_eq!(sweep(later + SWEEP_EVERY, cx), vec![idle]);
        cx.update(|_, cx| {
            let store = cowork.read(cx).thread_store.read(cx);
            assert!(store.loaded(idle, cx).is_none());
            assert!(store.listed(idle, cx).is_some(), "still listed, by its row");
            for id in [active, running] {
                assert!(store.loaded(id, cx).is_some());
            }
        });
    }

    /// Changing a chat's mode doesn't save it; only turn boundaries do. So
    /// the sweep saves it, and unloads it only once that save is confirmed:
    /// a failed one leaves it loaded.
    #[gpui::test]
    fn a_chat_unloads_only_once_its_changes_are_saved(cx: &mut TestAppContext) {
        use crate::project_mode::ProjectMode;
        let (cowork, _runtime, cx) = crate::tests::composer_test_cowork(cx);
        let (test_db, db) = TestDb::open();
        let start = Instant::now();
        let id = cx.update(|_, cx| {
            let thread = local_thread(cx);
            cowork.update(cx, |cowork, cx| {
                cowork.chat_db = Some(db);
                let store = cowork.thread_store.clone();
                store.update(cx, |store, cx| store.push_back_loaded(thread.clone(), cx));
                // Its last turn boundary.
                cowork.save_thread(&thread, cx);
                futures::executor::block_on(cowork.chat_db.as_ref().unwrap().flush());
                cowork.set_project_mode(Some(thread.downgrade()), ProjectMode::Network, cx);
                let id = thread.read(cx).instance_id;
                // Holding it would trip the sweep's check that nothing does.
                drop(thread);
                cowork.sweep_idle_threads(start, cx);
                let db = cowork.chat_db.as_ref().unwrap();
                db.fail_next_save();
                assert!(
                    cowork
                        .sweep_idle_threads(start + UNLOAD_AFTER, cx)
                        .is_empty()
                );
                futures::executor::block_on(cowork.chat_db.as_ref().unwrap().flush());
                // The save failed: still loaded, and saved again.
                assert!(
                    cowork
                        .sweep_idle_threads(start + UNLOAD_AFTER, cx)
                        .is_empty()
                );
                futures::executor::block_on(cowork.chat_db.as_ref().unwrap().flush());
                assert_eq!(cowork.sweep_idle_threads(start + UNLOAD_AFTER, cx), [id]);
                futures::executor::block_on(cowork.chat_db.take().unwrap().quit());
                id
            })
        });
        cx.update(|_, cx| {
            let (db, saved) = test_db.reopen();
            let rows = saved.chats;
            assert_eq!(rows.iter().map(|row| row.meta.id).collect::<Vec<_>>(), [id]);
            let meta = rows.into_iter().next().unwrap().meta;
            let rows = futures::executor::block_on((db.loader())(meta)).unwrap();
            let thread = rows.into_thread(Default::default(), cx).unwrap();
            assert_eq!(thread.project_mode(), ProjectMode::Network);
            futures::executor::block_on(db.quit());
        });
    }

    /// A menu or folder picker keeps a weak handle to the chat it was
    /// opened for. Once that chat is unloaded, its choice must not land on
    /// the new chat's draft instead.
    #[gpui::test]
    fn a_choice_for_an_unloaded_chat_goes_nowhere_else(cx: &mut TestAppContext) {
        let (cowork, _runtime, cx) = crate::tests::composer_test_cowork(cx);
        let (_test_db, db) = TestDb::open();
        let start = Instant::now();
        let target = cx.update(|_, cx| {
            let thread = local_thread(cx);
            cowork.update(cx, |cowork, cx| {
                cowork.chat_db = Some(db);
                let store = cowork.thread_store.clone();
                store.update(cx, |store, cx| store.push_back_loaded(thread.clone(), cx));
                cowork.save_thread(&thread, cx);
                futures::executor::block_on(cowork.chat_db.as_ref().unwrap().flush());
            });
            thread.downgrade()
        });
        cx.update(|_, cx| {
            cowork.update(cx, |cowork, cx| {
                cowork.sweep_idle_threads(start, cx);
                assert_eq!(cowork.sweep_idle_threads(start + UNLOAD_AFTER, cx).len(), 1);
                cowork.add_project_folders(
                    Some(target.clone()),
                    vec![std::path::PathBuf::from("/a/src")],
                    cx,
                );
                cowork.set_project_mode(
                    Some(target),
                    crate::project_mode::ProjectMode::Network,
                    cx,
                );
                assert!(cowork.new_thread_project_folders.is_empty());
                assert_eq!(
                    cowork.new_thread_project_mode,
                    crate::project_mode::ProjectMode::default()
                );
            })
        });
    }
}
