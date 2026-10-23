//! Saving the user's settings in `~/.cowork/settings.json` so they outlast a
//! launch: the model new chats start with, and the context settings (the
//! context window and when to compact). Saved whenever one changes. Only
//! loading and saving them is here; what a launch opens on, onboarding
//! included, is `Cowork::open_start_stage`'s.

use std::path::{Path, PathBuf};

use gpui::Context;
use serde::{Deserialize, Serialize};

use crate::{Cowork, context_settings::ContextSettings, models::ModelRef};

/// The saved file's format. Older or newer files aren't read, nor
/// overwritten. A new setting doesn't need a new format: missing ones read
/// as their defaults, and unknown ones are ignored.
const FILE_VERSION: u32 = 1;

/// The user's saved settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// The model new chats start with: the last one selected locally. Kept
    /// even when its provider doesn't offer it at the next launch, so the
    /// picker shows it unavailable rather than silently picking another.
    pub(crate) model: Option<ModelRef>,
    pub(crate) context: ContextSettings,
}

#[derive(Serialize, Deserialize)]
struct SettingsFile {
    version: u32,
    #[serde(flatten)]
    settings: Settings,
}

/// Where the settings are saved: next to Cowork's other files.
pub(crate) fn settings_file() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".cowork").join("settings.json"))
}

/// Writes `settings` whole, replacing the old file only once the new one is
/// written, so a crash never leaves half a file.
fn save_settings(path: &Path, settings: Settings) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = SettingsFile {
        version: FILE_VERSION,
        settings,
    };
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&file)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Reads the saved settings, or the defaults when there are none yet. Values
/// edited out of range by hand are brought back into it. A file that can't
/// be read is moved aside rather than overwritten by the next save.
fn load_settings(path: &Path) -> Settings {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Settings::default();
        }
        Err(error) => {
            eprintln!("could not read settings from {}: {error}", path.display());
            return Settings::default();
        }
    };
    match serde_json::from_slice::<SettingsFile>(&bytes) {
        Ok(file) if file.version == FILE_VERSION => Settings {
            context: file.settings.context.in_range(),
            ..file.settings
        },
        result => {
            let aside = path.with_extension("json.unreadable");
            match result {
                Ok(file) => eprintln!(
                    "settings in {} are format {}, not {FILE_VERSION}; moved to {}",
                    path.display(),
                    file.version,
                    aside.display()
                ),
                Err(error) => eprintln!(
                    "could not read settings from {}: {error}; moved to {}",
                    path.display(),
                    aside.display()
                ),
            }
            _ = std::fs::rename(path, aside);
            Settings::default()
        }
    }
}

impl Cowork {
    /// Loads the settings saved at `file`, and saves them there from now on.
    /// A saved model becomes the one new chats start with, its provider
    /// preselected for setup.
    pub(crate) fn start_settings(&mut self, file: Option<PathBuf>, cx: &mut Context<Self>) {
        let settings = file.as_deref().map(load_settings).unwrap_or_default();
        self.settings_file = file;
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

    /// Saves the settings as they are now. Small and written only when one
    /// changes, so it is written in place of waiting on another thread.
    pub(crate) fn save_settings(&self) {
        let Some(path) = &self.settings_file else {
            return;
        };
        let settings = Settings {
            model: self.new_thread_model.clone(),
            context: self.context_settings,
        };
        if let Err(error) = save_settings(path, settings) {
            eprintln!("could not save settings to {}: {error:#}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::models::ModelProvider;

    fn temporary_file() -> PathBuf {
        std::env::temp_dir()
            .join(format!("cowork-settings-{}", Uuid::new_v4()))
            .join("settings.json")
    }

    #[test]
    fn settings_are_saved_and_loaded() {
        let path = temporary_file();
        assert_eq!(load_settings(&path), Settings::default(), "no file yet");

        let settings = Settings {
            model: Some(ModelRef {
                provider: ModelProvider::Ollama,
                id: "qwen3:8b".into(),
            }),
            context: ContextSettings {
                window_tokens: 32_768,
                threshold_percent: 60,
            },
        };
        save_settings(&path, settings.clone()).unwrap();
        assert_eq!(load_settings(&path), settings);

        _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_and_out_of_range_settings_read_as_usable_values() {
        let path = temporary_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{"version": 1, "context": {"window_tokens": 12, "threshold_percent": 200}}"#,
        )
        .unwrap();

        let settings = load_settings(&path);
        assert_eq!(settings.model, None);
        assert_eq!(
            settings.context,
            ContextSettings {
                window_tokens: 1_024,
                threshold_percent: 95,
            }
        );

        _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn other_formats_are_moved_aside() {
        let path = temporary_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"version": 99, "model": null}"#).unwrap();

        assert_eq!(load_settings(&path), Settings::default());
        assert!(!path.exists());
        assert!(path.with_extension("json.unreadable").exists());

        _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
