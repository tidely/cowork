//! A nonmodal titlebar menu for sharing and host-owned peer access.
//!
//! Its content is a [`SharingPanel`], built from the active thread, so that
//! the share preview example draws every state without a thread or peers.

use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, App, Context, FontWeight, SharedString, WeakEntity, Window, div,
    prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenuItem},
    popover::Popover,
    switch::Switch,
};
use gpui_kit_assets::IconName;
use uuid::Uuid;

use crate::{
    Cowork,
    participant::ParticipantId,
    profile::Profile,
    thread::{
        DenialReason, ManageAccess, PeerMode, PermissionDenied, PermissionOperation, SharingStatus,
        Thread,
    },
};

const MODES: [PeerMode; 3] = [PeerMode::ReadOnly, PeerMode::Write, PeerMode::Admin];

pub(crate) const PANEL_WIDTH: gpui::Pixels = px(300.);
/// Every row's content starts and ends this far inside the panel's
/// padding, including the text inside full-width buttons, so that text,
/// avatars, and controls line up down the panel.
const INSET: gpui::Pixels = px(8.);
/// A person's access menu has a fixed width, so the menus line up from row
/// to row and don't move when a mode with a longer name is picked.
const PERSON_MENU_WIDTH: gpui::Pixels = px(92.);
const AVATAR: gpui::Pixels = px(24.);

#[derive(Clone)]
pub(crate) struct Person {
    pub(crate) id: ParticipantId,
    pub(crate) profile: Option<Profile>,
}

impl Person {
    fn name(&self) -> SharedString {
        crate::profile::participant_name(self.id, self.profile.as_ref())
    }

    fn avatar(&self) -> gpui::Div {
        Cowork::render_avatar_for(self.id, self.profile.as_ref(), AVATAR)
    }
}

#[derive(Clone)]
pub(crate) struct PeerRow {
    pub(crate) person: Person,
    pub(crate) override_mode: Option<PeerMode>,
}

pub(crate) struct SharingPanel {
    pub(crate) status: SharingStatus,
    pub(crate) copied: bool,
    pub(crate) default_mode: PeerMode,
    pub(crate) peers: Vec<PeerRow>,
    pub(crate) host: Option<Person>,
    pub(crate) local_mode: PeerMode,
}

type Action = Rc<dyn Fn(&mut Window, &mut App)>;
/// Which access menu opened or closed: the default's (`None`) or a peer's.
type MenuOpen = Rc<dyn Fn(Option<ParticipantId>, bool, &mut Window, &mut App)>;
type SetMode = Rc<dyn Fn(Option<ParticipantId>, Option<PeerMode>, &mut Window, &mut App)>;

#[derive(Clone)]
pub(crate) struct SharingActions {
    pub(crate) toggle: Action,
    pub(crate) copy: Action,
    /// Sets the default mode (no participant) or a participant's override
    /// (`None` returns them to the default).
    pub(crate) set_mode: SetMode,
    /// Told when an access menu opens or closes. The menu opens past the
    /// popover's edge, so the popover mustn't take a click in it for a
    /// click outside.
    pub(crate) menu_open: MenuOpen,
}

#[derive(Clone)]
struct AccessTarget {
    thread: Option<WeakEntity<Thread>>,
    cowork: WeakEntity<Cowork>,
}

impl Cowork {
    /// Draft identity survives materializing a new thread when sharing starts,
    /// but switching drafts still drops controls for the previous thread.
    pub(crate) fn render_thread_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let draft_id = self.readable_draft_id(cx);
        let status = self
            .active_thread(cx)
            .map(|thread| thread.read(cx).sharing.status())
            .unwrap_or(SharingStatus::NotShared);
        let cowork = cx.entity().downgrade();
        Popover::new(SharedString::from(format!(
            "thread-sharing-menu-{}",
            draft_id.unwrap_or(Uuid::nil()),
        )))
        .anchor(Anchor::TopRight)
        .offset(px(6.))
        .w(PANEL_WIDTH)
        .p_2()
        // Dismissed by the content instead, which knows when a click outside
        // is in one of its access menus.
        .overlay_closable(false)
        .trigger(
            Button::new("thread-menu-trigger")
                .debug_selector(|| "thread-menu-trigger".to_owned())
                .icon(Icon::new(IconName::Users).size_4())
                .label("Share")
                .ghost()
                .small()
                .h(px(28.))
                .accessibility_label("Sharing and access")
                .tooltip("Sharing and access")
                .when(
                    matches!(status, SharingStatus::Shared | SharingStatus::Connected),
                    |button| button.text_color(cx.theme().primary),
                ),
        )
        .content(move |_, window, cx| {
            let Some(cowork_entity) = cowork.upgrade() else {
                return div().into_any_element();
            };
            let app = cowork_entity.read(cx);
            if app.readable_draft_id(cx) != draft_id {
                return div().into_any_element();
            }
            let thread = app.active_thread(cx);
            let state = thread.as_ref().map(|thread| thread.read(cx));
            let status = state
                .map(|thread| thread.sharing.status())
                .unwrap_or(SharingStatus::NotShared);
            let person = |id: ParticipantId| Person {
                id,
                profile: state.and_then(|state| state.profiles.get(&id).cloned()),
            };
            let panel = SharingPanel {
                status,
                copied: state
                    .is_some_and(|state| app.copied_endpoint_id == Some(state.instance_id)),
                default_mode: state
                    .map(|state| state.peer_permissions().default_mode())
                    .unwrap_or_default(),
                peers: state
                    .filter(|state| state.is_host())
                    .map(|state| {
                        state
                            .participants()
                            .iter()
                            .copied()
                            .filter(|participant| *participant != state.participant_id())
                            .map(|participant| PeerRow {
                                person: person(participant),
                                override_mode: state.peer_permissions().override_for(participant),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                host: state
                    .filter(|state| !state.is_host())
                    .and_then(|state| state.host_id())
                    .map(person),
                local_mode: state.map(Thread::local_mode).unwrap_or_default(),
            };
            // As `render_sharing_panel` decides: everyone but a collaborator
            // has the default's menu, even before a thread exists.
            let has_default_menu = status != SharingStatus::Connected;

            let popover = cx.entity().downgrade();
            // The access menu that is open, if any: the default's (`None`) or
            // a peer's. A peer leaving with theirs open takes its row, and
            // with it the close notice, so a menu no longer drawn is closed.
            let menu_open = window.use_keyed_state("sharing-access-menu-open", cx, |_, _| {
                None::<Option<ParticipantId>>
            });
            let drawn = |menu: &Option<ParticipantId>| match menu {
                None => has_default_menu,
                Some(id) => panel.peers.iter().any(|peer| peer.person.id == *id),
            };
            if menu_open.read(cx).as_ref().is_some_and(|menu| !drawn(menu)) {
                menu_open.update(cx, |open, _| *open = None);
            }
            let target = AccessTarget {
                thread: thread.as_ref().map(|thread| thread.downgrade()),
                cowork: cowork.clone(),
            };
            let actions = SharingActions {
                toggle: Rc::new({
                    let cowork = cowork.clone();
                    let popover = popover.clone();
                    move |window, cx| {
                        // A collaborator's thread goes away with the connection.
                        if status == SharingStatus::Connected {
                            _ = popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                        }
                        _ = cowork.update(cx, |cowork, cx| {
                            if cowork.readable_draft_id(cx) == draft_id {
                                cowork.toggle_sharing(window, cx);
                            }
                        });
                        _ = popover.update(cx, |_, cx| cx.notify());
                        window.refresh();
                    }
                }),
                copy: Rc::new({
                    let cowork = cowork.clone();
                    let popover = popover.clone();
                    move |window, cx| {
                        _ = cowork.update(cx, |cowork, cx| {
                            if cowork.readable_draft_id(cx) == draft_id {
                                cowork.copy_endpoint_id(cx);
                            }
                        });
                        _ = popover.update(cx, |_, cx| cx.notify());
                        window.refresh();
                    }
                }),
                set_mode: Rc::new(move |participant, mode, window, cx| {
                    set_mode(&target, participant, mode, window, cx)
                }),
                menu_open: Rc::new({
                    let menu_open = menu_open.clone();
                    move |menu, open, _, cx| {
                        menu_open.update(cx, |state, _| {
                            if open {
                                *state = Some(menu);
                            } else if *state == Some(menu) {
                                *state = None;
                            }
                        })
                    }
                }),
            };
            div()
                .w_full()
                .on_mouse_down_out(move |_, window, cx| {
                    if menu_open.read(cx).is_none() {
                        _ = popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                    }
                })
                .child(render_sharing_panel(&panel, &actions, cx))
                .into_any_element()
        })
    }

    pub(crate) fn show_permission_denied(
        &mut self,
        thread_id: Uuid,
        denied: &PermissionDenied,
        cx: &mut Context<Self>,
    ) {
        let action = match denied.operation {
            PermissionOperation::EditDraft => "edit the draft",
            PermissionOperation::ControlGeneration => "submit or stop generation",
            PermissionOperation::ChangeModel => "change the model",
            PermissionOperation::ManageAccess => "manage peer access",
            PermissionOperation::UploadAttachment => "upload this attachment",
            PermissionOperation::ApproveTools => "allow or deny tool calls",
        };
        if let Some(thread) = self.thread_store.read(cx).loaded(thread_id, cx) {
            let draft_id = thread.read(cx).draft().id;
            let message = if denied.reason == DenialReason::StaleDraftGeneration {
                "The host reset your draft. Edits queued before the reset were discarded."
                    .to_owned()
            } else {
                format!("You no longer have permission to {action}.")
            };
            self.attachment_errors
                .retain(|error| error.draft_id != draft_id);
            self.attachment_errors
                .push(crate::composer_attachments::AttachmentError { draft_id, message });
        }
        cx.notify();
    }
}

/// The popover's content: a switch that shows and changes whether the
/// thread is shared, the link to copy once it is, a sentence saying what
/// people who join can do, and each collaborator's access. A collaborator
/// sees whose thread it is, what they may do, and a way to leave.
pub(crate) fn render_sharing_panel(
    panel: &SharingPanel,
    actions: &SharingActions,
    cx: &App,
) -> AnyElement {
    let content = div()
        .id("thread-sharing-menu")
        .debug_selector(|| "thread-sharing-menu".to_owned())
        .w_full()
        .flex()
        .flex_col()
        .gap_2();
    if panel.status == SharingStatus::Connected {
        return content
            .children(collaborator_rows(panel, actions, cx))
            .into_any_element();
    }

    let on = matches!(panel.status, SharingStatus::Sharing | SharingStatus::Shared);
    let caption_text = match panel.status {
        SharingStatus::NotShared | SharingStatus::Connected => "Only you can see this chat.",
        SharingStatus::Sharing => "Creating a link…",
        SharingStatus::Shared => "Anyone with the link can join.",
        SharingStatus::Failed => "Sharing failed. Turn it on to try again.",
    };
    let toggle = actions.toggle.clone();
    let header = row()
        .id("sharing-header")
        .debug_selector(|| "sharing-header".to_owned())
        .min_h(px(36.))
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(title("Share chat"))
                .child(
                    caption(caption_text, cx).when(panel.status == SharingStatus::Failed, |this| {
                        this.text_color(cx.theme().danger)
                    }),
                ),
        )
        .child(
            div()
                .flex_none()
                .debug_selector(|| "toggle-sharing".to_owned())
                .child(
                    Switch::new("toggle-sharing")
                        .checked(on)
                        .disabled(panel.status == SharingStatus::Sharing)
                        .accessibility_label("Share chat")
                        .on_click(move |_, window, cx| toggle(window, cx)),
                ),
        );

    let copy = on.then(|| {
        let action = actions.copy.clone();
        let sharing = panel.status == SharingStatus::Sharing;
        Button::new("copy-endpoint-id")
            .debug_selector(|| "copy-endpoint-id".to_owned())
            .icon(Icon::new(if panel.copied {
                IconName::Check
            } else {
                IconName::Link
            }))
            .label(if sharing {
                "Creating link…"
            } else if panel.copied {
                "Link copied"
            } else {
                "Copy link"
            })
            .primary()
            .w_full()
            .loading(sharing)
            .disabled(sharing)
            .on_click(move |_, window, cx| action(window, cx))
    });

    let summary = mode_menu(
        "default",
        div()
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .child(
                div()
                    .debug_selector(|| "default-mode-icon".to_owned())
                    .child(
                        Icon::new(IconName::Users)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(div().min_w_0().truncate().child(format!(
                "Anyone who joins {}",
                panel.default_mode.ability().to_lowercase()
            )))
            .into_any_element(),
    )
    .w_full()
    .px(INSET)
    .accessibility_label(format!("People who join: {}", panel.default_mode.ability()))
    .dropdown_menu_with_anchor(
        Anchor::TopLeft,
        mode_items("default", Some(panel.default_mode), None, None, actions),
    )
    .on_open_change(menu_open(actions, None));

    let people = (!panel.peers.is_empty()).then(|| {
        div()
            .id("peer-access-list")
            .max_h(px(260.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .children(
                panel
                    .peers
                    .iter()
                    .map(|peer| peer_row(peer, panel.default_mode, actions, cx)),
            )
    });

    content
        .child(header)
        .children(copy)
        .child(summary)
        .when_some(people, |this, people| {
            this.child(separator(cx)).child(people)
        })
        .into_any_element()
}

fn collaborator_rows(panel: &SharingPanel, actions: &SharingActions, cx: &App) -> Vec<AnyElement> {
    let leave = actions.toggle.clone();
    let host_name = panel
        .host
        .as_ref()
        .map(|host| SharedString::from(format!("{}'s chat", host.name())))
        .unwrap_or_else(|| "Shared chat".into());
    let header = row()
        .id("sharing-header")
        .debug_selector(|| "sharing-header".to_owned())
        .min_h(px(36.))
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .children(
                    panel
                        .host
                        .as_ref()
                        .map(|host| host.avatar().debug_selector(|| "host-avatar".to_owned())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(title(host_name))
                        .child(caption("You're connected", cx)),
                ),
        )
        .child(
            Button::new("toggle-sharing")
                .debug_selector(|| "toggle-sharing".to_owned())
                .icon(Icon::new(IconName::LogOut))
                .label("Leave")
                .outline()
                .small()
                .flex_none()
                .on_click(move |_, window, cx| leave(window, cx)),
        );
    let mode = panel.local_mode;
    let access = row()
        .id("your-peer-access")
        .debug_selector(|| "your-peer-access".to_owned())
        .min_h(px(28.))
        .gap_3()
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(Icon::new(mode_icon(mode)).size_4().flex_none())
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(format!("You {}", mode.ability().to_lowercase())),
                ),
        )
        .child(caption("Set by the host", cx).flex_none())
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(mode.description()).build(window, cx)
        });
    vec![
        header.into_any_element(),
        separator(cx),
        access.into_any_element(),
    ]
}

fn peer_row(peer: &PeerRow, default: PeerMode, actions: &SharingActions, cx: &App) -> AnyElement {
    let key = peer.person.id.as_uuid().to_string();
    let effective = peer.override_mode.unwrap_or(default);
    row()
        .id(SharedString::from(format!("{key}-access-row")))
        .debug_selector({
            let key = key.clone();
            move || format!("{key}-access-row")
        })
        .min_h(px(36.))
        // The menu brings its own inset, so its caret lines up with the
        // switch above while its hover background stays inside the panel.
        .pr_0()
        .gap_1()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(peer.person.avatar().debug_selector({
                    let key = key.clone();
                    move || format!("{key}-avatar")
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .debug_selector({
                                    let key = key.clone();
                                    move || format!("{key}-name")
                                })
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .child(peer.person.name()),
                        )
                        .child(caption(
                            if peer.override_mode.is_some() {
                                "Custom access"
                            } else {
                                "Default access"
                            },
                            cx,
                        )),
                ),
        )
        .child(
            mode_menu(
                &key,
                div()
                    .min_w_0()
                    .truncate()
                    .child(effective.ability())
                    .into_any_element(),
            )
            .flex_none()
            .w(PERSON_MENU_WIDTH)
            .px(INSET)
            .accessibility_label(format!("{}: {}", peer.person.name(), effective.ability()))
            .dropdown_menu_with_anchor(
                Anchor::TopRight,
                mode_items(
                    &key,
                    peer.override_mode,
                    Some(peer.person.id),
                    Some(default),
                    actions,
                ),
            )
            .on_open_change(menu_open(actions, Some(peer.person.id))),
        )
        .into_any_element()
}

fn menu_open(
    actions: &SharingActions,
    menu: Option<ParticipantId>,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    let menu_open = actions.menu_open.clone();
    move |open, window, cx| menu_open(menu, *open, window, cx)
}

/// The button opening a mode menu; its label is left-aligned with the caret
/// at the right edge.
fn mode_menu(key: &str, label: AnyElement) -> Button {
    let selector = format!("{key}-mode-menu");
    Button::new(SharedString::from(&selector))
        .debug_selector(move || selector.clone())
        .ghost()
        .small()
        .dropdown_caret(true)
        .child(label)
}

type MenuBuilder = Box<
    dyn Fn(
            gpui_component::menu::PopupMenu,
            &mut Window,
            &mut Context<gpui_component::menu::PopupMenu>,
        ) -> gpui_component::menu::PopupMenu
        + 'static,
>;

/// The modes, each with what it allows, and for a person "Use default".
fn mode_items(
    key: &str,
    selected: Option<PeerMode>,
    participant: Option<ParticipantId>,
    default: Option<PeerMode>,
    actions: &SharingActions,
) -> MenuBuilder {
    let key = key.to_owned();
    let set_mode = actions.set_mode.clone();
    Box::new(move |menu, _, _| {
        let menu = MODES.into_iter().fold(menu, |menu, mode| {
            let set_mode = set_mode.clone();
            let selector = format!("{key}-{}", mode.label());
            menu.item(
                PopupMenuItem::element(move |_, cx| {
                    let selector = selector.clone();
                    div()
                        .debug_selector(move || selector.clone())
                        .flex()
                        .flex_col()
                        .child(div().text_sm().child(mode.ability()))
                        .child(caption(mode.description(), cx))
                })
                .checked(selected == Some(mode))
                .on_click(move |_, window, cx| set_mode(participant, Some(mode), window, cx)),
            )
        });
        match (participant, default) {
            (Some(participant), Some(default)) => {
                let set_mode = set_mode.clone();
                let selector = format!("{key}-inherit");
                menu.separator().item(
                    PopupMenuItem::element(move |_, _| {
                        let selector = selector.clone();
                        div()
                            .debug_selector(move || selector.clone())
                            .text_sm()
                            .child(format!(
                                "Use default ({})",
                                default.ability().to_lowercase()
                            ))
                    })
                    .checked(selected.is_none())
                    .on_click(move |_, window, cx| set_mode(Some(participant), None, window, cx)),
                )
            }
            _ => menu,
        }
    })
}

fn row() -> gpui::Div {
    div().px(INSET).flex().items_center().justify_between()
}

fn title(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .debug_selector(|| "sharing-title".to_owned())
        .min_w_0()
        .truncate()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(text.into())
}

fn caption(text: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .min_w_0()
        .truncate()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn separator(cx: &App) -> AnyElement {
    div()
        .h(px(1.))
        .flex_none()
        .bg(cx.theme().border)
        .into_any_element()
}

fn mode_icon(mode: PeerMode) -> IconName {
    match mode {
        PeerMode::ReadOnly => IconName::Eye,
        PeerMode::Write => IconName::Pencil,
        PeerMode::Admin => IconName::Shield,
    }
}

fn set_mode(
    target: &AccessTarget,
    participant: Option<ParticipantId>,
    mode: Option<PeerMode>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let thread = match &target.thread {
        Some(thread) => thread.upgrade(),
        None => target
            .cowork
            .update(cx, |cowork, cx| cowork.prepare_thread_for_sharing(cx))
            .ok(),
    };
    let Some(thread) = thread else {
        return;
    };
    let changed = thread.update(cx, |thread, cx| {
        let id = thread.instance_id;
        thread
            .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                match participant {
                    Some(participant) => auth.set_override(participant, mode, cx),
                    None => {
                        if let Some(mode) = mode {
                            auth.set_default_mode(mode, cx);
                        }
                    }
                }
            })
            .map(|_| id)
    });
    if let Ok(id) = changed {
        _ = target.cowork.update(cx, |cowork, cx| {
            cowork.reconcile_peer_access(id, cx);
            cx.notify();
        });
        window.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Pixels, Render};

    struct PanelView(SharingPanel);

    impl Render for PanelView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let actions = SharingActions {
                toggle: Rc::new(|_, _| {}),
                copy: Rc::new(|_, _| {}),
                set_mode: Rc::new(|_, _, _, _| {}),
                menu_open: Rc::new(|_, _, _, _| {}),
            };
            div()
                .w(PANEL_WIDTH)
                .p_2()
                .child(render_sharing_panel(&self.0, &actions, cx))
        }
    }

    fn person(name: Option<&str>) -> Person {
        Person {
            id: ParticipantId::new(),
            profile: name.map(|name| Profile {
                name: Some(SharedString::from(name)),
                ..Default::default()
            }),
        }
    }

    const LONG_NAME: &str = "Bartholomew Montgomery-Fitzgerald the Third of Westminster";

    fn bounds(cx: &mut gpui::VisualTestContext, selector: String) -> Bounds<Pixels> {
        cx.debug_bounds(Box::leak(selector.clone().into_boxed_str()))
            .unwrap_or_else(|| panic!("{selector} should be rendered"))
    }

    fn assert_inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>, what: &str) {
        assert!(
            inner.left() >= outer.left()
                && inner.right() <= outer.right()
                && inner.top() >= outer.top()
                && inner.bottom() <= outer.bottom(),
            "{what} {inner:?} overflows {outer:?}"
        );
    }

    fn assert_centered(a: Bounds<Pixels>, b: Bounds<Pixels>, what: &str) {
        assert!(
            (a.center().y - b.center().y).abs() <= px(0.5),
            "{what}: {a:?} isn't vertically centered on {b:?}"
        );
    }

    /// In every state, long names truncate instead of pushing controls out
    /// of the panel or under each other, controls are centered on their
    /// rows, and content shares one left edge and one right edge.
    #[gpui::test]
    fn panel_rows_align_and_fit_in_every_state(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let peers = vec![
            PeerRow {
                person: person(Some(LONG_NAME)),
                override_mode: None,
            },
            PeerRow {
                person: person(None),
                override_mode: Some(PeerMode::Admin),
            },
        ];
        let host_states = [
            (SharingStatus::NotShared, Vec::new()),
            (SharingStatus::Sharing, Vec::new()),
            (SharingStatus::Failed, Vec::new()),
            (SharingStatus::Shared, peers),
        ];
        for (status, peers) in host_states {
            let keys: Vec<String> = peers
                .iter()
                .map(|peer| peer.person.id.as_uuid().to_string())
                .collect();
            let (_, cx) = cx.add_window_view(|_, _| {
                PanelView(SharingPanel {
                    status,
                    copied: false,
                    default_mode: PeerMode::Write,
                    peers,
                    host: None,
                    local_mode: PeerMode::Write,
                })
            });
            cx.run_until_parked();
            let panel = bounds(cx, "thread-sharing-menu".into());
            let header = bounds(cx, "sharing-header".into());
            let switch = bounds(cx, "toggle-sharing".into());
            let title = bounds(cx, "sharing-title".into());
            let summary = bounds(cx, "default-mode-menu".into());
            let summary_icon = bounds(cx, "default-mode-icon".into());
            for (what, element) in [("header", header), ("switch", switch), ("summary", summary)] {
                assert_inside(element, panel, what);
            }
            assert_centered(switch, header, "switch");
            assert!(
                title.right() <= switch.left(),
                "the title runs under the switch"
            );
            assert_eq!(
                summary_icon.left(),
                title.left(),
                "summary and title share an edge"
            );
            assert_eq!(switch.right(), header.right() - INSET);
            if let Some(copy) = cx.debug_bounds("copy-endpoint-id") {
                assert_inside(copy, panel, "copy link");
            }
            for key in keys {
                let row = bounds(cx, format!("{key}-access-row"));
                let avatar = bounds(cx, format!("{key}-avatar"));
                let name = bounds(cx, format!("{key}-name"));
                let menu = bounds(cx, format!("{key}-mode-menu"));
                assert_inside(row, panel, "person row");
                assert_inside(menu, row, "person menu");
                assert_inside(name, row, "person name");
                assert!(
                    name.right() <= menu.left(),
                    "a long name runs under its menu"
                );
                assert_centered(avatar, row, "avatar");
                assert_centered(menu, row, "person menu");
                assert_eq!(
                    avatar.left(),
                    title.left(),
                    "avatars and titles share an edge"
                );
                assert_eq!(
                    menu.right() - INSET,
                    switch.right(),
                    "menu carets and the switch share an edge"
                );
            }
        }

        // A collaborator with a long host name.
        let (_, cx) = cx.add_window_view(|_, _| {
            PanelView(SharingPanel {
                status: SharingStatus::Connected,
                copied: false,
                default_mode: PeerMode::Write,
                peers: Vec::new(),
                host: Some(person(Some(LONG_NAME))),
                local_mode: PeerMode::ReadOnly,
            })
        });
        cx.run_until_parked();
        let panel = cx.debug_bounds("thread-sharing-menu").unwrap();
        let header = cx.debug_bounds("sharing-header").unwrap();
        let leave = cx.debug_bounds("toggle-sharing").unwrap();
        let title = cx.debug_bounds("sharing-title").unwrap();
        let avatar = cx.debug_bounds("host-avatar").unwrap();
        let access = cx.debug_bounds("your-peer-access").unwrap();
        for (what, element) in [
            ("header", header),
            ("leave", leave),
            ("title", title),
            ("access", access),
        ] {
            assert_inside(element, panel, what);
        }
        assert_centered(leave, header, "leave");
        assert_centered(avatar, header, "host avatar");
        assert!(
            title.right() <= leave.left(),
            "the host's name runs under Leave"
        );
        assert_eq!(leave.right(), header.right() - INSET);
        assert_eq!(avatar.left(), header.left() + INSET);
    }
}
