//! What a saved chat's `threads` row holds ([`ChatMeta`]), which the store
//! lists while the chat isn't loaded, and how the rest of it is loaded
//! ([`ChatLoader`]; the real one is `ChatDb::loader`).

#[cfg(test)]
use std::{cell::RefCell, collections::HashMap};
use std::{rc::Rc, time::Duration, time::SystemTime};

use futures::future::LocalBoxFuture;
use uuid::Uuid;

use crate::{chat_db::ChatRows, projects::ProjectId, thread::Thread, usage::TokenActivity};

/// A saved chat's `threads` row: everything the sidebar, archive, schedules
/// and statistics read. Where it is listed (recents or archive, and its
/// position) is where the store keeps it, so it isn't repeated here.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChatMeta {
    pub(crate) id: Uuid,
    pub(crate) title: String,
    pub(crate) project: Option<ProjectId>,
    pub(crate) tokens_used: u64,
    pub(crate) generation_time: Duration,
    pub(crate) last_run_at: Option<SystemTime>,
    pub(crate) token_activity: Vec<TokenActivity>,
}

impl ChatMeta {
    pub(crate) fn of(thread: &Thread) -> Self {
        Self {
            id: thread.instance_id,
            title: thread.summary.title.clone(),
            project: thread.project(),
            tokens_used: thread.tokens_used,
            generation_time: thread.generation_time(),
            last_run_at: thread.last_activity(),
            token_activity: thread.token_activity.clone(),
        }
    }
}

/// Reads a chat's rows. Loads go through the persistence layer's one job
/// queue, so a load never races a save or delete queued before it.
pub(crate) type ChatLoader = Rc<dyn Fn(ChatMeta) -> LocalBoxFuture<'static, Option<ChatRows>>>;

/// Without a database (ephemeral, and most tests) nothing is unloaded, so
/// nothing is loaded later. A load that does happen finds nothing.
pub(crate) fn no_loader() -> ChatLoader {
    Rc::new(|_| Box::pin(async { None }))
}

#[cfg(test)]
type PendingLoad = (ChatMeta, futures::channel::oneshot::Sender<ChatRows>);

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct ManualLoader {
    pending: Rc<RefCell<HashMap<Uuid, Vec<PendingLoad>>>>,
}

#[cfg(test)]
impl ManualLoader {
    pub(crate) fn loader(&self) -> ChatLoader {
        let pending = self.pending.clone();
        Rc::new(move |meta: ChatMeta| {
            let (done, finished) = futures::channel::oneshot::channel();
            pending
                .borrow_mut()
                .entry(meta.id)
                .or_default()
                .push((meta, done));
            Box::pin(async move { finished.await.ok() })
        })
    }

    pub(crate) fn started(&self, id: Uuid) -> usize {
        self.pending.borrow().get(&id).map_or(0, Vec::len)
    }

    /// Whether every started load of `id` was cancelled: its result can no
    /// longer be delivered.
    pub(crate) fn cancelled(&self, id: Uuid) -> bool {
        self.pending
            .borrow()
            .get(&id)
            .is_some_and(|loads| loads.iter().all(|(_, done)| done.is_canceled()))
    }

    pub(crate) fn finish(&self, id: Uuid) {
        let mut pending = self.pending.borrow_mut();
        let loads = pending.get_mut(&id).expect("a load of this chat");
        let (meta, done) = loads.remove(0);
        // The receiver is gone if the load was cancelled.
        _ = done.send(ChatRows::of_meta(&meta));
    }
}
