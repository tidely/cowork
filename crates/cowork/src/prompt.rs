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

/// `prompt` with `note`, where the agent's commands run (see
/// [`sandbox::Sandboxes::environment_note`]), as its first part, unless the
/// latest note in `history` already says the same. The note travels in the
/// prompt rather than in the tool's definition or the system prompt, which
/// are part of the prompt prefix the model caches, and it stays in the
/// transcript, so later runs see what earlier ones were told.
pub(crate) fn with_environment_note(
    prompt: RigMessage,
    history: &[RigMessage],
    note: String,
) -> RigMessage {
    if latest_environment_note(history) == Some(note.as_str()) {
        return prompt;
    }
    match prompt {
        RigMessage::User { mut content } => {
            content.insert(0, UserContent::text(note));
            RigMessage::User { content }
        }
        other => other,
    }
}

fn latest_environment_note(history: &[RigMessage]) -> Option<&str> {
    history.iter().rev().find_map(|message| {
        let RigMessage::User { content } = message else {
            return None;
        };
        content.iter().find_map(|part| match part {
            UserContent::Text(text) if text.text.starts_with(sandbox::ENVIRONMENT_NOTE_START) => {
                Some(text.text.as_str())
            }
            _ => None,
        })
    })
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
    fn the_environment_note_is_sent_only_when_it_changed() {
        let note = |text: &str| format!("{}{text}", sandbox::ENVIRONMENT_NOTE_START);
        let texts = |message: &RigMessage| {
            let RigMessage::User { content } = message else {
                panic!("expected a user message");
            };
            content
                .iter()
                .map(|part| match part {
                    UserContent::Text(text) => text.text.clone(),
                    _ => panic!("expected only text parts"),
                })
                .collect::<Vec<_>>()
        };

        let first = with_environment_note(RigMessage::user("Hi"), &[], note("a"));
        assert_eq!(texts(&first), [note("a"), "Hi".into()]);

        let history = [first, RigMessage::user("later, without a note")];
        let same = with_environment_note(RigMessage::user("Again"), &history, note("a"));
        assert_eq!(texts(&same), ["Again"]);

        let changed = with_environment_note(RigMessage::user("Now"), &history, note("b"));
        assert_eq!(texts(&changed), [note("b"), "Now".into()]);
    }

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
        let RigMessage::User { content } =
            agent_message(Some("Comments first."), &blocks, &files, &names)
        else {
            panic!("expected user message");
        };
        let texts = content
            .iter()
            .map(|part| match part {
                UserContent::Text(text) => text.text.clone(),
                UserContent::Image(image) => {
                    assert_eq!(
                        image.data,
                        rig::message::DocumentSourceKind::Base64("AQID".into())
                    );
                    assert_eq!(image.media_type, Some(ImageMediaType::PNG));
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
