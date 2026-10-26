//! The sidebar listing threads, and its bottom bar. While settings are open
//! the sidebar lists their tabs instead; see `settings.rs`.
//!
//! The user's own chats that aren't shared are listed under the project
//! they are filed in (see `projects.rs`), or else in Recents. Shared chats
//! are in "Shared by me" whatever their project, and go back to it once
//! sharing ends.

use std::{cmp::Reverse, rc::Rc};

use gpui::{
    AnyElement, App, ClickEvent, Context, FontWeight, Hsla, IntoElement, Role, SharedString,
    Window, div, linear_color_stop, linear_gradient, prelude::*, px, rems,
};
use gpui_component::{
    ActiveTheme, Collapsible, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    sidebar::{Sidebar, SidebarCollapsible, SidebarItem},
    v_flex,
};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use crate::{
    Cowork, MainStage,
    project_folders::add_folders,
    projects::ProjectId,
    settings::SettingsPage,
    thread::{ThreadSharing, ThreadSummary},
    thread_draft::ThreadDraft,
    top_bar::TOP_BAR_HEIGHT,
};

pub(crate) const SIDEBAR_WIDTH: gpui::Pixels = px(300.);

/// How far a thread's title fades out where it runs out of room, and before
/// the buttons shown over its end on hover.
const TITLE_FADE_WIDTH: gpui::Pixels = px(24.);

/// How far a project's chats are indented: to line up with its name, after
/// its folder icon.
const PROJECT_CHAT_INDENT: gpui::Pixels = px(32.);

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
    Menu(Vec<SidebarEntry>),
    Threads(Vec<ThreadRow>),
    Projects(Vec<ProjectRow>),
}

/// A thread as the sidebar lists it: its id, summary, and whether it is the
/// user's own, so it can be archived.
pub(crate) type ListedThread = (Uuid, ThreadSummary, bool);

/// The sidebar's threads, by the section listing them. Archived threads are
/// in none.
pub(crate) struct SidebarGroups {
    pub(crate) hosting: Vec<ListedThread>,
    pub(crate) collaborating: Vec<ListedThread>,
    /// Every project, newest first, each with its chats that aren't shared,
    /// most recently active first.
    pub(crate) projects: Vec<(ProjectId, Vec<ListedThread>)>,
    /// The user's chats that aren't shared and in no known project.
    pub(crate) recents: Vec<ListedThread>,
}

/// A sidebar action or destination. Only destinations carry selection;
/// actions such as composing or joining a chat are never active.
#[derive(Clone)]
pub(crate) struct SidebarEntry {
    label: SharedString,
    icon: AssetIconName,
    active: bool,
    on_click: ClickHandler,
}

impl SidebarEntry {
    pub(crate) fn new(
        label: impl Into<SharedString>,
        icon: AssetIconName,
        active: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            icon,
            active,
            on_click: Rc::new(on_click),
        }
    }

    fn render(self, cx: &App) -> impl IntoElement {
        let on_click = self.on_click;
        sidebar_row(self.label.clone(), self.active, cx)
            .aria_label(self.label.clone())
            .h(px(32.))
            .gap_2()
            .child(
                Icon::new(self.icon)
                    .size_4()
                    .text_color(cx.theme().secondary_foreground),
            )
            .child(self.label)
            .on_click(move |event, window, cx| on_click(event, window, cx))
    }
}

/// Keep navigation and thread rows consistent, including their visible and
/// accessible selection state.
fn sidebar_row(
    id: impl Into<gpui::ElementId>,
    active: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    h_flex()
        .id(id)
        .role(Role::TreeItem)
        .aria_selected(active)
        .w_full()
        .h(px(28.))
        .flex_shrink_0()
        .px_2()
        .rounded(theme.radius)
        .text_sm()
        .hover(|this| {
            this.bg(theme.sidebar_accent.opacity(0.8))
                .text_color(theme.sidebar_accent_foreground)
        })
        .when(active, |this| {
            this.bg(theme.sidebar_accent)
                .text_color(theme.sidebar_accent_foreground)
                .font_weight(FontWeight::SEMIBOLD)
        })
}

/// A row whose label fades out where it runs out of room, with `button`
/// shown over its end only while the row itself is hovered. Its debug
/// selectors are `sidebar-{kind}-{key}`, and `-title-` and `-buttons-`
/// ones for its parts.
fn fading_row(
    kind: &'static str,
    key: String,
    active: bool,
    leading: Option<AnyElement>,
    label: SharedString,
    button: Option<Button>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let hover_bg = theme.sidebar_accent.opacity(0.8);
    // What the row's background looks like while hovered, for the button
    // to sit on and the label to fade into.
    let buttons_bg = theme.sidebar.blend(hover_bg);
    let rest_bg = if active {
        theme.sidebar.blend(theme.sidebar_accent)
    } else {
        theme.sidebar
    };
    let group = SharedString::from(format!("sidebar-{kind}-{key}"));
    let (row_selector, title_selector, buttons_selector) = (
        group.to_string(),
        format!("sidebar-{kind}-title-{key}"),
        format!("sidebar-{kind}-buttons-{key}"),
    );

    sidebar_row(group.clone(), active, cx)
        .group(group.clone())
        .debug_selector(move || row_selector)
        .aria_label(label.clone())
        .relative()
        .overflow_hidden()
        .when_some(leading, |this, leading| this.gap_2().child(leading))
        .child(
            div()
                .debug_selector(move || title_selector)
                .relative()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .child(label)
                // Over a label too long for the row, fading out its end
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
        .when_some(button, |this, button| {
            this.child(
                h_flex()
                    .debug_selector(move || buttons_selector)
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .invisible()
                    .group_hover(group, |style| style.visible())
                    .child(div().h_full().w(TITLE_FADE_WIDTH).bg(fade_into(buttons_bg)))
                    // Overflow clipping is rectangular; the opaque overlay
                    // needs the row's corners too, or it squares off the hover.
                    .child(
                        h_flex()
                            .h_full()
                            .pr_1()
                            .rounded_r(theme.radius)
                            .bg(buttons_bg)
                            .child(button),
                    ),
            )
        })
}

/// A small icon button for over a row's end, whose click doesn't also
/// click the row.
fn row_button(
    id: &'static str,
    selector: String,
    icon: AssetIconName,
    label: &'static str,
    on_click: ClickHandler,
    cx: &App,
) -> Button {
    Button::new(id)
        .icon(
            Icon::new(icon)
                .size_4()
                .text_color(cx.theme().muted_foreground),
        )
        .ghost()
        .xsmall()
        .debug_selector(move || selector)
        .accessibility_label(label)
        .tooltip(label)
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx);
        })
}

/// A thread's row also shows its archive button only while hovered.
#[derive(Clone)]
struct ThreadRow {
    thread_id: Uuid,
    title: SharedString,
    active: bool,
    /// Whether it is listed under a project.
    indented: bool,
    on_open: ClickHandler,
    /// `None` for joined threads, which belong to their host.
    on_archive: Option<ClickHandler>,
}

impl ThreadRow {
    fn render(self, cx: &App) -> impl IntoElement {
        let thread_id = self.thread_id;
        let on_open = self.on_open;
        let archive = self.on_archive.map(|on_archive| {
            row_button(
                "archive-thread",
                format!("archive-thread-{thread_id}"),
                AssetIconName::Archive,
                "Archive thread",
                on_archive,
                cx,
            )
        });
        fading_row(
            "thread",
            thread_id.to_string(),
            self.active,
            None,
            self.title,
            archive,
            cx,
        )
        .when(self.indented, |this| this.pl(PROJECT_CHAT_INDENT))
        .on_click(move |event, window, cx| on_open(event, window, cx))
    }
}

/// A project's row, toggling whether its chats show below it, with a
/// button over its end while hovered composing a chat in it.
#[derive(Clone)]
struct ProjectRow {
    id: ProjectId,
    name: SharedString,
    open: bool,
    on_toggle: ClickHandler,
    on_compose: ClickHandler,
    /// The chats shown while open.
    chats: Vec<ThreadRow>,
    /// While some of its chats are not shown yet.
    on_show_more: Option<ClickHandler>,
}

impl ProjectRow {
    fn render(self, cx: &App) -> impl IntoElement {
        let id = self.id;
        let theme = cx.theme();
        let on_toggle = self.on_toggle;
        let compose = row_button(
            "compose-in-project",
            format!("compose-in-project-{id}"),
            AssetIconName::SquarePen,
            "New chat in project",
            self.on_compose,
            cx,
        );
        let folder = Icon::new(if self.open {
            AssetIconName::FolderOpen
        } else {
            AssetIconName::Folder
        })
        .size_4()
        .flex_none()
        .text_color(theme.secondary_foreground)
        .into_any_element();
        let header = fading_row(
            "project",
            id.to_string(),
            false,
            Some(folder),
            self.name,
            Some(compose),
            cx,
        )
        .aria_expanded(self.open)
        .on_click(move |event, window, cx| on_toggle(event, window, cx));

        let empty = self.open && self.chats.is_empty();
        v_flex()
            .child(header)
            .when(self.open, |this| {
                this.children(self.chats.into_iter().map(|chat| chat.render(cx)))
            })
            .when(empty, |this| {
                this.child(
                    div()
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .pl(PROJECT_CHAT_INDENT)
                        .text_sm()
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child("No chats yet"),
                )
            })
            .when_some(
                self.on_show_more.filter(|_| self.open),
                |this, on_show_more| {
                    this.child(
                        sidebar_row(SharedString::from(format!("project-more-{id}")), false, cx)
                            .debug_selector(move || format!("sidebar-project-show-more-{id}"))
                            .aria_label("Show more chats")
                            .pl(PROJECT_CHAT_INDENT)
                            .text_color(theme.muted_foreground)
                            .child("Show more")
                            .on_click(move |event, window, cx| on_show_more(event, window, cx)),
                    )
                },
            )
    }
}

#[derive(Clone)]
pub(crate) struct CoworkSidebarSection {
    pub(crate) label: Option<SharedString>,
    items: SectionItems,
    pub(crate) collapsed: bool,
    pub(crate) open: bool,
    on_label_click: Option<ClickHandler>,
    label_action: Option<SidebarEntry>,
}

impl CoworkSidebarSection {
    pub(crate) fn new(label: Option<impl Into<SharedString>>, menu: Vec<SidebarEntry>) -> Self {
        Self::with_items(label, SectionItems::Menu(menu))
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
            label_action: None,
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

    fn label_action(mut self, action: SidebarEntry) -> Self {
        self.label_action = Some(action);
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
        _window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        let open = self.open;
        let on_label_click = self.on_label_click;
        let group = SharedString::from(format!("{id}-section"));
        let header_group = SharedString::from(format!("{id}-header"));
        let action_selector = self
            .label
            .as_ref()
            .map(|label| format!("sidebar-section-action-{label}"))
            .unwrap_or_default();

        div()
            .group(group.clone())
            .flex()
            .flex_col()
            .when_some(self.label, |this, label| {
                this.pt_3().child(
                    div()
                        .id(format!("{id}-label"))
                        .group(header_group.clone())
                        .debug_selector(|| format!("sidebar-section-{label}"))
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground.opacity(0.7))
                        .child(label)
                        .when_some(on_label_click, |this, on_click| {
                            this.cursor_pointer()
                                .hover(|this| this.text_color(cx.theme().muted_foreground))
                                .on_click(move |event, window, cx| on_click(event, window, cx))
                                .child(
                                    div()
                                        .flex()
                                        .when(open, |this| {
                                            this.invisible()
                                                .group_hover(group, |style| style.visible())
                                        })
                                        .child(
                                            Icon::new(if open {
                                                AssetIconName::ChevronDown
                                            } else {
                                                AssetIconName::ChevronRight
                                            })
                                            .size_4()
                                            .text_color(cx.theme().muted_foreground),
                                        ),
                                )
                        })
                        .when_some(self.label_action, |this, action| {
                            this.child(
                                div()
                                    .ml_auto()
                                    .invisible()
                                    .group_hover(header_group, |style| style.visible())
                                    .child(
                                        Button::new(format!("{id}-action"))
                                            .icon(Icon::new(action.icon).size_4())
                                            .ghost()
                                            .xsmall()
                                            .debug_selector(move || action_selector)
                                            .accessibility_label(action.label.clone())
                                            .tooltip(action.label)
                                            .on_click(move |event, window, cx| {
                                                cx.stop_propagation();
                                                (action.on_click)(event, window, cx);
                                            }),
                                    ),
                            )
                        }),
                )
            })
            .when(open, |this| match self.items {
                SectionItems::Menu(menu) => this.child(
                    v_flex()
                        .gap(px(2.))
                        .children(menu.into_iter().map(|entry| entry.render(cx))),
                ),
                SectionItems::Threads(threads) => this
                    .child(v_flex().children(threads.into_iter().map(|thread| thread.render(cx)))),
                SectionItems::Projects(projects) => this.child(
                    v_flex().children(projects.into_iter().map(|project| project.render(cx))),
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

    /// Shows the composer of a new chat in no project, with an empty draft.
    /// Folders copied in from a project are dropped with it; others chosen
    /// for the new chat stay, as does its mode.
    pub(crate) fn start_new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_thread_project.take().is_some() {
            self.new_thread_project_folders.clear();
        }
        self.show_new_chat(window, cx);
    }

    /// Shows the composer of a new chat in `project`, with an empty draft
    /// and the project's folders, copied: changing them changes only the
    /// chat. The chat is filed under the project once it is created, by its
    /// first submission.
    pub(crate) fn compose_in_project(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self.projects.record(project) else {
            return;
        };
        self.new_thread_project_folders.clear();
        add_folders(
            &mut self.new_thread_project_folders,
            record.folders.iter().cloned(),
        );
        self.new_thread_project = Some(project);
        self.show_new_chat(window, cx);
    }

    fn show_new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_thread_draft = ThreadDraft::new(self.local_participant_id);
        self.active_thread_id = None;
        self.selection_message_id = None;
        self.main_stage = MainStage::Thread;
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Sorts the store's threads into the sidebar's sections, keeping the
    /// store's order except within projects.
    pub(crate) fn sidebar_groups(&self, cx: &App) -> SidebarGroups {
        let mut groups = SidebarGroups {
            hosting: Vec::new(),
            collaborating: Vec::new(),
            projects: Vec::new(),
            recents: Vec::new(),
        };
        let mut projects = self
            .projects
            .records()
            .iter()
            .map(|record| (record.id, Vec::new()))
            .collect::<Vec<_>>();
        for thread in &self.thread_store.read(cx).threads {
            let thread = thread.read(cx);
            let entry = (thread.instance_id, thread.summary.clone(), thread.is_host());
            match thread.sharing {
                ThreadSharing::Sharing | ThreadSharing::Shared { .. } => groups.hosting.push(entry),
                ThreadSharing::Connected { .. } => groups.collaborating.push(entry),
                ThreadSharing::NotShared | ThreadSharing::Failed => {
                    let project = thread
                        .project()
                        .and_then(|project| projects.iter_mut().find(|(id, _)| *id == project));
                    match project {
                        Some((_, chats)) => chats.push((thread.last_activity(), entry)),
                        None => groups.recents.push(entry),
                    }
                }
            }
        }
        groups.projects = projects
            .into_iter()
            .map(|(id, mut chats)| {
                // Stable, so chats never run keep the store's order, last.
                chats.sort_by_key(|(last_activity, _)| Reverse(*last_activity));
                (id, chats.into_iter().map(|(_, entry)| entry).collect())
            })
            .collect();
        groups
    }

    fn sidebar_thread_rows(
        &self,
        threads: &[ListedThread],
        indented: bool,
        cx: &mut Context<Self>,
    ) -> Vec<ThreadRow> {
        threads
            .iter()
            .map(|(thread_id, thread, own)| {
                self.sidebar_thread_row(*thread_id, thread, *own, indented, cx)
            })
            .collect()
    }

    fn sidebar_thread_row(
        &self,
        thread_id: Uuid,
        thread: &ThreadSummary,
        archivable: bool,
        indented: bool,
        cx: &mut Context<Self>,
    ) -> ThreadRow {
        ThreadRow {
            thread_id,
            title: thread.title.clone().into(),
            active: self.main_stage == MainStage::Thread
                && self.active_thread_id == Some(thread_id),
            indented,
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
        let settings = matches!(self.main_stage, MainStage::Settings(_));
        // Separate ids keep each view's scroll position to itself.
        let sidebar = Sidebar::new(if settings {
            "settings-sidebar"
        } else {
            "cowork-sidebar"
        })
        .w(SIDEBAR_WIDTH)
        .bg(cx.theme().sidebar)
        .border_r_0()
        .collapsible(SidebarCollapsible::Offcanvas)
        .collapsed(!self.sidebar_open)
        .footer(self.render_sidebar_bottom_bar(cx));
        if settings {
            sidebar
                .header(self.render_settings_sidebar_header(cx))
                .child(self.settings_sidebar_tabs(cx))
        } else {
            self.app_sidebar(sidebar, cx)
        }
    }

    /// The sidebar's usual content: actions and threads.
    fn app_sidebar(
        &self,
        sidebar: Sidebar<CoworkSidebarSection>,
        cx: &mut Context<Self>,
    ) -> Sidebar<CoworkSidebarSection> {
        let groups = self.sidebar_groups(cx);

        let new_chat = SidebarEntry::new(
            "New chat",
            AssetIconName::SquarePen,
            false,
            cx.listener(|this, _, window, cx| this.start_new_chat(window, cx)),
        );
        let actions = CoworkSidebarSection::new(
            None::<SharedString>,
            vec![
                new_chat.clone(),
                SidebarEntry::new(
                    "Join shared thread",
                    AssetIconName::UsersRound,
                    false,
                    cx.listener(|this, _, window, cx| {
                        this.open_join_dialog(window, cx);
                    }),
                ),
                SidebarEntry::new(
                    "Scheduled",
                    AssetIconName::CalendarClock,
                    self.main_stage == MainStage::Schedules,
                    cx.listener(|this, _, window, cx| {
                        this.open_schedules(window, cx);
                    }),
                ),
            ],
        );

        let hosting = CoworkSidebarSection::threads(
            "Shared by me",
            self.sidebar_thread_rows(&groups.hosting, false, cx),
        );
        let collaborating = CoworkSidebarSection::threads(
            "Collaborating",
            self.sidebar_thread_rows(&groups.collaborating, false, cx),
        );
        let mut project_rows = Vec::new();
        for (id, chats) in &groups.projects {
            let id = *id;
            let Some(record) = self.projects.record(id) else {
                continue;
            };
            let shown = self.projects.shown_chats(id);
            let shown_chats = &chats[..shown.unwrap_or(0).min(chats.len())];
            project_rows.push(ProjectRow {
                id,
                name: record.name.clone().into(),
                open: shown.is_some(),
                on_toggle: Rc::new(cx.listener(move |this, _, _, cx| {
                    this.projects.toggle(id);
                    cx.notify();
                })),
                on_compose: Rc::new(cx.listener(move |this, _, window, cx| {
                    this.compose_in_project(id, window, cx);
                })),
                chats: self.sidebar_thread_rows(shown_chats, true, cx),
                on_show_more: (shown_chats.len() < chats.len()).then(|| -> ClickHandler {
                    Rc::new(cx.listener(move |this, _, _, cx| {
                        this.projects.show_more(id);
                        cx.notify();
                    }))
                }),
            });
        }
        let projects = CoworkSidebarSection::with_items(
            Some("Projects"),
            SectionItems::Projects(project_rows),
        )
        .label_toggle(
            self.projects.section_open(),
            cx.listener(|this, _, _, cx| {
                this.projects.toggle_section();
                cx.notify();
            }),
        )
        .label_action(SidebarEntry::new(
            "New project",
            AssetIconName::Plus,
            false,
            cx.listener(|this, _, window, cx| {
                this.open_create_project_dialog(window, cx);
            }),
        ));
        let recents = CoworkSidebarSection::threads(
            "Recents",
            self.sidebar_thread_rows(&groups.recents, false, cx),
        )
        .label_toggle(
            self.recents_open,
            cx.listener(|this, _, _, cx| {
                this.recents_open = !this.recents_open;
                cx.notify();
            }),
        )
        .label_action(new_chat);

        let sidebar = sidebar
            .header(
                // `Sidebar` puts its header in a row, so fill it for the
                // buttons to reach the right edge.
                div()
                    .flex_1()
                    .h(px(36.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .child(
                        div()
                            .text_size(px(16.))
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
            .child(actions);
        let sidebar = if groups.hosting.is_empty() {
            sidebar
        } else {
            sidebar.child(hosting)
        };
        let sidebar = if groups.collaborating.is_empty() {
            sidebar
        } else {
            sidebar.child(collaborating)
        };

        sidebar.child(projects).child(recents)
    }

    /// Mirrors the main stage's bottom bar, but with its divider always shown.
    /// The profile button is a row like the sidebar's own, its avatar and
    /// name lining up with their icons and labels.
    fn render_sidebar_bottom_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // `Sidebar` pads its footer slot by `px_3` and `pb_3`; bleeding over
        // that padding lets the bar span the sidebar's full width and line up
        // with the main stage's bottom bar.
        const FOOTER_INSET: gpui::Rems = rems(-0.75);
        // A little larger than the rows' 16px icons, with the gap trimmed so
        // the name starts where their labels do.
        const AVATAR_SIZE: gpui::Pixels = px(18.);

        div()
            .id("sidebar-bottom-bar")
            .relative()
            .flex_1()
            .mx(FOOTER_INSET)
            .mb(FOOTER_INSET)
            .h(TOP_BAR_HEIGHT)
            .flex()
            .items_center()
            // Keep the hover evenly inset from all four edges of the bar.
            .p_1()
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
                sidebar_row("identity", false, cx)
                    .role(Role::Button)
                    .debug_selector(|| "identity-button".to_owned())
                    .aria_label("Open profile")
                    .h_full()
                    // Preserve the sidebar rows' icon and label alignment
                    // while letting the hover extend into their outer gutter.
                    .px_4()
                    .gap_1p5()
                    .child(self.render_profile_avatar(AVATAR_SIZE))
                    .child(div().min_w_0().truncate().child(self.profile_name()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(SettingsPage::Profile, window, cx);
                    })),
            )
    }
}
