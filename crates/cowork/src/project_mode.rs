//! Whether the thread's project is open for writing or only reading, and
//! the menu in the bottom bar choosing it.
//!
//! Kept in [`Thread::project_mode`] and, before the thread exists,
//! [`Cowork::new_thread_project_mode`]. The thread's sandbox mounts the
//! folders read-only unless it is [`ProjectMode::Write`].

use gpui::{Anchor, App, Context, IntoElement, SharedString, WeakEntity, div, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Side, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName;
use serde::{Deserialize, Serialize};

use crate::{Cowork, project_folders::ProjectChange, thread::Thread};

/// Shown while hovering the Write option.
pub(crate) const WRITE_WARNING: &str = "While there is still sandboxing, a malicious agent may be \
     able to run commands outside of it.";

/// How the project's folders are open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProjectMode {
    Write,
    /// The default, so nothing is changed until someone asks for it.
    #[default]
    Read,
}

impl ProjectMode {
    /// In the order the menu lists them.
    pub(crate) const ALL: [Self; 2] = [Self::Write, Self::Read];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Write => "Write",
            Self::Read => "Read",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Write => IconName::Pencil,
            Self::Read => IconName::Eye,
        }
    }
}

impl Cowork {
    /// The active thread's mode, or that of the thread about to be created,
    /// and whether the local user may change it: only the host can, as for
    /// the folders.
    pub(crate) fn active_project_mode(&self, cx: &App) -> (ProjectMode, bool) {
        match self.active_thread(cx) {
            Some(thread) => {
                let thread = thread.read(cx);
                (thread.project_mode(), thread.is_host())
            }
            None => (self.new_thread_project_mode, true),
        }
    }

    pub(crate) fn render_project_mode(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (mode, editable) = self.active_project_mode(cx);
        let trigger = Button::new("project-mode")
            .debug_selector(|| "project-mode".to_owned())
            .label(mode.label())
            .dropdown_caret(true)
            .ghost()
            .small()
            .flex_none()
            .accessibility_label(format!("Project mode: {}", mode.label()));
        if !editable {
            return trigger
                .disabled(true)
                .tooltip("Only the host can change the project mode")
                .into_any_element();
        }
        // Applies to the thread the menu was opened for, even if another one
        // is active by the time an option is picked.
        let target = self.active_thread(cx).map(|thread| thread.downgrade());
        let cowork = cx.entity().downgrade();
        trigger
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                ProjectMode::ALL
                    .into_iter()
                    .fold(menu.check_side(Side::Right), |menu, option| {
                        let cowork = cowork.clone();
                        let target = target.clone();
                        let item = match option {
                            ProjectMode::Write => {
                                PopupMenuItem::element(|_, cx| Self::render_write_option(cx))
                            }
                            ProjectMode::Read => {
                                PopupMenuItem::new(SharedString::from(option.label()))
                            }
                        };
                        menu.item(
                            item.icon(Icon::new(option.icon()))
                                .checked(option == mode)
                                .on_click(move |_, window, cx| {
                                    _ = cowork.update(cx, |cowork, cx| {
                                        cowork.request_project_change(
                                            target.clone(),
                                            ProjectChange::Mode(option),
                                            window,
                                            cx,
                                        );
                                    });
                                }),
                        )
                    })
            })
            .into_any_element()
    }

    /// The Write option, warning while hovered what writing allows.
    fn render_write_option(cx: &App) -> impl IntoElement + use<> {
        let warning = cx.theme().warning;
        h_flex()
            .id("project-mode-write")
            .debug_selector(|| "project-mode-write".to_owned())
            .flex_1()
            .gap_2()
            .justify_between()
            .child(ProjectMode::Write.label())
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_3p5()
                    .text_color(warning),
            )
            .tooltip(move |window, cx| {
                Tooltip::element(move |_, _| {
                    h_flex()
                        .debug_selector(|| "project-mode-write-warning".to_owned())
                        .items_start()
                        .gap_2()
                        // A fixed width: sized by its content, the text would
                        // lay out as one line and run past the tooltip.
                        .w(px(280.))
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .size_4()
                                .flex_none()
                                .mt_0p5()
                                .text_color(warning),
                        )
                        .child(
                            div()
                                .debug_selector(|| "project-mode-write-warning-text".to_owned())
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .child(WRITE_WARNING),
                        )
                })
                .build(window, cx)
            })
    }

    /// Sets a thread's mode, or with `None` that of the thread about to be
    /// created.
    pub(crate) fn set_project_mode(
        &mut self,
        thread: Option<WeakEntity<Thread>>,
        mode: ProjectMode,
        cx: &mut Context<Self>,
    ) {
        match thread.and_then(|thread| thread.upgrade()) {
            Some(thread) => {
                thread.update(cx, |thread, cx| thread.set_project_mode(mode, cx));
                self.sync_sandbox_project(&thread, cx);
            }
            None => self.new_thread_project_mode = mode,
        }
        cx.notify();
    }
}
