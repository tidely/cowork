use std::borrow::Cow;

use async_trait::async_trait;
use llm::{Tool, ToolError, ToolOutput, parse_args, schema_for};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ToolIoError, resolve_path};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WriteFileInput {
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    pub path: String,
    /// Full contents to write to the file.
    pub content: String,
    /// Set to true to replace an existing file. If false, existing files are left untouched.
    pub overwrite: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> Cow<'static, str> {
        "write_file".into()
    }

    fn description(&self) -> Cow<'static, str> {
        "Create a text file, or overwrite an existing text file when explicitly allowed.".into()
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<WriteFileInput>()
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let input = parse_args(arguments)?;
        write_file(input)
            .map(ToolOutput::text)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

pub fn write_file(input: WriteFileInput) -> Result<String, ToolIoError> {
    let path = resolve_path(&input.path)?;

    if let Ok(metadata) = std::fs::metadata(&path) {
        if !metadata.is_file() {
            return Err(std::io::Error::other(format!("{} is not a file", path.display())).into());
        }
        if !input.overwrite {
            return Err(std::io::Error::other(format!(
                "{} already exists; set overwrite to true to replace it",
                path.display()
            ))
            .into());
        }
    } else if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        return Err(std::io::Error::other(format!(
            "parent directory {} does not exist",
            parent.display()
        ))
        .into());
    }

    std::fs::write(&path, input.content)?;
    Ok(format!("Wrote {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_creates_new_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("new.txt");

        let result = write_file(WriteFileInput {
            path: path.to_string_lossy().to_string(),
            content: "hello".into(),
            overwrite: false,
        })
        .expect("file is written");

        assert!(result.contains("Wrote"));
        assert_eq!(std::fs::read_to_string(&path).expect("read file"), "hello");
    }

    #[test]
    fn write_file_refuses_to_overwrite_without_flag() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").expect("seed file");

        let error = write_file(WriteFileInput {
            path: path.to_string_lossy().to_string(),
            content: "updated".into(),
            overwrite: false,
        })
        .expect_err("overwrite is rejected");

        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read file"),
            "original"
        );
    }

    #[test]
    fn write_file_overwrites_when_allowed() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("overwrite.txt");
        std::fs::write(&path, "original").expect("seed file");

        write_file(WriteFileInput {
            path: path.to_string_lossy().to_string(),
            content: "updated".into(),
            overwrite: true,
        })
        .expect("overwrite succeeds");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read file"),
            "updated"
        );
    }

    #[test]
    fn write_file_rejects_missing_parent() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("missing-parent").join("file.txt");

        let error = write_file(WriteFileInput {
            path: path.to_string_lossy().to_string(),
            content: "content".into(),
            overwrite: false,
        })
        .expect_err("missing parent is rejected");

        assert!(error.to_string().contains("parent directory"));
    }
}
