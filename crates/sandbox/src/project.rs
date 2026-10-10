//! The host folders a thread's sandbox shows its commands, under
//! [`settings::PROJECTS_DIR`].
//!
//! They are the host's real folders, shared live: commands see the host's
//! changes as they happen, and when the project is writable the host sees
//! theirs, immediately and without review. A sandbox mounts its project when
//! it boots; microsandbox cannot change the mounts of a running VM, so a
//! changed project means a new sandbox, started by the next command.

use std::path::{Path, PathBuf};

use crate::{SandboxError, settings};

/// A thread's project, as its sandbox mounts it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Project {
    /// Mounted at `/projects/<name>`, in this order. Names must be unique
    /// and valid mount names; see [`folder_name`].
    pub folders: Vec<ProjectFolder>,
    /// Whether commands may change the folders. Read-only is enforced by
    /// the host's file server as well as the guest kernel.
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectFolder {
    pub name: String,
    pub path: PathBuf,
}

/// What a host folder is called under `/projects`: its last component, with
/// the characters a mount path cannot hold replaced. Callers make names
/// unique themselves, as only they know the other folders.
pub fn folder_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let name = name
        .chars()
        .map(|c| {
            if is_forbidden(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    if name.is_empty() || name == "." || name == ".." {
        // A drive or filesystem root, such as `/` or `C:\`.
        "root".to_owned()
    } else {
        name
    }
}

/// `/` and `\` would make more than one path component; microsandbox
/// refuses the rest in guest mount paths.
fn is_forbidden(c: char) -> bool {
    matches!(c, '/' | '\\' | ':' | ';' | ',')
}

impl Project {
    pub(crate) fn validate(&self) -> Result<(), SandboxError> {
        for (index, folder) in self.folders.iter().enumerate() {
            let name = &folder.name;
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.chars().any(|c| is_forbidden(c) || c.is_control())
            {
                return Err(SandboxError::Setup(format!(
                    "the project folder name {name:?} cannot be mounted"
                )));
            }
            if self.folders[..index]
                .iter()
                .any(|earlier| earlier.name == *name)
            {
                return Err(SandboxError::Setup(format!(
                    "two project folders are named {name:?}"
                )));
            }
        }
        Ok(())
    }

    /// Where `folder` is in the guest.
    pub(crate) fn guest_path(folder: &ProjectFolder) -> String {
        format!("{}/{}", settings::PROJECTS_DIR, folder.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(names: &[&str]) -> Project {
        Project {
            folders: names
                .iter()
                .map(|name| ProjectFolder {
                    name: (*name).to_owned(),
                    path: PathBuf::from("/host").join(name),
                })
                .collect(),
            writable: false,
        }
    }

    #[test]
    fn folders_are_named_by_a_mountable_last_component() {
        assert_eq!(folder_name(Path::new("/home/me/src/zed")), "zed");
        assert_eq!(folder_name(Path::new("/home/me/src/zed/")), "zed");
        assert_eq!(folder_name(Path::new("/home/me/a:b;c,d")), "a_b_c_d");
        assert_eq!(folder_name(Path::new("/home/me/tab\there")), "tab_here");
        assert_eq!(folder_name(Path::new("/")), "root");
    }

    #[test]
    fn unmountable_or_repeated_names_are_refused() {
        assert!(project(&["zed", "cowork"]).validate().is_ok());
        for names in [&["zed", "zed"][..], &[""], &[".."], &["a/b"], &["a:b"]] {
            assert!(project(names).validate().is_err(), "{names:?}");
        }
    }
}
