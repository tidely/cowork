use std::borrow::Cow;

use async_trait::async_trait;
use llm::{Tool, ToolError, ToolOutput, parse_args, schema_for};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ToolIoError, resolve_path};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ListDirectoryInput {
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    pub path: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ListDirectory;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
    Other,
}

#[async_trait]
impl Tool for ListDirectory {
    fn name(&self) -> Cow<'static, str> {
        "list_directory".into()
    }

    fn description(&self) -> Cow<'static, str> {
        "List files and directories inside a directory.".into()
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<ListDirectoryInput>()
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let input = parse_args(arguments)?;
        list_directory(input)
            .map(ToolOutput::text)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

pub fn list_directory(input: ListDirectoryInput) -> Result<String, ToolIoError> {
    let path = resolve_path(&input.path)?;
    let mut entries = std::fs::read_dir(&path)?
        .map(|entry| {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let kind = if metadata.is_dir() {
                EntryKind::Directory
            } else if metadata.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };

            Ok((
                entry.file_name().to_string_lossy().to_string(),
                kind,
                metadata.len(),
            ))
        })
        .collect::<Result<Vec<_>, std::io::Error>>()?;

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    if entries.is_empty() {
        return Ok(format!("{} is empty", path.display()));
    }

    Ok(entries
        .into_iter()
        .map(|(name, kind, size)| match kind {
            EntryKind::Directory => format!("{name}/"),
            EntryKind::File => format!("{name} ({size} bytes)"),
            EntryKind::Other => format!("{name} (other)"),
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_directory_sorts_entries_and_marks_directories() {
        let dir = tempfile::tempdir().expect("create temp dir");
        std::fs::write(dir.path().join("b.txt"), "bb").expect("seed b");
        std::fs::write(dir.path().join("a.txt"), "a").expect("seed a");
        std::fs::create_dir(dir.path().join("child")).expect("seed child");

        let output = list_directory(ListDirectoryInput {
            path: dir.path().to_string_lossy().to_string(),
        })
        .expect("list succeeds");

        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines, vec!["a.txt (1 bytes)", "b.txt (2 bytes)", "child/"]);
    }

    #[test]
    fn list_directory_reports_empty_directory() {
        let dir = tempfile::tempdir().expect("create temp dir");

        let output = list_directory(ListDirectoryInput {
            path: dir.path().to_string_lossy().to_string(),
        })
        .expect("list succeeds");

        assert!(output.contains("is empty"));
    }
}
