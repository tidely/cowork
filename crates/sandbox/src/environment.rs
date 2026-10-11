//! The environment commands inherit: the user's login shell environment,
//! captured the way Zed does it.
//!
//! An app started from the Dock or a desktop launcher gets a bare
//! environment, without the `PATH` entries and variables the user's shell
//! startup files add for their tools. So the login shell is started
//! interactively in the user's home folder (letting tools such as direnv,
//! asdf, or mise hook in as they would in a terminal), and runs this
//! executable with [`PRINT_ENV_FLAG`], which prints its environment, the
//! shell's, as JSON. Started from a terminal, Cowork already has the shell's
//! environment and uses its own; see [`EnvironmentSource`].
//!
//! The JSON goes to the shell's file descriptor 0 rather than its standard
//! output, which interactive startup files often write to; the shells that
//! cannot redirect to it write to standard output, and the JSON is found
//! among whatever else is printed there.

use std::collections::HashMap;

/// The argument that makes the app print its environment and exit; see
/// [`print_env`].
pub const PRINT_ENV_FLAG: &str = "--printenv";

/// How long the login shell may take to print its environment before
/// Cowork's own is used instead, so that a startup file waiting on input
/// cannot stall every command.
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Variables of the captured environment that describe the capturing shell
/// rather than the user's setup, and are left out.
const SHELL_ONLY: &[&str] = &["SHLVL", "PWD", "OLDPWD", "_"];

/// Prints this process's environment as JSON on standard output. The app
/// calls it, and exits, when started with [`PRINT_ENV_FLAG`].
pub fn print_env() {
    let environment = std::env::vars().collect::<HashMap<_, _>>();
    match serde_json::to_string(&environment) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("could not print the environment: {error}");
            std::process::exit(1);
        }
    }
}

/// Where commands' environment comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentSource {
    /// The user's login shell, captured once, at the first command, by
    /// running this executable with [`PRINT_ENV_FLAG`] in it. The executable
    /// must handle that flag. Falls back to this process's environment when
    /// the shell fails to print one.
    LoginShell,
    /// This process's own: when it was started from a terminal, and so
    /// already has the shell's, on Windows, and in tests.
    Process,
}

/// The environment `source` gives, without the shell's own variables.
pub(crate) async fn load(source: EnvironmentSource) -> HashMap<String, String> {
    let mut environment = match source {
        EnvironmentSource::Process => None,
        EnvironmentSource::LoginShell => match tokio::task::spawn_blocking(login_shell).await {
            Ok(Ok(environment)) => Some(environment),
            Ok(Err(error)) => {
                eprintln!("using Cowork's own environment for commands: {error}");
                None
            }
            Err(error) => {
                eprintln!("using Cowork's own environment for commands: {error}");
                None
            }
        },
    }
    .unwrap_or_else(|| std::env::vars().collect());
    environment.retain(|name, _| !SHELL_ONLY.contains(&name.as_str()));
    environment
}

#[cfg(not(unix))]
fn login_shell() -> Result<HashMap<String, String>, String> {
    Err("login shells are only read on Unix".into())
}

/// The shells whose invocation or quoting differs from a POSIX shell's.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    Posix,
    Fish,
    /// csh and tcsh: a login shell is started as `-`, and `>&` would
    /// redirect both streams to a file.
    Csh,
    /// Nushell refuses `-l -i -c`.
    Nushell,
}

#[cfg(unix)]
impl ShellKind {
    fn of(shell: &std::path::Path) -> Self {
        match shell.file_name().and_then(|name| name.to_str()) {
            Some("fish") => Self::Fish,
            Some("csh" | "tcsh") => Self::Csh,
            Some("nu") => Self::Nushell,
            _ => Self::Posix,
        }
    }

    /// `value` as one word of this shell's command line.
    fn quote(self, value: &str) -> String {
        match self {
            Self::Posix | Self::Csh => format!("'{}'", value.replace('\'', r"'\''")),
            Self::Fish => format!("'{}'", value.replace('\\', r"\\").replace('\'', r"\'")),
            // Nushell's single quotes have no escapes; a path holding one
            // fails the capture, which falls back to Cowork's environment.
            Self::Nushell => format!("'{value}'"),
        }
    }

    /// Whether the JSON can be written to file descriptor 0.
    fn prints_to_stdin(self) -> bool {
        matches!(self, Self::Posix | Self::Fish)
    }
}

#[cfg(unix)]
fn login_shell() -> Result<HashMap<String, String>, String> {
    use std::{io::Read as _, os::unix::process::CommandExt as _, process::Stdio};

    let shell = user_shell();
    let kind = ShellKind::of(&shell);
    let home = std::env::home_dir().ok_or("the home directory is unknown")?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut script = String::new();
    if kind == ShellKind::Fish {
        // asdf and direnv hook fish's prompt event.
        script.push_str("emit fish_prompt; ");
    }
    // `cd` triggers the directory hooks of tools such as direnv and mise.
    // A path starting with `-` would read as an option.
    let home = home.to_string_lossy();
    let home = if home.starts_with('-') {
        format!("./{home}")
    } else {
        home.into_owned()
    };
    script.push_str(&format!(
        "cd {}; {} {PRINT_ENV_FLAG}",
        kind.quote(&home),
        kind.quote(&executable.to_string_lossy()),
    ));
    if kind.prints_to_stdin() {
        script.push_str(" >&0");
    }

    let mut command = std::process::Command::new(&shell);
    match kind {
        ShellKind::Csh => {
            command.arg0("-").args(["-i", "-c", &script]);
        }
        ShellKind::Nushell => {
            command.args(["-l", "-e", &format!("{script}; exit")]);
        }
        ShellKind::Posix | ShellKind::Fish => {
            command.args(["-l", "-i", "-c", &script]);
        }
    }
    let (mut reader, writer) = std::io::pipe().map_err(|error| error.to_string())?;
    if kind.prints_to_stdin() {
        command.stdin(writer).stdout(Stdio::null());
    } else {
        command.stdin(Stdio::null()).stdout(writer);
    }
    command.stderr(Stdio::null());
    // A session of its own, so an interactive shell cannot take over the
    // terminal Cowork was started from.
    // SAFETY: `setsid` is async-signal-safe, and nothing else runs between
    // fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", shell.display()))?;
    // Cowork's copy of the pipe's write end, so the read ends with the shell.
    drop(command);

    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let read = reader.read_to_end(&mut output).map(|_| output);
        _ = sender.send(read);
    });
    let output = match receiver.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(read) => read.map_err(|error| error.to_string()),
        Err(_) => {
            // Everything the shell started, such as a startup file's command
            // waiting on input, is in the session `setsid` made it lead, so
            // its process group is the shell's id.
            if let Ok(group) = libc::pid_t::try_from(child.id()) {
                // SAFETY: signals only processes of the group the shell leads.
                unsafe { libc::killpg(group, libc::SIGKILL) };
            }
            Err(format!(
                "{} did not print its environment within {} seconds",
                shell.display(),
                CAPTURE_TIMEOUT.as_secs()
            ))
        }
    };
    _ = child.kill();
    _ = child.wait();
    let output = String::from_utf8_lossy(&output?).into_owned();
    // Startup files may fail and the shell exit with an error after the
    // environment was printed, so only the output decides.
    parse_noisy(&output)
        .ok_or_else(|| format!("{} printed no environment: {output:?}", shell.display()))
}

/// The user's shell: `SHELL` when it exists, otherwise the user database's.
#[cfg(unix)]
fn user_shell() -> std::path::PathBuf {
    if let Some(shell) = std::env::var_os("SHELL").map(std::path::PathBuf::from)
        && shell.is_file()
    {
        return shell;
    }
    passwd_shell().unwrap_or_else(|| "/bin/sh".into())
}

#[cfg(unix)]
fn passwd_shell() -> Option<std::path::PathBuf> {
    let length = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n < 0 => 1024,
        n => n as usize,
    };
    let mut buffer = vec![0 as libc::c_char; length];
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    // SAFETY: the buffers outlive the call, and `result` is only read when
    // the entry was found.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: `getpwuid_r` filled `entry`, whose strings live in `buffer`.
    let shell = unsafe { std::ffi::CStr::from_ptr(entry.assume_init().pw_shell) };
    let shell = std::path::PathBuf::from(shell.to_str().ok()?);
    shell.is_file().then_some(shell)
}

/// The first JSON object of strings in `output`, skipping whatever a shell
/// printed around it.
fn parse_noisy(output: &str) -> Option<HashMap<String, String>> {
    output.match_indices('{').find_map(|(position, _)| {
        let mut deserializer = serde_json::Deserializer::from_str(&output[position..]);
        serde::Deserialize::deserialize(&mut deserializer).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_found_among_shell_noise() {
        let output = "Last login: today {not json}\n{\"PATH\":\"/usr/bin\",\"HOME\":\"/h\"}\nbye";
        let environment = parse_noisy(output).unwrap();
        assert_eq!(environment["PATH"], "/usr/bin");
        assert_eq!(environment["HOME"], "/h");
        assert_eq!(parse_noisy("no environment"), None);
    }

    #[cfg(unix)]
    #[test]
    fn words_are_quoted_for_each_shell() {
        assert_eq!(ShellKind::Posix.quote("it's"), r"'it'\''s'");
        assert_eq!(ShellKind::Fish.quote(r"it's\"), r"'it\'s\\'");
        assert_eq!(
            ShellKind::of(std::path::Path::new("/opt/homebrew/bin/fish")),
            ShellKind::Fish
        );
        assert_eq!(
            ShellKind::of(std::path::Path::new("/bin/zsh")),
            ShellKind::Posix
        );
    }

    #[tokio::test]
    async fn the_capturing_shells_own_variables_are_left_out() {
        let environment = load(EnvironmentSource::Process).await;
        assert!(environment.contains_key("PATH"));
        assert!(!environment.contains_key("PWD"));
        assert!(!environment.contains_key("SHLVL"));
    }
}
