//! Helpers shared by the tests of several modules.

use std::{collections::HashMap, sync::Arc};

use draft::{AttachmentId, AttachmentRecord};
use uuid::Uuid;

use crate::{
    attachments::{FileAttachment, FileAttachmentContent},
    protocol,
    thread::{Thread, ThreadSharing},
};

/// Simulates a client that does not honor its local permission or epoch guards.
/// The real host receiver still validates every request delivered this way.
pub(crate) fn request_unchecked(thread: &Thread, request: protocol::CollaboratorMessage) -> bool {
    let ThreadSharing::Connected { host, .. } = &thread.sharing else {
        panic!("a connected test peer");
    };
    host.try_send(request).is_ok()
}

/// Sandboxes for a test `Cowork`. Nothing is set up until a command runs, and
/// tests run none, so the home is never created.
pub(crate) fn unused_sandboxes() -> Arc<sandbox::Sandboxes> {
    Arc::new(sandbox::Sandboxes::new(
        std::env::temp_dir().join("cowork-test-sandboxes-unused"),
        "test",
    ))
}

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
