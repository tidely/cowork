//! What the agent's tools work with: one thread's commands and files
//! ([`ThreadSandbox`], the tools' context), and what one call needs to be
//! allowed ([`Needs`], the tools' access).
//!
//! A call's needs are computed from its arguments before anything runs, and
//! the host decides on them: [`ThreadSandbox::covers`] says whether they are
//! within what the thread already allows, the project's mode and folders and
//! the run's grants, so the call needs nobody's approval. The tool then runs
//! with exactly these needs.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{Access, Folders, Grant, Project, Sandboxes};

/// What one call of a sandbox tool needs beyond reading, as computed from its
/// arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Needs {
    /// Nothing beyond reading, which every call may do.
    #[default]
    Nothing,
    /// A command: the access it asks for of its own (`None` runs with the
    /// project's mode, or the run's grant if more), and the folders outside
    /// the project it names.
    Command {
        access: Option<Access>,
        folders: Folders,
    },
    /// A file write, to this path as the model named it.
    Write { path: String },
    /// More permissions for the rest of the run.
    Permissions(Grant),
    /// What the call asks for can't be granted, and why: a folder that
    /// doesn't exist, or a request for nothing.
    Refused(String),
}

/// Tools that need nothing, such as a calculator, need nothing here either.
impl From<()> for Needs {
    fn from(_: ()) -> Self {
        Self::Nothing
    }
}

/// One thread's place on the host: its project, scratch folder, and the
/// grants of its current run. The sandbox tools' context.
pub struct ThreadSandbox {
    sandboxes: Arc<Sandboxes>,
    thread: String,
}

impl ThreadSandbox {
    pub fn new(sandboxes: Arc<Sandboxes>, thread: impl Into<String>) -> Self {
        Self {
            sandboxes,
            thread: thread.into(),
        }
    }

    pub fn sandboxes(&self) -> &Arc<Sandboxes> {
        &self.sandboxes
    }

    pub fn thread(&self) -> &str {
        &self.thread
    }

    /// The project the thread's next command sees.
    pub fn project(&self) -> Project {
        self.sandboxes.project(&self.thread)
    }

    /// What `request_permissions` has granted the run so far.
    pub fn run_grant(&self) -> Grant {
        self.sandboxes.run_grant(&self.thread)
    }

    /// Forgets the run's grants, as a run starts or ends.
    pub fn end_run(&self) {
        self.sandboxes.end_run(&self.thread);
    }

    /// What a command asking for `access` and naming `folders` runs with
    /// now: its own access, or the project's mode or the run's grant,
    /// whichever is more; and the run's folders as well as its own.
    pub fn command_grant(&self, access: Option<Access>, folders: &Folders) -> Grant {
        let mut grant = self.run_grant();
        grant.access = access.unwrap_or_else(|| self.project().access.max(grant.access));
        grant.folders.extend(folders);
        grant
    }

    /// Whether `needs` are within what the thread already allows, so the
    /// call needs nobody's approval: for a command, nothing beyond the
    /// project's mode and folders and the run's grants (reads are allowed
    /// across the host already); for a write, a writable project; for more
    /// permissions, that the run has them already. Refusals are never
    /// covered.
    pub fn covers(&self, needs: &Needs) -> bool {
        let project = self.project();
        let mut granted = self.run_grant();
        granted.access = granted.access.max(project.access);
        // Naming a folder already in the project does not ask for more. Only
        // existing roots resolve; the sandbox and the file tools still
        // enforce their credential and symlink restrictions.
        for folder in &project.folders {
            if let Ok(path) = std::fs::canonicalize(&folder.path) {
                if granted.access.writable() {
                    granted.folders.write.push(path);
                } else {
                    granted.folders.read.push(path);
                }
            }
        }
        let writing = |access, folders: &Folders| Grant {
            access,
            folders: Folders {
                read: Vec::new(),
                write: folders.write.clone(),
            },
        };
        match needs {
            Needs::Nothing => true,
            Needs::Command { access, folders } => granted.covers(&writing(
                access.unwrap_or_else(|| project.access.max(granted.access)),
                folders,
            )),
            Needs::Write { .. } => project.writable(),
            Needs::Permissions(requested) => {
                granted.covers(&writing(requested.access, &requested.folders))
            }
            Needs::Refused(_) => false,
        }
    }
}
