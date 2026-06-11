use futures::FutureExt;
use llm::{Tool, ToolError, ToolOutput, ToolSchemaFormat, parse_args, schema_for};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{MAX_READ_BYTES, ToolIoError, resolve_path};

pub const MAX_PDF_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ReadPdfInput {
    /// Absolute path, path relative to the current working directory, `~`, or `~/...`.
    pub path: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadPdf;

impl Tool for ReadPdf {
    fn name(&self) -> &'static str {
        "read_pdf"
    }

    fn description(&self) -> &'static str {
        "Convert a local PDF file into markdown text."
    }

    fn parameters_schema(&self, format: ToolSchemaFormat) -> Result<serde_json::Value, ToolError> {
        schema_for::<ReadPdfInput>(format)
    }

    fn call(
        &self,
        arguments: serde_json::Value,
    ) -> futures::future::BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input = parse_args(arguments)?;
            read_pdf(input)
                .map(ToolOutput::text)
                .map_err(|error| ToolError::Execution(error.to_string()))
        }
        .boxed()
    }
}

pub fn read_pdf(input: ReadPdfInput) -> Result<String, ToolIoError> {
    let path = resolve_path(&input.path)?;
    let metadata = std::fs::metadata(&path)?;

    if !metadata.is_file() {
        return Err(std::io::Error::other(format!("{} is not a file", path.display())).into());
    }

    if metadata.len() > MAX_PDF_BYTES {
        return Err(std::io::Error::other(format!(
            "{} is too large to convert as PDF ({} bytes, max {MAX_PDF_BYTES})",
            path.display(),
            metadata.len()
        ))
        .into());
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
        let dir = unique_temp_dir("non-file");

        let error = read_pdf(ReadPdfInput {
            path: dir.to_string_lossy().to_string(),
        })
        .expect_err("directory is rejected");

        assert!(error.to_string().contains("is not a file"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_pdf_rejects_large_files() {
        let dir = unique_temp_dir("large");
        let path = dir.join("large.pdf");
        std::fs::write(&path, vec![b'%'; MAX_PDF_BYTES as usize + 1]).expect("seed file");

        let error = read_pdf(ReadPdfInput {
            path: path.to_string_lossy().to_string(),
        })
        .expect_err("large PDF is rejected before parsing");

        assert!(error.to_string().contains("too large to convert as PDF"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-tools-read-pdf-test-{name}-{}-{}",
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
