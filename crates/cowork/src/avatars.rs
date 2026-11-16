//! Participants' names, colors, and avatars as shown in the UI.

use std::collections::HashMap;

use gpui::{App, FontWeight, SharedString, div, img, prelude::*, px, rgb};
use gpui_component::{ActiveTheme, Icon};
use gpui_kit_assets::IconName;

use crate::{
    Cowork,
    participant::ParticipantId,
    profile::{Profile, appearance, participant_name},
    thread::Thread,
    timeline::MessageAuthor,
};

const MIN_TEXT: gpui::Pixels = px(10.);
const MIN_TWO_INITIALS: gpui::Pixels = px(20.);

impl Cowork {
    /// Everyone's profile in `thread`, with the local user's current one
    /// under each id they have there.
    pub(crate) fn profiles_for(&self, thread: Option<&Thread>) -> HashMap<ParticipantId, Profile> {
        let mut profiles = thread
            .map(|thread| thread.profiles.clone())
            .unwrap_or_default();
        profiles.insert(self.local_participant_id, self.profile.clone());
        if let Some(thread) = thread {
            profiles.insert(thread.participant_id(), self.profile.clone());
        }
        profiles
    }

    pub(crate) fn name_of(&self, participant: ParticipantId) -> SharedString {
        participant_name(participant, self.shown_profiles.get(&participant))
    }

    pub(crate) fn color_of(&self, participant: ParticipantId) -> u32 {
        appearance(participant, self.shown_profiles.get(&participant)).color()
    }

    pub(crate) fn render_participant_avatar(
        &self,
        participant: ParticipantId,
        size: gpui::Pixels,
    ) -> gpui::Div {
        Self::render_avatar_for(participant, self.shown_profiles.get(&participant), size)
    }

    pub(crate) fn render_avatar_for(
        participant: ParticipantId,
        profile: Option<&Profile>,
        size: gpui::Pixels,
    ) -> gpui::Div {
        if let Some(picture) = profile.and_then(|profile| profile.picture.clone()) {
            return div()
                .size(size)
                .flex_none()
                .overflow_hidden()
                .rounded_full()
                .child(img(picture).size_full().rounded_full());
        }
        let appearance = appearance(participant, profile);
        // Never below the 10 px text minimum; avatars too small for two
        // letters at that size show one, so they never come down to color
        // alone.
        let initials = appearance.initials();
        let initials = if size < MIN_TWO_INITIALS {
            initials.chars().take(1).collect()
        } else {
            initials
        };
        div()
            .size(size)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(appearance.color()))
            .text_size((size * 0.42).max(MIN_TEXT))
            .line_height(size)
            .font_weight(FontWeight::SEMIBOLD)
            // Identity colors are independent of the theme, and each one keeps
            // white initials at 4.5:1 (see `participant::COLORS`).
            .text_color(rgb(0xffffff))
            .child(initials)
    }

    /// The agent's avatar shows the logo of the provider whose model wrote
    /// the message, or a generic one when no model did.
    pub(crate) fn render_avatar(&self, author: MessageAuthor, cx: &App) -> gpui::Div {
        const SIZE: gpui::Pixels = px(22.);
        const LOGO: gpui::Pixels = px(14.);

        match author {
            MessageAuthor::User(participant) => self.render_participant_avatar(participant, SIZE),
            MessageAuthor::Agent(provider) => div()
                .size(SIZE)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(cx.theme().secondary)
                .child(match provider {
                    Some(provider) => provider.icon(LOGO, cx),
                    None => div().child(
                        Icon::new(IconName::Bot)
                            .size(LOGO)
                            .text_color(cx.theme().foreground),
                    ),
                }),
        }
    }

    /// A participant's avatar with smaller avatars of `others` overlapping
    /// its lower edge. Positioned absolutely so that people coming and going
    /// never move the row's text.
    pub(crate) fn render_layered_avatars(
        &self,
        primary: ParticipantId,
        others: &[ParticipantId],
        cx: &App,
    ) -> gpui::Div {
        const MAX_OTHERS: usize = 2;
        const SMALL: gpui::Pixels = px(16.);
        const STEP: f32 = 12.;

        let hidden = others.len().saturating_sub(MAX_OTHERS);
        let mut layered = others
            .iter()
            .take(MAX_OTHERS)
            .map(|&participant| {
                self.render_participant_avatar(participant, SMALL)
                    .border_1()
                    .border_color(cx.theme().background)
            })
            .collect::<Vec<_>>();
        if hidden > 0 {
            layered.push(
                div()
                    .size(SMALL)
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(cx.theme().background)
                    .bg(cx.theme().secondary_active)
                    .text_size(MIN_TEXT)
                    .text_color(cx.theme().secondary_foreground)
                    .child(format!("+{hidden}")),
            );
        }
        let count = layered.len();
        div()
            .relative()
            .child(self.render_avatar(MessageAuthor::User(primary), cx))
            .children(layered.into_iter().enumerate().map(|(index, avatar)| {
                // Centered under the primary avatar, fanned out sideways.
                let offset = (index as f32 - (count as f32 - 1.) / 2.) * STEP;
                avatar.absolute().top(px(13.)).left(px(11. - 8. + offset))
            }))
    }
}
