//! The model picker and context window indicator in the bottom bar, and how
//! providers are shown: their names and logos. Discovering the models is
//! `providers.rs`'s.

use std::{collections::BTreeMap, sync::Arc};

use gpui::{
    App, AppContext, Context, Entity, IntoElement, Pixels, SharedString, Subscription, WeakEntity,
    Window, div, img, prelude::*, px, svg,
};
use gpui_component::{
    ActiveTheme as _,
    combobox::{ComboboxEvent, ComboboxState},
    searchable_list::{SearchableGroup, SearchableListItem, SearchableVec},
    tooltip::Tooltip,
};

use crate::{
    Cowork, assets,
    context_settings::{context_details, context_ring},
    models::{ModelCatalog, ModelProvider, ModelRef},
    thread::{Thread, ThreadOwnership},
    usage::ContextUsage,
};

/// How a provider's logo is drawn.
enum Logo {
    /// A one-color mark, drawn in the theme's text color so it reads on any
    /// background (the files' own fill would be black).
    Monochrome(&'static str),
    /// A mark in its brand colors, drawn as it is.
    Colored(&'static str),
}

impl ModelProvider {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
        }
    }

    fn logo(self) -> Logo {
        match self {
            Self::Ollama => Logo::Monochrome(assets::OLLAMA_LOGO_PATH),
            Self::Anthropic => Logo::Colored(assets::ANTHROPIC_LOGO_PATH),
            Self::OpenAi => Logo::Monochrome(assets::OPENAI_LOGO_PATH),
        }
    }

    /// The provider's logo, `size` square. Dimming the returned element
    /// (`opacity`) dims the logo.
    pub(crate) fn icon(self, size: Pixels, cx: &App) -> gpui::Div {
        let logo = match self.logo() {
            Logo::Monochrome(path) => svg()
                .path(path)
                .size_full()
                .text_color(cx.theme().foreground)
                .into_any_element(),
            Logo::Colored(path) => img(path).size_full().into_any_element(),
        };
        div().size(size).flex_none().child(logo)
    }

    /// Shown under the name on the provider's setup card.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Ollama => "Run models locally",
            Self::Anthropic => "Claude models, with an API key",
            Self::OpenAi => "GPT models, with an API key",
        }
    }
}

/// Shown wherever a thread's selected model is missing from its catalog.
pub(crate) const UNAVAILABLE_MODEL_TOOLTIP: &str =
    "This model is no longer available. Pick another to send.";

/// A row in the model picker.
#[derive(Clone)]
pub(crate) struct LanguageModel {
    pub(crate) name: SharedString,
    pub(crate) model: ModelRef,
    /// `false` only for a thread's selected model that its catalog no longer
    /// offers. It is listed grayed out until another model is picked.
    pub(crate) available: bool,
}

impl SearchableListItem for LanguageModel {
    type Value = ModelRef;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .id(SharedString::from(format!("model-{}", self.model.id)))
            .flex()
            .items_center()
            .gap_2()
            .child(
                self.model
                    .provider
                    .icon(px(16.), cx)
                    .when(!self.available, |this| this.opacity(0.5)),
            )
            .child(self.name.clone())
            .when(!self.available, |this| {
                this.tooltip(|window, cx| Tooltip::new(UNAVAILABLE_MODEL_TOOLTIP).build(window, cx))
            })
    }

    fn value(&self) -> &Self::Value {
        &self.model
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        self.name.to_lowercase().contains(&query)
            || self.model.provider.label().to_lowercase().contains(&query)
            || self.model.id.to_lowercase().contains(&query)
    }

    fn disabled(&self) -> bool {
        !self.available
    }
}

pub(crate) type ModelPickerItems = SearchableVec<SearchableGroup<LanguageModel>>;

pub(crate) type ModelPickerState = ComboboxState<ModelPickerItems>;

/// The picker's rows: `catalog`'s models grouped by provider and sorted by
/// name, plus `selected` at the top of its provider's group when the catalog
/// no longer offers it.
pub(crate) fn language_model_groups(
    catalog: &ModelCatalog,
    selected: Option<&ModelRef>,
) -> ModelPickerItems {
    let mut groups = BTreeMap::<ModelProvider, Vec<LanguageModel>>::new();
    if let Some(model) = selected.filter(|model| !catalog.contains(model)) {
        groups
            .entry(model.provider)
            .or_default()
            .push(LanguageModel {
                name: model.id.clone().into(),
                model: model.clone(),
                available: false,
            });
    }
    for (provider, models) in catalog.providers() {
        let group = groups.entry(provider).or_default();
        let start = group.len();
        group.extend(models.iter().map(|(id, info)| LanguageModel {
            name: info.name.clone().into(),
            model: ModelRef {
                provider,
                id: id.clone(),
            },
            available: true,
        }));
        group[start..].sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.model.id.cmp(&b.model.id))
        });
    }
    SearchableVec::new(
        groups
            .into_iter()
            .map(|(provider, models)| {
                models
                    .into_iter()
                    .fold(SearchableGroup::new(provider.label()), |group, model| {
                        group.item(model)
                    })
            })
            .collect::<Vec<_>>(),
    )
}

impl Cowork {
    /// Creates the model picker and subscribes `Cowork` to the user's picks.
    pub(crate) fn new_model_picker(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<ModelPickerState>, Subscription) {
        let picker = cx.new(|cx| {
            ComboboxState::new(
                language_model_groups(&ModelCatalog::default(), None),
                Vec::new(),
                window,
                cx,
            )
            .searchable(true)
        });
        let subscription = cx.subscribe(&picker, Self::model_picker_event);
        (picker, subscription)
    }

    fn model_picker_event(
        &mut self,
        _: Entity<ModelPickerState>,
        event: &ComboboxEvent<ModelPickerItems>,
        cx: &mut Context<Self>,
    ) {
        if let ComboboxEvent::Change(selection) = event
            && let Some(model) = selection.first().cloned()
        {
            self.select_model(model, cx);
        }
    }

    /// Makes `catalog` the models this app can run, in every thread whose
    /// agent it runs. Mirrored threads keep their host's catalog.
    pub(crate) fn set_models(&mut self, catalog: ModelCatalog, cx: &mut Context<Self>) {
        self.models = Arc::new(catalog.clone());
        let store = self.thread_store.read(cx);
        // Archived threads too, as a scheduled run may bring one back.
        let threads = store
            .threads
            .iter()
            .chain(&store.archived)
            .cloned()
            .collect::<Vec<_>>();
        for thread in threads {
            thread.update(cx, |thread, cx| {
                thread.set_model_catalog(catalog.clone(), cx);
            });
        }
        // Scheduled runs may have been waiting for models.
        self.run_next_scheduled(chrono::Local::now().naive_local(), cx);
    }

    /// Applies a model picked by the local user to the active thread. Choices
    /// on someone else's thread do not become defaults for local threads.
    pub(crate) fn select_model(&mut self, model: ModelRef, cx: &mut Context<Self>) {
        // The picker only offers these, and the host checks again anyway.
        if !self.active_catalog(cx).contains(&model) {
            return;
        }
        if let Some(thread) = self.active_thread(cx) {
            let local = thread.read(cx).ownership() == ThreadOwnership::Local;
            if thread
                .update(cx, |thread, cx| thread.select_model(model.clone(), cx))
                .is_err()
            {
                return;
            }
            if local {
                self.set_new_thread_model(model);
            }
        } else {
            self.set_new_thread_model(model);
        }
        cx.notify();
    }

    /// The model of the active thread, or of the thread about to be created.
    pub(crate) fn active_model(&self, cx: &App) -> Option<ModelRef> {
        self.active_thread(cx)
            .map(|thread| thread.read(cx).model().cloned())
            .unwrap_or_else(|| self.new_thread_model.clone())
    }

    /// The catalog of the active thread, or of the thread about to be created.
    pub(crate) fn active_catalog(&self, cx: &App) -> Arc<ModelCatalog> {
        self.active_thread(cx).map_or_else(
            || self.models.clone(),
            |thread| thread.read(cx).models().clone(),
        )
    }

    /// Whether the active thread, or the thread about to be created, has a
    /// model selected that its catalog offers.
    pub(crate) fn active_model_is_runnable(&self, cx: &App) -> bool {
        self.active_model(cx)
            .is_some_and(|model| self.active_catalog(cx).contains(&model))
    }

    /// Points the picker at the active thread's catalog and model.
    ///
    /// The picker is shared by every thread, while each thread has its own
    /// catalog and model that collaborators can change at any time, so it is
    /// re-synced on every render rather than at each of the places either can
    /// change. Setting the selection does not emit a picker event, so this
    /// never feeds back into [`Cowork::select_model`].
    pub(crate) fn sync_model_picker(&mut self, window: &mut Window, cx: &mut App) {
        let catalog = self.active_catalog(cx);
        let model = self.active_model(cx);
        // Catalogs are only ever replaced, never changed in place, so
        // comparing pointers is enough and keeps this cheap on every frame.
        let rows_changed = self
            .picker_rows
            .as_ref()
            .is_none_or(|(rows_catalog, rows_model)| {
                !Arc::ptr_eq(rows_catalog, &catalog) || *rows_model != model
            });
        if rows_changed {
            self.model_picker.update(cx, |picker, cx| {
                picker.set_items(language_model_groups(&catalog, model.as_ref()), window, cx);
            });
            self.picker_rows = Some((catalog, model.clone()));
        }
        // Re-selected whenever the rows change, even to the same model, since
        // the selection keeps the row it was made from, and with it whether
        // the model is available.
        if rows_changed || self.model_picker.read(cx).selected_value() != model {
            self.model_picker.update(cx, |picker, cx| {
                if let Some(model) = model {
                    picker.set_selected_values(&[model], window, cx);
                } else {
                    picker.clear_selection(cx);
                }
            });
        }
    }

    /// A ring that fills as the active thread nears the end of its model's
    /// context window. Hovering it shows the numbers.
    pub(crate) fn render_context_indicator(
        &self,
        thread: Option<&Entity<Thread>>,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let new_thread_max_tokens = self
            .new_thread_model
            .as_ref()
            .and_then(|model| self.models.get(model))
            .map_or(0, |info| info.max_tokens);
        let usage = ContextUsage::of(thread.map(|thread| thread.read(cx)), new_thread_max_tokens);
        let compacting = thread.is_some_and(|thread| thread.read(cx).is_compacting());
        let thread = thread.map(Entity::downgrade);
        let settings = thread
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .is_none_or(|thread| thread.read(cx).is_host())
            .then_some(self.context_settings);
        div()
            .id("context-indicator")
            .debug_selector(|| "context-indicator".to_owned())
            .size(px(28.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .child(context_ring(usage, compacting, cx))
            .tooltip(move |window, cx| {
                let thread = thread.clone();
                // Reads the thread on every render, so the numbers keep up
                // with a streaming reply while the tooltip is open.
                Tooltip::element(move |_, cx| {
                    let thread = thread.as_ref().and_then(WeakEntity::upgrade);
                    let usage = ContextUsage::of(
                        thread.as_ref().map(|thread| thread.read(cx)),
                        new_thread_max_tokens,
                    );
                    let compacting = thread
                        .as_ref()
                        .is_some_and(|thread| thread.read(cx).is_compacting());
                    context_details(usage, settings, compacting, cx)
                })
                .py_2()
                .px_3()
                .build(window, cx)
            })
    }
}
