//! Opening a chat from its saved row: the placeholder while it loads, the
//! error once a load fails, and the chat after a retry (see `opening.rs`).

use std::time::Duration;

use gpui::AppContext as _;
use uuid::Uuid;

use super::SceneContext;
use crate::{
    chat_meta::{ChatMeta, ManualLoader},
    opening::{PLACEHOLDER_DELAY, PLACEHOLDER_MIN_SHOWN},
    thread::ThreadStore,
};

pub(super) fn scene(context: &SceneContext) -> anyhow::Result<()> {
    let loader = ManualLoader::default();
    let saved = Uuid::new_v4();
    let mut stage = context.open(|_, cx| {
        let store = cx.new(|cx| {
            let mut store = ThreadStore::default();
            store.set_loader(loader.loader());
            store.push_unloaded(
                ChatMeta {
                    id: saved,
                    title: "Quarterly planning notes".into(),
                    project: None,
                    generation_time: Duration::ZERO,
                    last_run_at: None,
                },
                false,
                cx,
            );
            store
        });
        (store, None)
    })?;
    stage.capture("1-before")?;

    stage.update(|cowork, window, cx| cowork.open_thread(saved, window, cx))?;
    stage.advance(PLACEHOLDER_DELAY);
    stage.capture("2-placeholder")?;

    loader.fail(saved);
    stage.settle();
    stage.capture("3-failed")?;

    stage.update(|cowork, window, cx| cowork.open_thread(saved, window, cx))?;
    loader.finish(saved);
    stage.advance(PLACEHOLDER_MIN_SHOWN);
    stage.capture("4-loaded")?;
    Ok(())
}
