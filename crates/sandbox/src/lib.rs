//! Running the agent's shell commands in a microVM per thread, with
//! microsandbox.
//!
//! Every setting is in [`settings`]. In short: no network, nothing from the
//! host shared but the thread's project folders (read-only unless the project
//! is writable; see [`Project`]), an unprivileged user, a pinned image,
//! memory-only writes elsewhere, and limits on each command's time, output,
//! and resources.
//!
//! Nothing here runs until the first command: no runtime download, no image
//! pull, no files. The runtime (`msb` and `libkrunfw`) is then installed into
//! Cowork's own microsandbox home, downloaded from the microsandbox GitHub
//! release matching the crate's version, so a user's own microsandbox
//! installation and configuration are never used. A runtime of another
//! version there is replaced.
//!
//! Each VM runs in a child `msb` process. microsandbox stops it if Cowork
//! exits, and as the sandboxes are ephemeral the runtime then discards them.
//!
//! Sandboxes are independent of each other, and a thread's commands may run
//! at the same time in its one sandbox: the map of sandboxes is only locked to
//! look a thread's slot up or change it, never while a VM boots or a command
//! runs.

mod output;
mod project;
pub mod settings;
mod tool;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use dashmap::{DashMap, mapref::entry::Entry};
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use microsandbox::{
    ExecControl, ExecEvent, LocalBackend, Sandbox,
    sandbox::{
        DeploymentProfile, PullPolicy, RlimitResource, SandboxStatus, SecurityProfile,
        StatVirtualization,
    },
    setup::{InstallOptions, Version, install_runtime, resolve_runtime, resolve_runtime_version},
};
use output::CappedStream;
pub use output::{CommandOutput, Killed};
pub use project::{Project, ProjectFolder, folder_name};
pub use tool::{ReadOnlyPass, RunCommand, RunCommandArgs};

/// The platforms, as `std::env::consts` `(OS, ARCH)`, microsandbox publishes
/// a runtime for.
const RUNTIME_PLATFORMS: &[(&str, &str)] = &[
    ("macos", "aarch64"),
    ("linux", "x86_64"),
    ("linux", "aarch64"),
    ("windows", "x86_64"),
    ("windows", "aarch64"),
];

/// The label every Cowork sandbox carries, so leftovers can be found.
const APP_LABEL: (&str, &str) = ("app", "cowork");

/// Why a command could not run.
#[derive(Debug, Clone)]
pub enum SandboxError {
    /// microsandbox publishes no runtime for this platform (Intel Macs, for
    /// one).
    Unsupported,
    /// The runtime, the image, or the VM could not be set up.
    Setup(String),
    /// The sandbox failed while running the command. It has been discarded.
    Failed(String),
    /// The command could not be started, but the sandbox is still running
    /// and keeps its files, as when other commands use up its process limit.
    NotStarted(String),
    /// The model's command was rejected before reaching the sandbox.
    InvalidCommand(&'static str),
    /// The command was let through without asking because the project was
    /// read-only, but it became writable before the command started, so it
    /// was not run.
    NeedsApproval,
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                formatter.write_str("sandboxes are not supported on this computer")
            }
            Self::Setup(error) => write!(formatter, "the sandbox could not be started: {error}"),
            Self::Failed(error) => write!(
                formatter,
                "the sandbox failed and was discarded, losing its files; \
                 the next command starts a fresh one: {error}"
            ),
            Self::NotStarted(error) => write!(formatter, "the command did not start: {error}"),
            Self::InvalidCommand(reason) => formatter.write_str(reason),
            Self::NeedsApproval => formatter.write_str(
                "the project became writable before the command started, so it was not run; \
                 call the tool again to ask the user to allow it",
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// A boot's outcome, which every command waiting on it gets a copy of.
type Boot = Shared<BoxFuture<'static, Result<Arc<Sandbox>, SandboxError>>>;

/// One thread's sandbox, from the moment its first command reserves it,
/// with the project it mounts.
enum Slot {
    /// Its first command started a boot; commands arriving meanwhile wait
    /// for the same one. The boot runs in its own task, so it finishes even if
    /// every command waiting for it is stopped, and then replaces this slot
    /// with [`Slot::Running`], or removes it if the boot failed. `id` tells
    /// this reservation from a later one for the same thread. `restart` is
    /// whether it replaces a sandbox whose project changed.
    Booting {
        id: u64,
        ready: Boot,
        project: Project,
        restart: bool,
    },
    /// Commands run in it at the same time, each with its own clone.
    Running {
        sandbox: Arc<Sandbox>,
        project: Project,
    },
}

impl Slot {
    fn is_boot(&self, boot: u64) -> bool {
        matches!(self, Self::Booting { id, .. } if *id == boot)
    }

    fn holds(&self, held: &Arc<Sandbox>) -> bool {
        matches!(self, Self::Running { sandbox, .. } if Arc::ptr_eq(sandbox, held))
    }
}

/// The sandboxes of this app instance, one per thread, by thread id.
pub struct Sandboxes {
    home: PathBuf,
    /// Tells this instance's sandboxes from another running instance's.
    instance: String,
    /// Set once the runtime is installed and the backend is microsandbox's
    /// default, after which every call goes to Cowork's home.
    ready: tokio::sync::OnceCell<()>,
    /// Sharded, so threads look up and change their slots without waiting
    /// on each other. Shared with boot tasks, which settle their own slot.
    /// A shard is never locked across an `await`.
    slots: Arc<DashMap<String, Slot>>,
    /// The project each thread's next command should see. A sandbox that
    /// mounts another one is replaced before that command runs.
    projects: DashMap<String, Project>,
    next_boot: AtomicU64,
}

impl Sandboxes {
    /// Sandboxes kept under `home`, which should be short: microsandbox puts
    /// Unix sockets beneath it, whose paths are limited to about 100 bytes.
    /// `instance` must be unique among running Cowork instances.
    pub fn new(home: PathBuf, instance: impl Into<String>) -> Self {
        Self {
            home,
            instance: instance.into(),
            ready: tokio::sync::OnceCell::new(),
            slots: Arc::new(DashMap::new()),
            projects: DashMap::new(),
            next_boot: AtomicU64::new(0),
        }
    }

    /// Sets the project `thread`'s commands see from its next command on.
    /// A sandbox already running with another project keeps running until
    /// then, and is replaced by a fresh one, losing the files commands made
    /// outside the project, when that command starts.
    pub fn set_project(&self, thread: &str, project: Project) {
        if project == Project::default() {
            self.projects.remove(thread);
        } else {
            self.projects.insert(thread.to_owned(), project);
        }
    }

    /// The project `thread`'s next command will see.
    pub fn project(&self, thread: &str) -> Project {
        self.projects
            .get(thread)
            .map(|project| project.clone())
            .unwrap_or_default()
    }

    /// Whether `thread` has a sandbox, running or starting, that a change
    /// to its project would replace.
    pub fn has_sandbox(&self, thread: &str) -> bool {
        self.slots.contains_key(thread)
    }

    /// Runs `command` with `/bin/sh -c` in `thread`'s sandbox, starting one
    /// if it has none. Commands of the same thread may run at the same time,
    /// sharing the sandbox's files and limits.
    pub async fn run(&self, thread: &str, command: &str) -> Result<CommandOutput, SandboxError> {
        self.run_with(thread, command, false).await
    }

    /// Like [`Sandboxes::run`], but only in a read-only project: refuses the
    /// command, without running it, when `thread`'s project is writable by
    /// the time it would start. For commands let through without asking
    /// because the project was read-only; see [`ReadOnlyPass`].
    pub async fn run_read_only(
        &self,
        thread: &str,
        command: &str,
    ) -> Result<CommandOutput, SandboxError> {
        self.run_with(thread, command, true).await
    }

    async fn run_with(
        &self,
        thread: &str,
        command: &str,
        read_only: bool,
    ) -> Result<CommandOutput, SandboxError> {
        if command.trim().is_empty() {
            return Err(SandboxError::InvalidCommand("the command is empty"));
        }
        if command.len() > settings::MAX_COMMAND_BYTES {
            return Err(SandboxError::InvalidCommand(
                "the command is longer than 8 KiB",
            ));
        }
        self.ready().await?;
        let (sandbox, restarted) = self.sandbox(thread, read_only).await?;
        let result = run_command(&sandbox, command).await.map(|mut output| {
            output.sandbox_restarted = restarted;
            output
        });
        if let Err(SandboxError::Failed(error)) = &result {
            if matches!(sandbox.status().await, Ok(SandboxStatus::Running)) {
                // Only this command failed, so the sandbox and the commands
                // running beside it are kept.
                return Err(SandboxError::NotStarted(error.clone()));
            }
            // Stopped by its idle or lifetime limit, or crashed: commands
            // running beside this one fail too. A later boot may already have
            // taken the slot, so it is left alone.
            self.slots.remove_if(thread, |_, slot| slot.holds(&sandbox));
            _ = sandbox.kill().await;
        }
        result
    }

    /// Stops every sandbox of this instance, as when Cowork quits. Boots in
    /// progress lose their slots, so they stop their sandboxes once started,
    /// without being waited for: an image pull can take minutes.
    pub async fn shutdown(&self) {
        for sandbox in self.take_running() {
            _ = sandbox.kill().await;
        }
    }

    /// Empties the map, returning the sandboxes that had started.
    fn take_running(&self) -> Vec<Arc<Sandbox>> {
        let mut running = Vec::new();
        self.slots.retain(|_, slot| {
            if let Slot::Running { sandbox, .. } = slot {
                running.push(sandbox.clone());
            }
            false
        });
        running
    }

    /// `thread`'s sandbox, once it has started, starting it if no command
    /// has yet or its project changed. Also returns whether it replaces one
    /// whose project changed.
    /// With `read_only`, fails instead when the project is writable: the
    /// project checked is the one the command runs with.
    async fn sandbox(
        &self,
        thread: &str,
        read_only: bool,
    ) -> Result<(Arc<Sandbox>, bool), SandboxError> {
        let project = self.project(thread);
        if read_only && project.writable {
            return Err(SandboxError::NeedsApproval);
        }
        let mut replaced = None;
        let (id, ready, restart) = match self.slots.entry(thread.to_owned()) {
            Entry::Occupied(mut slot) => match slot.get() {
                Slot::Running {
                    sandbox,
                    project: mounted,
                } if *mounted == project => return Ok((sandbox.clone(), false)),
                Slot::Booting {
                    id,
                    ready,
                    project: mounting,
                    restart,
                } if *mounting == project => (*id, ready.clone(), *restart),
                // The project changed since this sandbox started. A boot it
                // replaces stops its sandbox itself, once it finds its
                // reservation gone.
                _ => {
                    let id = self.next_boot.fetch_add(1, Ordering::Relaxed);
                    let ready = self.boot(thread, id, project.clone());
                    let old = slot.insert(Slot::Booting {
                        id,
                        ready: ready.clone(),
                        project,
                        restart: true,
                    });
                    if let Slot::Running { sandbox, .. } = old {
                        replaced = Some(sandbox);
                    }
                    (id, ready, true)
                }
            },
            Entry::Vacant(slot) => {
                let id = self.next_boot.fetch_add(1, Ordering::Relaxed);
                let ready = self.boot(thread, id, project.clone());
                slot.insert(Slot::Booting {
                    id,
                    ready: ready.clone(),
                    project,
                    restart: false,
                });
                (id, ready, false)
            }
        };
        if let Some(replaced) = replaced {
            // Commands still running in it fail.
            _ = replaced.kill().await;
        }
        let booted = ready.await;
        if booted.is_err() {
            // The boot task normally removes its failed reservation itself;
            // this covers it having panicked before it could.
            self.slots.remove_if(thread, |_, slot| slot.is_boot(id));
        }
        booted.map(|sandbox| (sandbox, restart))
    }

    /// Starts a task booting `thread`'s sandbox with `project` for its
    /// reservation `id`.
    fn boot(&self, thread: &str, id: u64, project: Project) -> Boot {
        let slots = self.slots.clone();
        let thread = thread.to_owned();
        // Unique per boot, as a replaced sandbox may not be gone yet.
        let name = format!("cowork-{}-{thread}-{id}", self.instance);
        let instance = self.instance.clone();
        let task = tokio::spawn(async move {
            let created = create(name, &instance, &project).await;
            let sandbox = match created {
                Ok(sandbox) => Arc::new(sandbox),
                Err(error) => {
                    slots.remove_if(&thread, |_, slot| slot.is_boot(id));
                    return Err(error);
                }
            };
            let kept = match slots.get_mut(&thread) {
                Some(mut slot) if slot.is_boot(id) => {
                    *slot = Slot::Running {
                        sandbox: sandbox.clone(),
                        project,
                    };
                    true
                }
                _ => false,
            };
            if !kept {
                // Its reservation is gone: Cowork is shutting down, or the
                // project changed while it started.
                _ = sandbox.kill().await;
                return Err(SandboxError::Setup(
                    "the sandbox was stopped while it started".into(),
                ));
            }
            Ok(sandbox)
        });
        task.map(|joined| {
            joined.unwrap_or_else(|error| {
                Err(SandboxError::Setup(format!("starting it failed: {error}")))
            })
        })
        .boxed()
        .shared()
    }

    /// Installs the runtime and selects Cowork's home, once.
    async fn ready(&self) -> Result<(), SandboxError> {
        self.ready
            .get_or_try_init(|| async {
                if !runtime_published() {
                    return Err(SandboxError::Unsupported);
                }
                // Reading `config.json` from Cowork's home, which Cowork never
                // writes, keeps the defaults of a user's own microsandbox
                // configuration from loosening these sandboxes.
                let backend = LocalBackend::builder()
                    .home(&self.home)
                    .config_path(self.home.join("config.json"))
                    .build()
                    .await
                    .map_err(setup)?;
                let options = InstallOptions::default();
                // microsandbox launches only a runtime of its own version and
                // never replaces a complete installation by itself, so one
                // left by an earlier Cowork is replaced here.
                let wanted = Version::parse(&options.version).ok();
                let installed = resolve_runtime(backend.config())
                    .ok()
                    .and_then(|runtime| resolve_runtime_version(runtime.msb_path).ok().flatten());
                if installed.is_none() || installed != wanted {
                    install_runtime(
                        backend.config(),
                        InstallOptions {
                            force: true,
                            ..options
                        },
                    )
                    .await
                    .map_err(setup)?;
                }
                // Process-wide, as parts of microsandbox look the backend up
                // rather than being handed it. Cowork has no other use for it.
                microsandbox::set_default_backend(backend);
                remove_leftovers().await;
                Ok(())
            })
            .await
            .copied()
    }
}

async fn create(name: String, instance: &str, project: &Project) -> Result<Sandbox, SandboxError> {
    use settings::*;
    project.validate()?;
    let mut builder = Sandbox::builder(name);
    for folder in &project.folders {
        builder = builder.volume(Project::guest_path(folder), |mount| {
            let mount = mount
                .bind(&folder.path)
                .owner(PROJECT_OWNER.0, PROJECT_OWNER.1)
                .stat_virtualization(StatVirtualization::Relaxed)
                .nosuid()
                .nodev();
            if project.writable {
                mount
            } else {
                mount.readonly()
            }
        });
    }
    if project.folders.is_empty() {
        builder = builder.volume(PROJECTS_DIR, |mount| mount.tmpfs().size(1).readonly());
    }
    builder
        .label(APP_LABEL.0, APP_LABEL.1)
        .label("instance", instance)
        .image(IMAGE)
        .root_disk_with(|disk| disk.tmpfs().size(ROOT_DISK_MIB))
        .pull_policy(PullPolicy::IfMissing)
        .cpus(CPUS)
        .max_cpus(CPUS)
        .memory(MEMORY_MIB)
        .max_memory(MEMORY_MIB)
        .user(USER)
        .workdir(WORKDIR)
        .hostname(HOSTNAME)
        .security(SecurityProfile::Restricted)
        .deployment_profile(DeploymentProfile::MultiTenant)
        .disable_network()
        .idle_timeout(IDLE_TIMEOUT.as_secs())
        .max_duration(MAX_DURATION.as_secs())
        .ephemeral(true)
        .detached(false)
        .quiet_logs()
        .disable_metrics_sample()
        .create()
        .await
        .map_err(setup)
}

impl Drop for Sandboxes {
    fn drop(&mut self) {
        // microsandbox stops the VMs when Cowork exits; this covers dropping
        // the manager earlier, as tests do.
        let running = self.take_running();
        if running.is_empty() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for sandbox in running {
                    _ = sandbox.kill().await;
                }
            });
        }
    }
}

/// Whether microsandbox publishes a runtime for this platform.
fn runtime_published() -> bool {
    RUNTIME_PLATFORMS.contains(&(std::env::consts::OS, std::env::consts::ARCH))
}

fn setup(error: microsandbox::MicrosandboxError) -> SandboxError {
    SandboxError::Setup(error.to_string())
}

/// Removes stopped Cowork sandboxes a crashed instance left behind. Running
/// ones belong to a live instance (microsandbox stops a VM whose parent
/// died), so they are left alone.
async fn remove_leftovers() {
    let mut cursor = None;
    loop {
        let page = Sandbox::list_with(|list| {
            let list = list
                .limit(microsandbox::sandbox::MAX_SANDBOX_LIST_LIMIT)
                .label(APP_LABEL.0, APP_LABEL.1);
            match cursor.take() {
                Some(cursor) => list.cursor(cursor),
                None => list,
            }
        })
        .await;
        let Ok(page) = page else {
            return;
        };
        for handle in page.sandboxes {
            if matches!(
                handle.status_snapshot(),
                SandboxStatus::Stopped | SandboxStatus::Crashed | SandboxStatus::Created
            ) {
                _ = handle.remove().await;
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return,
        }
    }
}

/// Runs `command` in `sandbox`, keeping its output up to the limit and
/// killing it past the time or output limit.
async fn run_command(sandbox: &Sandbox, command: &str) -> Result<CommandOutput, SandboxError> {
    use settings::rlimits;
    let mut handle = sandbox
        .shell_stream_with(command, |exec| {
            exec.cwd(settings::WORKDIR)
                .stdin_null()
                .tty(false)
                .rlimit(RlimitResource::Cpu, rlimits::CPU_SECONDS)
                .rlimit(RlimitResource::Nproc, rlimits::PROCESSES)
                .rlimit(RlimitResource::Nofile, rlimits::OPEN_FILES)
                .rlimit(RlimitResource::Fsize, rlimits::FILE_SIZE)
                .rlimit(RlimitResource::Core, rlimits::CORE_SIZE)
        })
        .await
        .map_err(|error| SandboxError::Failed(error.to_string()))?;
    // Kills the command if the run is stopped while it executes, as dropping
    // the handle alone would leave it running.
    let mut guard = KillOnDrop(Some(handle.control()));
    let mut stdout = CappedStream::new(settings::OUTPUT_LIMIT);
    let mut stderr = CappedStream::new(settings::OUTPUT_LIMIT);
    let mut killed = None;
    let mut exit_code = None;
    // Until the command is killed, its time limit; then how long it has to
    // report its exit.
    let deadline = tokio::time::sleep(settings::COMMAND_TIMEOUT);
    tokio::pin!(deadline);
    let mut in_grace = false;
    loop {
        let kill_for = tokio::select! {
            event = handle.recv() => match event {
                Some(ExecEvent::Stdout(chunk)) => {
                    stdout.push(&chunk).then_some(Killed::OutputLimit)
                }
                Some(ExecEvent::Stderr(chunk)) => {
                    stderr.push(&chunk).then_some(Killed::OutputLimit)
                }
                Some(ExecEvent::Exited { code }) => {
                    exit_code = Some(code);
                    break;
                }
                Some(ExecEvent::Failed(failure)) => {
                    guard.0 = None;
                    return Err(SandboxError::Failed(format!(
                        "the shell did not start: {failure:?}"
                    )));
                }
                Some(ExecEvent::Started { .. } | ExecEvent::StdinError(_)) => None,
                None => break,
            },
            () = &mut deadline => {
                if in_grace {
                    break;
                }
                Some(Killed::TimedOut)
            }
        };
        if let Some(reason) = kill_for
            && !in_grace
        {
            killed = Some(reason);
            in_grace = true;
            _ = handle.kill().await;
            deadline
                .as_mut()
                .reset(tokio::time::Instant::now() + settings::KILL_GRACE);
        }
    }
    if exit_code.is_some() {
        guard.0 = None;
    }
    Ok(CommandOutput::new(exit_code, stdout, stderr, killed))
}

// Replace with `DropGuard` after Rust 1.100
struct KillOnDrop(Option<ExecControl>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move { _ = control.kill().await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bad_commands_are_rejected_before_anything_starts() {
        let sandboxes = Sandboxes::new(PathBuf::from("/nonexistent"), "test");
        assert!(matches!(
            sandboxes.run("thread", "  ").await,
            Err(SandboxError::InvalidCommand(_))
        ));
        let long = "x".repeat(settings::MAX_COMMAND_BYTES + 1);
        assert!(matches!(
            sandboxes.run("thread", &long).await,
            Err(SandboxError::InvalidCommand(_))
        ));
        assert!(sandboxes.ready.get().is_none(), "nothing was set up");
    }
}
