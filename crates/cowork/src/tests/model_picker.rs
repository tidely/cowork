//! Picking models, and how the picker follows each thread's catalog.

use super::*;
use crate::provider_setup::ProviderSetupPlace;

#[gpui::test]
fn context_window_changes_update_local_and_archived_threads(cx: &mut gpui::TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());
    cowork.update(cx, |cowork, cx| {
        cowork.set_models(test_catalog(), cx);
        cowork.new_thread_model = Some(ollama_qwen());
        let local = cowork.active_thread(cx).expect("local thread");
        local.update(cx, |thread, cx| {
            thread.select_model(ollama_qwen(), cx).unwrap()
        });
        let archived = Cowork::new_empty_local_thread(
            ThreadDraft::new(cowork.local_participant_id),
            cowork.local_participant_id,
            cowork.models.clone(),
            Some(ollama_qwen()),
            cx,
        );
        cowork.thread_store.update(cx, |store, _| {
            store.archived.push_front(archived.clone());
        });
        cowork.set_context_settings(
            crate::context_settings::ContextSettings {
                window_tokens: 32_768,
                ..Default::default()
            },
            cx,
        );
        assert_eq!(local.read(cx).max_tokens(), 32_768);
        assert_eq!(archived.read(cx).max_tokens(), 32_768);
        assert_eq!(
            cowork.models.get(&ollama_qwen()).unwrap().max_tokens,
            32_768
        );
        assert_eq!(
            cowork.models.get(&recommended_qwen()).unwrap().max_tokens,
            32_768
        );
    });
}

/// A key typed on a provider's models step connects the provider; removing
/// it takes only that provider's models out of the catalog. Discovery with
/// the key never runs, as the test's Tokio runtime isn't driven.
#[gpui::test]
fn api_keys_are_saved_and_removed_from_the_providers_page(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let provider = ModelProvider::Anthropic;
    cowork.update_in(cx, |cowork, window, cx| {
        cowork.open_settings(SettingsPage::Providers, window, cx);
        cowork.show_provider_step(
            ProviderSetupPlace::Settings,
            ProviderStep::Models(provider),
            cx,
        );
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("api-key-remove").is_none());

    let input = cx.debug_bounds("api-key-input").expect("the key field");
    cx.simulate_click(input.center(), gpui::Modifiers::none());
    cx.simulate_input("sk-ant-test");
    let save = cx.debug_bounds("api-key-save").expect("the save button");
    cx.simulate_click(save.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let claude = ModelRef {
        provider,
        id: "claude".into(),
    };
    cowork.update(cx, |cowork, cx| {
        assert!(cowork.providers.has_api_key(provider));
        // As discovery with the key would have found.
        cowork.set_models(catalog_of(&[ollama_qwen(), claude.clone()]), cx);
    });
    cx.run_until_parked();

    let remove = cx.debug_bounds("api-key-remove").expect("removing the key");
    cx.simulate_click(remove.center(), gpui::Modifiers::none());
    cowork.read_with(cx, |cowork, _| {
        assert!(!cowork.providers.has_api_key(provider));
        assert!(!cowork.models.contains(&claude));
        assert!(cowork.models.contains(&ollama_qwen()));
    });
}

#[gpui::test]
fn context_settings_page_accepts_custom_token_counts(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update_in(cx, |cowork, window, cx| {
        cowork.open_settings(SettingsPage::Context, window, cx);
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("context-page").is_some());
    for (text, expected) in [
        ("100000", 100_000),
        ("1024", 1_024),
        ("1023", 1_024),
        ("not a token count", 1_024),
    ] {
        let bounds = cx
            .debug_bounds("context-window-input")
            .expect("editable token count");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input(text);
        cx.run_until_parked();
        assert_eq!(
            cowork.read_with(cx, |cowork, _| cowork.context_settings.window_tokens),
            expected
        );
    }
}

#[gpui::test]
fn context_window_changes_reach_peers_and_respect_their_host(cx: &mut gpui::TestAppContext) {
    let mut pair = Collaboration::start(cx);
    pair.host.update(pair.cx, |cowork, cx| {
        cowork.set_models(test_catalog(), cx);
        cowork.set_context_settings(
            crate::context_settings::ContextSettings {
                window_tokens: 32_768,
                ..Default::default()
            },
            cx,
        );
    });
    pair.wait_until("the updated context window reaches the peer", |pair| {
        pair.collaborator_thread()
            .unwrap()
            .read_with(pair.cx, |thread, _| thread.max_tokens() == 32_768)
    });
    pair.collaborator.update(pair.cx, |cowork, cx| {
        cowork.set_models(test_catalog(), cx);
        cowork.set_context_settings(
            crate::context_settings::ContextSettings {
                window_tokens: 16_384,
                ..Default::default()
            },
            cx,
        );
        assert_eq!(
            cowork.models.get(&ollama_qwen()).unwrap().max_tokens,
            16_384
        );
        assert_eq!(
            cowork.active_thread(cx).unwrap().read(cx).max_tokens(),
            32_768
        );
    });
    pair.settle();
    assert_eq!(
        pair.host_thread
            .read_with(pair.cx, |thread, _| thread.max_tokens()),
        32_768
    );
}

#[gpui::test]
fn provider_setup_starts_onboarding_with_the_selected_model(cx: &mut gpui::TestAppContext) {
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
        .debug_bounds("setup-provider-Ollama")
        .expect("Ollama is offered even before model setup");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let continue_button = cx
        .debug_bounds("provider-continue")
        .expect("continue follows the provider card");
    cx.simulate_click(continue_button.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    cowork.read_with(cx, |cowork, _| {
        assert_eq!(
            cowork.main_stage,
            MainStage::ProviderSetup(ModelProvider::Ollama)
        );
    });
    let card = cx
        .debug_bounds("setup-model-Ollama-test-default")
        .expect("the discovered model has a setup card");
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
        .debug_bounds("setup-continue")
        .expect("continue follows the model cards");
    cx.simulate_click(continue_button.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    // The greeting streams in as previews of the seeded run, and ends it,
    // with no "Working for" line to vanish when it does.
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(100));
    cx.run_until_parked();
    let greeting_id = cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).expect("setup starts a chat");
        let thread = thread.read(cx);
        assert!(thread.is_generating());
        let TimelineMessage::Agent(message) = &thread.timeline[0] else {
            panic!("the greeting is an agent message");
        };
        assert!(!message.ui.output.text.is_empty());
        assert!(!message.ui.output.text.contains("quick tour"));
        message.record.id
    });
    let work_line = format!("toggle-work-{greeting_id}").leak();
    assert!(cx.debug_bounds(work_line).is_none());
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();

    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(cowork.main_stage, MainStage::Thread);
        assert_eq!(cowork.active_model(cx), Some(recommended_qwen()));
        let thread = cowork.active_thread(cx).expect("setup starts a chat");
        let thread = thread.read(cx);
        assert_eq!(thread.summary.title, "Welcome to Cowork");
        assert!(!thread.is_generating());
        assert!(cowork.agents.is_empty());
        assert!(cowork.active_generations.is_empty());
        assert!(thread.project_folders().is_empty());
        assert_eq!(thread.submission_sequence, 0);
        assert_eq!(thread.tokens_used, 0);
        assert_eq!(thread.timeline.len(), 1);
        assert_eq!(thread.transcript.len(), 2);
        assert!(matches!(
            thread.transcript.entries()[0].origin,
            agent::Origin::Input {
                source: crate::transcript::RunSource::Host,
                ..
            }
        ));
        assert!(matches!(&thread.timeline[0], TimelineMessage::Agent(_)));
        let welcome = thread.welcome_for_test();
        Thread::validate_welcome(&welcome).expect("onboarding can be saved and shared");
    });
    cx.update(|_, cx| {
        let thread = cowork.read(cx).active_thread(cx).unwrap();
        let welcome = thread.read(cx).welcome_for_test();
        let restored = Thread::join_for_test(welcome, cx);
        assert_eq!(restored.transcript, thread.read(cx).transcript);
        let TimelineMessage::Agent(message) = &restored.timeline[0] else {
            panic!("the greeting remains an agent message after replay");
        };
        assert!(message.ui.output.text.contains("Welcome to Cowork!"));
    });
    assert!(cx.debug_bounds("welcome-stage").is_none());
    assert!(cx.debug_bounds("bottom-bar").is_some());

    // The first reply uses this thread's seeded conversation, not a new chat.
    // The Tokio runtime is deliberately not driven: no live model is needed.
    cx.simulate_input("hi");
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    });
    cx.run_until_parked();
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.active_thread(cx).unwrap();
        let thread = thread.read(cx);
        assert_eq!(cowork.thread_store.read(cx).threads.len(), 1);
        assert!(cowork.agents.contains_key(&thread.instance_id));
        assert!(thread.is_generating());
        assert_eq!(thread.timeline.len(), 3);
        assert_eq!(thread.transcript.len(), 2);
        let TimelineMessage::User(message) = &thread.timeline[1] else {
            panic!("the first reply follows the greeting");
        };
        assert_eq!(message.record.blocks[0].text, "hi");
    });
}

#[gpui::test]
fn provider_can_be_set_up_without_discovered_models(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cx.update(|_, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.set_models(ModelCatalog::default(), cx);
            cowork.main_stage = MainStage::Welcome;
            cx.notify();
        });
    });
    cx.run_until_parked();

    let card = cx
        .debug_bounds("setup-provider-Ollama")
        .expect("provider remains available without models");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let continue_button = cx.debug_bounds("provider-continue").unwrap();
    cx.simulate_click(continue_button.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    assert!(cx.debug_bounds("provider-setup-stage").is_some());
    assert!(cx.debug_bounds("setup-refresh").is_some());
    let continue_button = cx.debug_bounds("setup-continue").unwrap();
    cx.simulate_click(continue_button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(
            cowork.main_stage,
            MainStage::ProviderSetup(ModelProvider::Ollama)
        );
    });

    let back = cx.debug_bounds("setup-back").unwrap();
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("welcome-stage").is_some());
}

#[gpui::test]
fn providers_settings_pick_the_new_chats_model(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let dir = std::env::temp_dir().join(format!("cowork-settings-{}", Uuid::new_v4()));
    let settings = dir.join("settings.json");
    cowork.update(cx, |cowork, cx| {
        cowork.new_thread_model = None;
        cowork.start_settings(Some(settings.clone()), cx);
    });
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.open_settings(SettingsPage::Providers, window, cx);
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("providers-page").is_some());
    // The welcome screen's steps, without its welcome text or first chat.
    assert!(cx.debug_bounds("welcome-stage").is_none());

    let card = cx.debug_bounds("setup-provider-Ollama").unwrap();
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let continue_button = cx.debug_bounds("provider-continue").unwrap();
    cx.simulate_click(continue_button.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    cowork.read_with(cx, |cowork, _| {
        assert_eq!(
            cowork.main_stage,
            MainStage::Settings(SettingsPage::Providers)
        );
        assert_eq!(
            cowork.settings_provider_step,
            ProviderStep::Models(ModelProvider::Ollama)
        );
    });
    assert!(cx.debug_bounds("provider-models-page").is_some());
    assert!(cx.debug_bounds("setup-continue").is_none());
    // Settings refresh even when models were found.
    assert!(cx.debug_bounds("setup-refresh").is_some());

    let card = cx
        .debug_bounds("setup-model-Ollama-test-other")
        .expect("the discovered model has a card");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(cowork.new_thread_model, Some(ollama_qwen()));
    });

    let back = cx.debug_bounds("setup-back").unwrap();
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("providers-page").is_some());

    // The next launch loads it, and the context settings.
    let context = crate::context_settings::ContextSettings {
        window_tokens: 32_768,
        threshold_percent: 60,
    };
    cowork.update(cx, |cowork, cx| {
        cowork.set_context_settings(context, cx);
        cowork.context_settings = Default::default();
        cowork.new_thread_model = None;
        cowork.start_settings(Some(settings.clone()), cx);
        assert_eq!(cowork.new_thread_model, Some(ollama_qwen()));
        assert_eq!(cowork.context_settings, context);
    });
    _ = std::fs::remove_dir_all(dir);
}

/// Onboarding depends only on whether the user has threads; the model only
/// decides whether it waits for the user to choose one.
#[gpui::test]
fn launch_onboards_only_users_without_threads(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update_in(cx, |cowork, window, cx| {
        // An archived thread is enough to skip onboarding, with no model
        // selected yet: it is picked in the new chat's composer.
        let thread = Cowork::new_empty_local_thread(
            ThreadDraft::new(cowork.local_participant_id),
            cowork.local_participant_id,
            cowork.models.clone(),
            None,
            cx,
        );
        cowork
            .thread_store
            .update(cx, |store, _| store.archived.push_front(thread));
        cowork.new_thread_model = None;
        cowork.main_stage = MainStage::Welcome;
        cowork.open_start_stage(window, cx);
        assert_eq!(cowork.main_stage, MainStage::Thread);
        assert_eq!(cowork.stage_before_settings, MainStage::Thread);
        assert!(cowork.active_thread_id.is_none());

        // Without threads, as after deleting the onboarding chat, the welcome
        // screen waits for discovery (not driven here), then onboarding
        // starts with the selected model without choosing it again.
        cowork
            .thread_store
            .update(cx, |store, _| store.archived.clear());
        cowork.new_thread_model = Some(ollama_qwen());
        cowork.open_start_stage(window, cx);
        assert_eq!(cowork.main_stage, MainStage::Welcome);
        cowork.onboard_if_welcome(window, cx);
        assert_eq!(cowork.main_stage, MainStage::Thread);
        let thread = cowork.active_thread(cx).expect("onboarding starts a chat");
        assert_eq!(thread.read(cx).summary.title, "Welcome to Cowork");
        assert_eq!(thread.read(cx).model(), Some(&ollama_qwen()));
    });
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
        assert_eq!(thread.read(cx).model().cloned(), None);
        assert_eq!(**thread.read(cx).models(), test_catalog());

        // Picking a model changes the active thread and later new threads.
        cowork.select_model(ollama_qwen(), cx);
        assert_eq!(thread.read(cx).model().cloned(), Some(ollama_qwen()));
        assert_eq!(cowork.new_thread_model, Some(ollama_qwen()));

        // A model the catalog does not offer cannot be picked.
        cowork.select_model(ollama_model("no-such-model"), cx);
        assert_eq!(thread.read(cx).model().cloned(), Some(ollama_qwen()));

        // A change made by someone else only moves the picker along.
        thread.update(cx, |thread, cx| {
            thread.apply_for_test(protocol::HostMessage::ModelSelected(recommended_qwen()), cx);
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
        assert_eq!(thread.read(cx).model().cloned(), Some(ollama_qwen()));
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
            draft_generation: 0,
            participant_id: ParticipantId::new().into_bytes(),
            thread: snapshot,
            draft: local.read(cx).draft().encode_state(),
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
        assert_eq!(**mirror.read(cx).models(), host_catalog);
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), host_catalog);
        cowork.set_models(local_catalog.clone(), cx);

        mirror.update(cx, |thread, cx| {
            thread.apply_for_test(protocol::HostMessage::ModelSelected(host_model.clone()), cx);
        });
        cowork.sync_model_picker(window, cx);
        assert_eq!(
            cowork.model_picker.read(cx).selected_value(),
            Some(host_model)
        );
        // Choices on the host's thread are not defaults for local ones.
        assert_eq!(cowork.new_thread_model, Some(ollama_qwen()));

        // The host's catalog changes reach the picker.
        mirror.update(cx, |thread, cx| {
            thread.apply_for_test(
                protocol::HostMessage::ModelCatalogChanged(ModelCatalog::default()),
                cx,
            );
        });
        cowork.sync_model_picker(window, cx);
        assert_eq!(picker_catalog(cowork), ModelCatalog::default());

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
