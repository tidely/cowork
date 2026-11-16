use std::{sync::Arc, time::Duration};

use gpui::{Entity, TestAppContext};
use uuid::Uuid;

use super::*;
use crate::chat_meta::ManualLoader;

fn row(title: &str) -> ChatMeta {
    ChatMeta {
        id: Uuid::new_v4(),
        title: title.into(),
        project: None,
        tokens_used: 7,
        generation_time: Duration::from_secs(3),
        last_run_at: None,
        token_activity: Vec::new(),
    }
}

/// A store listing `titles` unloaded in the recents, loading through a
/// loader the test finishes.
fn store_of(
    titles: &[&str],
    cx: &mut TestAppContext,
) -> (Entity<ThreadStore>, ManualLoader, Vec<Uuid>) {
    let loader = ManualLoader::default();
    let rows: Vec<ChatMeta> = titles.iter().map(|title| row(title)).collect();
    let ids = rows.iter().map(|row| row.id).collect();
    let store = cx.new(|cx| {
        let mut store = ThreadStore::default();
        store.set_loader(loader.loader());
        for row in rows {
            store.push_unloaded(row, false, cx);
        }
        store
    });
    (store, loader, ids)
}

fn open(store: &Entity<ThreadStore>, id: Uuid, cx: &mut TestAppContext) -> Opened {
    store.update(cx, |store, cx| store.open(id, Arc::default(), cx))
}

#[gpui::test]
fn the_latest_open_wins_over_an_earlier_load(cx: &mut TestAppContext) {
    let (store, loader, ids) = store_of(&["a", "b"], cx);
    let [a, b] = ids[..] else { unreachable!() };
    assert_eq!(open(&store, a, cx), Opened::Loading);
    assert_eq!(open(&store, b, cx), Opened::Loading);

    loader.finish(a);
    cx.run_until_parked();
    store.read_with(cx, |store, cx| {
        assert!(store.is_loaded(a, cx), "a's load still fills it");
        assert_eq!(store.active_id(), None, "but b was opened since");
    });

    loader.finish(b);
    cx.run_until_parked();
    store.read_with(cx, |store, cx| {
        assert_eq!(store.active_id(), Some(b));
        assert!(store.active(cx).is_some());
    });
}

#[gpui::test]
fn opening_a_loading_chat_again_starts_no_second_load(cx: &mut TestAppContext) {
    let (store, loader, ids) = store_of(&["a", "b"], cx);
    let [a, b] = ids[..] else { unreachable!() };
    open(&store, a, cx);
    open(&store, b, cx);
    open(&store, a, cx);
    assert_eq!(loader.started(a), 1);

    loader.finish(a);
    cx.run_until_parked();
    loader.finish(b);
    cx.run_until_parked();
    store.read_with(cx, |store, _| assert_eq!(store.active_id(), Some(a)));
}

#[gpui::test]
fn a_load_finishing_after_its_chat_was_deleted_is_dropped(cx: &mut TestAppContext) {
    let (store, loader, ids) = store_of(&["a", "b"], cx);
    let [a, b] = ids[..] else { unreachable!() };
    open(&store, a, cx);
    store.update(cx, |store, cx| assert!(store.remove(a, cx).is_some()));
    cx.run_until_parked();
    assert!(loader.cancelled(a), "deleting cancels the load");

    loader.finish(a);
    cx.run_until_parked();
    store.read_with(cx, |store, cx| {
        assert_eq!(store.recent_ids(cx), [b]);
        assert_eq!(store.active_id(), None);
    });
}

#[gpui::test]
fn the_active_chat_is_never_unloaded(cx: &mut TestAppContext) {
    let (store, loader, ids) = store_of(&["a", "b"], cx);
    let [a, b] = ids[..] else { unreachable!() };
    open(&store, a, cx);
    loader.finish(a);
    cx.run_until_parked();

    store.update(cx, |store, cx| {
        assert!(!store.unload_for_test(a, cx));
        assert!(store.active(cx).is_some());
        // Nor can an unloaded chat be made active.
        assert!(!store.activate(b, cx));
        assert_eq!(store.active_id(), Some(a));
    });
}

/// What the sidebar, statistics and saves read of a loaded chat follows the
/// chat, and unloading keeps the latest.
#[gpui::test]
fn a_loaded_chats_row_follows_the_chat(cx: &mut TestAppContext) {
    let (store, loader, ids) = store_of(&["a", "b"], cx);
    let [a, b] = ids[..] else { unreachable!() };
    open(&store, a, cx);
    loader.finish(a);
    cx.run_until_parked();
    let thread = store.read_with(cx, |store, cx| store.loaded(a, cx).unwrap());
    thread.update(cx, |thread, _| {
        thread.summary.title = "renamed".into();
        thread.tokens_used += 5;
    });
    store.read_with(cx, |store, cx| {
        let row = store.meta(a, cx).unwrap();
        assert_eq!((row.title.as_str(), row.tokens_used), ("renamed", 12));
    });

    // Unloading keeps it, once another chat is active.
    open(&store, b, cx);
    loader.finish(b);
    cx.run_until_parked();
    store.update(cx, |store, cx| {
        assert!(store.unload_for_test(a, cx));
        assert!(!store.is_loaded(a, cx));
        let row = store.meta(a, cx).unwrap();
        assert_eq!((row.title.as_str(), row.tokens_used), ("renamed", 12));
    });
}
