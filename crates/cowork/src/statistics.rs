//! The profile page's statistics: the usage of every run in the user's own
//! chats, deleted ones included, and the chat counts, of deleted chats too
//! so they outlast them. Usage is counted per run, so a run shared by
//! several chats counts once: in memory it stays in one list however chats
//! are deleted, and in the database a deleted chat's removed runs move to
//! the deleted chats' usage in the transaction that deletes it
//! (`chat_db.rs`); usage of a run ending after that is added on its own. The
//! profile lazily caches the totals and activity, rebuilding after counted
//! data changes; chart range and calendar-bucket changes rebuild only the
//! chart.

use std::time::Duration;

use chrono::{DateTime, Local};
use gpui::App;

use crate::chat_meta::ChatMeta;
use crate::{
    Cowork,
    usage::{CachedActivityChart, TokenActivity},
};

/// A snapshot of owned threads and deleted usage. Rendering only reads this;
/// mutations that change counted data discard it, and rebuilding stays lazy.
pub(crate) struct ProfileStatistics {
    pub(crate) totals: Statistics,
    pub(crate) chart: CachedActivityChart,
}

/// The totals the profile shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Statistics {
    pub(crate) tokens_used: u64,
    pub(crate) chats: usize,
    pub(crate) longest_chat: Duration,
    pub(crate) token_activity: Vec<TokenActivity>,
}

/// What statistics count of deleted chats beyond their runs' usage.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DeletedChats {
    pub(crate) chats: usize,
    pub(crate) longest_chat: Duration,
}

impl DeletedChats {
    /// What a chat being deleted counts, as it is now. A run of it still
    /// ending adds its time with `Cowork::settle_retired_thread`.
    pub(crate) fn of_chat(chat: &ChatMeta) -> Self {
        Self {
            chats: 1,
            longest_chat: chat.generation_time,
        }
    }

    /// Counts `more` too: sums, but the longest chat is the longer one, as
    /// the database adds them (`chat_db/app_state.rs`).
    pub(crate) fn add(&mut self, more: &DeletedChats) {
        self.chats += more.chats;
        self.longest_chat = self.longest_chat.max(more.longest_chat);
    }
}

impl Cowork {
    /// Counts a run of one of the user's own chats; `deleted` if the chat
    /// was deleted before the run ended, which the database then counts
    /// with the deleted chats.
    pub(crate) fn count_run(&mut self, activity: TokenActivity, deleted: bool) {
        if deleted && let Some(db) = &self.chat_db {
            db.saver().count_deleted_run(&activity);
        }
        self.token_activity.push(activity);
    }

    pub(crate) fn prepare_profile_statistics(&mut self, now: DateTime<Local>, cx: &App) {
        if self.profile_statistics.is_none() {
            let totals = Statistics {
                tokens_used: self.tokens_used(),
                chats: self.total_chats(cx),
                longest_chat: self.longest_chat(cx),
                token_activity: self.token_activity(),
            };
            self.profile_statistics = Some(ProfileStatistics {
                chart: CachedActivityChart::new(&totals.token_activity, self.activity_range, now),
                totals,
            });
        }
        let cached = self.profile_statistics.as_mut().unwrap();
        if !cached.chart.is_current(self.activity_range, now) {
            cached.chart =
                CachedActivityChart::new(&cached.totals.token_activity, self.activity_range, now);
        }
    }

    /// Tokens used across the user's own threads, deleted ones included,
    /// and none joined from someone else.
    pub(crate) fn tokens_used(&self) -> u64 {
        self.token_activity
            .iter()
            .map(|activity| activity.tokens)
            .fold(0, u64::saturating_add)
    }

    pub(crate) fn token_activity(&self) -> Vec<TokenActivity> {
        self.token_activity.clone()
    }
}
