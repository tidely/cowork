//! The sidebar listing threads, and its bottom bar.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, Context, FontWeight, Hsla, IntoElement, Role, SharedString, Window, div,
    linear_color_stop, linear_gradient, prelude::*, px, rems,
};
use gpui_component::{
    ActiveTheme, Collapsible, Icon, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    sidebar::{Sidebar, SidebarCollapsible, SidebarItem, SidebarMenu, SidebarMenuItem},
    v_flex,
};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use crate::{
    Cowork, MainStage,
    thread::{ThreadSharing, ThreadSummary},
    thread_draft::ThreadDraft,
    top_bar::TOP_BAR_HEIGHT,
};

pub(crate) const SIDEBAR_WIDTH: gpui::Pixels = px(300.);

/// How far a thread's title fades out where it runs out of room, and before
/// the buttons shown over its end on hover.
const TITLE_FADE_WIDTH: gpui::Pixels = px(24.);

/// A fade from transparent into `color`, left to right.
fn fade_into(color: Hsla) -> gpui::Background {
    linear_gradient(
        90.,
        linear_color_stop(color.opacity(0.), 0.),
        linear_color_stop(color, 1.),
    )
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(Clone)]
enum SectionItems {
    Menu(Box<SidebarMenu>),
    Threads(Vec<ThreadRow>),
}

/// A thread's row in the sidebar. `SidebarMenuItem` can't show buttons only
/// while it is hovered, so threads get their own row, styled like one.
#[derive(Clone)]
struct ThreadRow {
    thread_id: Uuid,
    title: SharedString,
    active: bool,
    on_open: ClickHandler,
    /// `None` for joined threads, which belong to their host.
    on_archive: Option<ClickHandler>,
}

impl ThreadRow {
    fn render(self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let hover_bg = theme.sidebar_accent.opacity(0.8);
        // What the row's background looks like while hovered, for the
        // buttons to sit on and the title to fade into.
        let buttons_bg: Hsla = if self.active {
            theme.tokens.sidebar_accent.into()
        } else {
            theme.sidebar.blend(hover_bg)
        };
        let rest_bg: Hsla = if self.active {
            buttons_bg
        } else {
            theme.sidebar
        };
        let group = SharedString::from(format!("sidebar-thread-{}", self.thread_id));
        let thread_id = self.thread_id;
        let on_open = self.on_open;

        h_flex()
            .id(self.thread_id)
            .group(group.clone())
            .debug_selector(|| format!("sidebar-thread-{thread_id}"))
            .role(Role::TreeItem)
            .aria_label(self.title.clone())
            .aria_selected(self.active)
            .relative()
            .w_full()
            .h(px(30.))
            .flex_shrink_0()
            .p_2()
            .rounded(theme.radius)
            .overflow_hidden()
            .text_sm()
            .when(!self.active, |this| {
                this.hover(|this| {
                    this.bg(hover_bg)
                        .text_color(theme.sidebar_accent_foreground)
                })
            })
            .when(self.active, |this| {
                this.font_weight(FontWeight::MEDIUM)
                    .bg(theme.tokens.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .on_click(move |event, window, cx| on_open(event, window, cx))
            .child(
                div()
                    .debug_selector(move || format!("sidebar-thread-title-{thread_id}"))
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(self.title)
                    // Over a title too long for the row, fading out its end
                    // instead of marking the cut; short ones end before it.
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .w(TITLE_FADE_WIDTH)
                            .bg(fade_into(rest_bg))
                            .group_hover(group.clone(), |style| style.bg(fade_into(buttons_bg))),
                    ),
            )
            .when_some(self.on_archive, |this, on_archive| {
                this.child(
                    h_flex()
                        .debug_selector(|| format!("sidebar-thread-buttons-{thread_id}"))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .invisible()
                        .group_hover(group, |style| style.visible())
                        .child(div().h_full().w(TITLE_FADE_WIDTH).bg(fade_into(buttons_bg)))
                        .child(
                            h_flex().h_full().pr_1().bg(buttons_bg).child(
                                Button::new("archive-thread")
                                    .icon(
                                        Icon::new(AssetIconName::Archive)
                                            .size_4()
                                            .text_color(theme.muted_foreground),
                                    )
                                    .ghost()
                                    .xsmall()
                                    .debug_selector(move || format!("archive-thread-{thread_id}"))
                                    .accessibility_label("Archive thread")
                                    .tooltip("Archive thread")
                                    .on_click(move |event, window, cx| {
                                        // Archiving must not also open the thread.
                                        cx.stop_propagation();
                                        on_archive(event, window, cx);
                                    }),
                            ),
                        ),
                )
            })
    }
}

#[derive(Clone)]
struct CoworkSidebarSection {
    pub(crate) label: Option<SharedString>,
    items: SectionItems,
    pub(crate) collapsed: bool,
    pub(crate) open: bool,
    on_label_click: Option<ClickHandler>,
}

impl CoworkSidebarSection {
    pub(crate) fn new(label: Option<impl Into<SharedString>>, menu: SidebarMenu) -> Self {
        Self::with_items(label, SectionItems::Menu(Box::new(menu)))
    }

    fn threads(label: impl Into<SharedString>, threads: Vec<ThreadRow>) -> Self {
        Self::with_items(Some(label), SectionItems::Threads(threads))
    }

    fn with_items(label: Option<impl Into<SharedString>>, items: SectionItems) -> Self {
        Self {
            label: label.map(Into::into),
            items,
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
            .when(open, |this| match self.items {
                SectionItems::Menu(menu) => this.child(
                    <SidebarMenu as SidebarItem>::render(
                        (*menu).collapsed(self.collapsed),
                        format!("{id}-menu"),
                        window,
                        cx,
                    )
                    .into_any_element(),
                ),
                // Spaced like `SidebarMenu` spaces its items.
                SectionItems::Threads(threads) => this.child(
                    v_flex()
                        .gap_2()
                        .children(threads.into_iter().map(|thread| thread.render(cx))),
                ),
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
        let can_write = thread.read(cx).can_edit_draft();
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

    fn sidebar_thread_row(
        &self,
        thread_id: Uuid,
        thread: &ThreadSummary,
        archivable: bool,
        cx: &mut Context<Self>,
    ) -> ThreadRow {
        ThreadRow {
            thread_id,
            title: thread.title.clone().into(),
            active: self.main_stage == MainStage::Thread
                && self.active_thread_id == Some(thread_id),
            on_open: Rc::new(cx.listener(move |this, _, window, cx| {
                this.open_thread(thread_id, window, cx);
            })),
            on_archive: archivable.then(|| -> ClickHandler {
                Rc::new(cx.listener(move |this, _, window, cx| {
                    this.archive_thread(thread_id, window, cx);
                }))
            }),
        }
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut hosting_threads = Vec::new();
        let mut collaborating_threads = Vec::new();
        let mut recent_threads = Vec::new();
        for thread in &self.thread_store.read(cx).threads {
            let thread = thread.read(cx);
            let entry = (thread.instance_id, thread.summary.clone(), thread.is_host());
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
                )
                .child(
                    SidebarMenuItem::new("Scheduled")
                        .min_h(px(34.))
                        .icon(
                            Icon::new(AssetIconName::CalendarClock)
                                .size_4()
                                .text_color(cx.theme().secondary_foreground),
                        )
                        .active(self.main_stage == MainStage::Schedules)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_schedules(window, cx);
                        })),
                ),
        );

        let mut rows = |threads: &[(Uuid, ThreadSummary, bool)]| {
            threads
                .iter()
                .map(|(thread_id, thread, own)| {
                    self.sidebar_thread_row(*thread_id, thread, *own, cx)
                })
                .collect::<Vec<_>>()
        };
        let hosting = CoworkSidebarSection::threads("Shared by me", rows(&hosting_threads));
        let collaborating =
            CoworkSidebarSection::threads("Collaborating", rows(&collaborating_threads));
        let recents = CoworkSidebarSection::threads("Recents", rows(&recent_threads)).label_toggle(
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
                // buttons to reach the right edge.
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
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("archived-threads")
                                    .icon(
                                        Icon::new(AssetIconName::Archive)
                                            .size_4()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .ghost()
                                    .small()
                                    .size(px(28.))
                                    .selected(self.main_stage == MainStage::Archive)
                                    .debug_selector(|| "archived-threads".to_owned())
                                    .accessibility_label("Archived threads")
                                    .tooltip("Archived threads")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_archive(window, cx);
                                    })),
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
