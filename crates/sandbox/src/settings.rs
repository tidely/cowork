//! Every setting a sandbox runs with, in one place, so what the agent's
//! commands may do can be read here rather than reconstructed from builder
//! calls and library defaults.
//!
//! Each choice is the most restrictive one that still runs a command. Where a
//! microsandbox default is looser, the default is named so the difference is
//! visible.
//!
//! Settings without a constant here, set in [`crate::Sandboxes`]:
//!
//! - Network: disabled. The guest has no network device at all, instead of
//!   microsandbox's default of public internet access.
//! - Security profile: restricted, so commands get `no_new_privs`, no
//!   `CAP_SYS_ADMIN`, and `nosuid,nodev` mounts. The default keeps full
//!   guest root.
//! - Deployment profile: multi-tenant, turning on the host runtime's
//!   isolation floors (host vsock routes refused, connection limits).
//! - Lifetime: ephemeral, so the runtime discards a sandbox once it stops,
//!   and never detached, so it stops when Cowork exits.
//! - Runtime logs and metrics sampling: off.
//! - Pull policy: if missing, so the registry is contacted only while the
//!   image is not cached.
//!
//! Left at microsandbox defaults that are already as restrictive as they go:
//! no host folders shared, no published ports, no vsock routes, no secrets,
//! nothing added to the image's environment, stdin from `/dev/null`, no
//! terminal, and an anonymous HTTPS registry checked against system roots.
//!
//! One thing cannot be turned off: the runtime copies the output of a
//! sandbox's first command into an `exec.log` in its directory under
//! Cowork's microsandbox home. The directory is removed with the sandbox.

use std::time::Duration;

/// The guest image: Alpine, pinned to the digest of its multi-arch index so
/// the registry cannot substitute different contents for the tag. It is
/// pulled once from Docker Hub by the host, then served from the local cache.
/// All supported hosts use this Linux image, not their native shell or tools.
/// Its x86-64 and ARM64 variants have the same executable paths: `/bin/sh`
/// is BusyBox ash, and Bash and development toolchains are not installed.
/// Keep the model-facing tool inventory in `RunCommand::description` aligned
/// with this image when changing it.
pub const IMAGE: &str = "docker.io/library/alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6";

/// Virtual CPUs, with no hotplug headroom (`max_cpus` is the same). Shared by
/// a thread's commands running at the same time, as is [`MEMORY_MIB`].
pub const CPUS: u8 = 1;

/// Guest memory in MiB, with no hotplug headroom (`max_memory` is the same).
pub const MEMORY_MIB: u32 = 512;

/// The writable layer over the image, in MiB. It is a tmpfs in guest memory
/// (counting against [`MEMORY_MIB`]) instead of microsandbox's default 4 GiB
/// disk file on the host, so nothing a command writes reaches the host's disk.
pub const ROOT_DISK_MIB: u32 = 256;

/// Who commands run as: Alpine's `nobody` user and group. microsandbox's
/// default is root inside the guest.
pub const USER: &str = "65534:65534";

/// Where commands start. `nobody`'s home, `/`, is not writable; `/tmp` is.
pub const WORKDIR: &str = "/tmp";

/// The guest's hostname, instead of one derived from the sandbox's name,
/// which would tell commands which thread they run for.
pub const HOSTNAME: &str = "sandbox";

/// How long a sandbox may sit without commands before the runtime stops and
/// discards it. microsandbox's default is never.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a sandbox may live at all before the runtime stops and discards
/// it. microsandbox's default is forever.
pub const MAX_DURATION: Duration = Duration::from_secs(2 * 60 * 60);

/// How long one command may run before it is killed. microsandbox has no
/// default limit.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for a killed command to report its exit before giving up
/// on it.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// The most output kept from each of stdout and stderr. A command that writes
/// more is killed, so a flood cannot exhaust the host's memory: microsandbox
/// keeps all of a command's output otherwise.
pub const OUTPUT_LIMIT: usize = 16 * 1024;

/// The longest command the model may send.
pub const MAX_COMMAND_BYTES: usize = 8 * 1024;

/// Per-command resource limits, applied to the command's process tree only:
/// on the sandbox as a whole they would also bind its agent. Each command
/// gets its own, except [`rlimits::PROCESSES`].
pub mod rlimits {
    /// CPU seconds, matching [`super::COMMAND_TIMEOUT`].
    pub const CPU_SECONDS: u64 = super::COMMAND_TIMEOUT.as_secs();
    /// Processes the command's user may have. The kernel counts them per
    /// user, so commands running at the same time share this budget, and one
    /// that cannot start for it fails alone, keeping the sandbox.
    pub const PROCESSES: u64 = 128;
    /// Open files per process.
    pub const OPEN_FILES: u64 = 256;
    /// The largest file a process may write, in bytes.
    pub const FILE_SIZE: u64 = 64 * 1024 * 1024;
    /// Core dumps are off.
    pub const CORE_SIZE: u64 = 0;
}
