use futures::FutureExt;
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

impl Tool for ListDirectory {
    fn name(&self) -> &'static str {
        "list_directory"
    }

    fn description(&self) -> &'static str {
        "List files and directories inside a directory."
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<ListDirectoryInput>()
    }

    fn call(
        &self,
        arguments: serde_json::Value,
    ) -> futures::future::BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input = parse_args(arguments)?;
            list_directory(input)
                .map(ToolOutput::text)
                .map_err(|error| ToolError::Execution(error.to_string()))
        }
        .boxed()
    }
}

pub fn list_directory(input: ListDirectoryInput) -> Result<String, ToolIoError> {
    let path = resolve_path(&input.path)?;
    let mut entries = std::fs::read_dir(&path)?
        .map(|entry| {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let kind = if metadata.is_dir() {
                "dir"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };

            Ok((
                entry.file_name().to_string_lossy().to_string(),
                kind.to_string(),
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
        .map(|(name, kind, size)| match kind.as_str() {
            "dir" => format!("{name}/"),
            "file" => format!("{name} ({size} bytes)"),
            _ => format!("{name} ({kind})"),
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_directory_sorts_entries_and_marks_directories() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("b.txt"), "bb").expect("seed b");
        std::fs::write(dir.join("a.txt"), "a").expect("seed a");
        std::fs::create_dir(dir.join("child")).expect("seed child");

        let output = list_directory(ListDirectoryInput {
            path: dir.to_string_lossy().to_string(),
        })
        .expect("list succeeds");

        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines, vec!["a.txt (1 bytes)", "b.txt (2 bytes)", "child/"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn list_directory_reports_empty_directory() {
        let dir = unique_temp_dir();

        let output = list_directory(ListDirectoryInput {
            path: dir.to_string_lossy().to_string(),
        })
        .expect("list succeeds");

        assert!(output.contains("is empty"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-tools-list-directory-test-{}-{}",
            std::process::id(),
            unique_id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn unique_id() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    }
}
