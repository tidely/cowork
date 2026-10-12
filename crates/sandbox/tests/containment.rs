//! Runs real commands, checking what they may and may not do.
//!
//! They take milliseconds and download nothing. On a computer without a
//! supported sandbox, such as Linux without `bwrap`, they pass without
//! running anything, saying so.

#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use sandbox::{Access, CommandOutput, EnvironmentSource, Project, ProjectFolder, Sandboxes};

/// A fresh folder for one test, removed when dropped.
struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("cowork-sandbox-test-{}-{name}", std::process::id()));
        _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    /// A host folder holding `notes.txt`.
    fn folder(&self, name: &str) -> PathBuf {
        let folder = self.0.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("notes.txt"), "from the host\n").unwrap();
        folder
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

fn supported() -> bool {
    let support = mxc_sdk::v1::platform_support();
    if !support.is_supported {
        eprintln!("skipped: no supported sandbox: {:?}", support.reason);
    }
    support.is_supported
}

fn sandboxes(root: &Root) -> Arc<Sandboxes> {
    Arc::new(Sandboxes::new(
        root.0.join("scratch"),
        "test",
        EnvironmentSource::Process,
    ))
}

fn project(folders: &[(&str, &Path)], writable: bool) -> Project {
    Project {
        folders: folders
            .iter()
            .map(|(name, path)| ProjectFolder {
                name: (*name).to_owned(),
                path: path.to_path_buf(),
            })
            .collect(),
        access: if writable {
            Access::Write
        } else {
            Access::Read
        },
    }
}

async fn run(sandboxes: &Sandboxes, command: &str) -> CommandOutput {
    sandboxes
        .run("thread", command)
        .await
        .unwrap_or_else(|error| panic!("`{command}` did not run: {error}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn null_device_redirection_runs_the_command() {
    if !supported() {
        return;
    }
    let root = Root::new("null-device");
    let sandboxes = sandboxes(&root);
    let output = run(
        &sandboxes,
        "{ echo ran > \"$TMPDIR/redirected\"; echo discarded; echo discarded >&2; } \
         >/dev/null 2>/dev/null && cat \"$TMPDIR/redirected\" && \
         cat </dev/null && echo read-null",
    )
    .await;
    assert_eq!(output.exit_code, 0, "redirection succeeds: {output:?}");
    assert_eq!(
        output.stdout, "ran\nread-null\n",
        "the body ran: {output:?}"
    );
    assert_eq!(
        output.stderr, "",
        "both streams were redirected: {output:?}"
    );

    #[cfg(target_os = "linux")]
    {
        let devices = run(
            &sandboxes,
            "test -c /dev/null && test ! -e /dev/disk && \
             { touch /dev/cowork-test && echo private; } && rm /dev/cowork-test",
        )
        .await;
        assert_eq!(devices.exit_code, 0, "private devices: {devices:?}");
        assert_eq!(devices.stdout, "private\n", "{devices:?}");
        assert!(
            !Path::new("/dev/cowork-test").exists(),
            "host /dev is untouched"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_are_confined() {
    if !supported() {
        return;
    }
    let root = Root::new("confined");
    let sandboxes = sandboxes(&root);
    let outside = root.0.join("outside");

    let scratch = run(
        &sandboxes,
        "pwd; echo \"$TMPDIR\"; echo \"$HOME\"; echo kept > \"$TMPDIR/kept\"",
    )
    .await;
    assert_eq!(scratch.exit_code, 0, "{scratch:?}");
    let lines = scratch.stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[0], lines[1],
        "without folders, commands start in the scratch folder, their TMPDIR"
    );
    assert!(
        lines[0].starts_with(&root.0.to_string_lossy().into_owned()),
        "TMPDIR is the thread's scratch folder: {scratch:?}"
    );
    let home = std::env::home_dir().unwrap();
    assert_eq!(
        lines[2],
        home.to_string_lossy(),
        "HOME is the user's real home, as `~` is in grants"
    );
    let kept = run(&sandboxes, "cat \"$TMPDIR/kept\"").await;
    assert_eq!(kept.stdout, "kept\n", "scratch files persist: {kept:?}");
    let marker = format!(".cowork-sandbox-test-{}", std::process::id());
    let home_write = run(&sandboxes, &format!("touch ~/{marker} || echo denied")).await;
    assert!(
        home_write.stdout.ends_with("denied\n") && !home.join(&marker).exists(),
        "the real home stays read-only without a write grant: {home_write:?}"
    );

    let writes = run(
        &sandboxes,
        &format!("touch '{}' || echo denied", outside.display()),
    )
    .await;
    assert!(writes.stdout.contains("denied"), "{writes:?}");
    assert!(!outside.exists());

    let home = std::env::home_dir().unwrap();
    let readable = run(&sandboxes, &format!("ls '{}'", home.display())).await;
    assert_eq!(
        readable.exit_code, 0,
        "the real home can be listed: {readable:?}"
    );
    for credential in [".ssh", ".aws", ".gnupg"] {
        let path = home.join(credential);
        if path.exists() {
            // Bubblewrap masks denied directories with an empty filesystem;
            // Seatbelt refuses the listing. Neither may expose host entries.
            let denied = run(&sandboxes, &format!("ls -A '{}'", path.display())).await;
            assert_eq!(denied.stdout, "", "credentials stay hidden: {denied:?}");
            if let Ok(mut entries) = std::fs::read_dir(&path)
                && let Some(Ok(entry)) = entries.next()
            {
                let path = entry.path().to_string_lossy().replace('\'', "'\\''");
                let hidden = run(&sandboxes, &format!("test ! -e '{path}'")).await;
                assert_eq!(hidden.exit_code, 0, "the host entry is hidden: {hidden:?}");
            }
        }
    }

    let network = run(
        &sandboxes,
        "command -v curl >/dev/null || { echo no-curl; exit 0; }; \
         curl -sS -m 3 http://1.1.1.1 >/dev/null 2>&1 && echo reached || echo blocked",
    )
    .await;
    assert!(!network.stdout.contains("reached"), "{network:?}");

    let failing = run(&sandboxes, "echo oops >&2; exit 3").await;
    assert_eq!(failing.exit_code, 3);
    assert_eq!(failing.stderr, "oops\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosts_tools_are_there() {
    if !supported() {
        return;
    }
    let Ok(cargo) = std::env::var("CARGO") else {
        return;
    };
    let root = Root::new("tools");
    let sandboxes = sandboxes(&root);
    let version = run(&sandboxes, &format!("'{cargo}' --version")).await;
    assert_eq!(version.exit_code, 0, "the toolchain runs: {version:?}");
    assert!(version.stdout.starts_with("cargo "), "{version:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn project_folders_follow_the_mode() {
    if !supported() {
        return;
    }
    let root = Root::new("projects");
    let sandboxes = sandboxes(&root);
    let zed = root.folder("zed");
    let other = root.folder("other");

    sandboxes.set_project("thread", project(&[("zed", &zed), ("src", &other)], false));
    let read = run(
        &sandboxes,
        &format!(
            "pwd; cat notes.txt; echo agent > notes.txt || echo denied; \
             touch '{}/new' || echo denied",
            other.display()
        ),
    )
    .await;
    assert_eq!(
        read.stdout,
        format!("{}\nfrom the host\ndenied\ndenied\n", zed.display()),
        "commands start in the first folder, which is read-only: {read:?}"
    );
    assert_eq!(
        std::fs::read_to_string(zed.join("notes.txt")).unwrap(),
        "from the host\n"
    );
    assert!(!other.join("new").exists());

    std::fs::write(zed.join("live.txt"), "later\n").unwrap();
    let live = run(&sandboxes, "cat live.txt").await;
    assert_eq!(live.stdout, "later\n", "host changes show at once");

    // Applies from the next command, with no restart.
    sandboxes.set_project("thread", project(&[("zed", &zed), ("src", &other)], true));
    let write = run(
        &sandboxes,
        &format!(
            "echo agent > notes.txt && touch '{}/new' && echo wrote",
            other.display()
        ),
    )
    .await;
    assert_eq!(write.stdout, "wrote\n", "{write:?}");
    assert_eq!(
        std::fs::read_to_string(zed.join("notes.txt")).unwrap(),
        "agent\n"
    );
    assert!(other.join("new").exists());

    sandboxes.set_project("thread", project(&[("src", &other)], true));
    let removed = run(
        &sandboxes,
        &format!(
            "cat '{}/notes.txt'; touch '{}/new' || echo denied",
            zed.display(),
            zed.display()
        ),
    )
    .await;
    assert_eq!(
        removed.stdout, "agent\ndenied\n",
        "a removed folder remains readable but loses write access: {removed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn background_processes_end_with_their_command() {
    if !supported() {
        return;
    }
    let root = Root::new("background");
    let sandboxes = sandboxes(&root);
    let started = Instant::now();
    let output = run(&sandboxes, "sleep 30 & echo started").await;
    assert_eq!(output.stdout, "started\n", "{output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the command did not wait for its background job: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_run_kills_its_command() {
    if !supported() {
        return;
    }
    let root = Root::new("stopped");
    let sandboxes = sandboxes(&root);
    let ticks = || {
        std::fs::read_dir(root.0.join("scratch"))
            .unwrap()
            .flatten()
            .find_map(|instance| {
                std::fs::read_to_string(instance.path().join("thread").join("ticks")).ok()
            })
            .map_or(0, |ticks| ticks.lines().count())
    };
    let running = sandboxes.run(
        "thread",
        "while true; do echo tick >> \"$TMPDIR/ticks\"; sleep 0.05; done",
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), running)
            .await
            .is_err(),
        "it runs until stopped"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_stop = ticks();
    assert!(after_stop > 0, "it ran");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(ticks(), after_stop, "and stopped with its run");
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_run_at_the_same_time() {
    if !supported() {
        return;
    }
    let root = Root::new("concurrent");
    let sandboxes = sandboxes(&root);
    let started = Instant::now();
    let (writer, reader) = tokio::join!(
        sandboxes.run("thread", "echo shared > \"$TMPDIR/shared\"; sleep 2"),
        sandboxes.run("thread", "sleep 0.5; cat \"$TMPDIR/shared\""),
    );
    let elapsed = started.elapsed();
    assert_eq!(writer.unwrap().exit_code, 0);
    assert_eq!(reader.unwrap().stdout, "shared\n");
    assert!(
        elapsed < Duration::from_millis(2400),
        "the commands overlapped: {elapsed:?}"
    );

    let other = sandboxes
        .run("other", "cat \"$TMPDIR/shared\" 2>&1 || echo missing")
        .await;
    assert!(
        other.unwrap().stdout.ends_with("missing\n"),
        "each thread has its own scratch folder"
    );
}
