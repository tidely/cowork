//! The user message sent to the agent for a submission.

use std::collections::HashMap;

use base64::Engine as _;
use draft::AttachmentId;
use gpui::SharedString;
use rig::completion::{
    Message as RigMessage,
    message::{ImageMediaType, UserContent},
};

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

/// The user message sent to the agent for one submission: the comment
/// instructions, then every prompt block under its creator's name, each
/// followed by its own attachments, whose bytes come from `files`.
pub(crate) fn agent_message(
    preface: Option<&str>,
    blocks: &[PromptBlock],
    files: &HashMap<AttachmentId, FileAttachment>,
    names: &HashMap<ParticipantId, SharedString>,
) -> RigMessage {
    let mut content = preface
        .map(UserContent::text)
        .into_iter()
        .collect::<Vec<_>>();
    for block in blocks {
        content.push(UserContent::text(format!(
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
    RigMessage::User { content }
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

fn attachment_content(attachment: &FileAttachment) -> UserContent {
    match &attachment.content {
        FileAttachmentContent::Text(body) => UserContent::text(format!(
            "<file name=\"{}\">\n{body}\n</file>",
            escape_xml_attribute(&attachment.name)
        )),
        FileAttachmentContent::Png(image) => UserContent::image_base64(
            base64::engine::general_purpose::STANDARD.encode(image.bytes()),
            Some(ImageMediaType::PNG),
            None,
        ),
        FileAttachmentContent::Jpeg(image) => UserContent::image_base64(
            base64::engine::general_purpose::STANDARD.encode(image.bytes()),
            Some(ImageMediaType::JPEG),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use uuid::Uuid;

    use crate::test_support::{attached, text_attachment};

    #[test]
    fn attachments_become_ollama_text_and_base64_image_parts() {
        let author = ParticipantId::from_bytes([7; 16]);
        let (records, files) = attached([
            text_attachment("say \"hi\".txt", "hello"),
            FileAttachment {
                name: "photo.png".into(),
                content: FileAttachmentContent::Png(Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    vec![1, 2, 3],
                ))),
            },
        ]);
        let block = PromptBlock {
            id: Uuid::new_v4(),
            author,
            text: "Question".into(),
            attachments: records,
        };
        let RigMessage::User { content } = agent_message(None, &[block], &files, &HashMap::new())
        else {
            panic!("expected user message");
        };
        assert_eq!(content.len(), 3);
        assert!(
            matches!(&content[0], UserContent::Text(text) if text.text == "Mossy Crane:\nQuestion")
        );
        assert!(
            matches!(&content[1], UserContent::Text(text) if text.text == "<file name=\"say &quot;hi&quot;.txt\">\nhello\n</file>")
        );
        assert!(matches!(&content[2], UserContent::Image(image)
            if image.data == rig::message::DocumentSourceKind::Base64("AQID".into())
                && image.media_type == Some(ImageMediaType::PNG)));
    }

    /// Comments come first, then each block under its creator's name with its
    /// own attachments right after it.
    #[test]
    fn agent_message_keeps_attachments_with_their_blocks() {
        let alice = ParticipantId::from_bytes([7; 16]);
        let bob = ParticipantId::new();
        let (records, files) = attached([text_attachment("crash.log", "boom")]);
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
        let RigMessage::User { content } =
            agent_message(Some("Comments first."), &blocks, &files, &names)
        else {
            panic!("expected user message");
        };
        let texts = content
            .iter()
            .map(|part| match part {
                UserContent::Text(text) => text.text.clone(),
                _ => panic!("expected only text parts"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "Comments first.".to_owned(),
                "Mossy Crane:\nInvestigate the crash.".to_owned(),
                "<file name=\"crash.log\">\nboom\n</file>".to_owned(),
                "Bob:\nAlso check the logs.".to_owned(),
            ]
        );
    }
}
