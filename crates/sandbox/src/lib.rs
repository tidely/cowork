//! Running the agent's shell commands on the host, contained by the
//! operating system's own sandbox, with mxc-sdk.
//!
//! Every setting is in [`settings`]. In short: the user's own shell, tools,
//! and environment; the thread's project folders at their real paths,
//! read-only unless the project is writable (see [`Project`]); a scratch
//! folder per thread; global reads except denied credentials; no network unless the command's
//! [`Access`] is `Network`. A command the host granted more ([`grant`]) may
//! also have more access to the project, and other host folders.
//!
//! Each command is a sandbox of its own, made from the thread's project as
//! it is when the command starts, so a thread's commands may run at the same
//! time and a project change needs no restart. Nothing runs before the first
//! command: the login shell environment is captured then, and the scratch
//! folders are created as threads first need them.
//!
//! The file tools ([`files`]) reach the same folders from the host process
//! itself, with every path they accept mapped in [`paths`].

pub mod environment;
pub mod files;
pub mod grant;

mod output;
pub mod paths;
mod permissions;
mod project;
pub mod settings;
mod tool;

use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use mxc_sdk::v1::{
    self, ContainerRequest, FilesystemPolicy, MxcProcess, NetworkAction, NetworkEgressPolicy,
    NetworkIngressPolicy, NetworkPolicy, StreamCloser,
};

pub use environment::{EnvironmentSource, PRINT_ENV_FLAG, print_env};
pub use files::{ListDirectory, ReadFile, WriteFile};
pub use grant::{Folders, Grant};
pub use output::CommandOutput;
pub use paths::FileError;
pub use permissions::{RequestPermissions, RequestPermissionsArgs};
pub use project::{Access, Project, ProjectFolder, folder_name};
pub use tool::{RunCommand, RunCommandArgs};

/// How the environment note sent to the model starts; see
/// [`Sandboxes::environment_note`].
pub const ENVIRONMENT_NOTE_START: &str = "<environment>";

/// How often a running command is checked for having exited or been
/// stopped. mxc-sdk's processes are waited on synchronously and cannot be
/// killed while a wait blocks, so the wait is a poll.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long a command's output may stay open once the command and what it
/// left in the background have been ended, before reading it stops.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);

/// Why a command could not run.
#[derive(Debug, Clone)]
pub enum SandboxError {
    /// This computer has no sandbox mxc-sdk can use, such as Linux without
    /// `bwrap`, with why.
    Unsupported(String),
    /// The scratch folders could not be set up.
    Setup(String),
    /// The command could not be started or waited on.
    Failed(String),
    /// The model's command was rejected before anything ran.
    InvalidCommand(String),
    /// The command exceeded its requested runtime. Its sandbox was ended;
    /// output from before termination is kept for the model to diagnose it.
    Timeout {
        timeout: Duration,
        output: CommandOutput,
    },
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(
                formatter,
                "commands cannot run on this computer, as it has no supported sandbox: {reason}"
            ),
            Self::Setup(error) => write!(formatter, "the sandbox could not be set up: {error}"),
            Self::Failed(error) => write!(formatter, "the command failed to run: {error}"),
            Self::InvalidCommand(reason) => formatter.write_str(reason),
            Self::Timeout { timeout, output } => write!(
                formatter,
                "the command timed out after {} ms\nstdout:\n{}\nstderr:\n{}",
                timeout.as_millis(),
                output.stdout,
                output.stderr
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// The commands of this app instance, with each thread's project and scratch
/// folder, by thread id.
pub struct Sandboxes {
    /// Every instance's scratch folders: `<root>/<instance>/<thread>`, and
    /// the lock `<root>/<instance>.lock` that a running instance holds.
    root: PathBuf,
    instance: String,
    source: EnvironmentSource,
    /// Set by the first command; see [`Sandboxes::ready`].
    ready: OnceLock<Result<(), SandboxError>>,
    /// Set by the first command or file write; see [`Sandboxes::claim`].
    claimed: OnceLock<Result<(), SandboxError>>,
    /// Held while this instance runs, so another instance does not take its
    /// scratch folders for leftovers.
    lock: Mutex<Option<File>>,
    environment: tokio::sync::OnceCell<Arc<HashMap<String, String>>>,
    /// The project each thread's next command sees. Never locked across an
    /// `await`.
    projects: Mutex<HashMap<String, Project>>,
}

impl Sandboxes {
    /// Commands whose scratch folders are kept under `root`, with their
    /// environment from `source`. `instance` must be unique among running
    /// Cowork instances.
    pub fn new(root: PathBuf, instance: impl Into<String>, source: EnvironmentSource) -> Self {
        Self {
            root,
            instance: instance.into(),
            source,
            ready: OnceLock::new(),
            claimed: OnceLock::new(),
            lock: Mutex::new(None),
            environment: tokio::sync::OnceCell::new(),
            projects: Mutex::new(HashMap::new()),
        }
    }

    /// Sets the project `thread`'s commands see from its next command on.
    /// Commands already running keep the project they started with.
    pub fn set_project(&self, thread: &str, project: Project) {
        let mut projects = lock(&self.projects);
        if project == Project::default() {
            projects.remove(thread);
        } else {
            projects.insert(thread.to_owned(), project);
        }
    }

    /// The project `thread`'s next command will see.
    pub fn project(&self, thread: &str) -> Project {
        lock(&self.projects)
            .get(thread)
            .cloned()
            .unwrap_or_default()
    }

    /// What the model is told about where `thread`'s commands run: the
    /// project's folders at the paths commands see them, whether commands may
    /// change them, where they start, and the scratch folder. Starts with
    /// [`ENVIRONMENT_NOTE_START`]. The same project always gives the same
    /// note, so it only needs sending when it changed.
    pub fn environment_note(&self, thread: &str) -> String {
        let project = self.project(thread);
        let scratch = self.scratch_dir(thread);
        let folders = resolved_folders(&project);
        let mut note = format!("{ENVIRONMENT_NOTE_START}\n");
        if folders.is_empty() {
            note.push_str("The user has added no project folders.\n");
        } else {
            note.push_str("The user's project folders, by name and path:\n");
            for (name, path) in &folders {
                note.push_str(&format!("- {name}: {}\n", path.display()));
            }
        }
        note.push_str(match project.access {
            Access::Read => {
                "The project's auto-approval level is read: commands can read the host filesystem but not \
                 change them, and have no network access. They run without asking the user.\n"
            }
            Access::Write => {
                "The project's auto-approval level is write: commands can change the project folders, and \
                 their changes apply to the user's computer at once. They have no network \
                 access. Commands within this level run without asking the user.\n"
            }
            Access::Network => {
                "The project's auto-approval level is network: commands can change the project folders, and \
                 their changes apply to the user's computer at once, and they can reach the \
                 network. Commands within this level run without asking the user.\n"
            }
        });
        note.push_str("Reads span the host filesystem except denied credentials. Explicit command access may be narrower than this level. Higher access or extra writable folders require approval unless already granted for this run. Approving a file write in read mode permits that call only.\n");
        note.push_str(&format!(
            "Commands start in {}. Their TMPDIR is the scratch folder {}, which the file \
             tools call `{}`. Their HOME is the user's home folder{}, so `~` means the same \
             in commands as in `read_folders` and `write_folders`.\n\
             This replaces any earlier environment note.\n</environment>",
            working_directory(&folders, &scratch).display(),
            scratch.display(),
            paths::SCRATCH,
            std::env::home_dir()
                .map(|home| format!(" {}", home.display()))
                .unwrap_or_default(),
        ));
        note
    }

    /// The places `thread`'s file tools reach now; see [`paths`].
    pub(crate) fn places(&self, thread: &str) -> paths::Places {
        paths::Places::new(
            &self.project(thread),
            &self.scratch_dir(thread),
            std::env::home_dir().as_deref(),
        )
    }

    /// Runs `command` with the platform's shell, contained by `thread`'s
    /// project. Commands of the same thread may run at the same time, sharing
    /// its scratch folder. Dropping the future kills the command and whatever
    /// it started.
    pub async fn run(&self, thread: &str, command: &str) -> Result<CommandOutput, SandboxError> {
        self.run_with(thread, command, None, None).await
    }

    /// Like [`Sandboxes::run`], with `grant` in place of the project's mode
    /// when given, whatever the mode is by the time the command starts: what
    /// someone allowed, or what a command needing no approval was let through
    /// with. `timeout` limits runtime from process launch, excluding approval,
    /// environment loading, and sandbox setup. Omit it for no time limit.
    pub async fn run_with(
        &self,
        thread: &str,
        command: &str,
        grant: Option<Grant>,
        timeout: Option<Duration>,
    ) -> Result<CommandOutput, SandboxError> {
        if command.trim().is_empty() {
            return Err(SandboxError::InvalidCommand("the command is empty".into()));
        }
        self.ready()?;
        let environment = self
            .environment
            .get_or_init(|| async { Arc::new(environment::load(self.source).await) })
            .await
            .clone();
        let scratch = self.scratch_dir(thread);
        std::fs::create_dir_all(&scratch).map_err(|error| {
            SandboxError::Setup(format!(
                "could not create the scratch folder {}: {error}",
                scratch.display()
            ))
        })?;
        let mut project = self.project(thread);
        let folders = match grant {
            Some(grant) => {
                project.access = grant.access;
                grant.folders
            }
            None => Folders::default(),
        };
        let request = request(command, &project, &folders, &scratch, &environment)?;
        let stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
        let stopped = stop.0.clone();
        let result = tokio::task::spawn_blocking(move || execute(request, &stopped, timeout)).await;
        drop(stop);
        result.unwrap_or_else(|error| Err(SandboxError::Failed(error.to_string())))
    }

    /// Forgets `thread`'s project and removes its scratch folder, as when the
    /// thread is deleted. Its commands end with its run.
    pub async fn remove(&self, thread: &str) {
        lock(&self.projects).remove(thread);
        let scratch = self.scratch_dir(thread);
        _ = tokio::task::spawn_blocking(move || remove_dir(&scratch)).await;
    }

    /// Removes this instance's scratch folders, as when Cowork quits.
    pub fn shutdown(&self) {
        if lock(&self.lock).is_none() {
            // Never started, or already shut down: nothing of this instance's
            // is there, and another instance with the same name, as in tests,
            // keeps its folders.
            return;
        }
        remove_dir(&self.root.join(&self.instance));
        if lock(&self.lock).take().is_some() {
            _ = std::fs::remove_file(self.lock_path(&self.instance));
        }
    }

    fn scratch_dir(&self, thread: &str) -> PathBuf {
        self.root.join(&self.instance).join(thread)
    }

    fn lock_path(&self, instance: &str) -> PathBuf {
        self.root.join(format!("{instance}.lock"))
    }

    /// Checks, once, that this computer has a sandbox, and claims the
    /// scratch folders.
    fn ready(&self) -> Result<(), SandboxError> {
        self.ready
            .get_or_init(|| {
                let support = v1::platform_support();
                if !support.is_supported {
                    return Err(SandboxError::Unsupported(
                        support
                            .reason
                            .unwrap_or_else(|| "mxc-sdk supports no sandbox here".into()),
                    ));
                }
                self.claim()
            })
            .clone()
    }

    /// Takes, once, this instance's lock, so its scratch folders are
    /// removed when it quits rather than taken for leftovers, and removes
    /// the scratch folders of instances that did not quit cleanly. Needed
    /// before anything creates a scratch folder.
    pub(crate) fn claim(&self) -> Result<(), SandboxError> {
        self.claimed
            .get_or_init(|| {
                let setup = |error: std::io::Error| SandboxError::Setup(error.to_string());
                std::fs::create_dir_all(&self.root).map_err(setup)?;
                // Locked before the folder exists, so a folder without a lock
                // file is always a leftover.
                let held = File::create(self.lock_path(&self.instance)).map_err(setup)?;
                held.try_lock().map_err(|error| {
                    SandboxError::Setup(format!("the scratch lock is taken: {error}"))
                })?;
                *lock(&self.lock) = Some(held);
                self.remove_leftovers();
                Ok(())
            })
            .clone()
    }

    fn remove_leftovers(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let instance = name.strip_suffix(".lock").unwrap_or(name);
            if instance == self.instance {
                continue;
            }
            let lock_path = self.lock_path(instance);
            let abandoned = match File::open(&lock_path) {
                // Unlocked on drop, before the lock file is removed.
                Ok(file) => file.try_lock().is_ok(),
                Err(_) => true,
            };
            if abandoned {
                remove_dir(&self.root.join(instance));
                _ = std::fs::remove_file(lock_path);
            }
        }
    }
}

impl Drop for Sandboxes {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn remove_dir(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("could not remove {}: {error}", path.display());
    }
}

/// Tells a command's blocking task, when its run is stopped, to kill it.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// `path` as the sandbox matches it: Seatbelt compares fully resolved paths,
/// and Bubblewrap mounts a folder at the path it is given. `None` when it
/// does not exist.
fn resolved(path: &Path) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).ok()?;
    // Windows' verbatim `\\?\C:\` prefix is not a path its sandbox matches.
    #[cfg(windows)]
    let path = path
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .map(PathBuf::from)
        .unwrap_or(path);
    Some(path)
}

/// The project's folders that exist, by name and resolved path.
fn resolved_folders(project: &Project) -> Vec<(String, PathBuf)> {
    project
        .folders
        .iter()
        .filter_map(|folder| Some((folder.name.clone(), resolved(&folder.path)?)))
        .collect()
}

/// Where commands start: the project's first folder, or the scratch folder
/// without one.
fn working_directory(folders: &[(String, PathBuf)], scratch: &Path) -> PathBuf {
    folders
        .first()
        .map(|(_, path)| path.clone())
        .unwrap_or_else(|| resolved(scratch).unwrap_or_else(|| scratch.to_owned()))
}

/// What `command` may do, from [`settings`], with `folders` outside the
/// project granted to it.
fn request(
    command: &str,
    project: &Project,
    folders: &Folders,
    scratch: &Path,
    environment: &HashMap<String, String>,
) -> Result<ContainerRequest, SandboxError> {
    let home = std::env::home_dir();
    let scratch = resolved(scratch).unwrap_or_else(|| scratch.to_owned());
    let mut environment = environment.clone();
    // The user's real home, so that `~` in a command is the folder `~` names
    // in a grant; see `settings`.
    if let Some(home) = &home {
        environment.insert("HOME".to_owned(), home.to_string_lossy().into_owned());
    }
    let scratch_text = scratch.to_string_lossy().into_owned();
    for variable in ["TMPDIR", "TMP", "TEMP"] {
        environment.insert(variable.to_owned(), scratch_text.clone());
    }
    let environment = environment.into_iter().collect::<Vec<_>>();

    let granted = folders;
    let folders = resolved_folders(project);
    let mut readwrite = vec![scratch.clone()];
    #[cfg(unix)]
    readwrite.extend(settings::WRITABLE_DEVICES.iter().map(PathBuf::from));
    let mut readonly = Vec::new();
    for (_, path) in &folders {
        if project.writable() {
            readwrite.push(path.clone());
        } else {
            readonly.push(path.clone());
        }
    }
    readwrite.extend(granted.write.iter().cloned());
    readonly.extend(granted.read.iter().cloned());
    readonly.extend(system_read_paths()?);
    // A redundant explicit root read grant must not hide Linux's private devices.
    #[cfg(target_os = "linux")]
    readonly.retain(|path| path != Path::new("/"));
    readonly.sort();
    readonly.dedup();
    // The sandbox lets read-only win over read-write for the same path, so a
    // writable project folder that is also a tool folder stays writable.
    readonly.retain(|path| !readwrite.contains(path));
    let denied = home
        .iter()
        .flat_map(|home| settings::DENIED_PATHS.iter().map(|path| home.join(path)))
        .filter_map(|path| resolved(&path))
        .collect::<Vec<_>>();

    let text = |paths: Vec<PathBuf>| {
        paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    Ok(ContainerRequest {
        filesystem: Some(FilesystemPolicy {
            readwrite_paths: text(readwrite),
            readonly_paths: text(readonly),
            denied_paths: text(denied),
            ..Default::default()
        }),
        network: Some(NetworkPolicy {
            egress: Some(NetworkEgressPolicy {
                default: Some(if project.access.networked() {
                    NetworkAction::Allow
                } else {
                    NetworkAction::Deny
                }),
                ..Default::default()
            }),
            // Never inbound, nor the host's own loopback services, which
            // Bubblewrap cannot open in any case; see `settings`.
            ingress: Some(NetworkIngressPolicy {
                default: Some(NetworkAction::Deny),
                host_loopback: Some(NetworkAction::Deny),
            }),
            runtime_config: None,
        }),
        working_directory: Some(
            working_directory(&folders, &scratch)
                .to_string_lossy()
                .into_owned(),
        ),
        environment: Some(environment),
        ui: Some(v1::UiPolicy {
            disable: false,
            ..Default::default()
        }),
        ..ContainerRequest::new(command)
    })
}

/// Read the host filesystem; see [`settings`]. Linux mounts the root's
/// entries rather than `/` to preserve Bubblewrap's private `/dev` and `/proc`.
fn system_read_paths() -> Result<Vec<PathBuf>, SandboxError> {
    #[cfg(windows)]
    {
        // SAFETY: takes no arguments and only returns a bit mask.
        let drives = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        let mut paths = Vec::new();
        for letter in (0..26u8).filter(|index| drives & (1 << index) != 0) {
            let root = PathBuf::from(format!("{}:\\", char::from(b'A' + letter)));
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            paths.extend(entries.flatten().map(|entry| entry.path()));
            paths.push(root);
        }
        Ok(paths)
    }
    #[cfg(target_os = "linux")]
    {
        let paths = std::fs::read_dir("/")
            .and_then(|entries| {
                entries
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()
            })
            .map_err(|error| {
                SandboxError::Setup(format!("could not list the host filesystem root: {error}"))
            })?;
        Ok(paths
            .into_iter()
            .filter(|path| {
                if settings::PRIVATE_FILESYSTEMS
                    .iter()
                    .any(|private| path == Path::new(private))
                {
                    return false;
                }
                // The SDK rejects explicit grants the host user cannot read
                // (e.g. /lost+found). Such entries were inaccessible under `/`
                // too; omit them rather than failing every command.
                let Ok(path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
                    return false;
                };
                // SAFETY: `path` is a live NUL-terminated string; access only
                // checks the invoking user's permissions and retains no pointer.
                unsafe { libc::access(path.as_ptr(), libc::R_OK) == 0 }
            })
            .collect())
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    Ok(vec![PathBuf::from("/")])
}

/// Runs `request` to its end, until `stopped`, or until its runtime expires.
fn execute(
    request: ContainerRequest,
    stopped: &AtomicBool,
    timeout: Option<Duration>,
) -> Result<CommandOutput, SandboxError> {
    let mut process = v1::spawn(request, Default::default())
        .map_err(|error| SandboxError::Failed(error.to_string()))?;
    let started = Instant::now();
    let stdout = Output::read(&mut process, Stream::Stdout);
    let stderr = Output::read(&mut process, Stream::Stderr);
    let exit_code = loop {
        if stopped.load(Ordering::Acquire) {
            _ = process.kill();
            return Err(SandboxError::Failed("the command was stopped".into()));
        }
        match process.try_wait() {
            Ok(Some(code)) => break Ok(code),
            Ok(None) => {
                if let Some(timeout) = timeout {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        break Err(timeout);
                    }
                    std::thread::sleep(POLL_INTERVAL.min(remaining));
                } else {
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
            Err(error) => {
                _ = process.kill();
                return Err(SandboxError::Failed(error.to_string()));
            }
        }
    };
    // Ends the command if it timed out, and anything left in the background
    // in either case. Those processes may also hold its output open.
    _ = process.kill();
    drop(process);
    let output = CommandOutput::new(
        exit_code.as_ref().copied().unwrap_or(-1),
        &stdout.collect(),
        &stderr.collect(),
    );
    match exit_code {
        Ok(_) => Ok(output),
        Err(timeout) => Err(SandboxError::Timeout { timeout, output }),
    }
}

enum Stream {
    Stdout,
    Stderr,
}

/// One of a command's output streams, read on a thread of its own so that
/// neither can fill up and block the command.
struct Output {
    read: mpsc::Receiver<Vec<u8>>,
    closer: Option<StreamCloser>,
}

impl Output {
    fn read(process: &mut MxcProcess, stream: Stream) -> Self {
        let (stream, closer) = match stream {
            Stream::Stdout => (process.take_stdout(), process.stdout_closer()),
            Stream::Stderr => (process.take_stderr(), process.stderr_closer()),
        };
        let (sender, read) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                // What was read before an error is kept.
                _ = stream.read_to_end(&mut bytes);
            }
            _ = sender.send(bytes);
        });
        Self { read, closer }
    }

    /// Everything the stream carried, once it closed, or what it had carried
    /// after [`OUTPUT_GRACE`].
    fn collect(self) -> Vec<u8> {
        self.read.recv_timeout(OUTPUT_GRACE).unwrap_or_else(|_| {
            if let Some(closer) = &self.closer {
                closer.close();
            }
            self.read.recv().unwrap_or_default()
        })
    }
}

/// Sandboxes under a fresh temporary root, with this process's environment,
/// and the root's guard.
#[cfg(test)]
pub(crate) fn test_sandboxes() -> (Arc<Sandboxes>, TestRoot) {
    let root = TestRoot::new();
    let sandboxes = Sandboxes::new(root.0.clone(), "test", EnvironmentSource::Process);
    (Arc::new(sandboxes), root)
}

/// A temporary folder removed on drop.
#[cfg(test)]
pub(crate) struct TestRoot(pub(crate) PathBuf);

#[cfg(test)]
impl TestRoot {
    pub(crate) fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "cowork-sandbox-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

#[cfg(test)]
impl Drop for TestRoot {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise native denied-path enforcement with synthetic content even on
    /// hosts without credential folders. Metadata visibility is backend-specific;
    /// content reads, directory entries, and writes must still be protected.
    #[cfg(unix)]
    #[test]
    fn denied_paths_protect_contents_inside_a_writable_project() {
        if !v1::platform_support().is_supported {
            return;
        }
        let root = TestRoot::new();
        let root_path = std::fs::canonicalize(&root.0).unwrap();
        let denied = root_path.join("denied");
        let scratch = root_path.join("scratch");
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let secret = denied.join("synthetic.txt");
        std::fs::write(&secret, "synthetic protected content\n").unwrap();
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let command = format!(
            "/bin/cat {} >/dev/null 2>/dev/null && echo read-allowed || echo read-denied; \
             /bin/ls -A {} >\"$TMPDIR/listing\" 2>/dev/null; \
             test ! -s \"$TMPDIR/listing\" && echo listing-hidden; \
             {{ printf changed >> {}; }} 2>/dev/null && echo write-allowed || echo write-denied",
            quote(&secret),
            quote(&denied),
            quote(&secret),
        );
        let project = Project {
            folders: vec![ProjectFolder {
                name: "project".into(),
                path: root_path,
            }],
            access: Access::Write,
        };
        let mut request = request(
            &command,
            &project,
            &Folders::default(),
            &scratch,
            &HashMap::new(),
        )
        .unwrap();
        request
            .filesystem
            .as_mut()
            .unwrap()
            .denied_paths
            .push(denied.to_string_lossy().into_owned());
        let output = execute(request, &AtomicBool::new(false), None).unwrap();
        assert_eq!(output.exit_code, 0, "{output:?}");
        assert_eq!(
            output.stdout, "read-denied\nlisting-hidden\nwrite-denied\n",
            "{output:?}"
        );
        assert_eq!(
            std::fs::read_to_string(secret).unwrap(),
            "synthetic protected content\n"
        );
    }

    fn folder(root: &Path, name: &str) -> ProjectFolder {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        ProjectFolder {
            name: name.to_owned(),
            path,
        }
    }

    #[tokio::test]
    async fn empty_commands_are_rejected_before_anything_starts() {
        let (sandboxes, _root) = test_sandboxes();
        assert!(matches!(
            sandboxes.run("thread", "  ").await,
            Err(SandboxError::InvalidCommand(_))
        ));
        assert!(sandboxes.ready.get().is_none(), "nothing was set up");
    }

    #[tokio::test]
    async fn removing_a_thread_forgets_its_project_and_scratch() {
        let (sandboxes, _root) = test_sandboxes();
        let writable = Project {
            folders: Vec::new(),
            access: Access::Write,
        };
        sandboxes.set_project("thread", writable.clone());
        sandboxes.set_project("other", writable.clone());
        let scratch = sandboxes.scratch_dir("thread");
        std::fs::create_dir_all(&scratch).unwrap();

        sandboxes.remove("thread").await;

        assert_eq!(sandboxes.project("thread"), Project::default());
        assert_eq!(sandboxes.project("other"), writable);
        assert!(!scratch.exists());
    }

    #[test]
    fn the_note_names_the_folders_mode_and_scratch() {
        let (sandboxes, root) = test_sandboxes();
        let empty = sandboxes.environment_note("thread");
        assert!(empty.starts_with(ENVIRONMENT_NOTE_START), "{empty}");
        assert!(empty.contains("no project folders"), "{empty}");
        let home = std::env::home_dir().unwrap();
        assert!(
            empty.contains(&format!(
                "HOME is the user's home folder {}",
                home.display()
            )),
            "{empty}"
        );

        let zed = folder(&root.0, "zed");
        let resolved_zed = resolved(&zed.path).unwrap();
        sandboxes.set_project(
            "thread",
            Project {
                folders: vec![zed],
                access: Access::Read,
            },
        );
        let read_only = sandboxes.environment_note("thread");
        assert!(
            read_only.contains(&format!("- zed: {}", resolved_zed.display())),
            "{read_only}"
        );
        assert!(
            read_only.contains("auto-approval level is read"),
            "{read_only}"
        );
        assert!(
            read_only.contains(&format!("Commands start in {}.", resolved_zed.display())),
            "{read_only}"
        );
        assert_eq!(read_only, sandboxes.environment_note("thread"), "stable");
    }

    #[cfg(windows)]
    #[test]
    fn windows_commands_read_every_drive_but_write_only_the_project() {
        let root = TestRoot::new();
        let zed = folder(&root.0, "zed");
        let scratch = root.0.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let project = Project {
            folders: vec![zed.clone()],
            access: Access::Write,
        };
        let request = request(
            "dir",
            &project,
            &Folders::default(),
            &scratch,
            &HashMap::new(),
        )
        .unwrap();
        let filesystem = request.filesystem.unwrap();
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        for path in [format!("{system}\\"), format!("{system}\\Users")] {
            assert!(filesystem.readonly_paths.contains(&path), "{path}");
            assert!(!filesystem.readwrite_paths.contains(&path), "{path}");
        }
        let zed_path = resolved(&zed.path).unwrap().to_string_lossy().into_owned();
        assert!(filesystem.readwrite_paths.contains(&zed_path));
        assert!(!filesystem.readonly_paths.contains(&zed_path));
    }

    /// Git resolves its folder through every folder above it, which is why
    /// Windows commands read every drive; see `settings`.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn git_works_in_a_windows_project() {
        if !v1::platform_support().is_supported {
            return;
        }
        let git = |arguments: &[&str], folder: &Path| {
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(folder)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        let root = TestRoot::new();
        let repo = folder(&root.0, "repo");
        std::fs::write(repo.path.join("notes.txt"), "one\n").unwrap();
        if !git(&["init", "-q"], &repo.path) {
            return;
        }
        let sandboxes = Sandboxes::new(root.0.join("scratch"), "test", EnvironmentSource::Process);
        sandboxes.set_project(
            "thread",
            Project {
                folders: vec![repo.clone()],
                access: Access::Write,
            },
        );
        let status = sandboxes
            .run("thread", "git --no-optional-locks status --short")
            .await
            .unwrap();
        assert_eq!(status.exit_code, 0, "{status:?}");
        assert!(status.stdout.contains("notes.txt"), "{status:?}");
        let added = sandboxes.run("thread", "git add notes.txt").await.unwrap();
        assert_eq!(added.exit_code, 0, "{added:?}");

        let ssh = std::env::home_dir().unwrap().join(".ssh");
        if let Some(key) = std::fs::read_dir(&ssh)
            .ok()
            .and_then(|mut entries| entries.find_map(|entry| Some(entry.ok()?.path())))
            .filter(|path| path.is_file())
        {
            let read = sandboxes
                .run("thread", &format!("cmd /c type \"{}\"", key.display()))
                .await
                .unwrap();
            assert_ne!(read.exit_code, 0, "credentials stay denied: {read:?}");
        }
        sandboxes.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_requests_preserve_private_filesystems() {
        let root = TestRoot::new();
        let scratch = root.0.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let folders = Folders {
            read: vec![PathBuf::from("/")],
            ..Default::default()
        };
        let request = request(
            "true",
            &Project::default(),
            &folders,
            &scratch,
            &HashMap::new(),
        )
        .unwrap();
        let filesystem = request.filesystem.unwrap();
        for path in ["/", "/dev", "/proc", "/dev/null"] {
            assert!(!filesystem.readonly_paths.iter().any(|grant| grant == path));
            assert!(!filesystem.readwrite_paths.iter().any(|grant| grant == path));
        }
        assert!(filesystem.readonly_paths.iter().any(|path| path == "/home"));
        if let Some(home) = std::env::home_dir() {
            for credential in settings::DENIED_PATHS {
                if let Some(path) = resolved(&home.join(credential)) {
                    assert!(
                        filesystem
                            .denied_paths
                            .contains(&path.to_string_lossy().into_owned())
                    );
                }
            }
        }
    }

    #[test]
    fn requests_grant_the_project_and_the_network_by_mode() {
        let root = TestRoot::new();
        let zed = folder(&root.0, "zed");
        let scratch = root.0.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let environment = HashMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), "/somewhere/else".to_owned()),
        ]);
        let project = |access| Project {
            folders: vec![zed.clone()],
            access,
        };
        let zed_path = resolved(&zed.path).unwrap().to_string_lossy().into_owned();
        let scratch_path = resolved(&scratch).unwrap().to_string_lossy().into_owned();

        let none = Folders::default();
        let read_only =
            request("ls", &project(Access::Read), &none, &scratch, &environment).unwrap();
        let filesystem = read_only.filesystem.clone().unwrap();
        assert!(filesystem.readonly_paths.contains(&zed_path));
        let writable_paths = vec![scratch_path.clone()];
        #[cfg(unix)]
        let writable_paths = {
            let mut writable_paths = writable_paths;
            writable_paths.extend(
                settings::WRITABLE_DEVICES
                    .iter()
                    .map(|path| (*path).to_owned()),
            );
            writable_paths
        };
        assert_eq!(filesystem.readwrite_paths, writable_paths);
        for path in system_read_paths().unwrap() {
            assert!(
                filesystem
                    .readonly_paths
                    .contains(&path.to_string_lossy().into_owned())
            );
        }
        assert_eq!(read_only.working_directory.as_deref(), Some(&*zed_path));
        let environment_of = |request: &ContainerRequest, name: &str| {
            request
                .environment
                .as_ref()
                .unwrap()
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            environment_of(&read_only, "HOME"),
            std::env::home_dir().map(|home| home.to_string_lossy().into_owned()),
            "the user's real home, as grants read `~`"
        );
        assert_eq!(
            environment_of(&read_only, "TMPDIR"),
            Some(scratch_path.clone())
        );
        assert_eq!(
            environment_of(&read_only, "PATH").as_deref(),
            Some("/usr/bin:/bin")
        );
        let network = read_only.network.unwrap();
        assert_eq!(network.egress.unwrap().default, Some(NetworkAction::Deny));
        assert_eq!(network.ingress.unwrap().default, Some(NetworkAction::Deny));

        let writable =
            request("ls", &project(Access::Write), &none, &scratch, &environment).unwrap();
        let network = writable.network.clone().unwrap();
        assert_eq!(network.egress.unwrap().default, Some(NetworkAction::Deny));
        let filesystem = writable.filesystem.unwrap();
        assert!(filesystem.readwrite_paths.contains(&zed_path));

        let networked = request(
            "ls",
            &project(Access::Network),
            &none,
            &scratch,
            &environment,
        )
        .unwrap();
        let network = networked.network.unwrap();
        assert_eq!(network.egress.unwrap().default, Some(NetworkAction::Allow));
        let ingress = network.ingress.unwrap();
        assert_eq!(ingress.default, Some(NetworkAction::Deny));
        assert_eq!(ingress.host_loopback, Some(NetworkAction::Deny));
        let filesystem = networked.filesystem.unwrap();
        assert!(filesystem.readwrite_paths.contains(&zed_path));
        assert!(!filesystem.readonly_paths.contains(&zed_path));
    }

    /// Folders granted outside the project are added as asked, whatever
    /// the project's own access.
    #[test]
    fn requests_add_the_folders_granted_outside_the_project() {
        let root = TestRoot::new();
        let zed = folder(&root.0, "zed");
        let cache = folder(&root.0, "cache");
        let docs = folder(&root.0, "docs");
        let scratch = root.0.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let text = |path: &Path| resolved(path).unwrap().to_string_lossy().into_owned();
        let project = Project {
            folders: vec![zed.clone()],
            access: Access::Read,
        };
        let folders = Folders {
            read: vec![resolved(&docs.path).unwrap()],
            write: vec![resolved(&cache.path).unwrap()],
        };
        let request = request("ls", &project, &folders, &scratch, &HashMap::new()).unwrap();
        let filesystem = request.filesystem.unwrap();
        assert!(filesystem.readwrite_paths.contains(&text(&cache.path)));
        assert!(!filesystem.readonly_paths.contains(&text(&cache.path)));
        assert!(filesystem.readonly_paths.contains(&text(&docs.path)));
        assert!(filesystem.readonly_paths.contains(&text(&zed.path)));
        assert!(!filesystem.readwrite_paths.contains(&text(&zed.path)));
    }

    #[test]
    fn leftovers_of_other_instances_are_removed_but_not_live_ones() {
        let root = TestRoot::new();
        if !v1::platform_support().is_supported {
            return;
        }
        let stale = root.0.join("stale");
        std::fs::create_dir_all(stale.join("thread")).unwrap();
        std::fs::write(root.0.join("stale.lock"), "").unwrap();
        let live = Sandboxes::new(root.0.clone(), "live", EnvironmentSource::Process);
        live.ready().unwrap();
        let live_scratch = live.scratch_dir("thread");
        std::fs::create_dir_all(&live_scratch).unwrap();

        let current = Sandboxes::new(root.0.clone(), "current", EnvironmentSource::Process);
        current.ready().unwrap();

        assert!(!stale.exists(), "an unlocked instance's folders go");
        assert!(!root.0.join("stale.lock").exists());
        assert!(live_scratch.exists(), "a running instance's stay");

        live.shutdown();
        assert!(!root.0.join("live").exists());
        assert!(!root.0.join("live.lock").exists());
    }
}
