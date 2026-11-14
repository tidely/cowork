//! A window showing the sharing popover in each of its states, drawn by the
//! app's own [`render_sharing_panel`] with fake state so every control can
//! be clicked, including long names that must truncate. Below it are the
//! designs explored before choosing it ("B"), each in its states: mockups
//! drawn here only, kept for reference.
//! `cargo run -p cowork --example share_preview` opens it.

use std::{rc::Rc, time::Duration};

use gpui::{
    Anchor, AnyElement, App, Context, FontWeight, Hsla, QuitMode, Render, SharedString, WeakEntity,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Root, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenuItem},
    switch::Switch,
};
use gpui_kit_assets::IconName;

use crate::{
    Cowork,
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
                    title: Some("Cowork sharing popover designs".into()),
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

const MODES: [PeerMode; 3] = [PeerMode::ReadOnly, PeerMode::Write, PeerMode::Admin];
const LINK: &str = "b7e4c09a51f2d3e8a6c4f10e92b7d5a3c8e1f6049b2d7a5c3e8f1a6b4c9d2e7f";
const WIDTH: f32 = 300.;

#[derive(Clone, Copy, PartialEq)]
enum Status {
    NotShared,
    Sharing,
    Shared,
    Connected,
}

struct Peer {
    id: ParticipantId,
    mode: Option<PeerMode>,
}

struct Mock {
    status: Status,
    default: PeerMode,
    peers: Vec<Peer>,
    copied: bool,
}

#[derive(Clone, Copy)]
enum Design {
    Current,
    LinkField,
    Toggle,
    Refined,
    Menu,
}

const DESIGNS: [Design; 5] = [
    Design::Current,
    Design::LinkField,
    Design::Toggle,
    Design::Refined,
    Design::Menu,
];

impl Design {
    fn title(self) -> &'static str {
        match self {
            Self::Current => "0 · Original",
            Self::LinkField => "A · Link field",
            Self::Toggle => "B · Share switch",
            Self::Refined => "C · Refined current",
            Self::Menu => "D · Menu list",
        }
    }

    fn thesis(self) -> &'static str {
        match self {
            Self::Current => "The popover before the redesign, recreated for comparison.",
            Self::LinkField => {
                "Once shared, the link is the content: a read-only field with an attached Copy \
                 button, like Google Meet, Zoom, or Figma. Stop sharing becomes a quiet footer \
                 action."
            }
            Self::Toggle => {
                "Apple's sharing popover: a switch shows and controls the state, \"Copy link\" is \
                 the one primary button, and access is a plain sentence (\"Anyone who joins can \
                 edit\") that opens the options."
            }
            Self::Refined => {
                "The current layout with the fewest changes: a status in the header, text labels \
                 on the mode track, and Copy link as the primary footer button with Stop sharing \
                 beside it."
            }
            Self::Menu => {
                "Like the project-mode dropdown: every action is a menu row with an icon and a \
                 label, and access is a radio list. The most compact and keyboard-friendly."
            }
        }
    }

    fn notes(self) -> &'static [&'static str] {
        match self {
            Self::Current => &[
                "− Copy link is a 26 px icon beside a label that doesn't change with the state",
                "− Before sharing, Copy link is disabled with no explanation",
                "− Stop sharing, the destructive action, gets the full-width slot",
                "− Modes are icons only; you have to hover to read them",
                "− \"Default\" doesn't say what it's the default for",
            ],
            Self::LinkField => &[
                "+ You can see the link, so you know what you're copying",
                "+ Copy sits next to what it copies (proximity)",
                "+ Modes read as sentences (\"Can edit\") with descriptions in the menu",
                "+ Stop sharing is red, small, and last",
                "− The endpoint ID isn't readable, so showing it adds little meaning",
                "− Tallest of the four once people join",
            ],
            Self::Toggle => &[
                "+ Most familiar on macOS and iOS (HIG: Collaboration and sharing)",
                "+ The switch shows the state at a glance and is reversible",
                "+ One sentence summarizes access (HIG: \"succinct phrases\")",
                "+ The primary action changes from turning sharing on to Copy link",
                "− A switch for a network action has to show a loading state",
                "− Turning the switch off ends every session without asking",
            ],
            Self::Refined => &[
                "+ Least change, so the least retraining and code churn",
                "+ Labeled segments are faster to scan than icons",
                "+ Copy link is primary; Stop sharing is outlined and labeled by its tooltip",
                "− Three segments plus a reset per person get crowded",
                "− Stop sharing is icon-only",
            ],
            Self::Menu => &[
                "+ Matches the app's other dropdowns (consistency)",
                "+ Large hit areas: every row is a target",
                "+ Access options are visible without opening a second menu",
                "− Fine for actions but weaker at showing state",
                "− The radio list takes room even after you've set it",
            ],
        }
    }
}

const COLUMNS: [&str; 3] = [
    "Not shared (click to share)",
    "Shared, 2 collaborators",
    "As a collaborator",
];

struct SharePreview {
    /// The implemented popover's states, by column.
    implemented: Vec<SharingPanel>,
    cells: Vec<Mock>,
    host: ParticipantId,
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
            name: Some(SharedString::from(name.to_owned())),
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
        let cells = DESIGNS
            .iter()
            .flat_map(|_| {
                [Status::NotShared, Status::Shared, Status::Connected].map(|status| Mock {
                    status,
                    default: PeerMode::Write,
                    peers: if status == Status::Shared {
                        vec![
                            Peer {
                                id: ParticipantId::new(),
                                mode: None,
                            },
                            Peer {
                                id: ParticipantId::new(),
                                mode: Some(PeerMode::Admin),
                            },
                        ]
                    } else {
                        Vec::new()
                    },
                    copied: false,
                })
            })
            .collect();
        Self {
            implemented: implemented_states(),
            cells,
            host: ParticipantId::new(),
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
            menu_open: Rc::new(|_, _, _| {}),
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

    fn toggle(&mut self, idx: usize, cx: &mut Context<Self>) {
        let cell = &mut self.cells[idx];
        match cell.status {
            Status::NotShared => {
                cell.status = Status::Sharing;
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(900))
                        .await;
                    _ = this.update(cx, |this, cx| {
                        let cell = &mut this.cells[idx];
                        if cell.status == Status::Sharing {
                            cell.status = Status::Shared;
                            if cell.peers.is_empty() {
                                cell.peers.push(Peer {
                                    id: ParticipantId::new(),
                                    mode: None,
                                });
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            Status::Sharing | Status::Shared | Status::Connected => {
                cell.status = Status::NotShared;
                cell.peers.clear();
            }
        }
        cx.notify();
    }

    fn copy(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.cells[idx].copied = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1_500))
                .await;
            _ = this.update(cx, |this, cx| {
                this.cells[idx].copied = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn picker(&self, idx: usize, peer: Option<usize>, cx: &Context<Self>) -> Pick {
        let entity = cx.entity().downgrade();
        Rc::new(move |mode, cx| set(&entity, idx, peer, mode, cx))
    }

    fn render_cell(&self, design: Design, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let content: Vec<AnyElement> = match design {
            Design::Current => self.current(idx, cx),
            Design::LinkField => self.link_field(idx, cx),
            Design::Toggle => self.toggle_design(idx, cx),
            Design::Refined => self.refined(idx, cx),
            Design::Menu => self.menu(idx, cx),
        };
        div()
            .w(px(WIDTH))
            .p_2()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_2()
            .children(content)
            .into_any_element()
    }

    // ---- 0 · Current -------------------------------------------------------

    fn current(&self, idx: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let m = &self.cells[idx];
        let mut out = Vec::new();
        if m.status != Status::Connected {
            out.push(
                row()
                    .child(div().text_sm().child("Sharing"))
                    .child(
                        Button::new(id(idx, "copy"))
                            .icon(
                                Icon::new(if m.copied {
                                    IconName::Check
                                } else {
                                    IconName::Link
                                })
                                .size_4(),
                            )
                            .ghost()
                            .small()
                            .size(px(26.))
                            .disabled(m.status != Status::Shared)
                            .tooltip("Copy link")
                            .on_click(cx.listener(move |this, _, _, cx| this.copy(idx, cx))),
                    )
                    .into_any_element(),
            );
            out.push(
                row()
                    .h(px(34.))
                    .gap_2()
                    .child(div().flex_1().text_sm().child("Default"))
                    .child(track(
                        idx,
                        "default",
                        m.default,
                        false,
                        self.picker(idx, None, cx),
                        cx,
                    ))
                    .child(div().w(px(24.)))
                    .into_any_element(),
            );
            for (i, peer) in m.peers.iter().enumerate() {
                out.push(self.track_peer_row(idx, i, peer, false, cx));
            }
            out.push(sep(cx));
        } else {
            out.push(
                row()
                    .px_1()
                    .child(div().text_sm().child("Your access"))
                    .child(
                        Button::new(id(idx, "your"))
                            .icon(Icon::new(mode_icon(PeerMode::Write)).size_4())
                            .ghost()
                            .small()
                            .size(px(28.))
                            .tooltip("Write · Edit drafts and attachments"),
                    )
                    .into_any_element(),
            );
            out.push(sep(cx));
        }
        let (label, icon) = match m.status {
            Status::NotShared => ("Share thread", IconName::Share2),
            Status::Sharing => ("Sharing…", IconName::Share2),
            Status::Shared => ("Stop sharing", IconName::Unlink),
            Status::Connected => ("Disconnect", IconName::LogOut),
        };
        out.push(
            Button::new(id(idx, "toggle"))
                .icon(Icon::new(icon).size_4())
                .label(label)
                .ghost()
                .small()
                .w_full()
                .loading(m.status == Status::Sharing)
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx)))
                .into_any_element(),
        );
        out
    }

    fn track_peer_row(
        &self,
        idx: usize,
        i: usize,
        peer: &Peer,
        labels: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let m = &self.cells[idx];
        let entity = cx.entity().downgrade();
        row()
            .h(px(34.))
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .child(name(peer.id)),
            )
            .child(track(
                idx,
                &format!("peer{i}"),
                peer.mode.unwrap_or(m.default),
                labels,
                self.picker(idx, Some(i), cx),
                cx,
            ))
            .child(
                Button::new(id(idx, &format!("reset{i}")))
                    .icon(Icon::new(IconName::RotateCcw).size_3())
                    .ghost()
                    .small()
                    .size(px(24.))
                    .disabled(peer.mode.is_none())
                    .tooltip(if peer.mode.is_none() {
                        "Using default access"
                    } else {
                        "Reset to default access"
                    })
                    .on_click(move |_, _, cx| set(&entity, idx, Some(i), None, cx)),
            )
            .into_any_element()
    }

    // ---- A · Link field ----------------------------------------------------

    fn link_field(&self, idx: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let m = &self.cells[idx];
        let muted = cx.theme().muted_foreground;
        let mut out = Vec::new();
        match m.status {
            Status::NotShared | Status::Sharing => {
                out.push(
                    div()
                        .px_1()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(title("Share thread"))
                        .child(caption(
                            "Anyone with the link can join while this thread is open.",
                            cx,
                        ))
                        .into_any_element(),
                );
                out.push(self.new_people_row(idx, "New collaborators", cx));
                let sharing = m.status == Status::Sharing;
                out.push(
                    Button::new(id(idx, "start"))
                        .icon(Icon::new(IconName::Share2).size_4())
                        .label(if sharing {
                            "Creating link…"
                        } else {
                            "Start sharing"
                        })
                        .primary()
                        .small()
                        .w_full()
                        .loading(sharing)
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx)))
                        .into_any_element(),
                );
            }
            Status::Shared => {
                out.push(
                    row()
                        .px_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(dot(cx.theme().success))
                                .child(title("Sharing")),
                        )
                        .child(caption(people(m.peers.len()), cx))
                        .into_any_element(),
                );
                out.push(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h(px(28.))
                                .px_2()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().secondary)
                                .text_xs()
                                .text_color(muted)
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(div().truncate().child(LINK)),
                        )
                        .child(
                            Button::new(id(idx, "copy"))
                                .icon(
                                    Icon::new(if m.copied {
                                        IconName::Check
                                    } else {
                                        IconName::Copy
                                    })
                                    .size_3p5(),
                                )
                                .label(if m.copied { "Copied" } else { "Copy" })
                                .primary()
                                .small()
                                .h(px(28.))
                                .on_click(cx.listener(move |this, _, _, cx| this.copy(idx, cx))),
                        )
                        .into_any_element(),
                );
                out.push(self.new_people_row(idx, "New collaborators", cx));
                out.extend(self.people_rows(idx, cx));
                out.push(sep(cx));
                out.push(
                    div()
                        .flex()
                        .child(
                            Button::new(id(idx, "stop"))
                                .icon(Icon::new(IconName::Unlink).size_3p5())
                                .label("Stop sharing")
                                .ghost()
                                .small()
                                .text_color(cx.theme().danger)
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx))),
                        )
                        .into_any_element(),
                );
            }
            Status::Connected => {
                let host = self.host;
                out.push(
                    row()
                        .px_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(dot(cx.theme().success))
                                .child(title("Connected")),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(Cowork::render_avatar_for(host, None, px(16.)))
                                .child(caption(format!("Hosted by {}", name(host)), cx)),
                        )
                        .into_any_element(),
                );
                out.push(access_box(PeerMode::Write, cx));
                out.push(sep(cx));
                out.push(
                    div()
                        .flex()
                        .child(
                            Button::new(id(idx, "leave"))
                                .icon(Icon::new(IconName::LogOut).size_3p5())
                                .label("Leave thread")
                                .ghost()
                                .small()
                                .text_color(cx.theme().danger)
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx))),
                        )
                        .into_any_element(),
                );
            }
        }
        out
    }

    fn new_people_row(
        &self,
        idx: usize,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let m = &self.cells[idx];
        row()
            .pl_1()
            .child(div().text_sm().child(label))
            .child(mode_menu(
                id(idx, "default-menu"),
                can(m.default).into(),
                Some(m.default),
                None,
                self.picker(idx, None, cx),
            ))
            .into_any_element()
    }

    fn people_rows(&self, idx: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let m = &self.cells[idx];
        if m.peers.is_empty() {
            return Vec::new();
        }
        let mut out = vec![
            sep(cx),
            div().px_1().child(caption("People", cx)).into_any_element(),
        ];
        for (i, peer) in m.peers.iter().enumerate() {
            let effective = peer.mode.unwrap_or(m.default);
            let menu = mode_menu(
                id(idx, &format!("peer{i}-menu")),
                can(effective).into(),
                peer.mode,
                Some(m.default),
                self.picker(idx, Some(i), cx),
            );
            out.push(
                row()
                    .pl_1()
                    .h(px(28.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(Cowork::render_avatar_for(peer.id, None, px(20.)))
                            .child(div().text_sm().truncate().child(name(peer.id)))
                            .when(peer.mode.is_none(), |this| {
                                this.child(caption("default", cx))
                            }),
                    )
                    .child(menu)
                    .into_any_element(),
            );
        }
        out
    }

    // ---- B · Share switch --------------------------------------------------

    fn toggle_design(&self, idx: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let m = &self.cells[idx];
        let mut out = Vec::new();
        if m.status == Status::Connected {
            let host = self.host;
            out.push(
                row()
                    .px_1()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(Cowork::render_avatar_for(host, None, px(24.)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .child(title(format!("{}'s thread", name(host))))
                                    .child(caption("You're connected", cx)),
                            ),
                    )
                    .child(
                        Button::new(id(idx, "leave"))
                            .label("Leave")
                            .outline()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx))),
                    )
                    .into_any_element(),
            );
            out.push(sep(cx));
            out.push(
                row()
                    .px_1()
                    .h(px(28.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(mode_icon(PeerMode::Write)).size_4())
                            .child(div().text_sm().child("You can edit")),
                    )
                    .child(caption("Set by the host", cx))
                    .into_any_element(),
            );
            return out;
        }
        let on = matches!(m.status, Status::Sharing | Status::Shared);
        out.push(
            row()
                .px_1()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(title("Share thread"))
                        .child(caption(
                            match m.status {
                                Status::NotShared => "Only you can see this thread.",
                                Status::Sharing => "Creating link…",
                                _ => "Anyone with the link can join.",
                            },
                            cx,
                        )),
                )
                .child(
                    Switch::new(id(idx, "switch"))
                        .checked(on)
                        .on_click(cx.listener(move |this, _: &bool, _, cx| this.toggle(idx, cx))),
                )
                .into_any_element(),
        );
        if m.status == Status::Shared {
            out.push(
                Button::new(id(idx, "copy"))
                    .icon(
                        Icon::new(if m.copied {
                            IconName::Check
                        } else {
                            IconName::Link
                        })
                        .size_4(),
                    )
                    .label(if m.copied { "Link copied" } else { "Copy link" })
                    .primary()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| this.copy(idx, cx)))
                    .into_any_element(),
            );
        }
        let summary = format!("Anyone who joins {}", can(m.default).to_lowercase());
        out.push(
            mode_menu(
                id(idx, "summary"),
                summary.into(),
                Some(m.default),
                None,
                self.picker(idx, None, cx),
            )
            .into_any_element(),
        );
        if m.status == Status::Shared && !m.peers.is_empty() {
            out.push(sep(cx));
            for (i, peer) in m.peers.iter().enumerate() {
                let effective = peer.mode.unwrap_or(m.default);
                out.push(
                    row()
                        .px_1()
                        .h(px(32.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(Cowork::render_avatar_for(peer.id, None, px(24.)))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .min_w_0()
                                        .child(div().text_sm().truncate().child(name(peer.id)))
                                        .child(caption(
                                            if peer.mode.is_none() {
                                                "Joined · default access"
                                            } else {
                                                "Joined"
                                            },
                                            cx,
                                        )),
                                ),
                        )
                        .child(mode_menu(
                            id(idx, &format!("peer{i}-menu")),
                            can(effective).into(),
                            peer.mode,
                            Some(m.default),
                            self.picker(idx, Some(i), cx),
                        ))
                        .into_any_element(),
                );
            }
        }
        out
    }

    // ---- C · Refined current -----------------------------------------------

    fn refined(&self, idx: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let m = &self.cells[idx];
        let mut out = Vec::new();
        if m.status == Status::Connected {
            out.push(
                row()
                    .px_1()
                    .h(px(28.))
                    .child(div().text_sm().child("Your access"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .text_sm()
                            .child(Icon::new(mode_icon(PeerMode::Write)).size_4())
                            .child("Edit"),
                    )
                    .into_any_element(),
            );
            out.push(sep(cx));
            out.push(
                Button::new(id(idx, "toggle"))
                    .icon(Icon::new(IconName::LogOut).size_4())
                    .label("Disconnect")
                    .ghost()
                    .small()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx)))
                    .into_any_element(),
            );
            return out;
        }
        let shared = m.status == Status::Shared;
        out.push(
            row()
                .px_1()
                .h(px(24.))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Sharing"),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .when(shared, |this| this.child(dot(cx.theme().success)))
                        .child(caption(
                            match m.status {
                                Status::NotShared => "Off".to_owned(),
                                Status::Sharing => "Starting…".to_owned(),
                                _ => format!("On · {}", people(m.peers.len())),
                            },
                            cx,
                        )),
                )
                .into_any_element(),
        );
        out.push(
            row()
                .pl_1()
                .h(px(34.))
                .gap_2()
                .child(div().flex_1().text_sm().child("New people"))
                .child(track(
                    idx,
                    "default",
                    m.default,
                    true,
                    self.picker(idx, None, cx),
                    cx,
                ))
                .child(div().w(px(24.)))
                .into_any_element(),
        );
        for (i, peer) in m.peers.iter().enumerate() {
            out.push(self.track_peer_row(idx, i, peer, true, cx));
        }
        out.push(sep(cx));
        if shared {
            out.push(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new(id(idx, "copy"))
                            .icon(
                                Icon::new(if m.copied {
                                    IconName::Check
                                } else {
                                    IconName::Link
                                })
                                .size_4(),
                            )
                            .label(if m.copied { "Copied" } else { "Copy link" })
                            .primary()
                            .small()
                            .flex_1()
                            .on_click(cx.listener(move |this, _, _, cx| this.copy(idx, cx))),
                    )
                    .child(
                        Button::new(id(idx, "stop"))
                            .icon(Icon::new(IconName::Unlink).size_4())
                            .outline()
                            .small()
                            .tooltip("Stop sharing")
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx))),
                    )
                    .into_any_element(),
            );
        } else {
            let sharing = m.status == Status::Sharing;
            out.push(
                Button::new(id(idx, "toggle"))
                    .icon(Icon::new(IconName::Share2).size_4())
                    .label(if sharing {
                        "Sharing…"
                    } else {
                        "Share thread"
                    })
                    .primary()
                    .small()
                    .w_full()
                    .loading(sharing)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(idx, cx)))
                    .into_any_element(),
            );
        }
        out
    }

    // ---- D · Menu list -----------------------------------------------------

    fn menu(&self, idx: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let m = &self.cells[idx];
        let danger = cx.theme().danger;
        let mut out = Vec::new();
        let toggle = cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.toggle(idx, cx));
        match m.status {
            Status::NotShared | Status::Sharing => {
                let sharing = m.status == Status::Sharing;
                out.push(menu_row(
                    id(idx, "share"),
                    Some(Icon::new(if sharing {
                        IconName::Loader
                    } else {
                        IconName::Share2
                    })),
                    if sharing {
                        "Creating link…"
                    } else {
                        "Share thread"
                    },
                    None,
                    None,
                    Some(Rc::new(toggle)),
                    cx,
                ));
                out.push(sep(cx));
                out.extend(self.radio_list(idx, cx));
            }
            Status::Shared => {
                out.push(
                    div()
                        .px_2()
                        .pt_1()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(dot(cx.theme().success))
                        .child(caption(format!("Sharing · {}", people(m.peers.len())), cx))
                        .into_any_element(),
                );
                let copy = cx.listener(move |this, _: &gpui::ClickEvent, _, cx| this.copy(idx, cx));
                out.push(menu_row(
                    id(idx, "copy"),
                    Some(Icon::new(IconName::Link)),
                    "Copy link",
                    m.copied.then(|| {
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(cx.theme().success)
                            .child(Icon::new(IconName::Check).size_3())
                            .child("Copied")
                            .into_any_element()
                    }),
                    None,
                    Some(Rc::new(copy)),
                    cx,
                ));
                out.push(sep(cx));
                out.extend(self.radio_list(idx, cx));
                if !m.peers.is_empty() {
                    out.push(sep(cx));
                    out.push(div().px_2().child(caption("People", cx)).into_any_element());
                    for (i, peer) in m.peers.iter().enumerate() {
                        let effective = peer.mode.unwrap_or(m.default);
                        out.push(
                            row()
                                .h(px(28.))
                                .px_2()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .min_w_0()
                                        .child(Cowork::render_avatar_for(peer.id, None, px(16.)))
                                        .child(div().text_sm().truncate().child(name(peer.id))),
                                )
                                .child(mode_menu(
                                    id(idx, &format!("peer{i}-menu")),
                                    can(effective).into(),
                                    peer.mode,
                                    Some(m.default),
                                    self.picker(idx, Some(i), cx),
                                ))
                                .into_any_element(),
                        );
                    }
                }
                out.push(sep(cx));
                out.push(menu_row(
                    id(idx, "stop"),
                    Some(Icon::new(IconName::Unlink)),
                    "Stop sharing",
                    None,
                    Some(danger),
                    Some(Rc::new(toggle)),
                    cx,
                ));
            }
            Status::Connected => {
                let host = self.host;
                out.push(
                    div()
                        .px_2()
                        .pt_1()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(Cowork::render_avatar_for(host, None, px(16.)))
                        .child(caption(format!("Connected to {}", name(host)), cx))
                        .into_any_element(),
                );
                out.push(menu_row(
                    id(idx, "access"),
                    Some(Icon::new(mode_icon(PeerMode::Write))),
                    "You can edit",
                    Some(caption("Set by the host", cx).into_any_element()),
                    None,
                    None,
                    cx,
                ));
                out.push(sep(cx));
                out.push(menu_row(
                    id(idx, "leave"),
                    Some(Icon::new(IconName::LogOut)),
                    "Leave thread",
                    None,
                    Some(danger),
                    Some(Rc::new(toggle)),
                    cx,
                ));
            }
        }
        out
    }

    fn radio_list(&self, idx: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let m = &self.cells[idx];
        let mut out = vec![
            div()
                .px_2()
                .child(caption("New collaborators", cx))
                .into_any_element(),
        ];
        for mode in MODES {
            let selected = m.default == mode;
            let pick = self.picker(idx, None, cx);
            out.push(menu_row(
                id(idx, &format!("radio-{}", mode.label())),
                Some(
                    Icon::new(if selected {
                        IconName::Check
                    } else {
                        mode_icon(mode)
                    })
                    .when(selected, |icon| icon.text_color(cx.theme().primary)),
                ),
                can(mode),
                Some(caption(short_what(mode), cx).into_any_element()),
                None,
                Some(Rc::new(
                    move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| pick(Some(mode), cx),
                )),
                cx,
            ));
        }
        out
    }
}

impl Render for SharePreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let sections: Vec<AnyElement> = DESIGNS
            .iter()
            .enumerate()
            .map(|(d, design)| {
                let cells = (0..3).map(|col| {
                    let idx = d * 3 + col;
                    div()
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .child(div().text_xs().text_color(muted).child(COLUMNS[col]))
                        .child(self.render_cell(*design, idx, cx))
                });
                let cells: Vec<_> = cells.collect();
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .pb_8()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(design.title()),
                            )
                            .child(
                                div()
                                    .max_w(px(1000.))
                                    .text_sm()
                                    .text_color(muted)
                                    .child(design.thesis()),
                            ),
                    )
                    .child(
                        div().flex().items_start().gap_6().children(cells).child(
                            div()
                                .w(px(320.))
                                .pt_5()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_xs()
                                .children(design.notes().iter().map(|note| {
                                    div()
                                        .when(note.starts_with('−'), |this| {
                                            this.text_color(muted)
                                        })
                                        .child(*note)
                                })),
                        ),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .id("share-preview")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_8()
                    .child(self.render_implemented(cx))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Explored designs"),
                    )
                    .children(sections),
            )
    }
}

// ---- Shared pieces ---------------------------------------------------------

type Pick = Rc<dyn Fn(Option<PeerMode>, &mut App)>;
type Click = Rc<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>;

fn set(
    entity: &WeakEntity<SharePreview>,
    idx: usize,
    peer: Option<usize>,
    mode: Option<PeerMode>,
    cx: &mut App,
) {
    _ = entity.update(cx, |this, cx| {
        let cell = &mut this.cells[idx];
        match peer {
            None => {
                if let Some(mode) = mode {
                    cell.default = mode;
                }
            }
            Some(i) => cell.peers[i].mode = mode,
        }
        cx.notify();
    });
}

fn id(idx: usize, name: &str) -> SharedString {
    SharedString::from(format!("share-preview-{idx}-{name}"))
}

fn name(participant: ParticipantId) -> SharedString {
    crate::profile::participant_name(participant, None)
}

fn people(count: usize) -> String {
    match count {
        0 => "No one yet".to_owned(),
        1 => "1 person".to_owned(),
        n => format!("{n} people"),
    }
}

fn row() -> gpui::Div {
    div().flex().items_center().justify_between()
}

fn title(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .truncate()
        .child(text.into())
}

fn caption(text: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn sep(cx: &App) -> AnyElement {
    div().h(px(1.)).bg(cx.theme().border).into_any_element()
}

fn dot(color: Hsla) -> gpui::Div {
    div().size(px(8.)).flex_none().rounded_full().bg(color)
}

fn mode_icon(mode: PeerMode) -> IconName {
    match mode {
        PeerMode::ReadOnly => IconName::Eye,
        PeerMode::Write => IconName::Pencil,
        PeerMode::Admin => IconName::Shield,
    }
}

fn can(mode: PeerMode) -> &'static str {
    match mode {
        PeerMode::ReadOnly => "Can view",
        PeerMode::Write => "Can edit",
        PeerMode::Admin => "Can run",
    }
}

fn short(mode: PeerMode) -> &'static str {
    match mode {
        PeerMode::ReadOnly => "View",
        PeerMode::Write => "Edit",
        PeerMode::Admin => "Run",
    }
}

fn what(mode: PeerMode) -> &'static str {
    match mode {
        PeerMode::ReadOnly => "Follow the thread as it happens",
        PeerMode::Write => "Also edit the draft and attachments",
        PeerMode::Admin => "Also submit, stop, and change models",
    }
}

fn short_what(mode: PeerMode) -> &'static str {
    match mode {
        PeerMode::ReadOnly => "Follow along",
        PeerMode::Write => "Draft and attach",
        PeerMode::Admin => "Submit and stop",
    }
}

fn access_box(mode: PeerMode, cx: &App) -> AnyElement {
    div()
        .p_2()
        .rounded_md()
        .bg(cx.theme().secondary)
        .flex()
        .items_start()
        .gap_2()
        .child(Icon::new(mode_icon(mode)).size_4().mt_0p5())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("You {}", can(mode).to_lowercase())),
                )
                .child(caption(format!("{}. Set by the host.", what(mode)), cx)),
        )
        .into_any_element()
}

/// A segmented control, icons only or labeled.
fn track(
    idx: usize,
    key: &str,
    effective: PeerMode,
    labels: bool,
    pick: Pick,
    cx: &App,
) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .gap(px(2.))
        .p(px(2.))
        .rounded_lg()
        .bg(cx.theme().secondary)
        .children(MODES.into_iter().map(|mode| {
            let selected = effective == mode;
            let pick = pick.clone();
            let button = Button::new(id(idx, &format!("{key}-{}", mode.label())))
                .ghost()
                .small()
                .selected(selected)
                .when(selected, |button| button.primary())
                .tooltip(what(mode))
                .on_click(move |_, _, cx| pick(Some(mode), cx));
            if labels {
                button.label(short(mode)).h(px(24.)).px_2()
            } else {
                button
                    .icon(Icon::new(mode_icon(mode)).size_4())
                    .size(px(28.))
            }
        }))
}

/// A button opening the access options, with a "use default" item for people.
fn mode_menu(
    id: SharedString,
    label: SharedString,
    selected: Option<PeerMode>,
    default_for_peer: Option<PeerMode>,
    pick: Pick,
) -> impl IntoElement {
    Button::new(id)
        .label(label)
        .ghost()
        .xsmall()
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            let menu = MODES.into_iter().fold(menu, |menu, mode| {
                let pick = pick.clone();
                menu.item(
                    PopupMenuItem::element(move |_, cx| {
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(can(mode)))
                            .child(caption(what(mode), cx))
                    })
                    .checked(selected == Some(mode))
                    .on_click(move |_, _, cx| pick(Some(mode), cx)),
                )
            });
            match default_for_peer {
                Some(default) => {
                    let pick = pick.clone();
                    menu.separator().item(
                        PopupMenuItem::new(format!(
                            "Use default ({})",
                            can(default).to_lowercase()
                        ))
                        .checked(selected.is_none())
                        .on_click(move |_, _, cx| pick(None, cx)),
                    )
                }
                None => menu,
            }
        })
}

fn menu_row(
    id: SharedString,
    icon: Option<Icon>,
    label: &'static str,
    trailing: Option<AnyElement>,
    color: Option<Hsla>,
    on_click: Option<Click>,
    cx: &App,
) -> AnyElement {
    let hover = cx.theme().list_hover;
    let interactive = on_click.is_some();
    div()
        .id(id)
        .h(px(28.))
        .px_2()
        .rounded_md()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .when_some(color, |this, color| this.text_color(color))
        .when(interactive, |this| {
            this.cursor_pointer().hover(move |style| style.bg(hover))
        })
        .when_some(on_click, |this, on_click| {
            this.on_click(move |event, window, cx| on_click(event, window, cx))
        })
        .when_some(icon, |this, icon| this.child(icon.size_4()))
        .child(div().flex_1().child(label))
        .children(trailing)
        .into_any_element()
}
