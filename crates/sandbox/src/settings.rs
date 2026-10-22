//! Command sandbox settings, kept here as the single policy reference.
//!
//! Commands use the host shell and login environment, contained by Seatbelt
//! on macOS, Bubblewrap on Linux, or a Windows process container via mxc-sdk.
//! Each command gets its own sandbox with these permissions:
//! - Read the host filesystem, subject to OS permissions and `DENIED_PATHS`.
//!   macOS grants `/`; Windows grants each drive and its entries. Linux grants
//!   the root's entries except `/dev` and `/proc`, keeping Bubblewrap's private
//!   devices and process filesystem instead of shadowing them with host mounts.
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
//! have no output/resource limits and no time limit unless `run_command`
//! supplies a positive `timeout_ms`. Its runtime starts at process launch;
//! on expiry the command and its children are ended, and a timeout error
//! keeps captured stdout/stderr. Stopping the run also ends commands and any
//! background processes. See `docs/permissions.md` for approval behavior.

/// Where each thread's scratch folder is, in the thread's own folder
/// (`~/.cowork/threads/<thread>/scratch`). It is kept as long as the thread,
/// across restarts too, and removed with it when the thread is deleted.
pub const SCRATCH_DIR: &str = "scratch";

/// Linux's synthetic devices must not be shadowed by policy binds: ordinary
/// Bubblewrap binds are nodev, even for a read-write `/dev/null` grant.
#[cfg(target_os = "linux")]
pub const PRIVATE_FILESYSTEMS: &[&str] = &["/dev", "/proc"];

/// macOS's broad read-only root grant needs an explicit null-device exception.
/// Linux already has a writable null device in Bubblewrap's private `/dev`.
#[cfg(all(unix, not(target_os = "linux")))]
pub const WRITABLE_DEVICES: &[&str] = &["/dev/null"];
#[cfg(target_os = "linux")]
pub const WRITABLE_DEVICES: &[&str] = &[];

/// Credentials commands may not read even where a granted folder holds
/// them, relative to the user's home. Only paths that exist are passed on, as
/// some backends cannot deny a path that is not there.
/// mxc's macOS profile blocks contents and directory listings but permits file
/// metadata: a known path's existence may remain visible. Linux masks entries.
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
