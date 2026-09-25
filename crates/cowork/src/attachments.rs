//! Files attached to prompt blocks: reading them from disk or the
//! clipboard, classifying them by content, and their size limits.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use draft::{AttachmentId, AttachmentKind, AttachmentRecord};
use gpui::{ClipboardEntry, ClipboardItem};
use uuid::Uuid;

use crate::{participant::ParticipantId, protocol};

/// Largest file or clipboard image we will read before inspecting it. Larger
/// than the image limit so uncompressed screenshots (BMP, TIFF) can still be
/// converted to PNG.
const MAX_ATTACHMENT_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest encoded image sent to the model, in line with common provider limits.
pub(crate) const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// Text is inlined into the prompt, so keep it well within `OLLAMA_CONTEXT_TOKENS`.
pub(crate) const MAX_TEXT_ATTACHMENT_BYTES: u64 = 256 * 1024;
/// Largest total of the attachments submitted together.
pub(crate) const MAX_MESSAGE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct FileAttachment {
    pub(crate) name: String,
    pub(crate) content: FileAttachmentContent,
}

/// Images are held as `gpui::Image` so thumbnails are decoded and cached once
/// rather than rehashed on every frame.
#[derive(Clone)]
pub(crate) enum FileAttachmentContent {
    Text(String),
    Png(Arc<gpui::Image>),
    Jpeg(Arc<gpui::Image>),
}

impl FileAttachment {
    pub(crate) fn len(&self) -> u64 {
        match &self.content {
            FileAttachmentContent::Text(text) => text.len() as u64,
            FileAttachmentContent::Png(image) | FileAttachmentContent::Jpeg(image) => {
                image.bytes().len() as u64
            }
        }
    }

    /// The file's bytes as they travel between participants.
    pub(crate) fn bytes(&self) -> &[u8] {
        match &self.content {
            FileAttachmentContent::Text(text) => text.as_bytes(),
            FileAttachmentContent::Png(image) | FileAttachmentContent::Jpeg(image) => image.bytes(),
        }
    }

    pub(crate) fn kind(&self) -> AttachmentKind {
        match self.content {
            FileAttachmentContent::Text(_) => AttachmentKind::Text,
            FileAttachmentContent::Png(_) => AttachmentKind::Png,
            FileAttachmentContent::Jpeg(_) => AttachmentKind::Jpeg,
        }
    }

    /// Rebuilds a file another participant sent.
    pub(crate) fn from_bytes(
        name: String,
        kind: AttachmentKind,
        bytes: Vec<u8>,
    ) -> anyhow::Result<Self> {
        let content =
            match kind {
                AttachmentKind::Text => FileAttachmentContent::Text(
                    String::from_utf8(bytes).context("A text attachment is not UTF-8.")?,
                ),
                AttachmentKind::Png => FileAttachmentContent::Png(Arc::new(
                    gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes),
                )),
                AttachmentKind::Jpeg => FileAttachmentContent::Jpeg(Arc::new(
                    gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes),
                )),
            };
        Ok(Self { name, content })
    }
}

/// The largest file of a kind anyone may attach.
pub(crate) fn max_attachment_size(kind: AttachmentKind) -> u64 {
    match kind {
        AttachmentKind::Text => MAX_TEXT_ATTACHMENT_BYTES,
        AttachmentKind::Png | AttachmentKind::Jpeg => MAX_IMAGE_ATTACHMENT_BYTES,
    }
}

pub(crate) fn kind_to_protocol(kind: AttachmentKind) -> protocol::AttachmentKind {
    match kind {
        AttachmentKind::Text => protocol::AttachmentKind::Text,
        AttachmentKind::Png => protocol::AttachmentKind::Png,
        AttachmentKind::Jpeg => protocol::AttachmentKind::Jpeg,
    }
}

pub(crate) fn kind_from_protocol(kind: protocol::AttachmentKind) -> AttachmentKind {
    match kind {
        protocol::AttachmentKind::Text => AttachmentKind::Text,
        protocol::AttachmentKind::Png => AttachmentKind::Png,
        protocol::AttachmentKind::Jpeg => AttachmentKind::Jpeg,
    }
}

pub(crate) fn record_to_protocol(record: &AttachmentRecord) -> protocol::AttachmentRef {
    protocol::AttachmentRef {
        id: record.id.as_uuid().into_bytes(),
        name: record.name.clone(),
        kind: kind_to_protocol(record.kind),
        size: record.size,
        creator: record.creator.into_bytes(),
    }
}

pub(crate) fn record_from_protocol(record: protocol::AttachmentRef) -> AttachmentRecord {
    AttachmentRecord {
        id: AttachmentId::from_uuid(Uuid::from_bytes(record.id)),
        name: record.name,
        kind: kind_from_protocol(record.kind),
        size: record.size,
        creator: Uuid::from_bytes(record.creator),
    }
}

/// A file whose bytes are still arriving from another participant.
pub(crate) struct IncomingFile {
    pub(crate) name: String,
    pub(crate) kind: AttachmentKind,
    pub(crate) total: u64,
    pub(crate) bytes: Vec<u8>,
    /// Who is sending it; `None` when it comes from the host.
    pub(crate) uploader: Option<ParticipantId>,
}

impl IncomingFile {
    pub(crate) fn progress(&self) -> f32 {
        if self.total == 0 {
            return 100.;
        }
        (self.bytes.len() as f64 / self.total as f64 * 100.) as f32
    }
}

pub(crate) enum AttachmentSource {
    Path(PathBuf),
    ClipboardImage(gpui::Image),
}

impl AttachmentSource {
    pub(crate) fn name(&self) -> String {
        match self {
            Self::Path(path) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            Self::ClipboardImage(_) => "Pasted image".to_owned(),
        }
    }

    pub(crate) fn looks_like_image(&self) -> bool {
        match self {
            Self::Path(path) => image::ImageFormat::from_path(path).is_ok(),
            Self::ClipboardImage(_) => true,
        }
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

pub(crate) fn load_attachment(
    source: AttachmentSource,
    on_progress: impl FnMut(f32),
) -> anyhow::Result<FileAttachment> {
    let name = source.name();
    match source {
        AttachmentSource::Path(path) => {
            let bytes = read_attachment_file(&path, &name, on_progress)?;
            attachment_from_bytes(name, bytes)
        }
        AttachmentSource::ClipboardImage(image) => {
            anyhow::ensure!(
                image.bytes.len() as u64 <= MAX_ATTACHMENT_SOURCE_BYTES,
                "The pasted image is larger than {}",
                format_bytes(MAX_ATTACHMENT_SOURCE_BYTES)
            );
            let content = image_content(image.bytes)
                .map_err(|_| anyhow::anyhow!("The pasted image format is not supported"))?;
            let extension = match content {
                FileAttachmentContent::Jpeg(_) => "jpg",
                _ => "png",
            };
            let attachment = FileAttachment {
                name: format!("{name}.{extension}"),
                content,
            };
            ensure_image_size(&attachment)?;
            Ok(attachment)
        }
    }
}

fn read_attachment_file(
    path: &Path,
    name: &str,
    mut on_progress: impl FnMut(f32),
) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("Cannot read {name}"))?;
    let total = file.metadata().ok().map(|metadata| metadata.len());
    let too_large = || {
        anyhow::anyhow!(
            "{name} is larger than {}",
            format_bytes(MAX_ATTACHMENT_SOURCE_BYTES)
        )
    };
    if total.is_some_and(|total| total > MAX_ATTACHMENT_SOURCE_BYTES) {
        return Err(too_large());
    }
    // Read one byte past the limit so files that grew after `metadata` are caught.
    let mut file = file.take(MAX_ATTACHMENT_SOURCE_BYTES + 1);
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut buffer = [0; 256 * 1024];
    let mut last_progress = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Cannot read {name}"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() as u64 > MAX_ATTACHMENT_SOURCE_BYTES {
            return Err(too_large());
        }
        if let Some(total) = total.filter(|total| *total > 0) {
            let progress = ((bytes.len() as f64 / total as f64) * 100.).min(100.) as u32;
            if progress > last_progress {
                last_progress = progress;
                on_progress(progress as f32);
            }
        }
    }
    Ok(bytes)
}

/// Classifies an attachment by its content rather than its extension: images
/// are recognized by their signature, anything else must be UTF-8 text.
fn attachment_from_bytes(name: String, bytes: Vec<u8>) -> anyhow::Result<FileAttachment> {
    match image_content(bytes) {
        Ok(content) => {
            let attachment = FileAttachment { name, content };
            ensure_image_size(&attachment)?;
            Ok(attachment)
        }
        Err(bytes) => {
            let text = String::from_utf8(bytes)
                .ok()
                .filter(|text| !text.contains('\0'))
                .with_context(|| format!("{name} is not a text file or a supported image"))?;
            anyhow::ensure!(
                text.len() as u64 <= MAX_TEXT_ATTACHMENT_BYTES,
                "{name} is {}; text files can be at most {}",
                format_bytes(text.len() as u64),
                format_bytes(MAX_TEXT_ATTACHMENT_BYTES)
            );
            Ok(FileAttachment {
                name,
                content: FileAttachmentContent::Text(text),
            })
        }
    }
}

fn ensure_image_size(attachment: &FileAttachment) -> anyhow::Result<()> {
    anyhow::ensure!(
        attachment.len() <= MAX_IMAGE_ATTACHMENT_BYTES,
        "{} is {}; images can be at most {}",
        attachment.name,
        format_bytes(attachment.len()),
        format_bytes(MAX_IMAGE_ATTACHMENT_BYTES)
    );
    Ok(())
}

/// Keeps PNG and JPEG as they are and converts other common image formats to
/// PNG. Hands the bytes back when they are not a supported image.
fn image_content(bytes: Vec<u8>) -> Result<FileAttachmentContent, Vec<u8>> {
    let format = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Png) => {
            return Ok(FileAttachmentContent::Png(Arc::new(
                gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes),
            )));
        }
        Ok(image::ImageFormat::Jpeg) => {
            return Ok(FileAttachmentContent::Jpeg(Arc::new(
                gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes),
            )));
        }
        // Signatures like BMP's "BM" can also start a text file, so these
        // only count as images if they actually decode.
        Ok(
            format @ (image::ImageFormat::Gif
            | image::ImageFormat::WebP
            | image::ImageFormat::Bmp
            | image::ImageFormat::Tiff),
        ) => format,
        _ => return Err(bytes),
    };
    let Ok(decoded) = image::load_from_memory_with_format(&bytes, format) else {
        return Err(bytes);
    };
    let mut png = Vec::new();
    if decoded
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .is_err()
    {
        return Err(bytes);
    }
    Ok(FileAttachmentContent::Png(Arc::new(
        gpui::Image::from_bytes(gpui::ImageFormat::Png, png),
    )))
}

/// Files copied in a file manager are attached, as are images unless the
/// clipboard also holds text (spreadsheets put a rendered image next to the
/// cells, for example). Everything else is left to the regular text paste.
pub(crate) fn clipboard_attachment_sources(item: &ClipboardItem) -> Vec<AttachmentSource> {
    let paths = item
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::ExternalPaths(paths) => Some(paths.paths()),
            _ => None,
        })
        .flatten()
        .cloned()
        .map(AttachmentSource::Path)
        .collect::<Vec<_>>();
    if !paths.is_empty() {
        return paths;
    }
    let has_text = item.entries().iter().any(
        |entry| matches!(entry, ClipboardEntry::String(text) if !text.text().trim().is_empty()),
    );
    if has_text {
        return Vec::new();
    }
    item.entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(AttachmentSource::ClipboardImage(image.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ExternalPaths;

    use crate::test_support::encoded_image;

    #[test]
    fn attachments_are_classified_by_content() {
        let source = attachment_from_bytes("main.rs".into(), b"fn main() {}".to_vec())
            .expect("any UTF-8 file is text");
        assert!(matches!(source.content, FileAttachmentContent::Text(_)));

        let starts_like_bmp = attachment_from_bytes("notes".into(), b"BMW notes".to_vec())
            .expect("text with an image-like signature is still text");
        assert!(matches!(
            starts_like_bmp.content,
            FileAttachmentContent::Text(_)
        ));

        let binary = attachment_from_bytes("app.exe".into(), vec![0x4d, 0x5a, 0x00, 0xff]);
        assert!(binary.is_err());

        let png = encoded_image(2, image::ImageFormat::Png);
        let kept = attachment_from_bytes("photo".into(), png.clone()).expect("png");
        assert!(matches!(kept.content, FileAttachmentContent::Png(image) if image.bytes() == png));

        let jpeg =
            attachment_from_bytes("photo".into(), encoded_image(2, image::ImageFormat::Jpeg))
                .expect("jpeg");
        assert!(matches!(jpeg.content, FileAttachmentContent::Jpeg(_)));

        let bmp = attachment_from_bytes(
            "screen.bmp".into(),
            encoded_image(2, image::ImageFormat::Bmp),
        )
        .expect("bmp");
        assert!(matches!(bmp.content, FileAttachmentContent::Png(image)
            if image::guess_format(image.bytes()).ok() == Some(image::ImageFormat::Png)));
    }

    #[test]
    fn oversized_text_attachments_are_rejected() {
        let limit = MAX_TEXT_ATTACHMENT_BYTES as usize;
        assert!(attachment_from_bytes("ok.txt".into(), vec![b'a'; limit]).is_ok());
        let error = attachment_from_bytes("big.txt".into(), vec![b'a'; limit + 1])
            .err()
            .expect("text over the limit")
            .to_string();
        assert_eq!(error, "big.txt is 256 KB; text files can be at most 256 KB");
    }

    #[test]
    fn reading_attachment_file_reports_progress() {
        let path = std::env::temp_dir().join(format!("cowork-{}.txt", Uuid::new_v4()));
        let body = "a".repeat(400_000);
        std::fs::write(&path, &body).expect("write test attachment");
        let mut progress = Vec::new();
        let result = read_attachment_file(&path, "test.txt", |value| progress.push(value));
        std::fs::remove_file(&path).expect("remove test attachment");
        assert_eq!(result.expect("read attachment"), body.as_bytes());
        assert!(progress.len() >= 2);
        assert!(progress.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(progress.last(), Some(&100.));
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(400_000), "391 KB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10 MB");
    }

    #[test]
    fn clipboard_prefers_files_then_text_then_images() {
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![1]);
        let image_only = ClipboardItem::new_image(&image);
        assert!(matches!(
            clipboard_attachment_sources(&image_only).as_slice(),
            [AttachmentSource::ClipboardImage(_)]
        ));

        let text_only = ClipboardItem::new_string("hello".into());
        assert!(clipboard_attachment_sources(&text_only).is_empty());

        let mut text_and_image = ClipboardItem::new_string("A1\tB1".into());
        text_and_image
            .entries
            .push(ClipboardEntry::Image(image.clone()));
        assert!(clipboard_attachment_sources(&text_and_image).is_empty());

        let mut files_and_text = ClipboardItem::new_string("notes.txt".into());
        files_and_text
            .entries
            .push(ClipboardEntry::ExternalPaths(ExternalPaths(
                vec![PathBuf::from("notes.txt")].into(),
            )));
        assert!(matches!(
            clipboard_attachment_sources(&files_and_text).as_slice(),
            [AttachmentSource::Path(path)] if path == Path::new("notes.txt")
        ));
    }
}
