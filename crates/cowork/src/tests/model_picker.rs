//! Picking models, and how the picker follows each thread's catalog.

use super::*;

#[gpui::test]
fn welcome_cards_select_the_new_threads_model(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.update(|_, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.main_stage = MainStage::Welcome;
            cx.notify();
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("welcome-stage").is_some());
    assert!(cx.debug_bounds("bottom-bar").is_none());

    let card = cx
        .debug_bounds("welcome-model-Ollama-test-default")
        .expect("the discovered model has a welcome card");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.new_thread_model, Some(recommended_qwen()));
        assert_eq!(
            cowork.model_picker.read(cx).selected_value(),
            Some(recommended_qwen())
        );
    });

    let continue_button = cx
        .debug_bounds("welcome-continue")
        .expect("continue follows the model cards");
    cx.simulate_click(continue_button.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.main_stage, MainStage::Thread);
        assert_eq!(cowork.active_thread_id, None);
        assert_eq!(cowork.active_model(cx), Some(recommended_qwen()));
    });
    assert!(cx.debug_bounds("welcome-stage").is_none());
    assert!(cx.debug_bounds("bottom-bar").is_some());
}

#[test]
fn picker_lists_an_unavailable_selection_first_in_its_provider_group() {
    let catalog = catalog_of(&[ollama_model("b"), ollama_model("a")]);

    assert_eq!(
        picker_rows(&language_model_groups(&catalog, Some(&ollama_model("b")))),
        vec![(0, "a".into(), true), (0, "b".into(), true)]
    );
    assert_eq!(
        picker_rows(&language_model_groups(
            &catalog,
            Some(&ollama_model("gone"))
        )),
        vec![
            (0, "gone".into(), false),
            (0, "a".into(), true),
            (0, "b".into(), true)
        ]
    );
    // Still listed when its provider offers nothing at all.
    assert_eq!(
        picker_rows(&language_model_groups(
            &ModelCatalog::default(),
            Some(&ollama_model("gone"))
        )),
        vec![(0, "gone".into(), false)]
    );
}

#[gpui::test]
fn picked_models_apply_to_the_active_thread_and_new_threads(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());

    cowork.update_in(cx, |cowork, window, cx| {
        cowork.set_models(test_catalog(), cx);
        let thread = cowork.active_thread(cx).expect("active thread");
        assert_eq!(thread.read(cx).model, None);
        assert_eq!(*thread.read(cx).models, test_catalog());

        // Picking a model changes the active thread and later new threads.
        cowork.select_model(ollama_qwen(), cx);
        assert_eq!(thread.read(cx).model, Some(ollama_qwen()));
        assert_eq!(cowork.new_thread_model, Some(ollama_qwen()));

        // A model the catalog does not offer cannot be picked.
        cowork.select_model(ollama_model("no-such-model"), cx);
        assert_eq!(thread.read(cx).model, Some(ollama_qwen()));

        // A change made by someone else only moves the picker along.
        thread.update(cx, |thread, cx| {
            thread.apply(protocol::HostMessage::ModelSelected(recommended_qwen()), cx);
        });
        cowork.sync_model_picker(window, cx);
        assert_eq!(
            cowork.model_picker.read(cx).selected_value(),
            Some(recommended_qwen())
        );
        assert_eq!(cowork.new_thread_model, Some(ollama_qwen()));

        // Without an active thread the picker shows the new thread model.
        cowork.active_thread_id = None;
        cowork.sync_model_picker(window, cx);
        assert_eq!(
            cowork.model_picker.read(cx).selected_value(),
            Some(ollama_qwen())
        );
        cowork.active_thread_id = Some(thread_id);
    });
}

/// The catalog the picker's rows were last built from.
fn picker_catalog(cowork: &Cowork) -> ModelCatalog {
    let (catalog, _) = cowork.picker_rows.as_ref().expect("picker rows");
    (**catalog).clone()
}

/// Whether the picker shows its selection as available, if it has one.
fn picker_selection_available(cowork: &Cowork, cx: &App) -> Option<bool> {
    let picker = cowork.model_picker.read(cx);
    picker.selection().first().map(|(_, row)| row.available)
}

#[gpui::test]
fn a_model_dropped_from_the_catalog_is_grayed_out_until_another_is_picked(
    cx: &mut gpui::TestAppContext,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());

    cowork.update_in(cx, |cowork, window, cx| {
        cowork.set_models(test_catalog(), cx);
        cowork.select_model(ollama_qwen(), cx);
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_selection_available(cowork, cx), Some(true));
        assert!(cowork.active_model_is_runnable(cx));

        // Rediscovery drops the selected model: it stays selected, but
        // grayed out, and nothing can be sent with it.
        cowork.set_models(catalog_of(&[recommended_qwen()]), cx);
        cowork.sync_model_picker(window, cx);
        let thread = cowork.active_thread(cx).expect("active thread");
        assert_eq!(thread.read(cx).model, Some(ollama_qwen()));
        assert_eq!(
            cowork.model_picker.read(cx).selected_value(),
            Some(ollama_qwen())
        );
        assert_eq!(picker_selection_available(cowork, cx), Some(false));
        assert!(!cowork.active_model_is_runnable(cx));
        assert!(thread.read(cx).runnable_model().is_none());

        // It becomes available again if the catalog offers it again.
        cowork.set_models(test_catalog(), cx);
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_selection_available(cowork, cx), Some(true));
        assert!(cowork.active_model_is_runnable(cx));

        // Picking another model makes the unavailable one disappear.
        cowork.set_models(catalog_of(&[recommended_qwen()]), cx);
        cowork.select_model(recommended_qwen(), cx);
        cowork.sync_model_picker(window, cx);
        assert_eq!(
            cowork.picker_rows.as_ref().map(|(_, model)| model.clone()),
            Some(Some(recommended_qwen()))
        );
        assert_eq!(
            picker_rows(&language_model_groups(
                &picker_catalog(cowork),
                Some(&recommended_qwen())
            )),
            vec![(0, recommended_qwen().id, true)]
        );
        assert_eq!(picker_selection_available(cowork, cx), Some(true));
        assert!(cowork.active_model_is_runnable(cx));
    });
}

#[gpui::test]
fn mirrored_picker_uses_host_catalog_and_restores_local_models(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, local_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    cowork.update_in(cx, |cowork, window, cx| {
        let local_catalog = catalog_of(&[ollama_qwen()]);
        cowork.set_models(local_catalog.clone(), cx);
        cowork.new_thread_model = Some(ollama_qwen());
        let local = cowork.active_thread(cx).expect("local thread");
        let host_model = recommended_qwen();
        let host_catalog = catalog_of(std::slice::from_ref(&host_model));
        let mut snapshot = local.read(cx).to_protocol();
        snapshot.models = host_catalog.clone();
        let welcome = protocol::Welcome {
            participant_id: ParticipantId::new().into_bytes(),
            thread: snapshot,
            draft: local.read(cx).draft.doc.encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };
        let mirror = cx.new(|cx| {
            Thread::from_welcome(
                welcome.clone(),
                ThreadDraft::new(ParticipantId::new()),
                ThreadSharing::NotShared,
                cx,
            )
        });
        let mirror_id = mirror.read(cx).instance_id;
        cowork.thread_store.update(cx, |store, _| {
            store.threads.push_front(mirror.clone());
        });
        cowork.active_thread_id = Some(mirror_id);
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), host_catalog);
        // Local discovery must not replace the host's catalog while joined.
        cowork.set_models(ModelCatalog::default(), cx);
        assert_eq!(*mirror.read(cx).models, host_catalog);
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), host_catalog);
        cowork.set_models(local_catalog.clone(), cx);

        mirror.update(cx, |thread, cx| {
            thread.apply(protocol::HostMessage::ModelSelected(host_model.clone()), cx);
        });
        cowork.sync_model_picker(window, cx);
        assert_eq!(
            cowork.model_picker.read(cx).selected_value(),
            Some(host_model.clone())
        );
        // Choices on the host's thread are not defaults for local ones.
        assert_eq!(cowork.new_thread_model, Some(ollama_qwen()));

        // An empty catalog removes every choice, but not the selection,
        // which is grayed out instead.
        mirror.update(cx, |thread, cx| {
            thread.apply(
                protocol::HostMessage::ModelCatalogChanged(ModelCatalog::default()),
                cx,
            );
        });
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), ModelCatalog::default());
        assert_eq!(
            cowork.model_picker.read(cx).selected_value(),
            Some(host_model)
        );
        assert_eq!(picker_selection_available(cowork, cx), Some(false));
        assert!(!cowork.active_model_is_runnable(cx));

        // A lag-recovery welcome replaces the catalog too.
        mirror.update(cx, |thread, cx| thread.rebase(welcome, cx));
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), host_catalog);

        cowork.active_thread_id = Some(local_id);
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), local_catalog);
        cowork.active_thread_id = None;
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), local_catalog);
    });
}

/// `sync_model_picker` runs on every render, so a catalog model the picker
/// cannot select would make it re-select and redraw forever.
#[gpui::test]
fn picker_can_select_every_catalog_model(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());

    cowork.update_in(cx, |cowork, window, cx| {
        // Includes a selection the catalog no longer offers, which the
        // picker has to be able to show as well.
        let unavailable = ollama_model("gone");
        cowork.model_picker.update(cx, |picker, cx| {
            picker.set_items(
                language_model_groups(&test_catalog(), Some(&unavailable)),
                window,
                cx,
            );
        });
        for model in [recommended_qwen(), ollama_qwen(), unavailable] {
            cowork.model_picker.update(cx, |picker, cx| {
                picker.set_selected_values(std::slice::from_ref(&model), window, cx);
            });
            assert_eq!(cowork.model_picker.read(cx).selected_value(), Some(model));
        }
    });
}
