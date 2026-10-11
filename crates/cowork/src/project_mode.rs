//! What the agent's commands may do with the thread's project (read it,
//! change it, or change it and reach the network), and the menu in the
//! bottom bar choosing it.
//!
//! Kept in [`Thread::project_mode`] and, before the thread exists,
//! [`Cowork::new_thread_project_mode`]. It is the access a command runs
//! with unless it asks for other access of its own; commands with read
//! access run without asking, and others wait for the host; see
//! `tool_approval.rs`.

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

use crate::{Cowork, thread::Thread};

/// Shown while hovering the Write option.
pub(crate) const WRITE_WARNING: &str = "Commands you allow can change these folders on your \
     computer at once, with your own tools. While they are sandboxed, a malicious agent may be \
     able to run commands outside of the sandbox.";

/// Shown while hovering the Write + Network option.
pub(crate) const NETWORK_WARNING: &str = "Commands you allow can change these folders on your \
     computer at once, with your own tools, and reach the internet: they can download code and \
     send out anything they can read. While they are sandboxed, a malicious agent may be able \
     to run commands outside of the sandbox.";

/// What the agent's commands may do with the project. Its variant indices
/// are its wire encoding: add new modes at the end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProjectMode {
    Write,
    /// The default, so nothing is changed until someone asks for it.
    #[default]
    Read,
    /// Write, and reach the network; labelled "Write + Network".
    Network,
}

impl ProjectMode {
    /// In the order the menu lists them, most access first.
    pub(crate) const ALL: [Self; 3] = [Self::Network, Self::Write, Self::Read];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Network => "Write + Network",
            Self::Write => "Write",
            Self::Read => "Read",
        }
    }

    /// The mode in ids and debug selectors, which the label's spaces and
    /// `+` would make awkward.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Write => "write",
            Self::Read => "read",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Network => IconName::Globe,
            Self::Write => IconName::Pencil,
            Self::Read => IconName::Eye,
        }
    }

    /// What commands may do in this mode, unless they ask for other access.
    pub(crate) fn access(self) -> sandbox::Access {
        match self {
            Self::Read => sandbox::Access::Read,
            Self::Write => sandbox::Access::Write,
            Self::Network => sandbox::Access::Network,
        }
    }

    /// The mode giving `access`.
    pub(crate) fn with_access(access: sandbox::Access) -> Self {
        match access {
            sandbox::Access::Read => Self::Read,
            sandbox::Access::Write => Self::Write,
            sandbox::Access::Network => Self::Network,
        }
    }

    /// What choosing this mode allows, for the modes that change things.
    pub(crate) fn warning(self) -> Option<&'static str> {
        match self {
            Self::Network => Some(NETWORK_WARNING),
            Self::Write => Some(WRITE_WARNING),
            Self::Read => None,
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
                        let item = match option.warning() {
                            Some(warning) => PopupMenuItem::element(move |_, cx| {
                                Self::render_warned_option(option, warning, cx)
                            }),
                            None => PopupMenuItem::new(SharedString::from(option.label())),
                        };
                        menu.item(
                            item.icon(Icon::new(option.icon()))
                                .checked(option == mode)
                                .on_click(move |_, _, cx| {
                                    _ = cowork.update(cx, |cowork, cx| {
                                        cowork.set_project_mode(target.clone(), option, cx);
                                    });
                                }),
                        )
                    })
            })
            .into_any_element()
    }

    /// An option that changes things, warning while hovered what it allows.
    fn render_warned_option(
        option: ProjectMode,
        text: &'static str,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let warning = cx.theme().warning;
        let selector = format!("project-mode-{}", option.key());
        let warning_selector = format!("{selector}-warning");
        h_flex()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .flex_1()
            .gap_2()
            .justify_between()
            .child(option.label())
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_3p5()
                    .text_color(warning),
            )
            .tooltip(move |window, cx| {
                let warning_selector = warning_selector.clone();
                Tooltip::element(move |_, _| {
                    let text_selector = format!("{warning_selector}-text");
                    let warning_selector = warning_selector.clone();
                    h_flex()
                        .debug_selector(move || warning_selector.clone())
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
                                .debug_selector(move || text_selector.clone())
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .child(text),
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
