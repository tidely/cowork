//! A window showing the sharing popover in each of its states, drawn by the
//! app's own [`render_sharing_panel`] with fake state so every control can
//! be clicked, including long names that must truncate.
//! `cargo run -p cowork --example share_preview` opens it.

use std::{rc::Rc, time::Duration};

use gpui::{
    AnyElement, App, Context, FontWeight, QuitMode, Render, SharedString, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, size,
};
use gpui_component::{ActiveTheme as _, Root};

use crate::{
    participant::ParticipantId,
    peer_access::{
        PANEL_WIDTH, PeerRow, Person, SharingActions, SharingPanel, render_sharing_panel,
    },
    profile::Profile,
    thread::{PeerMode, SharingStatus},
};

pub fn run() -> anyhow::Result<()> {
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(|cx: &mut App| {
            crate::init_app(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
                    None,
                    size(px(1440.), px(960.)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Cowork sharing popover".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(error) = cx.open_window(options, |window, cx| {
                let preview = cx.new(|_| SharePreview::new());
                cx.new(|cx| Root::new(preview, window, cx))
            }) {
                eprintln!("failed to open the share preview: {error}");
                cx.quit();
            }
            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });
    Ok(())
}

struct SharePreview {
    implemented: Vec<SharingPanel>,
}

const IMPLEMENTED_COLUMNS: [&str; 5] = [
    "Not shared",
    "Creating the link",
    "Shared, with a long name and an override",
    "Sharing failed",
    "As a collaborator, with a long host name",
];

const LONG_NAME: &str = "Bartholomew Montgomery-Fitzgerald the Third";

fn preview_person(name: Option<&str>) -> Person {
    Person {
        id: ParticipantId::new(),
        profile: name.map(|name| Profile {
            name: Some(SharedString::from(name)),
            ..Default::default()
        }),
    }
}

fn implemented_states() -> Vec<SharingPanel> {
    let panel = |status, peers, host| SharingPanel {
        status,
        copied: false,
        default_mode: PeerMode::Write,
        peers,
        host,
        local_mode: PeerMode::ReadOnly,
    };
    vec![
        panel(SharingStatus::NotShared, Vec::new(), None),
        panel(SharingStatus::Sharing, Vec::new(), None),
        panel(
            SharingStatus::Shared,
            vec![
                PeerRow {
                    person: preview_person(Some(LONG_NAME)),
                    override_mode: None,
                },
                PeerRow {
                    person: preview_person(None),
                    override_mode: Some(PeerMode::Admin),
                },
            ],
            None,
        ),
        panel(SharingStatus::Failed, Vec::new(), None),
        panel(
            SharingStatus::Connected,
            Vec::new(),
            Some(preview_person(Some(LONG_NAME))),
        ),
    ]
}

impl SharePreview {
    fn new() -> Self {
        Self {
            implemented: implemented_states(),
        }
    }

    /// What the implemented popover's controls do to its state in column
    /// `idx`: sharing takes a moment to create the link, as in the app.
    fn implemented_actions(&self, idx: usize, cx: &Context<Self>) -> SharingActions {
        let entity = cx.entity().downgrade();
        SharingActions {
            toggle: Rc::new({
                let entity = entity.clone();
                move |_, cx| {
                    _ = entity.update(cx, |this, cx| {
                        let panel = &mut this.implemented[idx];
                        match panel.status {
                            SharingStatus::NotShared | SharingStatus::Failed => {
                                panel.status = SharingStatus::Sharing;
                                cx.spawn(async move |this, cx| {
                                    cx.background_executor()
                                        .timer(Duration::from_millis(900))
                                        .await;
                                    _ = this.update(cx, |this, cx| {
                                        let panel = &mut this.implemented[idx];
                                        if panel.status == SharingStatus::Sharing {
                                            panel.status = SharingStatus::Shared;
                                        }
                                        cx.notify();
                                    });
                                })
                                .detach();
                            }
                            SharingStatus::Sharing => {}
                            SharingStatus::Shared => {
                                panel.status = SharingStatus::NotShared;
                                panel.peers.clear();
                            }
                            // Leaving would close the popover; start over.
                            SharingStatus::Connected => *this = Self::new(),
                        }
                        cx.notify();
                    });
                }
            }),
            copy: Rc::new({
                let entity = entity.clone();
                move |_, cx| {
                    _ = entity.update(cx, |this, cx| {
                        this.implemented[idx].copied = true;
                        cx.notify();
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(1_500))
                                .await;
                            _ = this.update(cx, |this, cx| {
                                this.implemented[idx].copied = false;
                                cx.notify();
                            });
                        })
                        .detach();
                    });
                }
            }),
            set_mode: Rc::new(move |participant, mode, _, cx| {
                _ = entity.update(cx, |this, cx| {
                    let panel = &mut this.implemented[idx];
                    match participant {
                        None => {
                            if let Some(mode) = mode {
                                panel.default_mode = mode;
                            }
                        }
                        Some(participant) => {
                            if let Some(peer) = panel
                                .peers
                                .iter_mut()
                                .find(|peer| peer.person.id == participant)
                            {
                                peer.override_mode = mode;
                            }
                        }
                    }
                    cx.notify();
                });
            }),
            menu_open: Rc::new(|_, _, _, _| {}),
        }
    }

    fn render_implemented(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let cells: Vec<AnyElement> = self
            .implemented
            .iter()
            .enumerate()
            .map(|(idx, panel)| {
                let actions = self.implemented_actions(idx, cx);
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(IMPLEMENTED_COLUMNS[idx]),
                    )
                    .child(
                        // Styled as the app's popover draws it.
                        div()
                            .w(PANEL_WIDTH)
                            .p_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().popover)
                            .text_color(cx.theme().popover_foreground)
                            .shadow_lg()
                            .child(render_sharing_panel(panel, &actions, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .pb_8()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Sharing popover"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("As the app draws it. Every control works on this preview's own state."),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .gap_6()
                    .children(cells),
            )
            .into_any_element()
    }
}

impl Render for SharePreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("share-preview")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().p_8().child(self.render_implemented(cx)))
    }
}
