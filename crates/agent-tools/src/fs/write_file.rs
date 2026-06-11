use futures::FutureExt;
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

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create a text file, or overwrite an existing text file when explicitly allowed."
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<WriteFileInput>()
    }

    fn call(
        &self,
        arguments: serde_json::Value,
    ) -> futures::future::BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input = parse_args(arguments)?;
            write_file(input)
                .map(ToolOutput::text)
                .map_err(|error| ToolError::Execution(error.to_string()))
        }
        .boxed()
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
        let path = unique_temp_path("new.txt");
        let _ = std::fs::remove_file(&path);

        let result = write_file(WriteFileInput {
            path: path.to_string_lossy().to_string(),
            content: "hello".into(),
            overwrite: false,
        })
        .expect("file is written");

        assert!(result.contains("Wrote"));
        assert_eq!(std::fs::read_to_string(&path).expect("read file"), "hello");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_file_refuses_to_overwrite_without_flag() {
        let path = unique_temp_path("existing.txt");
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
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_file_overwrites_when_allowed() {
        let path = unique_temp_path("overwrite.txt");
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
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_file_rejects_missing_parent() {
        let path = unique_temp_path("missing-parent").join("file.txt");

        let error = write_file(WriteFileInput {
            path: path.to_string_lossy().to_string(),
            content: "content".into(),
            overwrite: false,
        })
        .expect_err("missing parent is rejected");

        assert!(error.to_string().contains("parent directory"));
    }

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-tools-write-file-test-{}-{}",
            std::process::id(),
            unique_id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(name)
    }

    fn unique_id() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    }
}
