//! The user message sent to the agent for a submission.

use std::collections::HashMap;

use agent::model::{Content, Image, ImageSource};
use base64::Engine as _;
use draft::AttachmentId;
use gpui::SharedString;

use crate::{
    PromptBlock,
    attachments::{FileAttachment, FileAttachmentContent},
    participant::ParticipantId,
};

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The content of the user message sent to the agent for one submission:
/// the comment instructions, then every prompt block under its creator's
/// name, each followed by its own attachments, whose bytes come from
/// `files`.
pub(crate) fn agent_message(
    preface: Option<&str>,
    blocks: &[PromptBlock],
    files: &HashMap<AttachmentId, FileAttachment>,
    names: &HashMap<ParticipantId, SharedString>,
) -> Vec<Content> {
    let mut content = preface.map(Content::text).into_iter().collect::<Vec<_>>();
    for block in blocks {
        content.push(Content::text(format!(
            "{}:\n{}",
            prompt_name(names, block.author),
            block.text
        )));
        content.extend(
            block
                .attachments
                .iter()
                .filter_map(|record| files.get(&record.id))
                .map(attachment_content),
        );
    }
    content
}

/// `participant`'s name in prompts; see [`Thread::prompt_names`].
pub(crate) fn prompt_name(
    names: &HashMap<ParticipantId, SharedString>,
    participant: ParticipantId,
) -> SharedString {
    names
        .get(&participant)
        .cloned()
        // Everyone is named before their items are sent, so this is only a
        // fallback that is stable as well.
        .unwrap_or_else(|| participant.display_name().into())
}

fn attachment_content(attachment: &FileAttachment) -> Content {
    let image = |media_type: &str, bytes: &[u8]| {
        Content::Image(Image {
            media_type: media_type.into(),
            source: ImageSource::Base64 {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        })
    };
    match &attachment.content {
        FileAttachmentContent::Text(body) => Content::text(format!(
            "<file name=\"{}\">\n{body}\n</file>",
            escape_xml_attribute(&attachment.name)
        )),
        FileAttachmentContent::Png(png) => image("image/png", png.bytes()),
        FileAttachmentContent::Jpeg(jpeg) => image("image/jpeg", jpeg.bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use uuid::Uuid;

    use crate::test_support::{attached, text_attachment};

    /// Comments come first, then each block under its creator's name with its
    /// own attachments right after it: text files as escaped `<file>`
    /// elements and images as base64 parts.
    #[test]
    fn agent_message_keeps_attachments_with_their_blocks() {
        let alice = ParticipantId::from_bytes([7; 16]);
        let bob = ParticipantId::new();
        let (records, files) = attached([
            text_attachment("say \"hi\".log", "boom"),
            FileAttachment {
                name: "photo.png".into(),
                content: FileAttachmentContent::Png(Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    vec![1, 2, 3],
                ))),
            },
        ]);
        let blocks = [
            PromptBlock {
                id: Uuid::new_v4(),
                author: alice,
                text: "Investigate the crash.".into(),
                attachments: records,
            },
            PromptBlock {
                id: Uuid::new_v4(),
                author: bob,
                text: "Also check the logs.".into(),
                attachments: Vec::new(),
            },
        ];
        let names = HashMap::from([(bob, SharedString::from("Bob"))]);
        let content = agent_message(Some("Comments first."), &blocks, &files, &names);
        let texts = content
            .iter()
            .map(|part| match part {
                Content::Text { text } => text.clone(),
                Content::Image(image) => {
                    assert_eq!(
                        image.source,
                        ImageSource::Base64 {
                            data: "AQID".into()
                        }
                    );
                    assert_eq!(image.media_type, "image/png");
                    "[image]".to_owned()
                }
                _ => panic!("expected only text and image parts"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "Comments first.".to_owned(),
                "Mossy Crane:\nInvestigate the crash.".to_owned(),
                "<file name=\"say &quot;hi&quot;.log\">\nboom\n</file>".to_owned(),
                "[image]".to_owned(),
                "Bob:\nAlso check the logs.".to_owned(),
            ]
        );
    }
}
