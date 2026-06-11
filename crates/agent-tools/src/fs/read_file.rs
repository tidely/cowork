use futures::FutureExt;
use llm::{Tool, ToolError, ToolOutput, parse_args, schema_for};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ToolIoError, resolve_path};

pub const MAX_READ_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ReadFileInput {
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    pub path: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from disk."
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<ReadFileInput>()
    }

    fn call(
        &self,
        arguments: serde_json::Value,
    ) -> futures::future::BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input = parse_args(arguments)?;
            read_file(input)
                .map(ToolOutput::text)
                .map_err(|error| ToolError::Execution(error.to_string()))
        }
        .boxed()
    }
}

pub fn read_file(input: ReadFileInput) -> Result<String, ToolIoError> {
    let path = resolve_path(&input.path)?;
    let metadata = std::fs::metadata(&path)?;

    if !metadata.is_file() {
        return Err(std::io::Error::other(format!("{} is not a file", path.display())).into());
    }

    if metadata.len() > MAX_READ_BYTES {
        return Err(std::io::Error::other(format!(
            "{} is too large to read ({} bytes, max {MAX_READ_BYTES})",
            path.display(),
            metadata.len()
        ))
        .into());
    }

    std::fs::read_to_string(&path).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_reads_small_utf8_file() {
        let path = unique_temp_path("read.txt");
        std::fs::write(&path, "hello").expect("seed file");

        let output = read_file(ReadFileInput {
            path: path.to_string_lossy().to_string(),
        })
        .expect("read succeeds");

        assert_eq!(output, "hello");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_file_rejects_large_files() {
        let path = unique_temp_path("large.txt");
        std::fs::write(&path, vec![b'x'; MAX_READ_BYTES as usize + 1]).expect("seed file");

        let error = read_file(ReadFileInput {
            path: path.to_string_lossy().to_string(),
        })
        .expect_err("large file is rejected");

        assert!(error.to_string().contains("too large to read"));
        let _ = std::fs::remove_file(path);
    }

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-tools-read-file-test-{}-{}",
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
