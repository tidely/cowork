//! Projects: named groups of the user's own chats in the sidebar, each with
//! the folders its new chats start with. See `docs/projects.md`.
//!
//! A project is only local organization. Its record (name, starting
//! folders, when it was made) is saved in `~/.cowork/projects.json`; which
//! project a chat belongs to is the chat's own [`Thread::project`], saved
//! with the chat in its `thread.json` and never sent to collaborators.
//! Membership is always explicit: it is never inferred from folders.
//!
//! A project's folders are not the folders its chats work in. Those are
//! each chat's own ([`Thread::project_folders`], what `sandbox::Project` is
//! built from); composing in a project only copies the project's folders
//! into the new chat, which changes them independently from then on.
//!
//! [`Thread::project`]: crate::thread::Thread::project
//! [`Thread::project_folders`]: crate::thread::Thread::project_folders

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a project, in its record and in the chats filed under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ProjectId(Uuid);

impl ProjectId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A project as it is saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectRecord {
    pub(crate) id: ProjectId,
    /// Trimmed and never blank. Names needn't be unique.
    pub(crate) name: String,
    /// The folders a chat composed in the project starts with, in the order
    /// they were chosen. Kept even when one no longer exists; a chat started
    /// with it handles that as any chat with a missing folder does.
    pub(crate) folders: Vec<PathBuf>,
    /// Projects are listed newest first.
    pub(crate) created_at: SystemTime,
}

/// How many more of a project's chats each "Show more" reveals, and how many
/// an open project shows to begin with.
pub(crate) const SHOWN_CHATS_STEP: usize = 5;

/// The saved projects, and how the sidebar shows them. Only the records are
/// saved: on every launch the section starts open and each project closed,
/// showing its first [`SHOWN_CHATS_STEP`] chats once opened.
pub(crate) struct Projects {
    /// Newest first.
    records: Vec<ProjectRecord>,
    /// Where the records are saved; `None` keeps them in memory, as in tests
    /// and ephemeral launches.
    file: Option<PathBuf>,
    /// Whether the sidebar's Projects section lists the projects.
    section_open: bool,
    /// The open projects, each with how many of its chats it shows. A
    /// project closed (alone, or with the whole section) is removed, so it
    /// shows its first chats again when reopened.
    open: HashMap<ProjectId, usize>,
}

impl Default for Projects {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            file: None,
            section_open: true,
            open: HashMap::new(),
        }
    }
}

impl Projects {
    /// The projects saved at `file`, saved there from now on.
    pub(crate) fn load(file: Option<PathBuf>) -> Self {
        let mut records = file.as_deref().map(load_records).unwrap_or_default();
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        Self {
            records,
            file,
            ..Self::default()
        }
    }

    /// Newest first.
    pub(crate) fn records(&self) -> &[ProjectRecord] {
        &self.records
    }

    pub(crate) fn record(&self, id: ProjectId) -> Option<&ProjectRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    /// Adds a project named `name`, trimmed, starting chats with `folders`,
    /// and opens it in the sidebar. `None` when the name is blank.
    pub(crate) fn create(
        &mut self,
        name: &str,
        folders: Vec<PathBuf>,
        now: SystemTime,
    ) -> Option<ProjectId> {
        let name = project_name(name)?;
        let id = ProjectId::new();
        self.records.insert(
            0,
            ProjectRecord {
                id,
                name,
                folders,
                created_at: now,
            },
        );
        self.open.insert(id, SHOWN_CHATS_STEP);
        self.save();
        Some(id)
    }

    pub(crate) fn section_open(&self) -> bool {
        self.section_open
    }

    /// Opens or closes the section. Closing it closes every project too.
    pub(crate) fn toggle_section(&mut self) {
        self.section_open = !self.section_open;
        if !self.section_open {
            self.open.clear();
        }
    }

    /// How many of the project's chats it shows; `None` while it is closed.
    pub(crate) fn shown_chats(&self, id: ProjectId) -> Option<usize> {
        self.open.get(&id).copied()
    }

    /// Opens the project on its first chats, or closes it.
    pub(crate) fn toggle(&mut self, id: ProjectId) {
        if self.open.remove(&id).is_none() {
            self.open.insert(id, SHOWN_CHATS_STEP);
        }
    }

    /// Shows [`SHOWN_CHATS_STEP`] more of an open project's chats.
    pub(crate) fn show_more(&mut self, id: ProjectId) {
        if let Some(shown) = self.open.get_mut(&id) {
            *shown += SHOWN_CHATS_STEP;
        }
    }

    /// Small and written only when a project is created, so it is written
    /// in place of waiting on another thread.
    fn save(&self) {
        let Some(path) = &self.file else {
            return;
        };
        if let Err(error) = save_records(path, &self.records) {
            eprintln!("could not save projects to {}: {error:#}", path.display());
        }
    }
}

/// `name` trimmed, or `None` when that leaves nothing.
pub(crate) fn project_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Appends the paths not already in `folders`, in the order given. Returns
/// whether any was added.
pub(crate) fn add_folder_paths(
    folders: &mut Vec<PathBuf>,
    paths: impl IntoIterator<Item = PathBuf>,
) -> bool {
    let before = folders.len();
    for path in paths {
        if !folders.contains(&path) {
            folders.push(path);
        }
    }
    folders.len() != before
}

/// The saved file's format. Older or newer files aren't read, nor
/// overwritten. A new field doesn't need a new format if it has a default.
const FILE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct ProjectsFile {
    version: u32,
    projects: Vec<ProjectRecord>,
}

/// Where the projects are saved: next to Cowork's other files.
pub(crate) fn projects_file() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".cowork").join("projects.json"))
}

/// Writes `records` whole, replacing the old file only once the new one is
/// written, so a crash never leaves half a file.
fn save_records(path: &Path, records: &[ProjectRecord]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = ProjectsFile {
        version: FILE_VERSION,
        projects: records.to_vec(),
    };
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&file)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Reads the saved projects, or none yet. A file that can't be read is moved
/// aside rather than overwritten by the next save. Chats filed under a
/// project no longer known show in Recents, keeping their membership.
fn load_records(path: &Path) -> Vec<ProjectRecord> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            eprintln!("could not read projects from {}: {error}", path.display());
            return Vec::new();
        }
    };
    match serde_json::from_slice::<ProjectsFile>(&bytes) {
        Ok(file) if file.version == FILE_VERSION => file.projects,
        result => {
            let aside = path.with_extension("json.unreadable");
            match result {
                Ok(file) => eprintln!(
                    "projects in {} are format {}, not {FILE_VERSION}; moved to {}",
                    path.display(),
                    file.version,
                    aside.display()
                ),
                Err(error) => eprintln!(
                    "could not read projects from {}: {error}; moved to {}",
                    path.display(),
                    aside.display()
                ),
            }
            _ = std::fs::rename(path, aside);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn temporary_file() -> PathBuf {
        std::env::temp_dir()
            .join(format!("cowork-projects-{}", Uuid::new_v4()))
            .join("projects.json")
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn projects_are_saved_newest_first_and_reopen_collapsed() {
        let path = temporary_file();
        let mut projects = Projects::load(Some(path.clone()));
        assert!(projects.records().is_empty(), "no file yet");
        assert_eq!(projects.create("  \t ", Vec::new(), at(1)), None);
        let missing = PathBuf::from("/no/such/folder");
        let first = projects
            .create(" Site ", vec![missing.clone()], at(1))
            .unwrap();
        // Names needn't be unique.
        let second = projects.create("Site", Vec::new(), at(2)).unwrap();
        assert_eq!(projects.shown_chats(second), Some(SHOWN_CHATS_STEP));

        let mut reopened = Projects::load(Some(path.clone()));
        let ids: Vec<_> = reopened.records().iter().map(|record| record.id).collect();
        assert_eq!(ids, [second, first]);
        assert_eq!(reopened.records()[1].name, "Site");
        assert_eq!(reopened.records()[1].folders, [missing]);
        assert!(reopened.section_open());
        assert_eq!(reopened.shown_chats(first), None);
        assert_eq!(reopened.shown_chats(second), None);

        // A project created after reopening goes first and is saved too.
        let third = reopened.create("Docs", Vec::new(), at(3)).unwrap();
        let ids: Vec<_> = Projects::load(Some(path.clone()))
            .records()
            .iter()
            .map(|record| record.id)
            .collect();
        assert_eq!(ids, [third, second, first]);

        _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn closing_a_project_or_the_section_shows_the_first_chats_again() {
        let mut projects = Projects::default();
        let one = projects.create("One", Vec::new(), at(1)).unwrap();
        let two = projects.create("Two", Vec::new(), at(2)).unwrap();
        projects.show_more(one);
        projects.show_more(one);
        assert_eq!(projects.shown_chats(one), Some(3 * SHOWN_CHATS_STEP));

        projects.toggle(one);
        assert_eq!(projects.shown_chats(one), None);
        // Closed projects show no more.
        projects.show_more(one);
        projects.toggle(one);
        assert_eq!(projects.shown_chats(one), Some(SHOWN_CHATS_STEP));

        projects.show_more(two);
        projects.toggle_section();
        assert!(!projects.section_open());
        projects.toggle_section();
        assert_eq!(projects.shown_chats(one), None);
        assert_eq!(projects.shown_chats(two), None);
    }

    #[test]
    fn chosen_folders_skip_ones_already_chosen() {
        let mut folders = vec![PathBuf::from("/src/a")];
        assert!(add_folder_paths(
            &mut folders,
            ["/src/b".into(), "/src/a".into(), "/src/b".into()],
        ));
        assert_eq!(folders, [PathBuf::from("/src/a"), PathBuf::from("/src/b")]);
        assert!(!add_folder_paths(&mut folders, ["/src/a".into()]));
    }

    #[test]
    fn other_formats_are_moved_aside() {
        let path = temporary_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"version": 99, "projects": []}"#).unwrap();

        assert!(Projects::load(Some(path.clone())).records().is_empty());
        assert!(!path.exists());
        assert!(path.with_extension("json.unreadable").exists());

        _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
