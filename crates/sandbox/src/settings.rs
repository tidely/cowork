//! Command sandbox settings, kept here as the single policy reference.
//!
//! Commands use the host shell and login environment, contained by Seatbelt
//! on macOS, Bubblewrap on Linux, or a Windows process container via mxc-sdk.
//! Each command gets its own sandbox with these permissions:
//! - Read the host filesystem, subject to OS permissions and `DENIED_PATHS`.
//!   Unix grants `/`; Windows grants each drive and its entries.
//! - Write the project with Write/Network scope, plus explicitly granted
//!   writable folders. The thread's scratch folder is always writable.
//!   Unix also permits writes to `/dev/null` for normal output redirection.
//! - Network only with Network scope: outbound connections, no inbound or
//!   host loopback connections.
//! - No clipboard or input injection. Windows keeps desktop access because
//!   console programs otherwise fail to start.
//!
//! Commands start in the first project folder, or scratch without one.
//! HOME is the real home; TMPDIR/TMP/TEMP point to scratch. Commands
//! have no time/output/resource limits; stopping the run ends them and any
//! background processes. See `docs/permissions.md` for approval behavior.

/// Where each thread's scratch folder is, under Cowork's home
/// (`~/.cowork/scratch/<app instance>/<thread>`). It is kept between the
/// thread's commands, removed when the thread is deleted, and removed with
/// the rest of the instance's when Cowork quits. Scratch folders of an
/// instance that did not quit cleanly are removed by the next instance.
pub const SCRATCH_DIR: &str = "scratch";

/// Broad read-only root grants otherwise override the backend's standard
/// null-device write permission, breaking ordinary shell redirection.
#[cfg(unix)]
pub const WRITABLE_DEVICES: &[&str] = &["/dev/null"];

/// Credentials commands may not read even where a granted folder holds
/// them, relative to the user's home. Only paths that exist are passed on, as
/// some backends cannot deny a path that is not there.
pub const DENIED_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker/config.json",
    ".config/gh",
    ".config/gcloud",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".npmrc",
    ".yarnrc.yml",
    ".pypirc",
    ".netrc",
    ".git-credentials",
];
