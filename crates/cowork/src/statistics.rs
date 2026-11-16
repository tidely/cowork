//! The profile page's statistics: what they count of deleted chats, so it
//! outlasts them, and the totals across those and the user's own chats. A
//! chat's counts are added to the deleted ones' in the transaction that
//! deletes it (`chat_db.rs`); usage of a run ending after that is added on
//! its own. The profile lazily caches the totals and activity, rebuilding
//! after counted data changes; chart range and calendar-bucket changes
//! rebuild only the chart.

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

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Statistics {
    pub(crate) tokens_used: u64,
    pub(crate) chats: usize,
    pub(crate) longest_chat: Duration,
    pub(crate) token_activity: Vec<TokenActivity>,
}

impl Statistics {
    /// What a chat being deleted counts, as it is now. A run of it still
    /// ending adds its usage with [`Cowork::record_turn_usage`].
    pub(crate) fn of_chat(chat: &ChatMeta) -> Self {
        Self {
            chats: 1,
            tokens_used: chat.tokens_used,
            longest_chat: chat.generation_time,
            token_activity: chat.token_activity.clone(),
        }
    }

    /// Counts `more` too: sums, but the longest chat is the longer one, as
    /// the database adds them (`chat_db/app_state.rs`).
    pub(crate) fn add(&mut self, more: &Statistics) {
        self.chats += more.chats;
        self.tokens_used = self.tokens_used.saturating_add(more.tokens_used);
        self.longest_chat = self.longest_chat.max(more.longest_chat);
        self.token_activity
            .extend(more.token_activity.iter().copied());
    }
}

impl Cowork {
    /// Adds usage of a deleted chat to what statistics count of deleted
    /// ones, saving it.
    pub(crate) fn count_deleted(&mut self, counted: Statistics) {
        self.profile_statistics = None;
        if let Some(db) = &self.chat_db {
            db.saver().count_deleted(&counted);
        }
        self.deleted.add(&counted);
    }

    pub(crate) fn prepare_profile_statistics(&mut self, now: DateTime<Local>, cx: &App) {
        if self.profile_statistics.is_none() {
            let totals = Statistics {
                tokens_used: self.tokens_used(cx),
                chats: self.total_chats(cx),
                longest_chat: self.longest_chat(cx),
                token_activity: self.token_activity(cx),
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
    pub(crate) fn tokens_used(&self, cx: &App) -> u64 {
        self.own_threads(cx)
            .map(|chat| chat.tokens_used)
            .fold(self.deleted.tokens_used, u64::saturating_add)
    }

    pub(crate) fn token_activity(&self, cx: &App) -> Vec<TokenActivity> {
        self.deleted
            .token_activity
            .iter()
            .chain(
                self.own_threads(cx)
                    .flat_map(|chat| chat.token_activity)
                    .collect::<Vec<_>>()
                    .iter(),
            )
            .copied()
            .collect()
    }
}
