//! The profile page: the local user's name and picture, and their usage
//! statistics.

use std::{sync::Arc, time::Duration};

use chrono::Local;
use gpui::{
    App, AppContext, Context, Entity, Focusable, FontWeight, IntoElement, PathPromptOptions,
    SharedString, WeakEntity, Window, div, prelude::*, px, rgb,
};
use gpui_base::input::{InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    chart::LineChart,
    dialog::{DialogDescription, DialogFooter, DialogHeader, DialogTitle},
    input::Input,
};
use gpui_kit_assets::IconName as AssetIconName;
use itertools::Itertools;

use crate::{
    Cowork, MainStage,
    profile::{Profile, display_name_error, load_profile_picture, participant_name},
    thread::{Thread, ThreadOwnership},
    usage::{
        ActivityBucket, ActivityRange, format_stat_count, format_stat_duration,
        token_activity_chart,
    },
};

impl Cowork {
    pub(crate) fn profile_name(&self) -> SharedString {
        participant_name(self.local_participant_id, Some(&self.profile))
    }

    /// The local user's picture if they chose one, their initials otherwise.
    pub(crate) fn render_profile_avatar(&self, size: gpui::Pixels) -> gpui::Div {
        Self::render_avatar_for(self.local_participant_id, Some(&self.profile), size)
    }

    pub(crate) fn open_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.main_stage = MainStage::Profile;
        self.profile_error = None;
        // The composer is hidden, so it must not keep taking keystrokes.
        window.blur(cx);
        cx.notify();
    }

    pub(crate) fn render_profile_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const PICTURE_SIZE: gpui::Pixels = px(96.);

        let picture = div()
            .id("profile-picture")
            .debug_selector(|| "profile-picture".to_owned())
            .group("profile-picture")
            .relative()
            .size(PICTURE_SIZE)
            .flex_none()
            .rounded_full()
            .cursor_pointer()
            .child(self.render_profile_avatar(PICTURE_SIZE).text_size(px(36.)))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(cx.theme().overlay)
                    .opacity(0.)
                    .group_hover("profile-picture", |this| this.opacity(1.))
                    .child(
                        // White ink contrasts with the dark scrim over arbitrary
                        // photos and participant colors in either theme mode.
                        Icon::new(AssetIconName::Pen)
                            .size_6()
                            .text_color(rgb(0xffffff)),
                    ),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.pick_profile_picture(window, cx);
            }));

        div()
            .id("profile-page")
            .debug_selector(|| "profile-page".to_owned())
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                div()
                    .id("profile-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .pt(px(72.))
                            .pb_6()
                            .px_6()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_3()
                            .child(picture)
                            .child(
                                div()
                                    .max_w_full()
                                    .text_ellipsis()
                                    .text_size(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().secondary_foreground)
                                    .child(self.profile_name()),
                            )
                            .children(self.profile_error.clone().map(|error| {
                                div().text_sm().text_color(cx.theme().danger).child(error)
                            }))
                            .child(self.render_usage_stats(cx))
                            .child(self.render_token_activity(cx)),
                    ),
            )
            .child(
                div().absolute().top_3().right_3().child(
                    Button::new("edit-profile")
                        .ghost()
                        .small()
                        .icon(Icon::new(AssetIconName::Pen))
                        .label("Edit")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_profile_name_dialog(window, cx);
                        })),
                ),
            )
    }

    /// The chats the user started here, archived ones included, excluding
    /// joined ones.
    fn own_threads<'a>(&self, cx: &'a App) -> impl Iterator<Item = &'a Thread> {
        let store = self.thread_store.read(cx);
        store
            .threads
            .iter()
            .chain(&store.archived)
            .map(|thread| thread.read(cx))
            .filter(|thread| thread.ownership() == ThreadOwnership::Local)
    }

    /// How many chats the user has started, including deleted ones.
    pub(crate) fn total_chats(&self, cx: &App) -> usize {
        self.own_threads(cx).count() + self.deleted_chats
    }

    /// The most time the agent has spent generating in one of the user's
    /// own chats, including archived and deleted ones.
    pub(crate) fn longest_chat(&self, cx: &App) -> Duration {
        self.own_threads(cx)
            .map(Thread::generation_time)
            .fold(self.longest_deleted_chat, Duration::max)
    }

    /// A row of the user's usage statistics, each a value over its label.
    fn render_usage_stats(&self, cx: &App) -> impl IntoElement {
        let stats = [
            (format_stat_count(self.tokens_used), "Lifetime tokens"),
            (self.total_chats(cx).to_string(), "Total chats"),
            (format_stat_duration(self.longest_chat(cx)), "Longest chat"),
        ];
        let divider = || div().flex_none().w(px(1.)).h(px(36.)).bg(cx.theme().muted);
        let stat = |(value, label): (String, &'static str)| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .text_sm()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground)
                        .child(value),
                )
                .child(
                    div()
                        .text_color(cx.theme().foreground.opacity(0.6))
                        .child(label),
                )
        };

        div()
            .debug_selector(|| "usage-stats".to_owned())
            .mt_5()
            .w_full()
            .max_w(px(444.))
            .py(px(10.))
            .flex()
            .items_center()
            .rounded(px(14.))
            .border_1()
            .border_color(cx.theme().muted)
            .bg(cx.theme().secondary)
            .children(Itertools::intersperse_with(
                stats
                    .into_iter()
                    .map(|entry| stat(entry).into_any_element()),
                || divider().into_any_element(),
            ))
    }

    /// A line chart of the tokens the user used over the chosen period.
    ///
    /// The busiest period is marked by a faint line at the height of the
    /// chart's top, labeled with its tokens, so the scale is readable
    /// without hovering. The label sits at the end away from the peak, so
    /// the two never collide.
    fn render_token_activity(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const PLOT_HEIGHT: f32 = 120.;
        /// How far below the plot's top `LineChart` draws its highest value.
        const PLOT_TOP_INSET: f32 = 10.;
        const LABEL_LINE_HEIGHT: f32 = 16.;
        /// The peak's line, just under its label, which tops the chart.
        const PEAK_LINE_TOP: f32 = LABEL_LINE_HEIGHT + 2.;
        /// Room above the plot, so that its highest value meets the line.
        const PEAK_LABEL_ROOM: f32 = PEAK_LINE_TOP - PLOT_TOP_INSET;

        let chart = token_activity_chart(&self.token_activity, self.activity_range, Local::now());
        let last_index = chart.buckets.len().saturating_sub(1);
        let peak = chart.peak().map(|(index, bucket)| {
            let text = format!(
                "Peak {} · {}",
                format_stat_count(bucket.tokens as u64),
                bucket.label
            );
            (index * 2 < chart.buckets.len(), text)
        });
        let empty = peak.is_none();

        let axis = chart.axis.iter().map(|label| {
            let text = div().whitespace_nowrap().child(label.text.clone());
            let anchored = div().absolute().top_0();
            if label.index == last_index && last_index > 0 {
                anchored.right_0().child(text)
            } else if label.index == 0 {
                anchored.left_0().child(text)
            } else {
                // A zero-width anchor at the point, which the label overflows
                // evenly on both sides.
                anchored
                    .left(gpui::relative(label.index as f32 / last_index as f32))
                    .w(px(0.))
                    .flex()
                    .justify_center()
                    .child(text)
            }
        });
        let ranges = ActivityRange::ALL.into_iter().map(|range| {
            let selected = range == self.activity_range;
            div()
                .id(range.label())
                .debug_selector(move || format!("activity-range-{}", range.label()))
                .cursor_pointer()
                .text_color(if selected {
                    cx.theme().foreground
                } else {
                    cx.theme().foreground.opacity(0.6)
                })
                .child(range.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.activity_range = range;
                    cx.notify();
                }))
        });

        div()
            .debug_selector(|| "token-activity".to_owned())
            .mt_6()
            .w_full()
            .max_w(px(444.))
            .flex()
            .flex_col()
            .gap_2()
            .text_sm()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .debug_selector(|| "token-activity-title".to_owned())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .child("Token activity"),
                    )
                    .child(div().flex().items_center().gap_3().children(ranges)),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .pt(px(PEAK_LABEL_ROOM))
                    .when_some(peak, |this, (label_on_right, text)| {
                        this.child(
                            div()
                                .debug_selector(|| "token-activity-peak".to_owned())
                                .absolute()
                                .top(px(PEAK_LINE_TOP))
                                .left_0()
                                .right_0()
                                .border_t_1()
                                .border_dashed()
                                .border_color(cx.theme().chart_grid),
                        )
                        .child(
                            div()
                                .debug_selector(|| "token-activity-peak-label".to_owned())
                                .absolute()
                                .top_0()
                                .map(|this| {
                                    if label_on_right {
                                        this.right_0()
                                    } else {
                                        this.left_0()
                                    }
                                })
                                .text_xs()
                                .line_height(px(LABEL_LINE_HEIGHT))
                                .text_color(cx.theme().foreground.opacity(0.5))
                                .child(text),
                        )
                    })
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(PLOT_HEIGHT))
                            .child(
                                LineChart::new(chart.buckets)
                                    .x(|bucket: &ActivityBucket| bucket.label.clone())
                                    .y(|bucket: &ActivityBucket| bucket.tokens)
                                    .stroke(cx.theme().chart_1)
                                    .linear()
                                    .grid(false)
                                    .x_axis(false)
                                    .name("Tokens")
                                    .id("token-activity-chart"),
                            )
                            .when(empty, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .inset_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_color(cx.theme().foreground.opacity(0.4))
                                        .child("No tokens used in this period"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .relative()
                            .mt(px(6.))
                            .w_full()
                            .h(px(LABEL_LINE_HEIGHT))
                            .text_xs()
                            .line_height(px(LABEL_LINE_HEIGHT))
                            .text_color(cx.theme().foreground.opacity(0.5))
                            .children(axis),
                    ),
            )
    }

    fn pick_profile_picture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The native pickers have no file-type filter here, so the image is
        // validated once chosen.
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a profile picture".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match selected.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(path) => path,
                    None => return,
                },
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    _ = this.update(cx, |this, cx| {
                        this.profile_error =
                            Some(format!("Could not choose a file: {error}").into());
                        cx.notify();
                    });
                    return;
                }
            };
            let result = cx
                .background_executor()
                .spawn(async move { load_profile_picture(&path) })
                .await;
            _ = this.update(cx, |this, cx| {
                match result {
                    Ok(picture) => {
                        this.set_profile(
                            Profile {
                                picture: Some(Arc::new(picture)),
                                ..this.profile.clone()
                            },
                            cx,
                        );
                        this.profile_error = None;
                    }
                    Err(error) => this.profile_error = Some(error.to_string().into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_profile_name_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current_name = self.profile_name();
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(current_name));
        let input_subscription = cx.subscribe_in(
            &name,
            window,
            |this, name, event: &InputEvent, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } if this.save_profile_name(name, cx) => {
                    window.close_dialog(cx);
                }
                _ => {}
            },
        );
        self.profile_name_subscription = Some(input_subscription);

        let cowork = cx.entity().downgrade();
        let dialog_name = name.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let name = dialog_name.clone();
            let value = name.read(cx).value();
            let error = display_name_error(&value);
            // An empty field is obvious enough without a message.
            let shown_error = error.clone().filter(|_| !value.trim().is_empty());
            let dismiss_cowork = cowork.clone();
            let cancel_cowork = cowork.clone();
            let save_cowork = cowork.clone();
            dialog
                .w(px(440.))
                .bg(cx.theme().popover)
                .on_cancel(move |_, _, cx| Self::dismiss_profile_name_dialog(&dismiss_cowork, cx))
                .content(move |content, _, cx| {
                    content
                        .child(
                            DialogHeader::new()
                                .child(DialogTitle::new().child("Edit profile"))
                                .child(DialogDescription::new().child("Change your display name.")),
                        )
                        .child(Input::new(&name).id("profile-name-input").h(px(38.)))
                        .children(
                            shown_error
                                .clone()
                                .map(|error| div().text_color(cx.theme().danger).child(error)),
                        )
                        .child(
                            DialogFooter::new()
                                .child(
                                    Button::new("cancel-profile-name")
                                        .outline()
                                        .label("Cancel")
                                        .on_click({
                                            let cancel_cowork = cancel_cowork.clone();
                                            move |_, window, cx| {
                                                if Self::dismiss_profile_name_dialog(
                                                    &cancel_cowork,
                                                    cx,
                                                ) {
                                                    window.close_dialog(cx);
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("save-profile-name")
                                        .primary()
                                        .label("Save")
                                        .disabled(error.is_some())
                                        .on_click({
                                            let save_cowork = save_cowork.clone();
                                            let name = name.clone();
                                            move |_, window, cx| {
                                                let saved = save_cowork
                                                    .update(cx, |cowork, cx| {
                                                        cowork.save_profile_name(&name, cx)
                                                    })
                                                    .unwrap_or(false);
                                                if saved {
                                                    window.close_dialog(cx);
                                                }
                                            }
                                        }),
                                ),
                        )
                })
        });
        name.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Applies the name typed into the dialog, unless it is not a valid name.
    pub(crate) fn save_profile_name(
        &mut self,
        name: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> bool {
        let value = name.read(cx).value();
        if display_name_error(&value).is_some() {
            return false;
        }
        self.set_profile(
            Profile {
                name: Some(value.trim().to_owned().into()),
                ..self.profile.clone()
            },
            cx,
        );
        self.profile_name_subscription = None;
        true
    }

    /// Replaces the local user's profile and tells everyone they share a
    /// thread with.
    pub(crate) fn set_profile(&mut self, profile: Profile, cx: &mut Context<Self>) {
        self.profile = profile;
        let threads = self.thread_store.read(cx).threads.clone();
        for thread in threads {
            thread.update(cx, |thread, cx| {
                thread.update_local_profile(&self.profile, cx);
            });
        }
        cx.notify();
    }

    fn dismiss_profile_name_dialog(cowork: &WeakEntity<Self>, cx: &mut App) -> bool {
        cowork
            .update(cx, |cowork, cx| {
                cowork.profile_name_subscription = None;
                cx.notify();
            })
            .is_ok()
    }
}
