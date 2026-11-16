//! The user's settings that outlast a launch: the model new chats start
//! with, and the context settings (the context window and when to compact).
//! Saved in the chats' database (`chat_db/app_state.rs`) whenever one
//! changes. Only loading and saving them is here; what a launch opens on,
//! onboarding included, is `Cowork::open_start_stage`'s.

use gpui::Context;

use crate::{Cowork, context_settings::ContextSettings, models::ModelRef};

/// The user's saved settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The model new chats start with: the last one selected locally. Kept
    /// even when its provider doesn't offer it at the next launch, so the
    /// picker shows it unavailable rather than silently picking another.
    pub(crate) model: Option<ModelRef>,
    pub(crate) context: ContextSettings,
}

impl Cowork {
    /// Takes up the saved `settings`. A saved model becomes the one new
    /// chats start with, its provider preselected for setup.
    pub(crate) fn start_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        self.apply_context_settings(settings.context, cx);
        if let Some(model) = settings.model {
            self.selected_provider = Some(model.provider);
            self.new_thread_model = Some(model);
        }
    }

    /// Makes `model` the one new chats start with, saving it for the next
    /// launch.
    pub(crate) fn set_new_thread_model(&mut self, model: ModelRef) {
        if self.new_thread_model.as_ref() == Some(&model) {
            return;
        }
        self.new_thread_model = Some(model);
        self.save_settings();
    }

    /// Saves the settings as they are now, unless nothing is saved.
    pub(crate) fn save_settings(&self) {
        let Some(db) = &self.chat_db else {
            return;
        };
        db.saver().save_settings(&Settings {
            model: self.new_thread_model.clone(),
            context: self.context_settings,
        });
    }
}
