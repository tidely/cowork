//! The chats the sidebar and archive list: each is an
//! `Entity<Chat>` with a stable identity, holding its row while unloaded,
//! its load while loading, and the `Thread` once loaded.
//!
//! Invariants:
//! - a chat's load lives in its `Loading` state, so there is at most one,
//!   and dropping the chat (deleting it) cancels it; a finished load fills
//!   only the chat that started it, and only while it is `Loading`;
//! - the active chat is listed in the recents and loaded (private fields);
//! - a chat's metadata has one copy: the row while unloaded or loading,
//!   the `Thread` once loaded.

use std::collections::VecDeque;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, EventEmitter, Task, WeakEntity};
use uuid::Uuid;

use crate::{
    chat_meta::{ChatLoader, ChatMeta},
    models::ModelCatalog,
    projects::ProjectId,
    thread::Thread,
    unload::Idle,
};

enum ChatState {
    Unloaded(ChatMeta),
    Loading { meta: ChatMeta, _load: Task<()> },
    Loaded(Entity<Thread>),
}

/// One listed chat. Only this module changes its state.
pub(crate) struct Chat {
    id: Uuid,
    state: ChatState,
}

impl Chat {
    pub(crate) fn view(&self) -> ListedChat<'_> {
        match &self.state {
            ChatState::Loaded(thread) => ListedChat::Loaded(thread),
            ChatState::Unloaded(meta) | ChatState::Loading { meta, .. } => {
                ListedChat::Unloaded(meta)
            }
        }
    }

    /// Starts loading, unless loaded or loading already.
    fn load(
        &mut self,
        loader: &ChatLoader,
        store: WeakEntity<ThreadStore>,
        models: Arc<ModelCatalog>,
        cx: &mut Context<Self>,
    ) {
        let ChatState::Unloaded(meta) = &self.state else {
            return;
        };
        let meta = meta.clone();
        let rows = loader(meta.clone());
        let load = cx.spawn(async move |chat, cx| {
            let rows = rows.await;
            // Fails if the chat was dropped; then this task was too.
            _ = chat.update(cx, |chat, cx| {
                let ChatState::Loading { meta, .. } = &chat.state else {
                    return;
                };
                let id = chat.id;
                chat.state = match rows.map(|rows| rows.into_thread((*models).clone(), cx)) {
                    Some(Ok(mut thread)) => {
                        // Its listing is newer than rows read behind earlier
                        // jobs: it may have left a project since.
                        thread.set_project(meta.project);
                        ChatState::Loaded(cx.new(|_| thread))
                    }
                    Some(Err(error)) => {
                        eprintln!("could not restore chat {id}: {error:#}; left as it is");
                        ChatState::Unloaded(meta.clone())
                    }
                    None => ChatState::Unloaded(meta.clone()),
                };
                let loaded = matches!(chat.state, ChatState::Loaded(_));
                let chat = cx.entity();
                cx.defer(move |cx| {
                    _ = store.update(cx, |store, cx| store.chat_loaded(id, &chat, loaded, cx));
                });
            });
        });
        self.state = ChatState::Loading { meta, _load: load };
    }
}

/// A listed chat as callers see it: loaded, or only its row (also while
/// loading).
#[derive(Clone, Copy)]
pub(crate) enum ListedChat<'a> {
    Loaded(&'a Entity<Thread>),
    Unloaded(&'a ChatMeta),
}

impl<'a> ListedChat<'a> {
    pub(crate) fn loaded(self) -> Option<&'a Entity<Thread>> {
        match self {
            Self::Loaded(thread) => Some(thread),
            Self::Unloaded(_) => None,
        }
    }

    pub(crate) fn id(self, cx: &App) -> Uuid {
        match self {
            Self::Loaded(thread) => thread.read(cx).instance_id,
            Self::Unloaded(meta) => meta.id,
        }
    }

    pub(crate) fn title(self, cx: &'a App) -> &'a str {
        match self {
            Self::Loaded(thread) => &thread.read(cx).summary.title,
            Self::Unloaded(meta) => &meta.title,
        }
    }

    pub(crate) fn project(self, cx: &App) -> Option<ProjectId> {
        match self {
            Self::Loaded(thread) => thread.read(cx).project(),
            Self::Unloaded(meta) => meta.project,
        }
    }

    pub(crate) fn meta(self, cx: &App) -> ChatMeta {
        match self {
            Self::Loaded(thread) => ChatMeta::of(thread.read(cx)),
            Self::Unloaded(meta) => meta.clone(),
        }
    }

    pub(crate) fn is_own(self, cx: &App) -> bool {
        match self {
            Self::Loaded(thread) => {
                thread.read(cx).ownership() == crate::thread::ThreadOwnership::Local
            }
            Self::Unloaded(_) => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    Recents(usize),
    Archived(usize),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Opened {
    Active,
    Loading,
    Missing,
}

pub(crate) enum ChatStoreEvent {
    Activated,
    Loaded,
}

pub(crate) struct ThreadStore {
    recents: VecDeque<Entity<Chat>>,
    archived: VecDeque<Entity<Chat>>,
    active: Option<Uuid>,
    /// The chat opened last, while it loads: by identity, so a chat
    /// removed meanwhile can't match.
    opening: Option<WeakEntity<Chat>>,
    loader: ChatLoader,
}

impl Default for ThreadStore {
    fn default() -> Self {
        Self {
            recents: VecDeque::new(),
            archived: VecDeque::new(),
            active: None,
            opening: None,
            loader: crate::chat_meta::no_loader(),
        }
    }
}

impl EventEmitter<ChatStoreEvent> for ThreadStore {}

fn chat_of(thread: Entity<Thread>, cx: &mut App) -> Entity<Chat> {
    let id = thread.read(cx).instance_id;
    cx.new(|_| Chat {
        id,
        state: ChatState::Loaded(thread),
    })
}

impl ThreadStore {
    #[cfg(test)]
    pub(crate) fn with_recents(
        threads: impl IntoIterator<Item = Entity<Thread>>,
        cx: &mut App,
    ) -> Self {
        Self {
            recents: threads
                .into_iter()
                .map(|thread| chat_of(thread, cx))
                .collect(),
            ..Default::default()
        }
    }

    pub(crate) fn set_loader(&mut self, loader: ChatLoader) {
        self.loader = loader;
    }

    pub(crate) fn recents<'a>(
        &'a self,
        cx: &'a App,
    ) -> impl DoubleEndedIterator<Item = ListedChat<'a>> {
        self.recents.iter().map(|chat| chat.read(cx).view())
    }

    pub(crate) fn archived<'a>(
        &'a self,
        cx: &'a App,
    ) -> impl DoubleEndedIterator<Item = ListedChat<'a>> {
        self.archived.iter().map(|chat| chat.read(cx).view())
    }

    pub(crate) fn all<'a>(&'a self, cx: &'a App) -> impl Iterator<Item = ListedChat<'a>> {
        self.recents
            .iter()
            .chain(&self.archived)
            .map(|chat| chat.read(cx).view())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.recents.is_empty() && self.archived.is_empty()
    }

    pub(crate) fn loaded_recents<'a>(
        &'a self,
        cx: &'a App,
    ) -> impl Iterator<Item = &'a Entity<Thread>> {
        self.recents(cx).filter_map(ListedChat::loaded)
    }

    fn find(&self, id: Uuid, cx: &App) -> Option<(Place, &Entity<Chat>)> {
        if let Some(index) = self.recents.iter().position(|chat| chat.read(cx).id == id) {
            return Some((Place::Recents(index), &self.recents[index]));
        }
        let index = self
            .archived
            .iter()
            .position(|chat| chat.read(cx).id == id)?;
        Some((Place::Archived(index), &self.archived[index]))
    }

    pub(crate) fn listed<'a>(&'a self, id: Uuid, cx: &'a App) -> Option<(Place, ListedChat<'a>)> {
        self.find(id, cx)
            .map(|(place, chat)| (place, chat.read(cx).view()))
    }

    pub(crate) fn loaded(&self, id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        match self.listed(id, cx)? {
            (Place::Recents(_), chat) => chat.loaded().cloned(),
            (Place::Archived(_), _) => None,
        }
    }

    pub(crate) fn loaded_archived(&self, id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        match self.listed(id, cx)? {
            (Place::Archived(_), chat) => chat.loaded().cloned(),
            (Place::Recents(_), _) => None,
        }
    }

    pub(crate) fn active_id(&self) -> Option<Uuid> {
        self.active
    }

    pub(crate) fn active(&self, cx: &App) -> Option<Entity<Thread>> {
        let thread = self.active.and_then(|id| self.loaded(id, cx));
        debug_assert_eq!(thread.is_some(), self.active.is_some());
        thread
    }

    #[cfg(test)]
    pub(crate) fn activate(&mut self, id: Uuid, cx: &App) -> bool {
        let found = self.loaded(id, cx).is_some();
        if found {
            self.active = Some(id);
            self.opening = None;
        }
        found
    }

    pub(crate) fn clear_active(&mut self) {
        self.active = None;
        self.opening = None;
    }

    pub(crate) fn push_front(&mut self, thread: Entity<Thread>, cx: &mut App) {
        let chat = chat_of(thread, cx);
        self.recents.push_front(chat);
    }

    pub(crate) fn push_front_active(&mut self, thread: Entity<Thread>, cx: &mut App) {
        let id = thread.read(cx).instance_id;
        self.push_front(thread, cx);
        self.active = Some(id);
        self.opening = None;
    }

    pub(crate) fn push_back_unloaded(&mut self, meta: ChatMeta, archived: bool, cx: &mut App) {
        let chat = cx.new(|_| Chat {
            id: meta.id,
            state: ChatState::Unloaded(meta),
        });
        if archived {
            self.archived.push_back(chat);
        } else {
            self.recents.push_back(chat);
        }
    }

    fn take(&mut self, id: Uuid, cx: &App) -> Option<(Place, Entity<Chat>)> {
        let (place, _) = self.find(id, cx)?;
        let chat = match place {
            Place::Recents(index) => self.recents.remove(index),
            Place::Archived(index) => self.archived.remove(index),
        }?;
        if self.active == Some(id) {
            self.active = None;
        }
        Some((place, chat))
    }

    /// Takes a chat off either list for good. Its row, derived if it was
    /// loaded; dropping the chat cancels its load.
    pub(crate) fn remove(&mut self, id: Uuid, cx: &App) -> Option<ChatMeta> {
        let (_, chat) = self.take(id, cx)?;
        let meta = chat.read(cx).view().meta(cx);
        Some(meta)
    }

    pub(crate) fn archive(&mut self, id: Uuid, cx: &App) -> bool {
        match self.take(id, cx) {
            Some((Place::Recents(_), chat)) => {
                self.archived.push_front(chat);
                true
            }
            Some((Place::Archived(index), chat)) => {
                self.archived.insert(index, chat);
                false
            }
            None => false,
        }
    }

    pub(crate) fn restore(&mut self, id: Uuid, cx: &App) -> bool {
        match self.take(id, cx) {
            Some((Place::Archived(_), chat)) => {
                self.recents.push_front(chat);
                true
            }
            Some((Place::Recents(index), chat)) => {
                self.recents.insert(index, chat);
                false
            }
            None => false,
        }
    }

    pub(crate) fn restore_all(&mut self) {
        while let Some(chat) = self.archived.pop_back() {
            self.recents.push_front(chat);
        }
    }

    pub(crate) fn set_project(&mut self, id: Uuid, project: Option<ProjectId>, cx: &mut App) {
        let Some((_, chat)) = self.find(id, cx) else {
            return;
        };
        chat.clone().update(cx, |chat, cx| match &mut chat.state {
            ChatState::Loaded(thread) => thread.update(cx, |thread, _| thread.set_project(project)),
            ChatState::Unloaded(meta) | ChatState::Loading { meta, .. } => meta.project = project,
        });
    }

    pub(crate) fn open(
        &mut self,
        id: Uuid,
        models: Arc<ModelCatalog>,
        cx: &mut Context<Self>,
    ) -> Opened {
        let Some((Place::Recents(_), chat)) = self.find(id, cx) else {
            return Opened::Missing;
        };
        let chat = chat.clone();
        if chat.read(cx).view().loaded().is_some() {
            self.active = Some(id);
            self.opening = None;
            return Opened::Active;
        }
        self.opening = Some(chat.downgrade());
        self.start_load(&chat, models, cx);
        Opened::Loading
    }

    pub(crate) fn load(&mut self, id: Uuid, models: Arc<ModelCatalog>, cx: &mut Context<Self>) {
        if let Some((_, chat)) = self.find(id, cx) {
            let chat = chat.clone();
            self.start_load(&chat, models, cx);
        }
    }

    fn start_load(
        &mut self,
        chat: &Entity<Chat>,
        models: Arc<ModelCatalog>,
        cx: &mut Context<Self>,
    ) {
        let loader = self.loader.clone();
        let store = cx.weak_entity();
        chat.update(cx, |chat, cx| chat.load(&loader, store, models, cx));
    }

    fn chat_loaded(&mut self, id: Uuid, chat: &Entity<Chat>, loaded: bool, cx: &mut Context<Self>) {
        let opened = self
            .opening
            .as_ref()
            .is_some_and(|opening| opening == &chat.downgrade());
        if opened {
            self.opening = None;
        }
        if !loaded {
            return;
        }
        cx.emit(ChatStoreEvent::Loaded);
        if opened && matches!(self.find(id, cx), Some((Place::Recents(_), _))) {
            self.active = Some(id);
            cx.emit(ChatStoreEvent::Activated);
        }
        cx.notify();
    }

    /// Puts `idle`'s chat back to its row, derived from it now. Only
    /// `unload.rs` makes an [`Idle`], from a check passed in this update;
    /// the active chat is refused here too.
    pub(crate) fn unload(&mut self, idle: Idle<'_>, cx: &mut App) -> bool {
        let id = idle.thread().read(cx).instance_id;
        self.unload_chat(id, cx)
    }

    fn unload_chat(&mut self, id: Uuid, cx: &mut App) -> bool {
        if self.active == Some(id) {
            return false;
        }
        let Some((_, chat)) = self.find(id, cx) else {
            return false;
        };
        chat.clone().update(cx, |chat, cx| {
            let ChatState::Loaded(thread) = &chat.state else {
                return false;
            };
            chat.state = ChatState::Unloaded(ChatMeta::of(thread.read(cx)));
            true
        })
    }

    /// Every loaded chat, recents then archive.
    pub(crate) fn loaded_all(&self, cx: &App) -> Vec<Entity<Thread>> {
        self.all(cx)
            .filter_map(ListedChat::loaded)
            .cloned()
            .collect()
    }

    /// Unloads without the eligibility check, for tests of what follows.
    #[cfg(test)]
    pub(crate) fn unload_for_test(&mut self, id: Uuid, cx: &mut App) -> bool {
        self.unload_chat(id, cx)
    }

    #[cfg(test)]
    pub(crate) fn rotate_recents(&mut self, n: usize) {
        self.recents.rotate_left(n);
    }

    #[cfg(test)]
    pub(crate) fn push_back_loaded(&mut self, thread: Entity<Thread>, cx: &mut App) {
        let chat = chat_of(thread, cx);
        self.recents.push_back(chat);
    }

    #[cfg(test)]
    pub(crate) fn push_front_archived(&mut self, thread: Entity<Thread>, cx: &mut App) {
        let chat = chat_of(thread, cx);
        self.archived.push_front(chat);
    }

    #[cfg(test)]
    pub(crate) fn push_unloaded(&mut self, meta: ChatMeta, archived: bool, cx: &mut App) {
        self.push_back_unloaded(meta, archived, cx);
    }

    #[cfg(test)]
    pub(crate) fn clear_archived(&mut self) {
        self.archived.clear();
    }

    #[cfg(test)]
    pub(crate) fn recent_ids(&self, cx: &App) -> Vec<Uuid> {
        self.recents.iter().map(|chat| chat.read(cx).id).collect()
    }

    #[cfg(test)]
    pub(crate) fn archived_ids(&self, cx: &App) -> Vec<Uuid> {
        self.archived.iter().map(|chat| chat.read(cx).id).collect()
    }

    #[cfg(test)]
    pub(crate) fn is_loaded(&self, id: Uuid, cx: &App) -> bool {
        self.listed(id, cx)
            .is_some_and(|(_, chat)| chat.loaded().is_some())
    }

    #[cfg(test)]
    pub(crate) fn meta(&self, id: Uuid, cx: &App) -> Option<ChatMeta> {
        self.listed(id, cx).map(|(_, chat)| chat.meta(cx))
    }
}

#[cfg(test)]
mod tests;
