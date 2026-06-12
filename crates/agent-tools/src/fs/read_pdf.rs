use std::borrow::Cow;

use async_trait::async_trait;
use llm::{Tool, ToolError, ToolOutput, parse_args, schema_for};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{MAX_READ_BYTES, ToolIoError, resolve_path};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ReadPdfInput {
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    pub path: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadPdf;

#[async_trait]
impl Tool for ReadPdf {
    fn name(&self) -> Cow<'static, str> {
        "read_pdf".into()
    }

    fn description(&self) -> Cow<'static, str> {
        "Convert a local PDF file into markdown text.".into()
    }

    fn parameters_schema(&self) -> Result<serde_json::Value, ToolError> {
        schema_for::<ReadPdfInput>()
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let input = parse_args(arguments)?;
        read_pdf(input)
            .map(ToolOutput::text)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

pub fn read_pdf(input: ReadPdfInput) -> Result<String, ToolIoError> {
    let path = resolve_path(&input.path)?;
    let metadata = std::fs::metadata(&path)?;

    if !metadata.is_file() {
        return Err(std::io::Error::other(format!("{} is not a file", path.display())).into());
    }

    let bytes = std::fs::read(&path)?;
    let pages = pdf_extract::extract_text_from_mem_by_pages(&bytes).map_err(|error| {
        std::io::Error::other(format!(
            "failed to extract PDF text from {}: {error}",
            path.display()
        ))
    })?;

    let mut output = format!("# {}\n\n", path.display());
    let mut text_found = false;
    let mut truncated = false;

    for (page_index, page_text) in pages.iter().enumerate() {
        let page_number = page_index + 1;
        let page_text = page_text.trim();
        text_found |= !page_text.is_empty();

        if !output.ends_with("\n\n") {
            output.push('\n');
        }
        output.push_str(&format!("## Page {page_number}\n\n"));
        output.push_str(page_text);
        output.push_str("\n\n");

        if output.len() as u64 > MAX_READ_BYTES {
            truncate_to_char_boundary(&mut output, MAX_READ_BYTES as usize);
            output.push_str("\n\n...\n\nPDF output truncated; ask for a narrower file or split the PDF before reading more.\n");
            truncated = true;
            break;
        }
    }

    if !text_found {
        return Ok(format!(
            "# {}\n\nNo extractable text was found in this PDF. It may be scanned or image-only.",
            path.display()
        ));
    }

    if !truncated {
        output.push_str("Converted from PDF with pdf-extract.\n");
    }

    Ok(output)
}

fn truncate_to_char_boundary(value: &mut String, max_len: usize) {
    if value.len() <= max_len {
        return;
    }

    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_len)
        .last()
        .unwrap_or(0);
    value.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pdf_rejects_non_file_paths() {
        let dir = tempfile::tempdir().expect("create temp dir");

        let error = read_pdf(ReadPdfInput {
            path: dir.path().to_string_lossy().to_string(),
        })
        .expect_err("directory is rejected");

        assert!(error.to_string().contains("is not a file"));
    }
}
