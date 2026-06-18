//! Session persistence: a single JSON snapshot under the platform data dir,
//! holding both the visible thread tree (for rendering) and every
//! conversation's model context (for resuming runs).
//!
//! The two are stored together because neither is a superset of the other — the
//! tree topology and per-agent metadata live only in `AppState`, while the
//! faithful model turns needed to resume live only in `ConversationStore`. See
//! `docs/tui-design.md` Decision 6.
//!
//! Writes are atomic (temp file + rename) so a crash mid-write cannot corrupt an
//! existing snapshot. A corrupt or version-mismatched file is ignored on load
//! rather than clobbered, so it survives for inspection until the next save.

use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
};

use llm::{ChatMessage, ConversationId, ConversationStore};
use serde::{Deserialize, Serialize};

use crate::app::{AppState, ThreadId, ThreadState};

/// Bumped when the on-disk shape changes incompatibly; an older `version` is
/// ignored on load (the session starts fresh) rather than mis-parsed.
const SNAPSHOT_VERSION: u32 = 2;

const SESSION_FILE: &str = "session.json";

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub threads: Vec<ThreadState>,
    pub next_thread_id: ThreadId,
    #[serde(default)]
    pub always_allowed_tools: Vec<String>,
    /// Every conversation's model context, keyed by conversation id.
    #[serde(default)]
    pub conversations: HashMap<ConversationId, Vec<ChatMessage>>,
}

/// Path to the session file, or `None` when no data directory can be resolved
/// (then the session simply does not persist). Honors `COWORK_DATA_DIR`, then
/// the platform convention.
pub fn session_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(SESSION_FILE))
}

/// `$COWORK_DATA_DIR`, else `~/Library/Application Support/cowork` on macOS,
/// else `$XDG_DATA_HOME/cowork` or `~/.local/share/cowork`.
fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("COWORK_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }

    let home = PathBuf::from(env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Application Support/cowork"))
    } else if let Some(base) = env::var_os("XDG_DATA_HOME") {
        Some(PathBuf::from(base).join("cowork"))
    } else {
        Some(home.join(".local/share/cowork"))
    }
}

/// Load a snapshot from `path`. Returns `None` when the file is absent, corrupt,
/// or from an incompatible version — every case starts a fresh session without
/// touching the file. Direct `eprintln!` is avoided: it would corrupt the TUI's
/// alternate screen.
pub fn load(path: &Path) -> Option<SessionSnapshot> {
    let bytes = fs::read(path).ok()?;
    let snapshot: SessionSnapshot = serde_json::from_slice(&bytes).ok()?;
    (snapshot.version == SNAPSHOT_VERSION).then_some(snapshot)
}

/// Snapshot the app and store, then write atomically. A no-op when no data
/// directory resolves. I/O errors are swallowed: a failed save must not crash
/// the TUI, and there is no non-corrupting surface to report it on yet.
pub async fn save(path: Option<&Path>, app: &AppState, store: &ConversationStore) {
    let Some(path) = path else {
        return;
    };

    let snapshot = SessionSnapshot {
        version: SNAPSHOT_VERSION,
        threads: app.snapshot_threads(),
        next_thread_id: app.next_thread_id(),
        always_allowed_tools: app.always_allowed_tools(),
        conversations: store.export().await,
    };

    let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) else {
        return;
    };
    let _ = write_atomic(path, &bytes);
}

/// Write `bytes` to `path` via a sibling temp file and a rename, so a reader
/// never observes a half-written file and a crash leaves the prior file intact.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;

    /// A unique temp directory per test so parallel runs don't collide.
    fn temp_dir(tag: &str) -> PathBuf {
        env::temp_dir().join(format!("cowork-persist-{tag}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn snapshot_round_trips_tree_and_conversations() {
        let app = AppState::new();
        let mut conversations = HashMap::new();
        conversations.insert(ConversationId::new(1), vec![ChatMessage::user("hi")]);
        let store = ConversationStore::from_conversations(conversations);

        let dir = temp_dir("round-trip");
        let path = dir.join(SESSION_FILE);
        save(Some(&path), &app, &store).await;

        let loaded = load(&path).expect("snapshot loads");
        assert_eq!(loaded.version, SNAPSHOT_VERSION);
        assert_eq!(loaded.threads.len(), 1);
        assert_eq!(loaded.conversations[&ConversationId::new(1)].len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_mismatch_is_ignored_rather_than_clobbered() {
        let dir = temp_dir("version");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SESSION_FILE);
        fs::write(&path, br#"{"version":999,"threads":[],"next_thread_id":1}"#).unwrap();

        assert!(load(&path).is_none(), "a future version is not loaded");
        assert!(path.exists(), "the unreadable file is left intact");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_file_loads_as_none() {
        assert!(load(&temp_dir("absent").join(SESSION_FILE)).is_none());
    }
}
