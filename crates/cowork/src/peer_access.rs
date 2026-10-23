//! A nonmodal titlebar menu for sharing and host-owned peer access.

use gpui::{Anchor, Context, SharedString, WeakEntity, Window, div, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    popover::Popover,
};
use gpui_kit_assets::IconName;
use uuid::Uuid;

use crate::{
    Cowork,
    participant::ParticipantId,
    thread::{
        DenialReason, ManageAccess, PeerMode, PermissionDenied, PermissionOperation, SharingStatus,
        Thread,
    },
};

const MODES: [PeerMode; 3] = [PeerMode::ReadOnly, PeerMode::Write, PeerMode::Admin];

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
        .w(px(300.))
        .p_2()
        .trigger(
            Button::new("thread-menu-trigger")
                .debug_selector(|| "thread-menu-trigger".to_owned())
                .icon(Icon::new(IconName::Users).size_4())
                .ghost()
                .small()
                .size(px(28.))
                .accessibility_label("Sharing and access")
                .tooltip("Sharing and access")
                .when(
                    matches!(status, SharingStatus::Shared | SharingStatus::Connected),
                    |button| button.text_color(cx.theme().primary),
                ),
        )
        .content(move |_, _, cx| {
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
            let popover = cx.entity().downgrade();
            let mut content = div()
                .id("thread-sharing-menu")
                .debug_selector(|| "thread-sharing-menu".to_owned())
                .w_full()
                .flex()
                .flex_col()
                .gap_2();

            if status != SharingStatus::Connected {
                let copied =
                    state.is_some_and(|state| app.copied_endpoint_id == Some(state.instance_id));
                content = content.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_sm().child("Sharing"))
                        .child(
                            Button::new("copy-endpoint-id")
                                .debug_selector(|| "copy-endpoint-id".to_owned())
                                .icon(
                                    Icon::new(if copied {
                                        IconName::Check
                                    } else {
                                        IconName::Link
                                    })
                                    .size_4(),
                                )
                                .ghost()
                                .small()
                                .size(px(26.))
                                .disabled(status != SharingStatus::Shared)
                                .accessibility_label(if copied {
                                    "Link copied"
                                } else {
                                    "Copy link"
                                })
                                .tooltip(if copied { "Copied" } else { "Copy link" })
                                .on_click({
                                    let cowork = cowork.clone();
                                    let popover = popover.clone();
                                    move |_, window, cx| {
                                        _ = cowork.update(cx, |cowork, cx| {
                                            if cowork.readable_draft_id(cx) == draft_id {
                                                cowork.copy_endpoint_id(cx);
                                            }
                                        });
                                        _ = popover.update(cx, |_, cx| cx.notify());
                                        window.refresh();
                                    }
                                }),
                        ),
                );
                let target = AccessTarget {
                    thread: thread.as_ref().map(|thread| thread.downgrade()),
                    cowork: cowork.clone(),
                };
                let default = state
                    .map(|state| state.peer_permissions().default_mode())
                    .unwrap_or_default();
                content = content.child(access_row(
                    "default",
                    "Default".into(),
                    None,
                    default,
                    None,
                    target.clone(),
                    cx,
                ));
                let rows = state
                    .into_iter()
                    .flat_map(|state| {
                        state
                            .participants()
                            .iter()
                            .copied()
                            .filter(|participant| *participant != state.participant_id())
                            .map(|participant| {
                                let mode = state.peer_permissions().override_for(participant);
                                access_row(
                                    &participant.as_uuid().to_string(),
                                    crate::profile::participant_name(
                                        participant,
                                        state.profiles.get(&participant),
                                    ),
                                    Some(participant),
                                    mode.unwrap_or(default),
                                    mode,
                                    target.clone(),
                                    cx,
                                )
                            })
                    })
                    .collect::<Vec<_>>();
                if !rows.is_empty() {
                    content = content.child(
                        div()
                            .id("peer-access-list")
                            .max_h(px(260.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(rows),
                    );
                }
                content = content.child(div().h(px(1.)).bg(cx.theme().border));
            } else if let Some(state) = state {
                let mode = state.local_mode();
                content = content
                    .child(
                        div()
                            .id("your-peer-access")
                            .debug_selector(|| "your-peer-access".to_owned())
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_1()
                            .child(div().text_sm().child("Your access"))
                            .child(
                                Button::new("your-peer-mode")
                                    .icon(Icon::new(mode_icon(mode)).size_4())
                                    .ghost()
                                    .small()
                                    .size(px(28.))
                                    .accessibility_label(mode.label())
                                    .tooltip(mode_tooltip(mode)),
                            ),
                    )
                    .child(div().h(px(1.)).bg(cx.theme().border));
            }

            let (label, icon) = match status {
                SharingStatus::NotShared => ("Share thread", IconName::Share2),
                SharingStatus::Sharing => ("Sharing…", IconName::Share2),
                SharingStatus::Shared => ("Stop sharing", IconName::Unlink),
                SharingStatus::Connected => ("Disconnect", IconName::LogOut),
                SharingStatus::Failed => ("Retry sharing", IconName::Share2),
            };
            content
                .child(
                    Button::new("toggle-sharing")
                        .debug_selector(|| "toggle-sharing".to_owned())
                        .icon(Icon::new(icon).size_4())
                        .label(label)
                        .ghost()
                        .small()
                        .w_full()
                        .disabled(status == SharingStatus::Sharing)
                        .loading(status == SharingStatus::Sharing)
                        .on_click({
                            let cowork = cowork.clone();
                            move |_, window, cx| {
                                if matches!(
                                    status,
                                    SharingStatus::Shared | SharingStatus::Connected
                                ) {
                                    _ = popover
                                        .update(cx, |popover, cx| popover.dismiss(window, cx));
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
                )
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
        if let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) {
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

fn mode_icon(mode: PeerMode) -> IconName {
    match mode {
        PeerMode::ReadOnly => IconName::Eye,
        PeerMode::Write => IconName::Pencil,
        PeerMode::Admin => IconName::Shield,
    }
}

fn mode_tooltip(mode: PeerMode) -> &'static str {
    match mode {
        PeerMode::ReadOnly => "Read only · View the thread",
        PeerMode::Write => "Write · Edit drafts and attachments",
        PeerMode::Admin => "Admin · Also submit, stop, and change models",
    }
}

fn access_row(
    id: &str,
    name: SharedString,
    participant: Option<ParticipantId>,
    effective: PeerMode,
    override_mode: Option<PeerMode>,
    target: AccessTarget,
    cx: &gpui::App,
) -> gpui::AnyElement {
    let track = div()
        .id(SharedString::from(format!("{id}-mode-track")))
        .debug_selector({
            let id = id.to_owned();
            move || format!("{id}-mode-track")
        })
        .flex_none()
        .flex()
        .gap(px(2.))
        .p(px(2.))
        .rounded_lg()
        .bg(cx.theme().secondary)
        .children(MODES.into_iter().map(|mode| {
            mode_button(
                format!("{id}-{}", mode.label()),
                mode,
                effective == mode,
                participant,
                target.clone(),
            )
        }));
    div()
        .id(SharedString::from(format!("{id}-access-row")))
        .h(px(34.))
        .flex()
        .items_center()
        .gap_2()
        .child(div().flex_1().min_w_0().text_sm().truncate().child(name))
        .child(track)
        .child(if let Some(participant) = participant {
            let selector = format!("{id}-inherit");
            Button::new(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .icon(Icon::new(IconName::RotateCcw).size_3())
                .ghost()
                .small()
                .size(px(24.))
                .disabled(override_mode.is_none())
                .accessibility_label("Use default access")
                .tooltip(if override_mode.is_none() {
                    "Using default access"
                } else {
                    "Reset to default access"
                })
                .on_click(move |_, window, cx| {
                    set_mode(&target, Some(participant), None, window, cx)
                })
                .into_any_element()
        } else {
            div().w(px(24.)).flex_none().into_any_element()
        })
        .into_any_element()
}

fn mode_button(
    id: String,
    mode: PeerMode,
    selected: bool,
    participant: Option<ParticipantId>,
    target: AccessTarget,
) -> Button {
    Button::new(SharedString::from(id.clone()))
        .debug_selector(move || id.clone())
        .icon(Icon::new(mode_icon(mode)).size_4())
        .ghost()
        .small()
        .size(px(28.))
        .selected(selected)
        .when(selected, |button| button.primary())
        .accessibility_label(mode.label())
        .tooltip(mode_tooltip(mode))
        .on_click(move |_, window, cx| set_mode(&target, participant, Some(mode), window, cx))
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
