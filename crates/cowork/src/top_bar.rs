//! The top bar: window controls, the sidebar toggle, sharing, and who is
//! in the thread.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, Context, Decorations, IntoElement, MouseButton, MouseDownEvent,
    SharedString, Window, WindowControlArea, div, point, prelude::*, px, rgb,
};
use gpui_base::GlobalState;
use gpui_component::{
    Icon, Sizable as _,
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

const MACOS_TRAFFIC_LIGHT_SPACING: gpui::Pixels = px(6.);

const MACOS_TRAFFIC_LIGHT_TRAILING_GAP: gpui::Pixels = px(12.);

pub(crate) fn macos_traffic_light_position() -> gpui::Point<gpui::Pixels> {
    point(
        MACOS_TRAFFIC_LIGHT_X_INSET,
        (TOP_BAR_HEIGHT - MACOS_TRAFFIC_LIGHT_SIZE) / 2.,
    )
}

fn macos_sidebar_toggle_margin() -> gpui::Pixels {
    MACOS_TRAFFIC_LIGHT_X_INSET
        + MACOS_TRAFFIC_LIGHT_SIZE * 3.
        + MACOS_TRAFFIC_LIGHT_SPACING * 2.
        + MACOS_TRAFFIC_LIGHT_TRAILING_GAP
}

impl Cowork {
    fn render_caption_button(
        id: &'static str,
        icon: AssetIconName,
        control_area: WindowControlArea,
        is_close: bool,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h_full()
            .w(px(46.))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .text_color(rgb(0xd4d4d8))
            .window_control_area(control_area)
            .when(is_close, |this| this.hover(|this| this.bg(rgb(0xe81123))))
            .when(!is_close, |this| this.hover(|this| this.bg(rgb(0x2d2d30))))
            .when(cfg!(target_os = "linux"), |this| {
                this.on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    match control_area {
                        WindowControlArea::Min => window.minimize_window(),
                        WindowControlArea::Max => window.zoom_window(),
                        WindowControlArea::Close => window.remove_window(),
                        _ => {}
                    }
                })
            })
            .child(Icon::new(icon).size(px(12.)))
    }

    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .ml(if cfg!(target_os = "macos") {
                macos_sidebar_toggle_margin()
            } else {
                SIDEBAR_TOGGLE_INSET
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.titlebar_click_armed = false;
                    cx.stop_propagation();
                }),
            )
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
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let show_controls =
            !cfg!(target_os = "macos") && (!cfg!(target_os = "linux") || client_decorated);
        let supported_controls = window.window_controls();
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.titlebar_click_armed = false;
                    cx.stop_propagation();
                }),
            )
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

        div()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(0x1c1c1f))
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    GlobalState::suppress_text_selection(cx);

                    if cfg!(target_os = "macos") {
                        cx.stop_propagation();
                        let is_titlebar_double_click =
                            event.click_count == 2 && this.titlebar_click_armed;
                        this.titlebar_click_armed = event.click_count == 1;

                        if is_titlebar_double_click {
                            window.titlebar_double_click();
                        } else {
                            window.start_window_move();
                        }
                    } else if cfg!(target_os = "linux")
                        && matches!(window.window_decorations(), Decorations::Client { .. })
                    {
                        cx.stop_propagation();
                        if event.click_count == 2 {
                            window.zoom_window();
                        } else {
                            window.start_window_move();
                        }
                    }
                }),
            )
            .when(cfg!(target_os = "linux") && client_decorated, |this| {
                this.on_mouse_down(MouseButton::Right, |event, window, _| {
                    window.show_window_menu(event.position);
                })
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
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.titlebar_click_armed = false;
                                        cx.stop_propagation();
                                    }),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_sharing(window, cx);
                                }))
                                .child(share_label),
                        )
                    })
                    .when(show_controls, |this| {
                        this.child(
                            div()
                                .h_full()
                                .flex()
                                .when(supported_controls.minimize, |this| {
                                    this.child(Self::render_caption_button(
                                        "minimize-window",
                                        AssetIconName::WindowMinimize,
                                        WindowControlArea::Min,
                                        false,
                                    ))
                                })
                                .when(supported_controls.maximize, |this| {
                                    this.child(Self::render_caption_button(
                                        "maximize-window",
                                        if window.is_maximized() {
                                            AssetIconName::WindowRestore
                                        } else {
                                            AssetIconName::WindowMaximize
                                        },
                                        WindowControlArea::Max,
                                        false,
                                    ))
                                })
                                .child(Self::render_caption_button(
                                    "close-window",
                                    AssetIconName::WindowClose,
                                    WindowControlArea::Close,
                                    true,
                                )),
                        )
                    }),
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
