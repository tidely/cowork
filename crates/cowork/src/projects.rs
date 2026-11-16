//! Projects: named groups of the user's own chats in the sidebar, each with
//! the folders its new chats start with. See `docs/projects.md`.
//!
//! A project is only local organization. Its record is saved in the chats'
//! database (`chat_db/app_state.rs`); which project a chat belongs to is the
//! chat's own [`Thread::project`], saved with the chat and never sent to
//! collaborators. Membership is always explicit: never inferred from folders.
//!
//! A project's folders are not the folders its chats work in. Those are
//! each chat's own ([`Thread::project_folders`]); composing in a project only
//! copies the project's folders into the new chat.
//!
//! [`Thread::project`]: crate::thread::Thread::project
//! [`Thread::project_folders`]: crate::thread::Thread::project_folders

use std::{collections::HashMap, path::PathBuf, time::SystemTime};

use uuid::Uuid;

use crate::chat_db::StateSaver;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProjectId(Uuid);

impl ProjectId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub(crate) fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub(crate) fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    records: Vec<ProjectRecord>,
    /// Saves changes to the records; `None` keeps them in memory, as in
    /// tests and ephemeral launches.
    saver: Option<StateSaver>,
    section_open: bool,
    /// The open projects, each with how many of its chats it shows. A
    /// project closed (alone, or with the whole section) is removed, so it
    /// shows its first chats again when reopened.
    open: HashMap<ProjectId, usize>,
    /// The project whose row's menu is open, which keeps the row's buttons
    /// shown while the pointer is over the menu instead of the row.
    menu_open: Option<ProjectId>,
}

impl Default for Projects {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            saver: None,
            section_open: true,
            open: HashMap::new(),
            menu_open: None,
        }
    }
}

impl Projects {
    pub(crate) fn new(mut records: Vec<ProjectRecord>, saver: Option<StateSaver>) -> Self {
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        Self {
            records,
            saver,
            ..Self::default()
        }
    }

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
        let record = ProjectRecord {
            id,
            name,
            folders,
            created_at: now,
        };
        if let Some(saver) = &self.saver {
            saver.create_project(&record);
        }
        self.records.insert(0, record);
        self.open.insert(id, SHOWN_CHATS_STEP);
        Some(id)
    }

    /// Removes the project's record. Its saved chats are unfiled with it,
    /// but not the chats in memory: the caller takes them out of it. Returns
    /// whether it was known.
    pub(crate) fn remove(&mut self, id: ProjectId) -> bool {
        let before = self.records.len();
        self.records.retain(|record| record.id != id);
        if self.records.len() == before {
            return false;
        }
        self.open.remove(&id);
        if self.menu_open == Some(id) {
            self.menu_open = None;
        }
        if let Some(saver) = &self.saver {
            saver.remove_project(id);
        }
        true
    }

    pub(crate) fn menu_open(&self) -> Option<ProjectId> {
        self.menu_open
    }

    pub(crate) fn set_menu_open(&mut self, id: ProjectId, open: bool) {
        if open {
            self.menu_open = Some(id);
        } else if self.menu_open == Some(id) {
            self.menu_open = None;
        }
    }

    pub(crate) fn section_open(&self) -> bool {
        self.section_open
    }

    /// Opens or closes the section. Closing it closes every project too.
    pub(crate) fn toggle_section(&mut self) {
        self.section_open.toggle();
        if !self.section_open {
            self.open.clear();
        }
    }

    /// How many of the project's chats it shows; `None` while it is closed.
    pub(crate) fn shown_chats(&self, id: ProjectId) -> Option<usize> {
        self.open.get(&id).copied()
    }

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
}

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::chat_db::TestDb;

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn projects_are_saved_newest_first_and_reopen_collapsed() {
        let (test_db, db) = TestDb::open();
        let reopen = |db: crate::chat_db::ChatDb| {
            futures::executor::block_on(db.quit());
            let (db, saved) = test_db.reopen();
            let projects = Projects::new(saved.state.projects, Some(db.saver()));
            (db, projects)
        };
        let mut projects = Projects::new(Vec::new(), Some(db.saver()));
        assert_eq!(projects.create("  \t ", Vec::new(), at(1)), None);
        let missing = PathBuf::from("/no/such/folder");
        let first = projects
            .create(" Site ", vec![missing.clone()], at(1))
            .unwrap();
        // Names needn't be unique.
        let second = projects.create("Site", Vec::new(), at(2)).unwrap();
        assert_eq!(projects.shown_chats(second), Some(SHOWN_CHATS_STEP));

        let (db, mut reopened) = reopen(db);
        let ids: Vec<_> = reopened.records().iter().map(|record| record.id).collect();
        assert_eq!(ids, [second, first]);
        assert_eq!(reopened.records()[1].name, "Site");
        assert_eq!(reopened.records()[1].folders, [missing]);
        assert!(reopened.section_open());
        assert_eq!(reopened.shown_chats(first), None);
        assert_eq!(reopened.shown_chats(second), None);

        // Removing one is saved too.
        assert!(reopened.remove(second));
        assert!(!reopened.remove(second));
        let (db, reopened) = reopen(db);
        let ids: Vec<_> = reopened.records().iter().map(|record| record.id).collect();
        assert_eq!(ids, [first]);
        futures::executor::block_on(db.quit());
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
}
