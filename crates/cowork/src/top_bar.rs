//! The top bar: window controls, the sidebar toggle, sharing, and who is
//! in the thread.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, Context, IntoElement, MouseButton, SharedString, Window, div, point,
    prelude::*, px, rgb,
};
use gpui_base::GlobalState;
use gpui_component::{
    Icon, Sizable as _, TitleBar,
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    sidebar::SidebarToggleButton,
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName as AssetIconName;

use crate::{
    Cowork, MainStage,
    thread::{SharingStatus, Thread},
};

pub(crate) const TOP_BAR_HEIGHT: gpui::Pixels = px(40.);

/// `SidebarToggleButton` is a small icon button (`size_6`, 24px) centered in
/// the top bar; matching its top gap on the left keeps it evenly inset from
/// the window corner.
const SIDEBAR_TOGGLE_INSET: gpui::Pixels = px((40. - 24.) / 2.);

const MACOS_TRAFFIC_LIGHT_X_INSET: gpui::Pixels = px(12.);

const MACOS_TRAFFIC_LIGHT_SIZE: gpui::Pixels = px(14.);

pub(crate) fn macos_traffic_light_position() -> gpui::Point<gpui::Pixels> {
    point(
        MACOS_TRAFFIC_LIGHT_X_INSET,
        (TOP_BAR_HEIGHT - MACOS_TRAFFIC_LIGHT_SIZE) / 2.,
    )
}

impl Cowork {
    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("top-bar-sidebar-toggle")
            .debug_selector(|| "top-bar-sidebar-toggle".to_owned())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                SidebarToggleButton::new()
                    .collapsed(!self.sidebar_open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    })),
            )
    }

    pub(crate) fn render_top_bar(
        &self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_thread = self
            .active_thread_id
            .filter(|_| self.main_stage == MainStage::Thread)
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        let sharing_status = active_thread
            .as_ref()
            .map(|thread| thread.read(cx).sharing.status())
            .unwrap_or(SharingStatus::NotShared);
        let share_label = match sharing_status {
            SharingStatus::NotShared => "Share",
            SharingStatus::Sharing => "Sharing…",
            SharingStatus::Shared => "Unshare",
            SharingStatus::Connected => "Disconnect",
            SharingStatus::Failed => "Retry share",
        };
        let sharing_enabled = sharing_status != SharingStatus::Sharing;
        let endpoint_copied = self.active_thread_id == self.copied_endpoint_id;
        let copy_endpoint_button = Button::new("copy-endpoint-id")
            .icon(Icon::new(if endpoint_copied {
                AssetIconName::Check
            } else {
                AssetIconName::Link
            }))
            .custom(
                ButtonCustomVariant::new(cx)
                    .hover(rgb(0x2d2d30).into())
                    .active(rgb(0x3f3f46).into()),
            )
            .small()
            .size(px(28.))
            .mr_1()
            .debug_selector(|| "copy-endpoint-id".to_owned())
            .accessibility_label(if endpoint_copied {
                "Endpoint link copied"
            } else {
                "Copy endpoint link"
            })
            .tooltip(if endpoint_copied {
                "Copied"
            } else {
                "Copy endpoint link"
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(!endpoint_copied, |this| {
                this.on_click(cx.listener(|this, _, _, cx| {
                    this.copy_endpoint_id(cx);
                }))
            });
        let copy_endpoint_button = copy_endpoint_button
            .with_animation(
                if endpoint_copied {
                    "endpoint-copy-copied"
                } else {
                    "endpoint-copy-ready"
                },
                Animation::new(Duration::from_millis(220)).with_easing(gpui::ease_out_quint()),
                |button, delta| button.opacity(0.45 + 0.55 * delta),
            )
            .into_any_element();

        TitleBar::new()
            .h(TOP_BAR_HEIGHT)
            .bg(rgb(0x1c1c1f))
            .border_0()
            .when(!cfg!(target_os = "macos"), |this| {
                this.pl(SIDEBAR_TOGGLE_INSET)
            })
            .child(
                div()
                    .id("top-bar-content")
                    .debug_selector(|| "top-bar-content".to_owned())
                    .h_full()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        GlobalState::suppress_text_selection(cx);
                    })
                    .child(self.render_sidebar_toggle(cx))
                    .child(
                        div()
                            .h_full()
                            .flex()
                            .items_center()
                            .children(
                                active_thread
                                    .as_ref()
                                    .and_then(|thread| self.render_participants(thread.read(cx))),
                            )
                            .when(sharing_status == SharingStatus::Shared, |this| {
                                this.child(copy_endpoint_button)
                            })
                            .when(self.main_stage == MainStage::Thread, |this| {
                                this.child(
                                    div()
                                        .id("toggle-sharing")
                                        .h(px(28.))
                                        .px_3()
                                        .mr_2()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .occlude()
                                        .text_sm()
                                        .text_color(rgb(0x71717a))
                                        .when(sharing_enabled, |this| {
                                            this.cursor_pointer()
                                                .text_color(rgb(0xd4d4d8))
                                                .hover(|this| this.bg(rgb(0x2d2d30)))
                                        })
                                        .when(sharing_status == SharingStatus::Failed, |this| {
                                            this.text_color(rgb(0xf87171))
                                        })
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.toggle_sharing(window, cx);
                                        }))
                                        .child(share_label),
                                )
                            }),
                    ),
            )
    }

    /// The connected participants of a shared thread as overlapping avatars,
    /// in join order, each naming its participant on hover.
    fn render_participants(&self, thread: &Thread) -> Option<gpui::AnyElement> {
        const MAX_VISIBLE: usize = 5;
        const AVATAR_SIZE: f32 = 24.;
        const AVATAR_OVERLAP: f32 = 6.;

        if thread.participants.is_empty() {
            return None;
        }
        let visible = thread.participants.len().min(MAX_VISIBLE);
        let hidden = thread.participants.len() - visible;
        let avatars = thread
            .participants
            .iter()
            .take(MAX_VISIBLE)
            .enumerate()
            .map(|(index, &participant)| {
                let name: SharedString = if participant == thread.participant_id {
                    format!("{} (you)", self.name_of(participant)).into()
                } else {
                    self.name_of(participant)
                };
                self.render_participant_avatar(participant, px(AVATAR_SIZE))
                    // Separates overlapping avatars from each other.
                    .border_2()
                    .border_color(rgb(0x1c1c1f))
                    .id(("participant", index))
                    .when(index > 0, |this| this.ml(px(-AVATAR_OVERLAP)))
                    .tooltip(move |window, cx| Tooltip::new(name.clone()).build(window, cx))
            });
        // Flex layout measures overlapping (negatively margined) children as
        // taking no room at all, so the row is sized explicitly.
        let avatars_width =
            AVATAR_SIZE + (AVATAR_SIZE - AVATAR_OVERLAP) * (visible.saturating_sub(1) as f32);

        Some(
            div()
                .id("participants")
                .debug_selector(|| "participants".to_owned())
                .mr_2()
                .flex_none()
                .flex()
                .items_center()
                .occlude()
                .child(
                    div()
                        .w(px(avatars_width))
                        .flex_none()
                        .flex()
                        .items_center()
                        .children(avatars),
                )
                .when(hidden > 0, |this| {
                    this.child(
                        div()
                            .ml_1()
                            .text_xs()
                            .text_color(rgb(0xa1a1aa))
                            .child(format!("+{hidden}")),
                    )
                })
                .into_any_element(),
        )
    }
}
