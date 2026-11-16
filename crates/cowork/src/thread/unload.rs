//! Which of a thread's own fields keep it loaded. Every field is named, so
//! a field added to [`Thread`] doesn't compile until it is classified here
//! as persisted, derived, or memory-only, and a memory-only one as dropped
//! or blocking. The rest of the rule, held on `Cowork`, is in `unload.rs`.

use super::{Thread, ThreadOwnership, ThreadSharing};
use crate::unload::UnloadBlocker;

impl Thread {
    /// Why this thread can't go back to its saved row, judged by its own
    /// state, if it can't.
    pub(crate) fn unload_blocker(&self) -> Option<UnloadBlocker> {
        let Thread {
            // Memory-only and blocking: never saved.
            pending,
            // Derived: the number of user messages once nothing is pending.
            submission_sequence: _,
            // Persisted (the saved thread's id).
            instance_id: _,
            // Persisted (id and title).
            summary: _,
            // Persisted.
            participant_id: _,
            // Memory-only, dropped: empty unless shared, which blocks.
            participants: _,
            // Persisted.
            profiles: _,
            // Persisted at turn boundaries; a failed save blocks (Cowork).
            transcript: _,
            // Memory-only and blocking: a run, compaction, or approval wait.
            active_run,
            // Persisted.
            prompt_names: _,
            // Persisted.
            tokens_used: _,
            token_activity: _,
            // Persisted.
            model: _,
            // Derived: the app's catalog, given again on load (`set_models`).
            models: _,
            // Persisted.
            context_tokens: _,
            streamed_bytes: _,
            // Persisted.
            timeline: _,
            // Memory-only; see `ThreadDraft::unload_blocker`.
            draft,
            // Memory-only, dropped: sharing policy, reset as after a restart.
            peer_permissions: _,
            // Memory-only, dropped: sharing epochs, meaningless unshared.
            peer_draft_generations: _,
            draft_generation: _,
            // Memory-only and blocking unless not shared or failed.
            sharing,
            // Persisted for local threads; a joined thread isn't saved at
            // all, so unloading it would lose it.
            ownership,
            // Persisted.
            project_folders: _,
            project_mode: _,
            project: _,
        } = self;
        if *ownership == ThreadOwnership::Remote {
            return Some(UnloadBlocker::Joined);
        }
        if !matches!(sharing, ThreadSharing::NotShared | ThreadSharing::Failed) {
            return Some(UnloadBlocker::Shared);
        }
        if active_run.is_some() {
            return Some(UnloadBlocker::Running);
        }
        if !pending.is_empty() {
            return Some(UnloadBlocker::PendingSubmissions);
        }
        draft.unload_blocker()
    }

    #[cfg(test)]
    pub(crate) fn draft_mut_for_test(&mut self) -> &mut super::draft::ThreadDraft {
        &mut self.draft
    }
}
