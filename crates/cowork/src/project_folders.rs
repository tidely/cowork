//! The thread's project in the bottom bar: the folders it works in, and,
//! for the host, a button adding more and a menu on each folder.
//!
//! The folders are kept in [`Thread::project_folders`] and, before the
//! thread exists, [`Cowork::new_thread_project_folders`]. Collaborators only
//! ever learn their names. Nothing gives them to the agent yet.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::{
    Anchor, App, ClickEvent, Context, IntoElement, PathPromptOptions, RenderOnce, SharedString,
    WeakEntity, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
};
use gpui_kit_assets::IconName;

use crate::{Cowork, thread::Thread};

/// One folder of a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectFolder {
    /// What the folder is shown as, and all collaborators learn of it.
    pub(crate) name: SharedString,
    /// Where the folder is on this machine. `None` in a thread mirrored from
    /// someone else, whose folders are on the host's machine.
    pub(crate) path: Option<PathBuf>,
}

impl ProjectFolder {
    pub(crate) fn local(path: PathBuf) -> Self {
        Self {
            name: folder_name(&path),
            path: Some(path),
        }
    }

    /// The folders of a host's project, by the names it sent.
    pub(crate) fn mirrored(names: Vec<String>) -> Vec<Self> {
        names
            .into_iter()
            .map(|name| Self {
                name: name.into(),
                path: None,
            })
            .collect()
    }
}

/// What a folder is shown as: its last component, or the whole path for
/// one without, such as `/`.
fn folder_name(folder: &Path) -> SharedString {
    folder
        .file_name()
        .map_or_else(
            || folder.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
        .into()
}

/// Appends the folders not already in the project, keeping the order they
/// were added in. Returns whether any was added.
pub(crate) fn add_folders(
    project: &mut Vec<ProjectFolder>,
    paths: impl IntoIterator<Item = PathBuf>,
) -> bool {
    let before = project.len();
    for path in paths {
        if !project
            .iter()
            .any(|folder| folder.path.as_ref() == Some(&path))
        {
            project.push(ProjectFolder::local(path));
        }
    }
    project.len() != before
}

/// Returns whether the folder was in the project.
pub(crate) fn remove_folder(project: &mut Vec<ProjectFolder>, path: &Path) -> bool {
    let before = project.len();
    project.retain(|folder| folder.path.as_deref() != Some(path));
    project.len() != before
}

/// What opens a folder in the platform's file manager is called there.
const OPEN_FOLDER_LABEL: &str = if cfg!(target_os = "macos") {
    "Open in Finder"
} else if cfg!(target_os = "windows") {
    "Open in File Explorer"
} else {
    "Open in File Manager"
};

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type FolderHandler = Rc<dyn Fn(&Path, &mut Window, &mut App)>;

/// What the host can do with its project.
struct FolderActions {
    on_add: ClickHandler,
    on_open: FolderHandler,
    on_remove: FolderHandler,
}

/// The project's folders, each with its name. The host can click one for a
/// menu opening or removing it, and has a button adding more; with no
/// folders, there is only that button. Collaborators just see the names.
#[derive(IntoElement)]
pub(crate) struct ProjectFolders {
    /// Prefixes the ids and debug selectors of the parts, so it must be
    /// unique among the pickers on screen.
    id: SharedString,
    folders: Vec<ProjectFolder>,
    actions: Option<FolderActions>,
}

impl ProjectFolders {
    pub(crate) fn new(id: impl Into<SharedString>, folders: Vec<ProjectFolder>) -> Self {
        Self {
            id: id.into(),
            folders,
            actions: None,
        }
    }

    /// Lets the folders be changed, as only the host may.
    pub(crate) fn editable(
        mut self,
        on_add: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_open: impl Fn(&Path, &mut Window, &mut App) + 'static,
        on_remove: impl Fn(&Path, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.actions = Some(FolderActions {
            on_add: Rc::new(on_add),
            on_open: Rc::new(on_open),
            on_remove: Rc::new(on_remove),
        });
        self
    }

    fn render_folder(
        id: &str,
        ix: usize,
        folder: ProjectFolder,
        actions: Option<&FolderActions>,
        cx: &App,
    ) -> gpui::AnyElement {
        let folder_id = format!("{id}-folder-{ix}");
        let (Some(actions), Some(path)) = (actions, folder.path) else {
            return h_flex()
                .id(SharedString::from(folder_id.clone()))
                .debug_selector(move || folder_id.clone())
                .min_w_0()
                .gap_1p5()
                .px_1()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::Folder).size_4().flex_none())
                .child(
                    div()
                        .min_w_0()
                        .max_w(px(160.))
                        .truncate()
                        .child(folder.name),
                )
                .into_any_element();
        };
        let on_open = actions.on_open.clone();
        let on_remove = actions.on_remove.clone();
        let path = Rc::new(path);
        Button::new(SharedString::from(folder_id.clone()))
            .debug_selector(move || folder_id.clone())
            .icon(Icon::new(IconName::Folder))
            .label(folder.name)
            .ghost()
            .small()
            .min_w_0()
            .max_w(px(200.))
            .tooltip(SharedString::from(path.display().to_string()))
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                let (open, remove) = (on_open.clone(), on_remove.clone());
                let (open_path, remove_path) = (path.clone(), path.clone());
                menu.item(
                    PopupMenuItem::new(OPEN_FOLDER_LABEL)
                        .icon(IconName::FolderOpen)
                        .on_click(move |_, window, cx| open(&open_path, window, cx)),
                )
                .item(
                    PopupMenuItem::new("Remove Project")
                        .icon(IconName::Trash)
                        .on_click(move |_, window, cx| remove(&remove_path, window, cx)),
                )
            })
            .into_any_element()
    }
}

impl RenderOnce for ProjectFolders {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let mut parts = self
            .folders
            .into_iter()
            .enumerate()
            .map(|(ix, folder)| Self::render_folder(&id, ix, folder, self.actions.as_ref(), cx))
            .collect::<Vec<_>>();
        let add = self.actions.map(|actions| {
            let on_add = actions.on_add;
            let add_id = format!("{id}-add");
            Button::new(SharedString::from(add_id.clone()))
                .debug_selector(move || add_id.clone())
                .icon(Icon::new(IconName::FolderPlus))
                .ghost()
                .small()
                .accessibility_label("Add a folder to the project")
                .tooltip("Add a folder to the project")
                .on_click(move |event, window, cx| on_add(event, window, cx))
                .into_any_element()
        });
        parts.extend(add);

        // The same line separates every folder from the next, and the last
        // from the add button.
        let mut row = h_flex().min_w_0().gap_1();
        for (ix, part) in parts.into_iter().enumerate() {
            if ix > 0 {
                let separator_id = format!("{id}-separator-{}", ix - 1);
                row = row.child(
                    div()
                        .debug_selector(move || separator_id.clone())
                        .flex_none()
                        .w(px(1.))
                        .h(px(16.))
                        .bg(cx.theme().border),
                );
            }
            row = row.child(part);
        }
        row
    }
}

impl Cowork {
    /// The active thread's folders, or those of the thread about to be
    /// created, and whether the local user may change them: only the host
    /// can, since the folders are on its machine.
    pub(crate) fn active_project(&self, cx: &App) -> (Vec<ProjectFolder>, bool) {
        match self.active_thread(cx) {
            Some(thread) => {
                let thread = thread.read(cx);
                (thread.project_folders().to_vec(), thread.is_host())
            }
            None => (self.new_thread_project_folders.clone(), true),
        }
    }

    pub(crate) fn render_project_folders(&self, cx: &mut Context<Self>) -> ProjectFolders {
        let (folders, editable) = self.active_project(cx);
        let project = ProjectFolders::new("project-folders", folders);
        if !editable {
            return project;
        }
        // Changes go to the thread the folders were shown for, even if
        // another one is active by the time the menu or picker is used.
        let target = self.active_thread(cx).map(|thread| thread.downgrade());
        project.editable(
            cx.listener({
                let target = target.clone();
                move |this, _: &ClickEvent, window, cx| {
                    this.pick_project_folders(target.clone(), window, cx)
                }
            }),
            |path, _, cx| cx.open_with_system(path),
            cx.listener(move |this, path: &Path, _, cx| {
                this.remove_project_folder(target.clone(), path, cx)
            }),
        )
    }

    fn pick_project_folders(
        &mut self,
        target: Option<WeakEntity<Thread>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            _ = this.update(cx, |this, cx| {
                this.add_project_folders(target, paths, cx);
            });
        })
        .detach();
    }

    /// Adds folders to a thread's project, or with `None` to the thread
    /// about to be created.
    pub(crate) fn add_project_folders(
        &mut self,
        thread: Option<WeakEntity<Thread>>,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        match thread {
            Some(thread) => {
                _ = thread.update(cx, |thread, _| thread.add_project_folders(paths));
            }
            None => {
                add_folders(&mut self.new_thread_project_folders, paths);
            }
        }
        cx.notify();
    }

    pub(crate) fn remove_project_folder(
        &mut self,
        thread: Option<WeakEntity<Thread>>,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        match thread {
            Some(thread) => {
                _ = thread.update(cx, |thread, _| thread.remove_project_folder(path));
            }
            None => {
                remove_folder(&mut self.new_thread_project_folders, path);
            }
        }
        cx.notify();
    }

    /// Moves the folders and mode chosen before the thread existed into it.
    /// The mode starts over for the next new thread.
    pub(crate) fn move_project_into(&mut self, thread: &gpui::Entity<Thread>, cx: &mut App) {
        let paths = std::mem::take(&mut self.new_thread_project_folders)
            .into_iter()
            .filter_map(|folder| folder.path)
            .collect();
        let mode = std::mem::take(&mut self.new_thread_project_mode);
        thread.update(cx, |thread, cx| {
            thread.add_project_folders(paths);
            thread.set_project_mode(mode, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(project: &[ProjectFolder]) -> Vec<PathBuf> {
        project
            .iter()
            .filter_map(|folder| folder.path.clone())
            .collect()
    }

    #[test]
    fn added_folders_keep_their_order_without_duplicates() {
        let mut project = vec![ProjectFolder::local("/src/zed".into())];
        assert!(add_folders(
            &mut project,
            ["/src/cowork".into(), "/src/zed".into(), "/src/rig".into()],
        ));
        assert_eq!(
            paths(&project),
            [
                PathBuf::from("/src/zed"),
                PathBuf::from("/src/cowork"),
                PathBuf::from("/src/rig"),
            ]
        );
        assert!(!add_folders(&mut project, ["/src/rig".into()]));
    }

    #[test]
    fn removing_a_folder_keeps_the_others_in_order() {
        let mut project = Vec::new();
        add_folders(
            &mut project,
            ["/src/zed".into(), "/src/cowork".into(), "/src/rig".into()],
        );
        assert!(remove_folder(&mut project, Path::new("/src/cowork")));
        assert!(!remove_folder(&mut project, Path::new("/src/cowork")));
        assert_eq!(
            paths(&project),
            [PathBuf::from("/src/zed"), PathBuf::from("/src/rig")]
        );
    }

    #[test]
    fn folders_are_named_by_their_last_component() {
        assert_eq!(folder_name(Path::new("/home/me/src/zed")), "zed");
        assert_eq!(folder_name(Path::new("/home/me/src/zed/")), "zed");
        assert_eq!(folder_name(Path::new("/")), "/");
    }
}
