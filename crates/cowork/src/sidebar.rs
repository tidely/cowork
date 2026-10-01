//! The sidebar listing threads, and its bottom bar.

use std::rc::Rc;

use gpui::{
    App, Context, FontWeight, IntoElement, SharedString, Window, div, prelude::*, px, rems,
};
use gpui_component::{
    ActiveTheme, Collapsible, Icon, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    sidebar::{Sidebar, SidebarCollapsible, SidebarItem, SidebarMenu, SidebarMenuItem},
};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use crate::{
    Cowork, MainStage,
    thread::{ThreadSharing, ThreadSummary},
    thread_draft::ThreadDraft,
    top_bar::TOP_BAR_HEIGHT,
};

pub(crate) const SIDEBAR_WIDTH: gpui::Pixels = px(275.);

#[derive(Clone)]
struct CoworkSidebarSection {
    pub(crate) label: Option<SharedString>,
    menu: SidebarMenu,
    pub(crate) collapsed: bool,
    pub(crate) open: bool,
    on_label_click: Option<Rc<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>>,
}

impl CoworkSidebarSection {
    pub(crate) fn new(label: Option<impl Into<SharedString>>, menu: SidebarMenu) -> Self {
        Self {
            label: label.map(Into::into),
            menu,
            collapsed: false,
            open: true,
            on_label_click: None,
        }
    }

    fn label_toggle(
        mut self,
        open: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.open = open;
        self.on_label_click = Some(Rc::new(on_click));
        self
    }
}

impl Collapsible for CoworkSidebarSection {
    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    fn is_collapsed(&self) -> bool {
        self.collapsed
    }
}

impl SidebarItem for CoworkSidebarSection {
    fn render(
        self,
        id: impl Into<gpui::ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        let open = self.open;
        let on_label_click = self.on_label_click;

        div()
            .flex()
            .flex_col()
            .when_some(self.label, |this, label| {
                this.child(
                    div()
                        .id(format!("{id}-label"))
                        .h(px(38.))
                        .flex()
                        .items_end()
                        .justify_between()
                        .px_2()
                        .pb_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground.opacity(0.7))
                        .child(label)
                        .when_some(on_label_click, |this, on_click| {
                            this.cursor_pointer()
                                .hover(|this| this.text_color(cx.theme().muted_foreground))
                                .on_click(move |event, window, cx| on_click(event, window, cx))
                                .child(
                                    Icon::new(if open {
                                        AssetIconName::ChevronDown
                                    } else {
                                        AssetIconName::ChevronRight
                                    })
                                    .size_4()
                                    .text_color(cx.theme().muted_foreground),
                                )
                        }),
                )
            })
            .when(open, |this| {
                this.child(
                    <SidebarMenu as SidebarItem>::render(
                        self.menu.collapsed(self.collapsed),
                        format!("{id}-menu"),
                        window,
                        cx,
                    )
                    .into_any_element(),
                )
            })
    }
}

impl Cowork {
    pub(crate) fn open_thread(
        &mut self,
        thread_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.thread_store.read(cx).thread(thread_id, cx).is_none() {
            return;
        }

        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let can_write = thread.read(cx).ownership.can_write();
        self.active_thread_id = Some(thread_id);
        self.selection_message_id = None;
        self.main_stage = MainStage::Thread;
        self.follow_generation = true;
        self.timeline_scroll_handle.scroll_to_bottom();
        if can_write {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    fn sidebar_thread_item(
        &self,
        thread_id: Uuid,
        thread: &ThreadSummary,
        cx: &mut Context<Self>,
    ) -> SidebarMenuItem {
        SidebarMenuItem::new(thread.title.clone())
            .min_h(px(30.))
            .active(
                self.main_stage == MainStage::Thread && self.active_thread_id == Some(thread_id),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_thread(thread_id, window, cx);
            }))
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut hosting_threads = Vec::new();
        let mut collaborating_threads = Vec::new();
        let mut recent_threads = Vec::new();
        for thread in &self.thread_store.read(cx).threads {
            let thread = thread.read(cx);
            let entry = (thread.instance_id, thread.summary.clone());
            match thread.sharing {
                ThreadSharing::Sharing | ThreadSharing::Shared { .. } => {
                    hosting_threads.push(entry)
                }
                ThreadSharing::Connected { .. } => collaborating_threads.push(entry),
                ThreadSharing::NotShared | ThreadSharing::Failed => recent_threads.push(entry),
            }
        }

        let actions = CoworkSidebarSection::new(
            None::<SharedString>,
            SidebarMenu::new()
                .child(
                    SidebarMenuItem::new("New chat")
                        .min_h(px(34.))
                        .icon(
                            Icon::new(AssetIconName::SquarePen)
                                .size_4()
                                .text_color(cx.theme().secondary_foreground),
                        )
                        .active(
                            self.main_stage == MainStage::Thread && self.active_thread_id.is_none(),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_thread_draft = ThreadDraft::new(this.local_participant_id);
                            this.active_thread_id = None;
                            this.selection_message_id = None;
                            this.main_stage = MainStage::Thread;
                            this.focus_composer(window, cx);
                            cx.notify();
                        })),
                )
                .child(
                    SidebarMenuItem::new("Join shared thread")
                        .min_h(px(34.))
                        .icon(
                            Icon::new(AssetIconName::UsersRound)
                                .size_4()
                                .text_color(cx.theme().secondary_foreground),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_join_dialog(window, cx);
                        })),
                ),
        );

        let hosting = CoworkSidebarSection::new(
            Some("Shared by me"),
            SidebarMenu::new().children(
                hosting_threads
                    .iter()
                    .map(|(thread_id, thread)| self.sidebar_thread_item(*thread_id, thread, cx)),
            ),
        );

        let collaborating = CoworkSidebarSection::new(
            Some("Collaborating"),
            SidebarMenu::new().children(
                collaborating_threads
                    .iter()
                    .map(|(thread_id, thread)| self.sidebar_thread_item(*thread_id, thread, cx)),
            ),
        );

        let recents = CoworkSidebarSection::new(
            Some("Recents"),
            SidebarMenu::new().children(
                recent_threads
                    .iter()
                    .map(|(thread_id, thread)| self.sidebar_thread_item(*thread_id, thread, cx)),
            ),
        )
        .label_toggle(
            self.recents_open,
            cx.listener(|this, _, _, cx| {
                this.recents_open = !this.recents_open;
                cx.notify();
            }),
        );

        let sidebar = Sidebar::new("cowork-sidebar")
            .w(SIDEBAR_WIDTH)
            .bg(cx.theme().sidebar)
            .border_r_0()
            .collapsible(SidebarCollapsible::Offcanvas)
            .collapsed(!self.sidebar_open)
            .header(
                // `Sidebar` puts its header in a row, so fill it for the
                // search button to reach the right edge.
                div()
                    .flex_1()
                    .h(px(42.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Cowork"),
                    )
                    .child(
                        Button::new("search-chats")
                            .icon(
                                Icon::new(AssetIconName::Search)
                                    .size_4()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .ghost()
                            .small()
                            .size(px(28.))
                            .debug_selector(|| "search-chats".to_owned())
                            .accessibility_label("Search chats")
                            .tooltip("Search chats")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_search_palette(window, cx);
                            })),
                    ),
            )
            .footer(self.render_sidebar_bottom_bar(cx))
            .child(actions);
        let sidebar = if hosting_threads.is_empty() {
            sidebar
        } else {
            sidebar.child(hosting)
        };
        let sidebar = if collaborating_threads.is_empty() {
            sidebar
        } else {
            sidebar.child(collaborating)
        };

        sidebar.child(recents)
    }

    /// Mirrors the main stage's bottom bar, but with its divider always shown.
    fn render_sidebar_bottom_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // `Sidebar` pads its footer slot by `px_3` and `pb_3`; bleeding over
        // that padding lets the bar span the sidebar's full width and line up
        // with the main stage's bottom bar.
        const FOOTER_INSET: gpui::Rems = rems(-0.75);

        div()
            .id("sidebar-bottom-bar")
            .debug_selector(|| "sidebar-bottom-bar".to_owned())
            .relative()
            .flex_1()
            .mx(FOOTER_INSET)
            .mb(FOOTER_INSET)
            .h(TOP_BAR_HEIGHT)
            .flex()
            .items_center()
            .px_1()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(1.))
                    .bg(cx.theme().border),
            )
            .child(
                Button::new("identity")
                    .ghost()
                    .debug_selector(|| "identity-button".to_owned())
                    .flex_1()
                    .px_1p5()
                    .selected(self.main_stage == MainStage::Profile)
                    .accessibility_label("Open profile")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_profile(window, cx);
                    }))
                    // `Button` centers its content, so fill it with a single
                    // left-aligned row.
                    .child(
                        div()
                            .debug_selector(|| "identity-button-content".to_owned())
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.render_profile_avatar(px(22.)))
                            .child(div().min_w_0().text_ellipsis().child(self.profile_name())),
                    ),
            )
    }
}
