//! Saving what the profile page's statistics count of deleted threads, so
//! they outlast them. The rest they count from the user's own threads, which
//! are saved themselves (see `thread_files.rs`), adding them up when Cowork
//! starts; see [`Cowork::tokens_used`], [`Cowork::total_chats`], and
//! [`Cowork::longest_chat`]. A thread whose folder is removed by hand is no
//! longer counted.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::App;
use serde::{Deserialize, Serialize};

use crate::{Cowork, thread::Thread, usage::TokenActivity};

/// The saved file's format. Older or newer files aren't read, nor
/// overwritten.
const FILE_VERSION: u32 = 2;

/// Format 1 counted every chat, as threads weren't saved: all of them were
/// gone by the next start, just as deleted ones are, so it reads the same.
const DELETED_EVERYTHING_VERSION: u32 = 1;

/// What the profile page counts of deleted threads.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Statistics {
    pub(crate) tokens_used: u64,
    pub(crate) chats: usize,
    pub(crate) longest_chat: Duration,
    pub(crate) token_activity: Vec<TokenActivity>,
}

impl Statistics {
    /// Counts `thread`, which is being deleted, as it is now. A run of it
    /// still ending adds its usage with [`Cowork::record_turn_usage`].
    pub(crate) fn add_thread(&mut self, thread: &Thread) {
        self.chats += 1;
        self.longest_chat = self.longest_chat.max(thread.generation_time());
        self.tokens_used += thread.tokens_used;
        self.token_activity
            .extend(thread.token_activity.iter().copied());
    }
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
        Ok(file) if matches!(file.version, FILE_VERSION | DELETED_EVERYTHING_VERSION) => {
            file.statistics
        }
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
    /// Loads what the statistics saved at `file` count of deleted threads,
    /// and saves it there from now on.
    pub(crate) fn start_statistics(&mut self, file: Option<PathBuf>) {
        if let Some(path) = &file {
            self.deleted = load_statistics(path);
        }
        self.statistics_file = file;
    }

    /// Tokens used across the user's own threads, deleted ones included,
    /// and none joined from someone else.
    pub(crate) fn tokens_used(&self, cx: &App) -> u64 {
        self.own_threads(cx)
            .map(|thread| thread.tokens_used)
            .fold(self.deleted.tokens_used, u64::saturating_add)
    }

    /// When those tokens were used, turn by turn.
    pub(crate) fn token_activity(&self, cx: &App) -> Vec<TokenActivity> {
        self.deleted
            .token_activity
            .iter()
            .chain(
                self.own_threads(cx)
                    .flat_map(|thread| &thread.token_activity),
            )
            .copied()
            .collect()
    }

    /// Saves what the statistics count of deleted threads, which changes
    /// when one is deleted or a run of one ends. Small and written only
    /// then, so it is written in place of waiting on another thread.
    pub(crate) fn save_statistics(&self) {
        let Some(path) = &self.statistics_file else {
            return;
        };
        if let Err(error) = save_statistics(path, self.deleted.clone()) {
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
