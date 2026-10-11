//! The host folders a thread's commands may use.
//!
//! They are the host's real folders at their real paths: commands see the
//! host's changes as they happen, and when the project is writable the host
//! sees theirs, immediately and without review. Each command is contained by
//! the project as it is when the command starts, with the access it was
//! granted in place of the project's when it has one (see
//! [`Sandboxes::run_with`](crate::Sandboxes::run_with)), so a change applies
//! from the next command on, and commands already running keep the access
//! they started with.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A thread's project, as its commands see it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Project {
    /// The folders, in the order the host added them. Names are unique; see
    /// [`folder_name`].
    pub folders: Vec<ProjectFolder>,
    /// What commands may do with the folders and the network, unless a
    /// command is granted other access of its own. Enforced by the operating
    /// system's sandbox.
    pub access: Access,
}

impl Project {
    /// Whether commands, and the file tools, may change the folders.
    pub fn writable(&self) -> bool {
        self.access.writable()
    }
}

/// What a command may do, each level including the one before it. The
/// scratch folder is always writable, and credentials always denied.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Read the project folders.
    #[default]
    Read,
    /// Read and change the project folders.
    Write,
    /// Read and change the project folders, and reach the network.
    Network,
}

impl Access {
    pub fn writable(self) -> bool {
        self >= Self::Write
    }

    pub fn networked(self) -> bool {
        self == Self::Network
    }

    /// As the model and the `access` argument name it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Network => "network",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectFolder {
    pub name: String,
    pub path: PathBuf,
}

/// What a host folder is called: its last component, with path separators
/// and characters that read badly in a name replaced. Callers make names
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

/// `/` and `\` would make more than one path component; the rest separate
/// lists in paths and settings.
fn is_forbidden(c: char) -> bool {
    matches!(c, '/' | '\\' | ':' | ';' | ',')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_named_by_their_last_component() {
        assert_eq!(folder_name(Path::new("/home/me/src/zed")), "zed");
        assert_eq!(folder_name(Path::new("/home/me/src/zed/")), "zed");
        assert_eq!(folder_name(Path::new("/home/me/a:b;c,d")), "a_b_c_d");
        assert_eq!(folder_name(Path::new("/home/me/tab\there")), "tab_here");
        assert_eq!(folder_name(Path::new("/")), "root");
    }
}
