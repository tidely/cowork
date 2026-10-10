//! Saving the profile page's statistics to disk, so they outlast the threads
//! they count. Threads aren't saved, so every chat of an earlier session is
//! gone once Cowork starts again, and counts as deleted ones do; see
//! [`Cowork::total_chats`] and [`Cowork::longest_chat`].

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::App;
use serde::{Deserialize, Serialize};

use crate::{Cowork, usage::TokenActivity};

/// The saved file's format. Older or newer files aren't read, nor
/// overwritten.
const FILE_VERSION: u32 = 1;

/// Everything the profile page counts, as of the last save.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Statistics {
    pub(crate) tokens_used: u64,
    /// Every chat the user has started, those still open included.
    pub(crate) chats: usize,
    pub(crate) longest_chat: Duration,
    pub(crate) token_activity: Vec<TokenActivity>,
}

#[derive(Serialize, Deserialize)]
struct StatisticsFile {
    version: u32,
    #[serde(flatten)]
    statistics: Statistics,
}

/// Where the statistics are saved: next to Cowork's other files.
pub(crate) fn statistics_file() -> Option<PathBuf> {
    Some(
        std::env::home_dir()?
            .join(".cowork")
            .join("statistics.json"),
    )
}

/// Writes `statistics` whole, replacing the old file only once the new one
/// is written, so a crash never leaves half a file.
fn save_statistics(path: &Path, statistics: Statistics) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = StatisticsFile {
        version: FILE_VERSION,
        statistics,
    };
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&file)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Reads the saved statistics, or none yet. A file that can't be read is
/// moved aside rather than overwritten by the next save.
fn load_statistics(path: &Path) -> Statistics {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Statistics::default();
        }
        Err(error) => {
            eprintln!("could not read statistics from {}: {error}", path.display());
            return Statistics::default();
        }
    };
    match serde_json::from_slice::<StatisticsFile>(&bytes) {
        Ok(file) if file.version == FILE_VERSION => file.statistics,
        result => {
            let aside = path.with_extension("json.unreadable");
            match result {
                Ok(file) => eprintln!(
                    "statistics in {} are format {}, not {FILE_VERSION}; moved to {}",
                    path.display(),
                    file.version,
                    aside.display()
                ),
                Err(error) => eprintln!(
                    "could not read statistics from {}: {error}; moved to {}",
                    path.display(),
                    aside.display()
                ),
            }
            _ = std::fs::rename(path, aside);
            Statistics::default()
        }
    }
}

impl Cowork {
    /// Loads the statistics saved at `file` and saves them there from now
    /// on. None of the chats they count are open any more.
    pub(crate) fn start_statistics(&mut self, file: Option<PathBuf>) {
        if let Some(path) = &file {
            let saved = load_statistics(path);
            self.tokens_used = saved.tokens_used;
            self.token_activity = saved.token_activity;
            self.deleted_chats = saved.chats;
            self.longest_deleted_chat = saved.longest_chat;
        }
        self.statistics_file = file;
    }

    pub(crate) fn statistics(&self, cx: &App) -> Statistics {
        Statistics {
            tokens_used: self.tokens_used,
            chats: self.total_chats(cx),
            longest_chat: self.longest_chat(cx),
            token_activity: self.token_activity.clone(),
        }
    }

    /// Saves the statistics, which change when the user starts a chat and
    /// when a run ends. Small and written only then, so it is written in
    /// place of waiting on another thread.
    pub(crate) fn save_statistics(&self, cx: &App) {
        let Some(path) = &self.statistics_file else {
            return;
        };
        if let Err(error) = save_statistics(path, self.statistics(cx)) {
            eprintln!("could not save statistics to {}: {error:#}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use uuid::Uuid;

    use super::*;

    fn temporary_file() -> PathBuf {
        std::env::temp_dir()
            .join(format!("cowork-statistics-{}", Uuid::new_v4()))
            .join("statistics.json")
    }

    #[test]
    fn statistics_are_saved_and_loaded() {
        let path = temporary_file();
        assert_eq!(load_statistics(&path), Statistics::default(), "no file yet");

        let statistics = Statistics {
            tokens_used: 1_500,
            chats: 3,
            longest_chat: Duration::from_millis(92_500),
            token_activity: vec![TokenActivity {
                at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                duration: Duration::from_millis(2_250),
                tokens: 1_500,
            }],
        };
        save_statistics(&path, statistics.clone()).unwrap();
        assert_eq!(load_statistics(&path), statistics);

        _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unreadable_file_is_moved_aside() {
        let path = temporary_file();
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&path, br#"{"version":99,"tokens_used":5}"#).unwrap();

        assert_eq!(load_statistics(&path), Statistics::default());
        assert!(!path.exists(), "the next save must not overwrite it");
        assert!(path.with_extension("json.unreadable").exists());

        _ = std::fs::remove_dir_all(dir);
    }
}
