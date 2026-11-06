//! The `read_pdf` tool: reading a PDF on the host as its pages' text or as
//! images of them, with `pdf_oxide`, under the same path checks as the other
//! file tools ([`crate::paths`]).
//!
//! The limits and rendering settings follow grok-build's PDF reading, its
//! `read_file`'s, with a text budget added like `read_file`'s here.

use std::{
    fmt::Write as _,
    fs::File,
    io::Read as _,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Duration,
};

use agent::{
    model::{Image, ImageSource, ToolResultContent},
    tool::{Tool, ToolCall, ToolError, ToolErrorKind, ToolOutput},
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{
    Needs, ThreadSandbox,
    files::{PATHS, io_error, location, with_places},
    paths::{FileError, Places},
};

/// The largest PDF `read_pdf` opens. The whole file is read into memory and
/// parsed, so this bounds both.
const PDF_SOURCE_LIMIT: u64 = 50 * 1024 * 1024;

/// How long one call may take. A PDF built to be slow to parse or render
/// must not hold the run; the blocking thread can't be stopped, but the call
/// fails and the run goes on.
const PDF_TIMEOUT: Duration = Duration::from_secs(60);

/// The most pages a call without a range reads; a longer document asks for a
/// range instead, so the model learns its length before reading any of it.
const AUTO_READ_PAGES: u32 = 10;

/// The most pages one call reads.
const MAX_PAGES: u32 = 20;

/// How much text one call returns, in bytes. A read stops at a page boundary
/// past it and says where to go on, as `read_file` does at a line. Larger
/// than `read_file`'s limit, since a dense page is a few KB and a call may
/// read 20 of them.
const TEXT_LIMIT: usize = 64 * 1024;

/// A US Letter page at 150 DPI is 1275 × 1650 pixels: legible small print,
/// and within every provider's limits without scaling.
const RENDER_DPI: u32 = 150;

const RENDER_JPEG_QUALITY: u8 = 85;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadPdfArgs {
    /// The PDF to read, such as `myproject/docs/spec.pdf` or
    /// `scratch://paper.pdf`.
    pub path: String,
    /// The first page to read, counting from 1.
    #[serde(default)]
    pub first_page: Option<u32>,
    /// The last page to read, inclusive.
    #[serde(default)]
    pub last_page: Option<u32>,
    /// Whether to read the pages' text or see them as images.
    #[serde(default)]
    pub mode: PdfMode,
}

/// How `read_pdf` shows the pages it reads.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PdfMode {
    /// The text on each page, as the PDF lays it out.
    #[default]
    Text,
    /// Each page as an image, for figures, tables, scans, and layout.
    Images,
}

/// Reads a host PDF, excluding denied credentials.
pub struct ReadPdf;

impl ReadPdf {
    pub const NAME: &'static str = "read_pdf";
}

/// The pages go only in the model's content; the structured result keeps
/// what was read rather than a second copy of it, as `view_image`'s does.
#[derive(Debug, Serialize)]
pub struct ReadPdfOutput {
    pub path: String,
    /// How many pages the whole document has.
    pub page_count: u32,
    /// The pages read, counting from 1, inclusive.
    pub first_page: u32,
    pub last_page: u32,
    pub mode: PdfMode,
    #[serde(skip)]
    pages: Vec<PdfPage>,
    #[serde(skip)]
    stopped: Option<Stop>,
}

/// One page as the model is sent it.
#[derive(Debug)]
enum PdfPage {
    Text(String),
    Image(Image),
}

/// Why a read ended before the last page it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The range was longer than [`MAX_PAGES`].
    PageLimit,
    /// The next page's text would have passed [`TEXT_LIMIT`].
    TextLimit,
    /// The first page's text alone passed [`TEXT_LIMIT`] and was cut.
    PageCut,
}

impl ToolOutput for ReadPdfOutput {
    /// Says which pages of how many come next, so the model knows where to
    /// go on, then each page under its number, then why it stopped early.
    fn model_output(&self) -> Option<Vec<ToolResultContent>> {
        let pages = if self.first_page == self.last_page {
            format!("page {}", self.first_page)
        } else {
            format!("pages {}–{}", self.first_page, self.last_page)
        };
        let mut content = vec![ToolResultContent::Text {
            text: format!("{} ({pages} of {})", self.path, self.page_count),
        }];
        for (number, page) in (self.first_page..).zip(&self.pages) {
            let mut text = format!("\n--- Page {number} ---\n");
            match page {
                PdfPage::Text(page_text) => {
                    text.push_str(page_text);
                    content.push(ToolResultContent::Text { text });
                }
                PdfPage::Image(image) => {
                    content.push(ToolResultContent::Text { text });
                    content.push(ToolResultContent::Image(image.clone()));
                }
            }
        }
        if let Some(stop) = self.stopped {
            let last = self.last_page;
            let mut text = match stop {
                Stop::PageLimit => {
                    format!(
                        "\n\n[Stopped after page {last}: one call reads at most {MAX_PAGES} pages."
                    )
                }
                Stop::TextLimit => {
                    format!("\n\n[Stopped after page {last} to keep the result short.")
                }
                Stop::PageCut => {
                    format!("\n\n[Page {last}'s text is longer than the read limit and was cut.")
                }
            };
            if last < self.page_count {
                _ = write!(text, " Read on with first_page {}.", last + 1);
            }
            text.push(']');
            content.push(ToolResultContent::Text { text });
        }
        Some(content)
    }
}

impl Tool for ReadPdf {
    type Args = ReadPdfArgs;
    type Output = ReadPdfOutput;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "Read a PDF on the user's computer or in scratch. In `text` mode (the default) the \
             result is each page's text under its page number; in `images` mode it shows you \
             each page as an image, for figures, tables, scanned pages, and layout. The result \
             starts with how many pages the document has. Without first_page and last_page, \
             documents of up to {AUTO_READ_PAGES} pages are read whole; for longer ones the \
             result gives the page count, so read them in ranges. One call reads at most \
             {MAX_PAGES} pages, and long text stops at a page and says where to go on. Only \
             read paths you have seen in a listing, a command's output, or the conversation. \
             {PATHS}"
        )
    }

    async fn run(
        &self,
        _call: ToolCall<Needs>,
        sandbox: &Self::Context,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let path = args.path.clone();
        let read = with_places(sandbox, move |_, places| read_pdf(places, &args));
        match tokio::time::timeout(PDF_TIMEOUT, read).await {
            Ok(output) => Ok(output?),
            Err(_) => Err(ToolError::new(
                ToolErrorKind::Timeout,
                format!(
                    "Reading {path} took longer than {} seconds.",
                    PDF_TIMEOUT.as_secs()
                ),
            )),
        }
    }
}

pub(crate) fn read_pdf(places: &Places, args: &ReadPdfArgs) -> Result<ReadPdfOutput, FileError> {
    let file = location(places, &args.path, "a PDF file")?;
    let display = file.display();
    if file.real.is_dir() {
        return Err(FileError::Invalid(format!(
            "{display} is a folder, not a PDF file. Use list_directory to see what it holds."
        )));
    }
    let source = File::open(&file.real).map_err(|error| io_error(error, &file))?;
    let mut bytes = Vec::new();
    source
        .take(PDF_SOURCE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(error, &file))?;
    if bytes.len() as u64 > PDF_SOURCE_LIMIT {
        return Err(FileError::Invalid(format!(
            "{display} is larger than 50 MiB, too large to read. Use run_command to split it \
             in scratch, then read the part you need."
        )));
    }
    // A malformed file may make the parser panic; that fails this call only.
    catch_unwind(AssertUnwindSafe(|| {
        read_pages(bytes, display.clone(), args)
    }))
    .unwrap_or_else(|_| {
        Err(FileError::Failed(format!(
            "{display} can't be read: the PDF reader failed on it."
        )))
    })
}

fn read_pages(
    bytes: Vec<u8>,
    display: String,
    args: &ReadPdfArgs,
) -> Result<ReadPdfOutput, FileError> {
    let doc = pdf_oxide::PdfDocument::from_bytes(bytes).map_err(|error| {
        FileError::Invalid(format!("{display} can't be opened as a PDF: {error}"))
    })?;
    let page_count = doc
        .page_count()
        .map_err(|error| FileError::Invalid(format!("{display}'s pages can't be read: {error}")))?;
    let page_count = u32::try_from(page_count).unwrap_or(u32::MAX);
    let (first, last, mut stopped) = page_range(page_count, args.first_page, args.last_page)
        .map_err(|reason| FileError::Invalid(format!("{display} {reason}")))?;

    let render_options =
        pdf_oxide::rendering::RenderOptions::with_dpi(RENDER_DPI).as_jpeg(RENDER_JPEG_QUALITY);
    let mut pages = Vec::new();
    let mut text_used = 0;
    for number in first..=last {
        let index = number as usize - 1;
        match args.mode {
            PdfMode::Text => {
                let mut text = doc.extract_text(index).unwrap_or_else(|error| {
                    format!("[The text on this page can't be extracted: {error}]")
                });
                if text_used + text.len() > TEXT_LIMIT {
                    if !pages.is_empty() {
                        stopped = Some(Stop::TextLimit);
                        break;
                    }
                    let mut end = TEXT_LIMIT;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                    pages.push(PdfPage::Text(text));
                    stopped = Some(Stop::PageCut);
                    break;
                }
                text_used += text.len();
                pages.push(PdfPage::Text(text));
            }
            PdfMode::Images => {
                pages.push(
                    match pdf_oxide::rendering::render_page(&doc, index, &render_options) {
                        Ok(image) => PdfPage::Image(Image {
                            media_type: "image/jpeg".into(),
                            source: ImageSource::Base64 {
                                data: base64::engine::general_purpose::STANDARD.encode(&image.data),
                            },
                        }),
                        Err(error) => {
                            PdfPage::Text(format!("[This page can't be rendered: {error}]"))
                        }
                    },
                );
            }
        }
    }

    Ok(ReadPdfOutput {
        path: display,
        page_count,
        first_page: first,
        last_page: first + pages.len() as u32 - 1,
        mode: args.mode,
        pages,
        stopped,
    })
}

/// The pages a call reads, counting from 1, inclusive, and whether that
/// stops short of the range asked for; or why it reads none, to follow the
/// document's name.
///
/// Models sometimes count from 0, or end before they start; either way they
/// get at least the page they started at, as `read_file` does with lines.
fn page_range(
    page_count: u32,
    first_page: Option<u32>,
    last_page: Option<u32>,
) -> Result<(u32, u32, Option<Stop>), String> {
    if page_count == 0 {
        return Err("has no pages.".into());
    }
    if first_page.is_none() && last_page.is_none() && page_count > AUTO_READ_PAGES {
        return Err(format!(
            "has {page_count} pages. Read it in ranges of up to {MAX_PAGES} pages with \
             first_page and last_page."
        ));
    }
    let first = first_page.unwrap_or(1).max(1);
    if first > page_count {
        return Err(format!(
            "has {page_count} pages, so first_page {first} is past its end."
        ));
    }
    let last = last_page.unwrap_or(page_count).clamp(first, page_count);
    if last - first >= MAX_PAGES {
        return Ok((first, first + MAX_PAGES - 1, Some(Stop::PageLimit)));
    }
    Ok((first, last, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Access, Project, ProjectFolder, TestRoot};

    fn places(root: &TestRoot) -> Places {
        let zed = root.0.join("zed");
        std::fs::create_dir_all(&zed).unwrap();
        let project = Project {
            folders: vec![ProjectFolder {
                name: "zed".into(),
                path: zed,
            }],
            access: Access::Read,
        };
        Places::new(&project, &root.0.join("scratch"), None)
    }

    /// A PDF with one page per text, each in Helvetica, built by hand so the
    /// tests need no fixture files.
    fn pdf(pages: &[&str]) -> Vec<u8> {
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                (0..pages.len())
                    .map(|page| format!("{} 0 R", 4 + page * 2))
                    .collect::<Vec<_>>()
                    .join(" "),
                pages.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        for (page, text) in pages.iter().enumerate() {
            let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
                 /Resources << /Font << /F1 3 0 R >> >> >>",
                5 + page * 2
            ));
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }
        let mut file = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(file.len());
            file.extend(format!("{} 0 obj\n{object}\nendobj\n", index + 1).bytes());
        }
        let xref = file.len();
        file.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            file.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        file.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        file
    }

    /// What the model reads: text as it is, images as `[image/jpeg]`.
    fn read(
        places: &Places,
        path: &str,
        range: (Option<u32>, Option<u32>),
        mode: PdfMode,
    ) -> String {
        let output = read_pdf(
            places,
            &ReadPdfArgs {
                path: path.into(),
                first_page: range.0,
                last_page: range.1,
                mode,
            },
        )
        .unwrap();
        let typed = agent::tool::TypedToolOutput::new(ReadPdf::NAME, output).unwrap();
        typed
            .model_output
            .iter()
            .map(|content| match content {
                ToolResultContent::Text { text } => text.clone(),
                ToolResultContent::Image(image) => format!("[{}]", image.media_type),
                ToolResultContent::Json { value } => value.to_string(),
            })
            .collect()
    }

    #[test]
    fn ranges_default_to_short_documents_and_stop_at_the_page_limit() {
        assert_eq!(page_range(3, None, None), Ok((1, 3, None)));
        assert!(page_range(11, None, None).unwrap_err().contains("11 pages"));
        assert_eq!(page_range(11, Some(5), None), Ok((5, 11, None)));
        assert_eq!(page_range(11, Some(0), Some(2)), Ok((1, 2, None)));
        assert_eq!(page_range(11, Some(4), Some(2)), Ok((4, 4, None)));
        assert_eq!(page_range(11, None, Some(99)), Ok((1, 11, None)));
        assert_eq!(
            page_range(100, Some(10), None),
            Ok((10, 29, Some(Stop::PageLimit)))
        );
        assert!(
            page_range(11, Some(12), None)
                .unwrap_err()
                .contains("past its end")
        );
        assert!(page_range(0, None, None).is_err());
    }

    #[test]
    fn pages_come_back_as_text_or_images_under_their_numbers() {
        let root = TestRoot::new();
        let places = places(&root);
        std::fs::write(root.0.join("zed/doc.pdf"), pdf(&["Alpha", "Beta", "Delta"])).unwrap();

        let text = read(&places, "zed/doc.pdf", (Some(2), None), PdfMode::Text);
        assert!(text.starts_with("zed/doc.pdf (pages 2–3 of 3)"), "{text}");
        assert!(text.contains("--- Page 2 ---\nBeta"), "{text}");
        assert!(text.contains("--- Page 3 ---\nDelta"), "{text}");
        assert!(!text.contains("Alpha"), "{text}");

        let images = read(&places, "zed/doc.pdf", (Some(1), Some(1)), PdfMode::Images);
        assert_eq!(
            images,
            "zed/doc.pdf (page 1 of 3)\n--- Page 1 ---\n[image/jpeg]"
        );
    }

    #[test]
    fn long_text_stops_at_a_page_and_says_where_to_go_on() {
        let root = TestRoot::new();
        let places = places(&root);
        // pdf_oxide extracts at most 32 KiB from one text run, so each page
        // stays under that.
        let page = "x".repeat(TEXT_LIMIT / 3 + 1);
        std::fs::write(root.0.join("zed/long.pdf"), pdf(&[&page, &page, &page])).unwrap();

        let text = read(&places, "zed/long.pdf", (None, None), PdfMode::Text);
        assert!(text.starts_with("zed/long.pdf (pages 1–2 of 3)"), "{text}");
        assert!(
            text.ends_with(
                "[Stopped after page 2 to keep the result short. Read on with first_page 3.]"
            ),
            "{text}"
        );
    }

    #[test]
    fn non_pdfs_folders_and_long_documents_without_a_range_are_errors() {
        let root = TestRoot::new();
        let places = places(&root);
        std::fs::write(root.0.join("zed/notes.pdf"), "not a PDF").unwrap();
        std::fs::write(root.0.join("zed/long.pdf"), pdf(&["page"; 11])).unwrap();
        let args = |path: &str| ReadPdfArgs {
            path: path.into(),
            first_page: None,
            last_page: None,
            mode: PdfMode::Text,
        };

        for path in ["zed/notes.pdf", "zed", "zed/long.pdf"] {
            assert!(
                matches!(read_pdf(&places, &args(path)), Err(FileError::Invalid(_))),
                "{path}"
            );
        }
        assert!(matches!(
            read_pdf(&places, &args("zed/missing.pdf")),
            Err(FileError::NotFound(_))
        ));
    }
}
