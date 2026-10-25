//! The Create project dialog, opened from the sidebar's Projects header: a
//! name, and the folders the project's chats start with. Creating a project
//! only adds it to the sidebar; see `projects.rs`.

use std::{path::PathBuf, time::SystemTime};

use gpui::{
    Context, Entity, Focusable as _, FontWeight, IntoElement, PathPromptOptions, Render,
    SharedString, Subscription, WeakEntity, Window, div, prelude::*, px,
};
use gpui_base::input::{InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogFooter, DialogHeader, DialogTitle},
    h_flex,
    input::Input,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit_assets::IconName;

use crate::{
    Cowork,
    projects::{add_folder_paths, project_name},
};

/// The dialog's form.
pub(crate) struct CreateProjectDialog {
    cowork: WeakEntity<Cowork>,
    name: Entity<InputState>,
    /// The chosen folders, in the order chosen, without duplicates.
    folders: Vec<PathBuf>,
    _subscription: Subscription,
}

impl CreateProjectDialog {
    fn new(cowork: WeakEntity<Cowork>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name your project"));
        let subscription = cx.subscribe_in(
            &name,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.create(window, cx),
                _ => {}
            },
        );
        Self {
            cowork,
            name,
            folders: Vec::new(),
            _subscription: subscription,
        }
    }

    fn pick_folders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add to project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths = match selected.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    eprintln!("could not choose project folders: {error:#}");
                    return;
                }
            };
            _ = this.update(cx, |this, cx| this.add_folders(paths, cx));
        })
        .detach();
    }

    /// Adds folders chosen in the picker, skipping ones already chosen.
    pub(crate) fn add_folders(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if add_folder_paths(&mut self.folders, paths) {
            cx.notify();
        }
    }

    fn remove_folder(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.folders.len() {
            self.folders.remove(index);
            cx.notify();
        }
    }

    /// Creates the project and closes the dialog, unless the name is blank.
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value();
        let folders = self.folders.clone();
        let created = self
            .cowork
            .update(cx, |cowork, cx| cowork.create_project(&name, folders, cx))
            .unwrap_or(false);
        if created {
            window.close_dialog(cx);
        }
    }

    fn label(text: &'static str, cx: &Context<Self>) -> impl IntoElement {
        div()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(cx.theme().foreground)
            .child(text)
    }

    /// A large area inviting the first folders.
    fn render_empty_folders(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .id("create-project-choose-folders")
            .debug_selector(|| "create-project-choose-folders".to_owned())
            .role(gpui::Role::Button)
            .aria_label("Add folders")
            .h(px(148.))
            .w_full()
            .items_center()
            .justify_center()
            .gap_1p5()
            .rounded(theme.radius_lg)
            .border_1()
            .border_dashed()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|this| this.bg(theme.muted.opacity(0.5)))
            .child(
                Icon::new(IconName::FolderPlus)
                    .size_6()
                    .text_color(theme.muted_foreground),
            )
            .child(div().text_sm().child("Add folders"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("New chats in this project start with them."),
            )
            .on_click(cx.listener(|this, _, window, cx| this.pick_folders(window, cx)))
    }

    /// The chosen folders, each removable, then a row adding more.
    fn render_folder_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.folders.iter().enumerate().map(|(index, path)| {
            let full_path = SharedString::from(path.display().to_string());
            h_flex()
                .debug_selector(move || format!("create-project-folder-{index}"))
                .h(px(36.))
                .px_3()
                .gap_2()
                .when(index > 0, |this| {
                    this.border_t_1().border_color(theme.border)
                })
                .child(
                    Icon::new(IconName::Folder)
                        .size_4()
                        .flex_none()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "create-project-folder-name-{index}"
                        )))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .tooltip(move |window, cx| {
                            Tooltip::new(full_path.clone()).build(window, cx)
                        })
                        .child(sandbox::folder_name(path)),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-project-folder-{index}")))
                        .icon(Icon::new(IconName::X).size_4())
                        .ghost()
                        .xsmall()
                        .debug_selector(move || format!("create-project-remove-folder-{index}"))
                        .accessibility_label("Remove folder")
                        .tooltip("Remove folder")
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_folder(index, cx))),
                )
        });
        let add = h_flex()
            .id("create-project-add-folder")
            .debug_selector(|| "create-project-add-folder".to_owned())
            .role(gpui::Role::Button)
            .aria_label("Add folder")
            .h(px(36.))
            .px_3()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .text_sm()
            .text_color(theme.muted_foreground)
            .cursor_pointer()
            .hover(|this| this.bg(theme.muted.opacity(0.5)))
            .child(Icon::new(IconName::Plus).size_4())
            .child("Add folder")
            .on_click(cx.listener(|this, _, window, cx| this.pick_folders(window, cx)));
        v_flex()
            .w_full()
            .overflow_hidden()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .id("create-project-folders")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(add)
    }
}

impl Render for CreateProjectDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_create = project_name(&self.name.read(cx).value()).is_some();
        let folders = if self.folders.is_empty() {
            self.render_empty_folders(cx).into_any_element()
        } else {
            self.render_folder_list(cx).into_any_element()
        };
        v_flex()
            .gap_5()
            .child(
                v_flex()
                    .gap_2()
                    .child(Self::label("Project name", cx))
                    .child(Input::new(&self.name).id("create-project-name")),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(Self::label("Source folders", cx))
                    .child(folders),
            )
            .child(
                DialogFooter::new()
                    .child(
                        Button::new("cancel-create-project")
                            .outline()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("create-project")
                            .primary()
                            .label("Create project")
                            .disabled(!can_create)
                            .debug_selector(|| "create-project".to_owned())
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}

impl Cowork {
    /// Opens the Create project dialog and returns its form, unless another
    /// dialog is open.
    pub(crate) fn open_create_project_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<CreateProjectDialog>> {
        if window.has_active_dialog(cx) {
            return None;
        }
        let cowork = cx.entity().downgrade();
        let form = cx.new(|cx| CreateProjectDialog::new(cowork, window, cx));
        let name = form.read(cx).name.clone();
        let dialog_form = form.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = dialog_form.clone();
            dialog
                .w(px(520.))
                .bg(cx.theme().popover)
                .content(move |content, _, _| {
                    content
                        .child(
                            DialogHeader::new().child(DialogTitle::new().child("Create project")),
                        )
                        .child(form.clone())
                })
        });
        name.focus_handle(cx).focus(window, cx);
        Some(form)
    }

    /// Adds a project to the sidebar, open, without starting a chat in it.
    /// Returns false when the name is blank.
    pub(crate) fn create_project(
        &mut self,
        name: &str,
        folders: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) -> bool {
        let created = self
            .projects
            .create(name, folders, SystemTime::now())
            .is_some();
        if created {
            cx.notify();
        }
        created
    }
}
