//! What a command may do beyond its project's mode: more access to the
//! project, and host folders outside it, read-only or writable.
//!
//! The agent asks for these for one command (`run_command`'s `access`,
//! `read_folders`, and `write_folders`), or for the rest of its run
//! (`request_permissions`), as Codex's agent asks for additional
//! permissions. Either way the host decides, and the command is granted
//! exactly a [`Grant`]; the sandbox never sees an unresolved path.
//!
//! The folders the model names are resolved here, on the host, before
//! anyone is asked: `~` is the user's real home (commands' `HOME` is the
//! scratch folder), paths must be absolute and exist, and none may be in
//! [`settings::DENIED_PATHS`]. Credentials inside a granted folder stay
//! denied, as inside a project folder.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Access, resolved, settings};

/// Host folders outside the project, resolved as the sandbox matches them.
/// A writable folder may also be read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folders {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
}

impl Folders {
    /// The folders the model asked to `read` and `write`, resolved against
    /// the user's real home; see the module documentation.
    pub fn resolve(read: &[String], write: &[String]) -> Result<Self, String> {
        Self::resolve_in(read, write, std::env::home_dir().as_deref())
    }

    pub(crate) fn resolve_in(
        read: &[String],
        write: &[String],
        home: Option<&Path>,
    ) -> Result<Self, String> {
        let denied = home
            .iter()
            .flat_map(|home| settings::DENIED_PATHS.iter().map(|path| home.join(path)))
            .filter_map(|path| resolved(&path))
            .collect::<Vec<_>>();
        let resolve_all = |inputs: &[String]| {
            let mut folders = Vec::new();
            for input in inputs {
                let folder = resolve_folder(input, home, &denied)?;
                if !folders.contains(&folder) {
                    folders.push(folder);
                }
            }
            Ok::<_, String>(folders)
        };
        Ok(Self {
            read: resolve_all(read)?,
            write: resolve_all(write)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }

    /// Whether everything `other` reaches, these already do.
    pub fn covers(&self, other: &Folders) -> bool {
        let inside = |path: &Path, folders: &[PathBuf]| {
            folders.iter().any(|folder| path.starts_with(folder))
        };
        other
            .read
            .iter()
            .all(|path| inside(path, &self.read) || inside(path, &self.write))
            && other.write.iter().all(|path| inside(path, &self.write))
    }

    /// Adds what `other` reaches that these don't yet.
    pub fn extend(&mut self, other: &Folders) {
        for path in &other.write {
            if !self.write.iter().any(|folder| path.starts_with(folder)) {
                self.write.push(path.clone());
            }
        }
        for path in &other.read {
            if !self
                .read
                .iter()
                .chain(&self.write)
                .any(|folder| path.starts_with(folder))
            {
                self.read.push(path.clone());
            }
        }
    }
}

/// Everything a command runs with in place of its project's mode: the
/// access to the project, and the folders outside it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub access: Access,
    pub folders: Folders,
}

impl Grant {
    /// Whether a command granted `other` may do nothing these don't allow.
    pub fn covers(&self, other: &Grant) -> bool {
        other.access <= self.access && self.folders.covers(&other.folders)
    }

    /// Adds what `other` allows, as when the host grants more of a run.
    pub fn extend(&mut self, other: &Grant) {
        self.access = self.access.max(other.access);
        self.folders.extend(&other.folders);
    }

    /// The grant as the model is told it: one line per part, each starting
    /// with `- `.
    pub fn describe(&self) -> String {
        let mut lines = vec![format!(
            "- project access: {} ({})",
            self.access.name(),
            match self.access {
                Access::Read => "read the project folders",
                Access::Write => "change the project folders",
                Access::Network => "change the project folders and reach the network",
            }
        )];
        lines.extend(
            self.folders
                .write
                .iter()
                .map(|path| format!("- read and write: {}", path.display())),
        );
        lines.extend(
            self.folders
                .read
                .iter()
                .map(|path| format!("- read: {}", path.display())),
        );
        lines.join("\n")
    }
}

/// The folders a call's `arguments` name in `read_folders` and
/// `write_folders`, as the model wrote them. Anything else there names none,
/// as the tool would refuse such arguments before running.
pub(crate) fn named_folders(arguments: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let named = |key: &str| {
        serde_json::from_value::<Vec<String>>(arguments.get(key).cloned().unwrap_or_default())
            .unwrap_or_default()
    };
    (named("read_folders"), named("write_folders"))
}

/// One folder the model named, resolved; see the module documentation.
fn resolve_folder(input: &str, home: Option<&Path>, denied: &[PathBuf]) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    let path = match trimmed.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            let home = home.ok_or_else(|| {
                format!("`{trimmed}`: the user's home folder is unknown; use an absolute path.")
            })?;
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(trimmed),
    };
    if trimmed.is_empty() || !path.is_absolute() {
        return Err(format!(
            "`{trimmed}` is not an absolute path. Name folders by their absolute path on the \
             user's computer, or starting with `~/` for the user's home folder."
        ));
    }
    let real = resolved(&path).ok_or_else(|| format!("`{trimmed}` does not exist."))?;
    if denied.iter().any(|denied| real.starts_with(denied)) {
        return Err(format!(
            "`{trimmed}` holds credentials, which commands may never reach."
        ));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestRoot;

    /// A home with a `.cargo` folder holding a `registry`, and `.ssh`.
    fn home() -> TestRoot {
        let home = TestRoot::new();
        std::fs::create_dir_all(home.0.join(".cargo/registry")).unwrap();
        std::fs::create_dir_all(home.0.join(".ssh")).unwrap();
        home
    }

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn folders_resolve_from_home_or_absolute_paths() {
        let home = home();
        let cargo = resolved(&home.0.join(".cargo")).unwrap();
        let absolute = cargo.to_string_lossy().into_owned();
        let folders = Folders::resolve_in(
            &strings(&["~"]),
            &strings(&["~/.cargo", &absolute, " ~/.cargo/ "]),
            Some(&home.0),
        )
        .unwrap();
        assert_eq!(folders.write, [cargo], "duplicates are dropped");
        assert_eq!(folders.read, [resolved(&home.0).unwrap()]);
    }

    #[test]
    fn folders_that_cannot_be_granted_are_refused() {
        let home = home();
        let refused = |path: &str| {
            Folders::resolve_in(&[], &strings(&[path]), Some(&home.0)).expect_err(path)
        };
        assert!(refused("src").contains("absolute"));
        assert!(refused("").contains("absolute"));
        assert!(refused("~user/x").contains("absolute"), "only `~` itself");
        assert!(refused("~/missing").contains("does not exist"));
        assert!(refused("~/.ssh").contains("credentials"));
        assert!(
            Folders::resolve_in(&strings(&["~/.cargo"]), &[], None)
                .unwrap_err()
                .contains("home folder is unknown")
        );
    }

    #[test]
    fn grants_cover_what_they_reach() {
        let cargo = PathBuf::from("/home/me/.cargo");
        let registry = cargo.join("registry");
        let granted = Grant {
            access: Access::Network,
            folders: Folders {
                read: vec![PathBuf::from("/srv/docs")],
                write: vec![cargo.clone()],
            },
        };
        let asking = |access, read: &[&str], write: &[&str]| Grant {
            access,
            folders: Folders {
                read: read.iter().map(PathBuf::from).collect(),
                write: write.iter().map(PathBuf::from).collect(),
            },
        };
        assert!(granted.covers(&Grant::default()));
        assert!(granted.covers(&asking(Access::Write, &[], &["/home/me/.cargo/registry"])));
        assert!(granted.covers(&asking(Access::Read, &["/home/me/.cargo"], &[])));
        assert!(granted.covers(&asking(Access::Read, &["/srv/docs/a"], &[])));
        assert!(!granted.covers(&asking(Access::Read, &[], &["/srv/docs"])));
        assert!(!granted.covers(&asking(Access::Read, &["/home/me"], &[])));
        assert!(!granted.covers(&asking(Access::Read, &["/home/me/.cargo-other"], &[])));
        assert!(!Grant::default().covers(&asking(Access::Write, &[], &[])));

        let mut extended = Grant::default();
        extended.extend(&asking(Access::Read, &["/home/me/.cargo/registry"], &[]));
        extended.extend(&asking(Access::Write, &[], &["/home/me/.cargo"]));
        extended.extend(&asking(
            Access::Read,
            &["/home/me/.cargo/git"],
            &[registry.to_str().unwrap()],
        ));
        assert_eq!(extended.access, Access::Write);
        assert_eq!(extended.folders.write, [cargo]);
        assert_eq!(
            extended.folders.read,
            [registry],
            "kept, as it was granted before the folder holding it"
        );
    }

    #[test]
    fn grants_describe_every_part() {
        let grant = Grant {
            access: Access::Network,
            folders: Folders {
                read: vec![PathBuf::from("/srv/docs")],
                write: vec![PathBuf::from("/home/me/.cargo")],
            },
        };
        assert_eq!(
            grant.describe(),
            "- project access: network (change the project folders and reach the network)\n\
             - read and write: /home/me/.cargo\n\
             - read: /srv/docs"
        );
    }
}
