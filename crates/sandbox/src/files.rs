//! The file tools: listing folders, reading files, and writing them in the
//! host filesystem for reads and project/scratch folders for writes, from the host process
//! rather than a sandboxed command. Which paths they reach is decided in
//! [`crate::paths`] alone.
//!
//! Like `run_command`'s, their definitions never change, so they name no
//! host paths: the folders' names come from the environment note.

use std::{
    fmt::Write as _,
    fs::File,
    io::{BufRead as _, BufReader},
    sync::Arc,
};

use agent::tool::{Tool, ToolCall, ToolError, ToolErrorKind};
use serde::Deserialize;

use crate::{
    Needs, Sandboxes, ThreadSandbox,
    paths::{FileError, Location, Places, Resolved, Root},
};

/// How much of a file one `read_file` call returns, in bytes. A longer read
/// stops at a line boundary and says where to go on from.
pub const READ_LIMIT: usize = 16 * 1024;

/// How many entries one `list_directory` call shows.
pub const LIST_LIMIT: usize = 500;

const PATHS: &str = "Paths start with the name of a project folder, as the latest <environment> \
note lists them, followed by a path inside it (`myproject/src/main.rs`), or with `scratch://` for \
this conversation's scratch folder (`scratch://notes.md`). Absolute paths anywhere on the host \
can be listed or read, except denied credentials. Writes stay within project and scratch folders.";

impl From<FileError> for ToolError {
    fn from(error: FileError) -> Self {
        let (kind, message) = match error {
            FileError::Invalid(message) => (ToolErrorKind::InvalidArguments, message),
            FileError::NotFound(message) => (ToolErrorKind::NotFound, message),
            FileError::Refused(message) => (ToolErrorKind::PermissionDenied, message),
            FileError::Failed(message) => (ToolErrorKind::Execution, message),
        };
        ToolError::new(kind, message)
    }
}

/// Runs `work` with the thread's places as they are now, off the async
/// runtime, as it does blocking file I/O.
async fn with_places<T: Send + 'static>(
    sandbox: Arc<ThreadSandbox>,
    work: impl FnOnce(&Sandboxes, &Places) -> Result<T, FileError> + Send + 'static,
) -> Result<T, FileError> {
    tokio::task::spawn_blocking(move || {
        let sandboxes = sandbox.sandboxes();
        work(sandboxes, &sandboxes.places(sandbox.thread()))
    })
    .await
    .unwrap_or_else(|error| Err(FileError::Failed(error.to_string())))
}

fn location(places: &Places, path: &str, what: &str) -> Result<Location, FileError> {
    match places.resolve(path)? {
        Resolved::Path(location) => Ok(location),
        Resolved::Places => Err(FileError::Invalid(format!(
            "`{path}` stands for the list of project folders, not {what}."
        ))),
    }
}

fn io_error(error: std::io::Error, location: &Location) -> FileError {
    let display = location.display();
    match error.kind() {
        std::io::ErrorKind::NotFound => FileError::NotFound(format!("{display} does not exist.")),
        _ => FileError::Failed(format!("{display}: {error}")),
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDirectoryArgs {
    /// The folder to list, such as `myproject`, `myproject/src`, or
    /// `scratch://`. `.` lists the project folders themselves.
    pub path: String,
}

/// Lists a host folder, excluding denied credentials.
pub struct ListDirectory;

impl ListDirectory {
    pub const NAME: &'static str = "list_directory";
}

impl Tool for ListDirectory {
    type Args = ListDirectoryArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "List the files and folders in a folder on the user's computer or in scratch, \
             folder, one level deep, as paths you can pass to the other file tools. Use `.` to \
             list the project folders themselves. {PATHS}"
        )
    }

    async fn run(
        &self,
        _call: ToolCall<Needs>,
        sandbox: Arc<ThreadSandbox>,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        Ok(with_places(sandbox, move |_, places| list_directory(places, &args.path)).await?)
    }
}

pub(crate) fn list_directory(places: &Places, path: &str) -> Result<String, FileError> {
    let folder = match places.resolve(path)? {
        Resolved::Places => {
            return Ok(format!(
                "# Folders:\n{}\n",
                places.names().collect::<Vec<_>>().join("\n")
            ));
        }
        Resolved::Path(location) => location,
    };
    let display = folder.display();
    let entries = match std::fs::read_dir(&folder.real) {
        Ok(entries) => entries,
        // Created by the first command or write that needs it.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && folder.root == Root::Scratch
                && folder.is_root() =>
        {
            return Ok(format!("{display} is empty.\n"));
        }
        Err(_) if folder.real.is_file() => {
            return Err(FileError::Invalid(format!(
                "{display} is a file, not a folder. Use read_file to read it."
            )));
        }
        Err(error) => return Err(io_error(error, &folder)),
    };

    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        // Followed, so a link is listed as what it leads to; one leading
        // outside the place, or nowhere, isn't listed at all.
        let Ok(real) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if !places.reaches(&folder.root, &real) {
            continue;
        }
        let is_folder = real.is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        let shown = folder.child(name, real).display();
        if is_folder {
            folders.push(shown);
        } else {
            files.push(shown);
        }
    }
    folders.sort();
    files.sort();

    let total = folders.len() + files.len();
    let mut output = String::new();
    let mut shown = 0;
    for (heading, entries) in [("# Folders:", folders), ("# Files:", files)] {
        let entries = entries
            .into_iter()
            .take(LIST_LIMIT - shown)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            continue;
        }
        shown += entries.len();
        if !output.is_empty() {
            output.push('\n');
        }
        _ = writeln!(output, "{heading}\n{}", entries.join("\n"));
    }
    if total == 0 {
        _ = writeln!(output, "{display} is empty.");
    } else if shown < total {
        _ = writeln!(
            output,
            "\n{} more entries are not shown. Use run_command to search the folder.",
            total - shown
        );
    }
    Ok(output)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    /// The file to read, such as `myproject/src/main.rs` or
    /// `scratch://notes.md`.
    pub path: String,
    /// The first line to read, counting from 1.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// The last line to read, inclusive.
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// Reads a host text file, excluding denied credentials.
pub struct ReadFile;

impl ReadFile {
    pub const NAME: &'static str = "read_file";
}

impl Tool for ReadFile {
    type Args = ReadFileArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "Read a text file on the user's computer or in scratch. Each line comes back \
             as its line number, right-aligned in six characters, then a tab, then the line; the \
             number and tab are not part of the file. A file longer than about 16 KB comes back \
             in parts: the result says where it stopped, and start_line and end_line read any \
             range. Only read paths you have seen in a listing, a command's output, or the \
             conversation. {PATHS}"
        )
    }

    async fn run(
        &self,
        _call: ToolCall<Needs>,
        sandbox: Arc<ThreadSandbox>,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        Ok(with_places(sandbox, move |_, places| {
            read_file(places, &args.path, args.start_line, args.end_line)
        })
        .await?)
    }
}

pub(crate) fn read_file(
    places: &Places,
    path: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> Result<String, FileError> {
    let file = location(places, path, "a file")?;
    let display = file.display();
    if file.real.is_dir() {
        return Err(FileError::Invalid(format!(
            "{display} is a folder, not a file. Use list_directory to see what it holds."
        )));
    }
    let mut reader =
        BufReader::new(File::open(&file.real).map_err(|error| io_error(error, &file))?);

    // Models sometimes count from 0, or end before they start; either way
    // they get at least the line they started at.
    let first = start_line.unwrap_or(1).max(1);
    let last = end_line.unwrap_or(u32::MAX).max(first);

    let mut output = String::new();
    let mut line = Vec::new();
    let mut number = 0u32;
    let mut last_shown = None;
    let mut stopped = false;
    let mut cut = false;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| io_error(error, &file))?;
        if read == 0 {
            break;
        }
        number = number.saturating_add(1);
        if number < first {
            continue;
        }
        if number > last {
            break;
        }
        if line.contains(&0) {
            return Err(FileError::Invalid(format!(
                "{display} is a binary file, which read_file can't show."
            )));
        }
        let text = std::str::from_utf8(&line).map_err(|_| {
            FileError::Invalid(format!(
                "{display} is not UTF-8 text, which read_file can't show."
            ))
        })?;
        let room = READ_LIMIT.saturating_sub(output.len());
        let text = if text.len() <= room {
            text
        } else if last_shown.is_none() {
            // A single line longer than the limit, as in minified files.
            cut = true;
            let mut end = room;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            stopped = true;
            break;
        };
        _ = write!(output, "{number:>6}\t{text}");
        last_shown = Some(number);
        if cut {
            stopped = true;
            break;
        }
    }

    let Some(last_shown) = last_shown else {
        return if number == 0 {
            Ok(format!("{display} is empty."))
        } else {
            Err(FileError::Invalid(format!(
                "{display} has {number} lines, so start_line {first} is past its end."
            )))
        };
    };
    if stopped {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        if cut {
            _ = write!(
                output,
                "\n[Line {last_shown} is longer than the read limit and was cut."
            );
        } else {
            _ = write!(
                output,
                "\n[Stopped after line {last_shown} to keep the result short."
            );
        }
        _ = write!(
            output,
            " Read on with start_line {}.]",
            last_shown.saturating_add(1)
        );
    }
    Ok(output)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArgs {
    /// The file to create or replace, such as `myproject/src/main.rs` or
    /// `scratch://notes.md`.
    pub path: String,
    /// The file's entire new content.
    pub content: String,
}

/// Creates or replaces a file in the thread's project folders or scratch
/// folder.
pub struct WriteFile;

impl WriteFile {
    pub const NAME: &'static str = "write_file";
}

impl Tool for WriteFile {
    type Args = WriteFileArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "Create a file, or replace a file's entire content, in the user's project or the \
             scratch folder. Missing parent folders are created. Project writes are automatically \
             approved when the latest <environment> note says the project's auto-approval level \
             is write or network; in read mode, approving this call permits this specific write. Changes \
             apply to the user's computer at once; the scratch folder can always be written, \
             and suits notes and intermediate results that are not part of the project. In write \
             or network mode, calls run at once. In read mode, every call waits for the user \
             to allow it, except in a scheduled run, where the first such call is refused with \
             an explanation. {PATHS}"
        )
    }

    fn access(&self, args: &WriteFileArgs, _: &ThreadSandbox) -> Needs {
        Needs::Write {
            path: args.path.clone(),
        }
    }

    /// Writes the target the call was allowed to write: in a read-only
    /// project, that is the one write the host approved. The target remains
    /// subject to every path check.
    async fn run(
        &self,
        call: ToolCall<Needs>,
        sandbox: Arc<ThreadSandbox>,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let approved = call.access
            == Needs::Write {
                path: args.path.clone(),
            };
        Ok(tokio::task::spawn_blocking(move || {
            let sandboxes = sandbox.sandboxes();
            let mut places = sandboxes.places(sandbox.thread());
            if approved {
                places.allow_project_write();
            }
            // Claimed first, so this instance removes a scratch folder the
            // write creates when it quits.
            sandboxes
                .claim()
                .map_err(|error| FileError::Failed(error.to_string()))?;
            write_file(&places, &args.path, &args.content)
        })
        .await
        .unwrap_or_else(|error| Err(FileError::Failed(error.to_string())))?)
    }
}

pub(crate) fn write_file(places: &Places, path: &str, content: &str) -> Result<String, FileError> {
    let file = location(places, path, "a file")?;
    let display = file.display();
    if !places.writable(&file) {
        return Err(FileError::Refused(format!(
            "{display} can't be written: file writes require a writable project or scratch target."
        )));
    }
    if file.is_root() || file.real.is_dir() {
        return Err(FileError::Invalid(format!(
            "{display} is a folder, not a file."
        )));
    }
    let previous = match std::fs::read(&file.real) {
        Ok(previous) => Some(previous),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(error, &file)),
    };
    if previous.as_deref() == Some(content.as_bytes()) {
        return Ok(format!(
            "{display} already has this content; nothing changed."
        ));
    }
    if let Some(parent) = file.real.parent() {
        std::fs::create_dir_all(parent).map_err(|error| io_error(error, &file))?;
    }
    std::fs::write(&file.real, content).map_err(|error| io_error(error, &file))?;
    let now = lines(content);
    Ok(match previous {
        None => format!("Created {display} ({now})."),
        Some(previous) => format!(
            "Replaced {display} ({} before, {now} now).",
            lines(&String::from_utf8_lossy(&previous))
        ),
    })
}

fn lines(text: &str) -> String {
    match text.lines().count() {
        1 => "1 line".to_owned(),
        count => format!("{count} lines"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Access, Project, ProjectFolder, TestRoot};

    fn places(root: &TestRoot, writable: bool) -> Places {
        let zed = root.0.join("zed");
        std::fs::create_dir_all(zed.join("src")).unwrap();
        let project = Project {
            folders: vec![ProjectFolder {
                name: "zed".into(),
                path: zed,
            }],
            access: if writable {
                Access::Write
            } else {
                Access::Read
            },
        };
        Places::new(&project, &root.0.join("scratch"), None)
    }

    #[test]
    fn listings_show_folders_then_files_as_paths_to_pass_back() {
        let root = TestRoot::new();
        let places = places(&root, false);
        std::fs::write(root.0.join("zed/Cargo.toml"), "").unwrap();
        std::fs::write(root.0.join("zed/README.md"), "").unwrap();

        assert_eq!(
            list_directory(&places, "zed").unwrap(),
            "# Folders:\nzed/src\n\n# Files:\nzed/Cargo.toml\nzed/README.md\n"
        );
        assert_eq!(
            list_directory(&places, "zed/src").unwrap(),
            "zed/src is empty.\n"
        );
        assert_eq!(
            list_directory(&places, ".").unwrap(),
            "# Folders:\nzed\nscratch://\n"
        );
        assert_eq!(
            list_directory(&places, "scratch://").unwrap(),
            "scratch:// is empty.\n",
            "a scratch folder not made yet is empty"
        );
        assert!(matches!(
            list_directory(&places, "zed/Cargo.toml"),
            Err(FileError::Invalid(_))
        ));
        assert!(matches!(
            list_directory(&places, "zed/missing"),
            Err(FileError::NotFound(_))
        ));
    }

    #[test]
    fn files_come_back_numbered_and_in_ranges() {
        let root = TestRoot::new();
        let places = places(&root, false);
        std::fs::write(root.0.join("zed/a.txt"), "one\ntwo\r\nthree").unwrap();

        assert_eq!(
            read_file(&places, "zed/a.txt", None, None).unwrap(),
            "     1\tone\n     2\ttwo\r\n     3\tthree"
        );
        assert_eq!(
            read_file(&places, "zed/a.txt", Some(2), Some(2)).unwrap(),
            "     2\ttwo\r\n"
        );
        assert_eq!(
            read_file(&places, "zed/a.txt", Some(0), Some(1)).unwrap(),
            "     1\tone\n",
            "line 0 is read as line 1"
        );
        assert_eq!(
            read_file(&places, "zed/a.txt", Some(3), Some(1)).unwrap(),
            "     3\tthree",
            "an end before the start reads the start"
        );
        assert!(matches!(
            read_file(&places, "zed/a.txt", Some(9), None),
            Err(FileError::Invalid(_))
        ));
        assert!(matches!(
            read_file(&places, "zed/src", None, None),
            Err(FileError::Invalid(_))
        ));
        assert!(matches!(
            read_file(&places, ".", None, None),
            Err(FileError::Invalid(_))
        ));
    }

    #[test]
    fn long_files_stop_at_a_line_and_say_where_to_go_on() {
        let root = TestRoot::new();
        let places = places(&root, false);
        let line = "x".repeat(99) + "\n";
        std::fs::write(root.0.join("zed/long.txt"), line.repeat(1000)).unwrap();

        let first = read_file(&places, "zed/long.txt", None, None).unwrap();
        let shown = first.lines().filter(|line| line.contains('\t')).count();
        assert!(shown > 100 && first.len() < READ_LIMIT + 200, "{shown}");
        assert!(
            first.ends_with(&format!(
                "[Stopped after line {shown} to keep the result short. Read on with start_line {}.]",
                shown + 1
            )),
            "{first}"
        );
        let next = read_file(&places, "zed/long.txt", Some(shown as u32 + 1), None).unwrap();
        assert!(next.starts_with(&format!("{:>6}\t", shown + 1)), "{next}");

        std::fs::write(root.0.join("zed/minified.js"), "é".repeat(READ_LIMIT)).unwrap();
        let cut = read_file(&places, "zed/minified.js", None, None).unwrap();
        assert!(cut.contains(
            "[Line 1 is longer than the read limit and was cut. Read on with start_line 2.]"
        ));
    }

    #[test]
    fn binary_and_non_utf8_files_are_refused() {
        let root = TestRoot::new();
        let places = places(&root, false);
        std::fs::write(root.0.join("zed/a.bin"), b"\x00\x01").unwrap();
        std::fs::write(root.0.join("zed/latin1.txt"), b"caf\xe9\n").unwrap();
        for path in ["zed/a.bin", "zed/latin1.txt"] {
            assert!(matches!(
                read_file(&places, path, None, None),
                Err(FileError::Invalid(_))
            ));
        }
        std::fs::write(root.0.join("zed/empty.txt"), "").unwrap();
        assert_eq!(
            read_file(&places, "zed/empty.txt", None, None).unwrap(),
            "zed/empty.txt is empty."
        );
    }

    #[test]
    fn writes_need_a_writable_project_but_scratch_always_takes_them() {
        let root = TestRoot::new();
        let read_only = places(&root, false);
        let outside = root.0.join("outside.txt");
        std::fs::write(&outside, "readable\n").unwrap();
        let path = outside.to_str().unwrap();
        assert!(
            read_file(&read_only, path, None, None)
                .unwrap()
                .contains("readable")
        );
        assert!(
            list_directory(&read_only, root.0.to_str().unwrap())
                .unwrap()
                .contains("outside.txt")
        );
        for places in [&read_only, &places(&root, true)] {
            assert!(matches!(
                write_file(places, path, "no"),
                Err(FileError::Refused(_))
            ));
        }
        assert!(matches!(
            write_file(&read_only, "zed/new.txt", "hello\n"),
            Err(FileError::Refused(_))
        ));
        assert!(!root.0.join("zed/new.txt").exists());

        assert_eq!(
            write_file(&read_only, "scratch://notes/a.md", "a\nb\n").unwrap(),
            "Created scratch://notes/a.md (2 lines)."
        );
        assert_eq!(
            std::fs::read_to_string(root.0.join("scratch/notes/a.md")).unwrap(),
            "a\nb\n"
        );

        let writable = places(&root, true);
        assert_eq!(
            write_file(&writable, "zed/src/lib.rs", "pub fn a() {}\n").unwrap(),
            "Created zed/src/lib.rs (1 line)."
        );
        assert_eq!(
            write_file(&writable, "zed/src/lib.rs", "pub fn a() {}\n").unwrap(),
            "zed/src/lib.rs already has this content; nothing changed."
        );
        assert_eq!(
            write_file(&writable, "zed/src/lib.rs", "\n\n").unwrap(),
            "Replaced zed/src/lib.rs (1 line before, 2 lines now)."
        );
        for folder in ["zed", "zed/src"] {
            assert!(matches!(
                write_file(&writable, folder, ""),
                Err(FileError::Invalid(_))
            ));
        }
    }
}
