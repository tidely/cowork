//! The archive: the user's own threads put away from the sidebar, where
//! they can be restored or deleted for good.

use gpui::{
    App, Context, Entity, FontWeight, IntoElement, SharedString, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::{DialogDescription, DialogFooter, DialogHeader, DialogTitle},
    h_flex, v_flex,
};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use crate::{Cowork, MainStage, thread::Thread};

impl Cowork {
    pub(crate) fn open_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.main_stage = MainStage::Archive;
        // The composer is hidden, so it must not keep taking keystrokes.
        window.blur(cx);
        cx.notify();
    }

    /// Moves one of the user's own threads to the archive, ending everything
    /// it was doing: its agent run, its sharing, and its sandbox. Joined
    /// threads belong to their host, so they can't be archived.
    pub(crate) fn archive_thread(
        &mut self,
        thread_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        if !thread.read(cx).is_host() {
            return;
        }
        let host = thread.read(cx).participant_id();
        self.cancel_generation(thread_id, host, None, cx);
        let endpoint = thread.update(cx, |thread, cx| thread.stop_hosting(cx));
        if let Some(endpoint) = endpoint {
            self.tokio_handle.spawn(async move {
                endpoint.close().await;
            });
        }

        let draft_id = thread.read(cx).draft().id;
        self.thread_store.update(cx, |store, cx| {
            store
                .threads
                .retain(|thread| thread.read(cx).instance_id != thread_id);
            store.archived.push_front(thread.clone());
        });
        // A run still being stopped settles this again once it has ended.
        self.settle_retired_thread(&thread, cx);
        self.published_presence.remove(&thread_id);
        self.segment_text_views
            .retain(|(message, _), _| message.thread_id != thread_id);
        self.shown_segments
            .retain(|message, _| message.thread_id != thread_id);
        if self.copied_endpoint_id == Some(thread_id) {
            self.copied_endpoint_id = None;
        }
        if self
            .typing_in
            .as_ref()
            .is_some_and(|(typing_in, _, _)| *typing_in == draft_id)
        {
            self.typing_in = None;
        }
        if self.active_thread_id == Some(thread_id) {
            self.active_thread_id = None;
            self.selection_message_id = None;
            if self.main_stage == MainStage::Thread {
                self.focus_composer(window, cx);
            }
        }
        cx.notify();
    }

    /// Puts an archived thread back at the top of the sidebar's recents,
    /// as it was but no longer shared.
    pub(crate) fn restore_thread(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        self.thread_store.update(cx, |store, cx| {
            let Some(index) = store
                .archived
                .iter()
                .position(|thread| thread.read(cx).instance_id == thread_id)
            else {
                return;
            };
            if let Some(thread) = store.archived.remove(index) {
                store.threads.push_front(thread);
            }
        });
        cx.notify();
    }

    /// Restores every archived thread, keeping the archive's order at the top
    /// of the recents.
    pub(crate) fn restore_all_threads(&mut self, cx: &mut Context<Self>) {
        self.thread_store.update(cx, |store, _| {
            while let Some(thread) = store.archived.pop_back() {
                store.threads.push_front(thread);
            }
        });
        cx.notify();
    }

    /// Deletes an archived thread for good. The profile's statistics keep
    /// counting it.
    pub(crate) fn delete_archived_thread(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_store.read(cx).archived_thread(thread_id, cx) else {
            return;
        };
        self.thread_store.update(cx, |store, cx| {
            store
                .archived
                .retain(|thread| thread.read(cx).instance_id != thread_id);
        });
        self.deleted_chats += 1;
        self.forget_scheduled_thread(thread_id);
        self.settle_retired_thread(&thread, cx);
        cx.notify();
    }

    /// Deletes an archived thread, first warning when scheduled tasks
    /// continue it: their next runs start a new thread instead, unless the
    /// tasks are deleted too.
    pub(crate) fn request_delete_archived_thread(
        &mut self,
        thread_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let continuing: Vec<(Uuid, String)> = self
            .scheduler
            .tasks_continuing(thread_id)
            .into_iter()
            .map(|task| (task.id, task.settings().title.clone()))
            .collect();
        if continuing.is_empty() {
            self.delete_archived_thread(thread_id, cx);
            return;
        }
        let names = continuing
            .iter()
            .map(|(_, title)| format!("“{title}”"))
            .collect::<Vec<_>>()
            .join(", ");
        let (description, delete_tasks) = if continuing.len() == 1 {
            (
                format!(
                    "The scheduled task {names} continues this thread. Its next run will start a new thread."
                ),
                "Delete Thread and Schedule",
            )
        } else {
            (
                format!(
                    "The scheduled tasks {names} continue this thread. Their next runs will start a new thread."
                ),
                "Delete Thread and Schedules",
            )
        };
        let task_ids: Vec<Uuid> = continuing.into_iter().map(|(task_id, _)| task_id).collect();
        let cowork = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let description = description.clone();
            let delete_thread = cowork.clone();
            let delete_both = cowork.clone();
            let task_ids = task_ids.clone();
            dialog
                .w(px(460.))
                .bg(cx.theme().popover)
                .content(move |content, _, _| {
                    let delete_thread = delete_thread.clone();
                    let delete_both = delete_both.clone();
                    let task_ids = task_ids.clone();
                    content
                        .child(
                            DialogHeader::new()
                                .child(
                                    DialogTitle::new().child("Delete a thread used by a schedule?"),
                                )
                                .child(DialogDescription::new().child(description.clone())),
                        )
                        .child(
                            DialogFooter::new()
                                .child(
                                    Button::new("cancel-delete-thread")
                                        .outline()
                                        .label("Cancel")
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("delete-thread-and-schedule")
                                        .outline()
                                        .label(delete_tasks)
                                        .debug_selector(|| "delete-thread-and-schedule".to_owned())
                                        .on_click(move |_, window, cx| {
                                            _ = delete_both.update(cx, |cowork, cx| {
                                                for &task_id in &task_ids {
                                                    cowork.delete_scheduled_task(task_id, cx);
                                                }
                                                cowork.delete_archived_thread(thread_id, cx);
                                            });
                                            window.close_dialog(cx);
                                        }),
                                )
                                .child(
                                    Button::new("delete-thread-only")
                                        .danger()
                                        .label("Delete Thread")
                                        .debug_selector(|| "delete-thread-only".to_owned())
                                        .on_click(move |_, window, cx| {
                                            _ = delete_thread.update(cx, |cowork, cx| {
                                                cowork.delete_archived_thread(thread_id, cx);
                                            });
                                            window.close_dialog(cx);
                                        }),
                                ),
                        )
                })
        });
    }

    /// Asks before deleting every thread the archive shows now.
    fn confirm_delete_all_archived(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread_ids: Vec<Uuid> = self
            .thread_store
            .read(cx)
            .archived
            .iter()
            .map(|thread| thread.read(cx).instance_id)
            .collect();
        if thread_ids.is_empty() {
            return;
        }
        let mut description = match thread_ids.len() {
            1 => "The archived thread will be deleted. This can't be undone.".to_owned(),
            count => format!("All {count} archived threads will be deleted. This can't be undone."),
        };
        let continued = thread_ids
            .iter()
            .map(|thread_id| self.scheduler.tasks_continuing(*thread_id).len())
            .sum::<usize>();
        match continued {
            0 => {}
            1 => description.push_str(
                " A scheduled task continues one of them; its next run will start a new thread.",
            ),
            count => description.push_str(&format!(
                " {count} scheduled tasks continue them; their next runs will start new threads."
            )),
        }
        let cowork = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let cowork = cowork.clone();
            let thread_ids = thread_ids.clone();
            alert
                .title("Delete all archived threads?")
                .description(description.clone())
                .ok_text("Delete All")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    _ = cowork.update(cx, |cowork, cx| {
                        for &thread_id in &thread_ids {
                            cowork.delete_archived_thread(thread_id, cx);
                        }
                    });
                    true
                })
        });
    }

    /// Stops the sandbox of a thread that left the sidebar, and keeps a
    /// deleted one's longest stretch of generating for the profile's
    /// statistics. Also called when a run of such a thread ends, which adds
    /// that run's time and stops a sandbox the run may have started while it
    /// was being stopped.
    pub(crate) fn settle_retired_thread(&mut self, thread: &Entity<Thread>, cx: &App) {
        let thread = thread.read(cx);
        let thread_id = thread.instance_id;
        if self
            .thread_store
            .read(cx)
            .archived_thread(thread_id, cx)
            .is_none()
        {
            self.longest_deleted_chat = self.longest_deleted_chat.max(thread.generation_time());
        }
        let sandboxes = self.sandboxes.clone();
        self.tokio_handle.spawn(async move {
            sandboxes.remove(&thread_id.to_string()).await;
        });
    }

    pub(crate) fn render_archive_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let archived: Vec<(Uuid, SharedString)> = self
            .thread_store
            .read(cx)
            .archived
            .iter()
            .map(|thread| {
                let thread = thread.read(cx);
                (thread.instance_id, thread.summary.title.clone().into())
            })
            .collect();
        let empty = archived.is_empty();
        let count = match archived.len() {
            0 => "Nothing archived".to_owned(),
            1 => "1 thread".to_owned(),
            count => format!("{count} threads"),
        };

        let header = h_flex()
            .w_full()
            .items_end()
            .justify_between()
            .gap_3()
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().secondary_foreground)
                            .child("Archived threads"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(count),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        Button::new("restore-all-threads")
                            .outline()
                            .small()
                            .icon(Icon::new(AssetIconName::ArchiveRestore))
                            .label("Restore all")
                            .disabled(empty)
                            .debug_selector(|| "restore-all-threads".to_owned())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.restore_all_threads(cx);
                            })),
                    )
                    .child(
                        Button::new("delete-all-threads")
                            .danger()
                            .small()
                            .icon(Icon::new(AssetIconName::Trash))
                            .label("Delete all")
                            .disabled(empty)
                            .debug_selector(|| "delete-all-threads".to_owned())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_delete_all_archived(window, cx);
                            })),
                    ),
            );

        let list =
            if empty {
                v_flex()
                    .w_full()
                    .py_12()
                    .items_center()
                    .gap_2()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(AssetIconName::Archive).size_8())
                    .child(div().text_sm().child("No archived threads"))
                    .child(
                        div()
                            .text_xs()
                            .child("Threads you archive from the sidebar show up here."),
                    )
                    .into_any_element()
            } else {
                v_flex()
                    .w_full()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().border)
                    .overflow_hidden()
                    .children(archived.into_iter().enumerate().map(
                        |(index, (thread_id, title))| {
                            self.render_archived_thread(index, thread_id, title, cx)
                        },
                    ))
                    .into_any_element()
            };

        div()
            .id("archive-page")
            .debug_selector(|| "archive-page".to_owned())
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
                    .id("archive-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .mx_auto()
                            .w_full()
                            .max_w(px(640.))
                            .pt(px(48.))
                            .pb_6()
                            .px_6()
                            .gap_4()
                            .child(header)
                            .child(list),
                    ),
            )
    }

    fn render_archived_thread(
        &self,
        index: usize,
        thread_id: Uuid,
        title: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        h_flex()
            .id(thread_id)
            .debug_selector(move || format!("archived-thread-{thread_id}"))
            .w_full()
            .h(px(44.))
            .pl_4()
            .pr_2()
            .gap_2()
            .when(index > 0, |this| {
                this.border_t_1().border_color(cx.theme().border)
            })
            .hover(|this| this.bg(cx.theme().muted.opacity(0.5)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(title.clone()),
            )
            .child(
                Button::new("restore-thread")
                    .ghost()
                    .small()
                    .icon(Icon::new(AssetIconName::ArchiveRestore))
                    .label("Restore")
                    .debug_selector(move || format!("restore-thread-{thread_id}"))
                    .accessibility_label(format!("Restore {title}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.restore_thread(thread_id, cx);
                    })),
            )
            .child(
                Button::new("delete-thread")
                    .ghost()
                    .small()
                    .icon(
                        Icon::new(AssetIconName::Trash)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .debug_selector(move || format!("delete-thread-{thread_id}"))
                    .accessibility_label(format!("Delete {title} permanently"))
                    .tooltip("Delete permanently")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_delete_archived_thread(thread_id, window, cx);
                    })),
            )
    }
}
