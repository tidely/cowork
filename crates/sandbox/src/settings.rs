//! Every setting a command runs with, in one place, so what the agent's
//! commands may do can be read here rather than reconstructed from request
//! fields and library defaults.
//!
//! Commands run on the host, in the host's own shell (`/bin/sh` on macOS and
//! Linux, the command line as given on Windows), contained by the operating
//! system's own sandbox through [mxc-sdk]: Seatbelt on macOS, Bubblewrap on
//! Linux (the `bwrap` program must be installed), and a process container on
//! Windows (a BaseContainer where Windows supports it, an AppContainer
//! otherwise). Each command is its own sandbox, made from the thread's project
//! as it is when the command starts; nothing is booted or kept between them.
//!
//! What a command may do, with each sandbox denying everything not granted:
//!
//! - Read, and write when the project is writable, the thread's project
//!   folders at their real paths.
//! - Read and write its thread's scratch folder ([`SCRATCH_DIR`]), which is
//!   its `HOME` and `TMPDIR`.
//! - Read the host's tools: the system's own locations (granted by the
//!   backend), every folder on the user's `PATH` with its prefix (see
//!   [`TOOL_PREFIX_DIRS`]), the toolchain folders named by well-known variables
//!   such as `CARGO_HOME` or `GOROOT`, and [`TOOLCHAIN_HOMES`].
//! - On Windows only, read every file on every drive, as Codex's sandbox
//!   does: each drive's root and, as root grants may not reach below it, each
//!   of its entries when the command starts. Programs there resolve their
//!   folder's full path through every folder above it, so without this `git`,
//!   PowerShell, and others fail in any project. This includes the user's
//!   other files; macOS and Linux don't need it, and stay limited to the
//!   folders above. In a read-only project, `git` commands that need the
//!   work tree, such as `status` and `diff`, still fail ("must be run in a
//!   work tree"), while `log` and `show` work: the sandbox denies changing
//!   back to a read-only folder (`git --work-tree=. status` says "cannot come
//!   back to cwd"). Codex's read-only sandbox fails the same way.
//! - Nothing in [`DENIED_PATHS`], even inside a folder granted above.
//! - No network: neither outbound nor inbound connections, loopback
//!   included.
//! - No clipboard or input injection. The desktop is not disabled: on
//!   Windows, a console program in a sandbox without one fails to start (`0xC0000142`), and that includes `git`, `cargo`, and
//!   PowerShell.
//!
//! Environment: the user's login shell environment, captured once the way
//! Zed does (see [`crate::environment`]), with `HOME` and `TMPDIR` pointing at
//! the scratch folder and the toolchain homes in [`TOOLCHAIN_HOMES`] set to
//! their real locations when the user's environment leaves them unset, so the
//! toolchains still find themselves.
//!
//! Commands are not limited in time, output, or resources. A command stops
//! when it exits or when its run is stopped; whatever it left running in the
//! background is ended with it.
//!
//! [mxc-sdk]: https://crates.io/crates/mxc-sdk

/// Where each thread's scratch folder is, under Cowork's home
/// (`~/.cowork/scratch/<app instance>/<thread>`). It is kept between the
/// thread's commands, removed when the thread is deleted, and removed with
/// the rest of the instance's when Cowork quits. Scratch folders of an
/// instance that did not quit cleanly are removed by the next instance.
pub const SCRATCH_DIR: &str = "scratch";

/// Folders on `PATH` whose parent is granted with them, as tools there are
/// usually links into, or load files from, their installation prefix:
/// `/opt/homebrew/bin` links into `/opt/homebrew/Cellar`, and
/// `~/.nvm/versions/node/v24/bin/npm` loads `../lib`. The parent is not
/// granted when it is the filesystem root or the user's home.
pub const TOOL_PREFIX_DIRS: &[&str] = &["bin", "sbin"];

/// Toolchain homes in the user's home folder, as the variable naming each
/// and its default location relative to home. Granted read-only when they
/// exist, and set in the environment when the user's leaves them unset,
/// since `HOME` points elsewhere for commands.
pub const TOOLCHAIN_HOMES: &[(&str, &str)] =
    &[("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")];

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
