//! Runs real sandboxes, checking what their commands may and may not do.
//!
//! Ignored by default: it downloads the microsandbox runtime and the Alpine
//! image on first run, and needs a hypervisor (Apple Silicon, or Linux with
//! KVM). Run it with `cargo test -p sandbox --test vm -- --ignored`.

use std::{path::PathBuf, time::Instant};

use sandbox::{Killed, Project, ProjectFolder, Sandboxes, settings};

/// A home kept between runs, so the runtime and image download once. Under
/// `/tmp` rather than `temp_dir()`, which on macOS is too long for
/// microsandbox's socket paths.
fn home() -> PathBuf {
    PathBuf::from("/tmp/cowork-msb-test")
}

async fn run(sandboxes: &Sandboxes, command: &str) -> sandbox::CommandOutput {
    sandboxes
        .run("thread", command)
        .await
        .unwrap_or_else(|error| panic!("`{command}` did not run: {error}"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a microVM; downloads the runtime and image on first run"]
async fn commands_are_confined() {
    let sandboxes = Sandboxes::new(home(), "vm-test");

    let identity = run(&sandboxes, "id -u; id -g; pwd; hostname").await;
    assert_eq!(identity.exit_code, Some(0), "{identity:?}");
    assert_eq!(identity.stdout, "65534\n65534\n/projects\nsandbox\n");

    let network = run(
        &sandboxes,
        "ls /sys/class/net; wget -q -T 3 -O - http://1.1.1.1 && echo reached",
    )
    .await;
    assert_ne!(network.exit_code, Some(0), "{network:?}");
    assert!(!network.stdout.contains("reached"), "{network:?}");
    assert!(
        !network.stdout.contains("eth"),
        "no network device: {network:?}"
    );

    let writes = run(
        &sandboxes,
        "touch /etc/x || echo denied; echo ok > /tmp/kept",
    )
    .await;
    assert!(writes.stdout.contains("denied"), "{writes:?}");
    let kept = run(&sandboxes, "cat /tmp/kept").await;
    assert_eq!(kept.stdout, "ok\n", "files persist between commands");

    let escalation = run(&sandboxes, "su -c id root </dev/null; echo $?").await;
    assert!(!escalation.stdout.starts_with("uid=0"), "{escalation:?}");

    // Only the image and the sandbox's own writes: nothing of the host's.
    // The one host share is microsandbox's control channel at `/.msb`, a
    // per-sandbox directory of the runtime's, which commands cannot write.
    let mounts = run(&sandboxes, "grep virtiofs /proc/mounts").await;
    assert_eq!(
        mounts.stdout, "msb_runtime /.msb virtiofs rw,relatime 0 0\n",
        "{mounts:?}"
    );
    let control = run(
        &sandboxes,
        "ls -la /.msb; touch /.msb/probe 2>&1 && echo wrote; ls /.msb/rootfs 2>&1",
    )
    .await;
    println!(
        "/.msb as the agent sees it:\n{}{}",
        control.stdout, control.stderr
    );
    assert!(!control.stdout.contains("wrote"), "{control:?}");
    let users = run(&sandboxes, "ls /Users /home 2>&1").await;
    assert!(!users.stdout.contains("rasmus"), "{users:?}");

    let flood = run(&sandboxes, "yes").await;
    assert_eq!(flood.killed, Some(Killed::OutputLimit), "{flood:?}");
    assert!(flood.truncated);
    assert_eq!(flood.stdout.len(), settings::OUTPUT_LIMIT);

    let started = Instant::now();
    let slow = run(&sandboxes, "sleep 600").await;
    assert_eq!(slow.killed, Some(Killed::TimedOut), "{slow:?}");
    assert!(started.elapsed() < settings::COMMAND_TIMEOUT + settings::KILL_GRACE * 2);

    sandboxes.shutdown().await;
}

/// A fresh host folder holding `notes.txt`.
fn host_folder(name: &str) -> PathBuf {
    let folder = PathBuf::from("/tmp/cowork-msb-test-projects").join(name);
    _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("notes.txt"), "from the host\n").unwrap();
    folder
}

fn project(folders: &[(&str, &PathBuf)], writable: bool) -> Project {
    Project {
        folders: folders
            .iter()
            .map(|(name, path)| ProjectFolder {
                name: (*name).to_owned(),
                path: (*path).clone(),
            })
            .collect(),
        writable,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots microVMs; downloads the runtime and image on first run"]
async fn project_folders_are_mounted_by_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let sandboxes = Sandboxes::new(home(), "vm-test-projects");

    // Without folders, `/projects` is there, empty, and closed.
    let empty = run(
        &sandboxes,
        "ls -A /projects; touch /projects/x || echo denied",
    )
    .await;
    assert_eq!(empty.stdout, "denied\n", "{empty:?}");
    run(&sandboxes, "echo scratch > /tmp/scratch").await;

    let zed = host_folder("zed");
    let other = host_folder("other-src");
    sandboxes.set_project("thread", project(&[("zed", &zed), ("src", &other)], false));
    assert!(sandboxes.has_sandbox("thread"));

    // Read-only: the host's files are there, live, and cannot be changed.
    let read = run(
        &sandboxes,
        "ls /projects; cat /projects/zed/notes.txt; ls /tmp; \
         echo agent > /projects/zed/notes.txt || echo denied; \
         touch /projects/src/new || echo denied; rm /projects/zed/notes.txt || echo denied",
    )
    .await;
    assert!(read.sandbox_restarted, "the project changed: {read:?}");
    assert_eq!(
        read.stdout, "src\nzed\nfrom the host\ndenied\ndenied\ndenied\n",
        "a fresh machine without the scratch file: {read:?}"
    );
    assert_eq!(
        std::fs::read_to_string(zed.join("notes.txt")).unwrap(),
        "from the host\n"
    );
    assert!(!other.join("new").exists());
    let relative = run(&sandboxes, "pwd; cat zed/notes.txt").await;
    assert_eq!(
        relative.stdout, "/projects\nfrom the host\n",
        "commands start in the project: {relative:?}"
    );
    std::fs::write(zed.join("live.txt"), "later\n").unwrap();
    let live = run(&sandboxes, "cat /projects/zed/live.txt").await;
    assert!(!live.sandbox_restarted, "{live:?}");
    assert_eq!(live.stdout, "later\n", "host changes show at once");

    // Writable: changes reach the host at once, but not mode bits.
    sandboxes.set_project("thread", project(&[("zed", &zed), ("src", &other)], true));
    let write = run(
        &sandboxes,
        "id -u; stat -c %u /projects/zed/notes.txt; \
         echo agent > /projects/zed/notes.txt && echo wrote; \
         printf '#!/bin/sh\\n' > /projects/src/run.sh && chmod +x /projects/src/run.sh; \
         test -x /projects/src/run.sh && echo executable",
    )
    .await;
    assert!(write.sandbox_restarted, "{write:?}");
    assert_eq!(
        write.stdout, "65534\n65534\nwrote\nexecutable\n",
        "{write:?}"
    );
    assert_eq!(
        std::fs::read_to_string(zed.join("notes.txt")).unwrap(),
        "agent\n"
    );
    let mode = std::fs::metadata(other.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0, "the host file is not executable: {mode:o}");

    // Only the project's folders are shared.
    let mounts = run(
        &sandboxes,
        "grep virtiofs /proc/mounts | cut -d' ' -f2 | sort",
    )
    .await;
    assert_eq!(
        mounts.stdout, "/.msb\n/projects/src\n/projects/zed\n",
        "{mounts:?}"
    );

    sandboxes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots microVMs; downloads the runtime and image on first run"]
async fn commands_run_at_the_same_time() {
    let sandboxes = Sandboxes::new(home(), "vm-test-concurrent");
    let boot_id = "cat /proc/sys/kernel/random/boot_id";

    // Both arrive before the thread has a sandbox: one boot serves them.
    let (first, second) = tokio::join!(
        sandboxes.run("thread", boot_id),
        sandboxes.run("thread", boot_id),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.exit_code, Some(0), "{first:?}");
    assert_eq!(first.stdout, second.stdout, "one sandbox for the thread");

    // A command can see another's file while that one is still running.
    let started = Instant::now();
    let (writer, reader) = tokio::join!(
        sandboxes.run("thread", "echo shared > /tmp/shared; sleep 3"),
        sandboxes.run("thread", "sleep 1; cat /tmp/shared"),
    );
    let elapsed = started.elapsed();
    assert_eq!(writer.unwrap().exit_code, Some(0));
    assert_eq!(reader.unwrap().stdout, "shared\n");
    assert!(
        elapsed < std::time::Duration::from_millis(3900),
        "the commands overlapped: {elapsed:?}"
    );

    let other = sandboxes.run("other", boot_id).await.unwrap();
    assert_eq!(other.exit_code, Some(0), "{other:?}");
    assert_ne!(
        other.stdout, first.stdout,
        "each thread has its own sandbox"
    );

    sandboxes.shutdown().await;
}
