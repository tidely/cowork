//! Helpers shared by the tests of several modules.

use std::collections::HashMap;

use draft::{AttachmentId, AttachmentRecord};
use uuid::Uuid;

use crate::attachments::{FileAttachment, FileAttachmentContent};

pub(crate) fn encoded_image(width: u32, format: image::ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(width, 2))
        .write_to(&mut std::io::Cursor::new(&mut bytes), format)
        .expect("encode test image");
    bytes
}

pub(crate) fn text_attachment(name: &str, text: &str) -> FileAttachment {
    FileAttachment {
        name: name.into(),
        content: FileAttachmentContent::Text(text.into()),
    }
}

/// Records for `files`, and the files by id, as a thread holds them.
pub(crate) fn attached(
    files: impl IntoIterator<Item = FileAttachment>,
) -> (Vec<AttachmentRecord>, HashMap<AttachmentId, FileAttachment>) {
    files
        .into_iter()
        .map(|file| {
            let record = AttachmentRecord {
                id: AttachmentId::new(),
                name: file.name.clone(),
                kind: file.kind(),
                size: file.len(),
                creator: Uuid::new_v4(),
            };
            (record.clone(), (record.id, file))
        })
        .unzip()
}
