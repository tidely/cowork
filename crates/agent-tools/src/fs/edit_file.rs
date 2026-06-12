use std::borrow::Cow;

use async_trait::async_trait;
use llm::{Tool, ToolError, ToolOutput, parse_args, schema_for};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ToolIoError, resolve_path};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EditFileInput {
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    pub path: String,
    /// Exact text to find. The edit fails unless this text appears exactly once.
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> Cow<'static, str> {
        "edit_file".into()
    }

    fn description(&self) -> Cow<'static, str> {
        "Edit a text file by replacing one exact text span with another.".into()
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<EditFileInput>()
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let input = parse_args(arguments)?;
        edit_file(input)
            .map(ToolOutput::text)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

pub fn edit_file(input: EditFileInput) -> Result<String, ToolIoError> {
    if input.old_text.is_empty() {
        return Err(std::io::Error::other("old_text must not be empty").into());
    }

    let path = resolve_path(&input.path)?;
    let original = std::fs::read_to_string(&path)?;

    let matches = original.match_indices(&input.old_text).count();
    match matches {
        0 => Err(std::io::Error::other("old_text was not found in the file").into()),
        1 => {
            let updated = original.replacen(&input.old_text, &input.new_text, 1);
            std::fs::write(&path, updated)?;
            Ok(format!("Edited {}", path.display()))
        }
        count => Err(std::io::Error::other(format!(
            "old_text appears {count} times; provide a more specific span"
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_file_replaces_one_exact_span() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("edit.txt");
        std::fs::write(&path, "hello world").expect("seed file");

        let output = edit_file(EditFileInput {
            path: path.to_string_lossy().to_string(),
            old_text: "world".into(),
            new_text: "there".into(),
        })
        .expect("edit succeeds");

        assert!(output.contains("Edited"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read file"),
            "hello there"
        );
    }

    #[test]
    fn edit_file_rejects_missing_span() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("missing.txt");
        std::fs::write(&path, "hello world").expect("seed file");

        let error = edit_file(EditFileInput {
            path: path.to_string_lossy().to_string(),
            old_text: "absent".into(),
            new_text: "there".into(),
        })
        .expect_err("missing span is rejected");

        assert!(error.to_string().contains("not found"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read file"),
            "hello world"
        );
    }

    #[test]
    fn edit_file_rejects_ambiguous_span() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("ambiguous.txt");
        std::fs::write(&path, "x x").expect("seed file");

        let error = edit_file(EditFileInput {
            path: path.to_string_lossy().to_string(),
            old_text: "x".into(),
            new_text: "y".into(),
        })
        .expect_err("ambiguous span is rejected");

        assert!(error.to_string().contains("appears 2 times"));
        assert_eq!(std::fs::read_to_string(&path).expect("read file"), "x x");
    }
}
