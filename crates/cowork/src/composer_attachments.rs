//! Attaching files to a draft: reading them, uploading them to the host,
//! and their cards in the composer.

use std::sync::Arc;

use draft::{AttachmentId, AttachmentKind, AttachmentRecord, ItemId};
use gpui::{
    App, AppContext, Context, Entity, ExternalPaths, Focusable, PathPromptOptions, Window,
    prelude::*, px,
};
use gpui_base::input::Paste;
use gpui_component::{
    Icon, Sizable as _,
    attachment::{
        Attachment, AttachmentActions, AttachmentContent, AttachmentDescription, AttachmentMedia,
        AttachmentStatus, AttachmentTitle,
    },
    button::{Button, ButtonVariants as _},
    progress::Progress,
};
use gpui_kit_assets::IconName as AssetIconName;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    Cowork,
    attachments::{
        AttachmentSource, FileAttachment, FileAttachmentContent, IncomingFile,
        MAX_MESSAGE_ATTACHMENT_BYTES, clipboard_attachment_sources, format_bytes, load_attachment,
    },
    protocol,
    thread::{Thread, ThreadSharing},
    thread_draft::{AttachmentTarget, EditorSlot, ThreadDraft},
};

/// What an attachment card shows, read out of a thread for rendering.
pub(crate) struct AttachmentCard {
    pub(crate) id: AttachmentId,
    pub(crate) name: String,
    pub(crate) kind: AttachmentKind,
    pub(crate) size: u64,
    pub(crate) image: Option<Arc<gpui::Image>>,
    transfer: Option<Transfer>,
}

/// A file on its way, with the percent done.
#[derive(Clone, Copy)]
enum Transfer {
    Uploading(f32),
    Downloading(f32),
}

impl AttachmentCard {
    pub(crate) fn new(draft: &ThreadDraft, record: &AttachmentRecord) -> Self {
        let file = draft.files.get(&record.id);
        let percent = |done: u64| {
            if record.size == 0 {
                100.
            } else {
                (done as f64 / record.size as f64 * 100.) as f32
            }
        };
        let transfer = if let Some(sent) = draft.uploads.get(&record.id) {
            Some(Transfer::Uploading(percent(*sent)))
        } else if file.is_none() {
            Some(Transfer::Downloading(
                draft
                    .incoming
                    .get(&record.id)
                    .map_or(0., IncomingFile::progress),
            ))
        } else {
            None
        };
        Self {
            id: record.id,
            name: record.name.clone(),
            kind: record.kind,
            size: record.size,
            image: file.and_then(|file| match &file.content {
                FileAttachmentContent::Png(image) | FileAttachmentContent::Jpeg(image) => {
                    Some(image.clone())
                }
                FileAttachmentContent::Text(_) => None,
            }),
            transfer,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PendingAttachment {
    pub(crate) id: Uuid,
    pub(crate) draft_id: Uuid,
    pub(crate) target: AttachmentTarget,
    pub(crate) name: String,
    pub(crate) is_image: bool,
    pub(crate) progress: Option<f32>,
}

enum AttachmentReadEvent {
    Progress(Uuid, f32),
    Finished(Uuid, anyhow::Result<FileAttachment>),
}

pub(crate) struct AttachmentError {
    pub(crate) draft_id: Uuid,
    pub(crate) message: String,
}

impl Cowork {
    /// Whether files are still on their way: being read by anyone, or not
    /// yet with the host. Either holds submission back.
    pub(crate) fn draft_is_loading_attachments(&self, draft_id: Uuid, cx: &App) -> bool {
        self.pending_attachments
            .iter()
            .any(|pending| pending.draft_id == draft_id)
            || self
                .read_draft(draft_id, cx, |draft| {
                    draft.others_are_reading_files() || draft.has_unstored_attachments()
                })
                .unwrap_or(false)
    }

    /// Where the attach button attaches to: the focused prompt block, or
    /// otherwise a new block. Buttons do not take focus, so the editor the
    /// user was typing in is still focused when one is clicked.
    pub(crate) fn attachment_target_at_focus(&self, window: &Window, cx: &App) -> AttachmentTarget {
        match self.focused_draft_editor(window, cx) {
            Some((_, EditorSlot::Prompt(id), _)) => AttachmentTarget::Block(id),
            _ => AttachmentTarget::NewBlock(Uuid::new_v4()),
        }
    }

    pub(crate) fn add_attachments(
        &mut self,
        draft_id: Uuid,
        target: AttachmentTarget,
        sources: Vec<AttachmentSource>,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        self.attachment_errors
            .retain(|error| error.draft_id != draft_id);
        let entries = sources
            .into_iter()
            .map(|source| {
                let id = Uuid::new_v4();
                self.pending_attachments.push(PendingAttachment {
                    id,
                    draft_id,
                    target,
                    name: source.name(),
                    is_image: source.looks_like_image(),
                    progress: None,
                });
                (id, source)
            })
            .collect::<Vec<_>>();
        self.publish_presence(cx);
        cx.notify();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        cx.background_executor()
            .spawn(async move {
                for (id, source) in entries {
                    let result = load_attachment(source, |progress| {
                        _ = sender.send(AttachmentReadEvent::Progress(id, progress));
                    });
                    _ = sender.send(AttachmentReadEvent::Finished(id, result));
                }
            })
            .detach();
        cx.spawn(async move |this, cx| {
            while let Some(event) = receiver.recv().await {
                let Ok(new_block) = this.update(cx, |this, cx| {
                    this.attachment_read_event(draft_id, event, cx)
                }) else {
                    break;
                };
                if let (Some(block), Some(window_handle)) = (new_block, window) {
                    _ = cx.update_window(window_handle, |_, window, cx| {
                        _ = this.update(cx, |this, cx| {
                            if matches!(
                                this.focused_draft_editor(window, cx),
                                Some((focused_draft, EditorSlot::DraftPosition, _))
                                    if focused_draft == draft_id
                            ) {
                                this.focus_draft_editor(
                                    draft_id,
                                    EditorSlot::Prompt(block),
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        });
                    });
                }
            }
        })
        .detach();
    }

    fn attachment_read_event(
        &mut self,
        draft_id: Uuid,
        event: AttachmentReadEvent,
        cx: &mut Context<Self>,
    ) -> Option<ItemId> {
        let id = match &event {
            AttachmentReadEvent::Progress(id, _) | AttachmentReadEvent::Finished(id, _) => *id,
        };
        let Some(index) = self
            .pending_attachments
            .iter()
            .position(|pending| pending.id == id)
        else {
            return None;
        };
        let mut new_block = None;
        match event {
            AttachmentReadEvent::Progress(_, progress) => {
                self.pending_attachments[index].progress = Some(progress);
            }
            AttachmentReadEvent::Finished(_, result) => {
                let target = self.pending_attachments.remove(index).target;
                let result = result.and_then(|attachment| {
                    self.update_draft(draft_id, cx, |draft| {
                        // By the records: others' files may not be here yet.
                        let total = draft
                            .attachment_records()
                            .iter()
                            .map(|record| record.size)
                            .sum::<u64>();
                        anyhow::ensure!(
                            total + attachment.len() <= MAX_MESSAGE_ATTACHMENT_BYTES,
                            "Cannot attach {}: attachments on one message can total at most {}",
                            attachment.name,
                            format_bytes(MAX_MESSAGE_ATTACHMENT_BYTES)
                        );
                        let block = draft.attachment_block(target);
                        let record = AttachmentRecord {
                            id: AttachmentId::new(),
                            name: attachment.name.clone(),
                            kind: attachment.kind(),
                            size: attachment.len(),
                            creator: draft.author.as_uuid(),
                        };
                        let id = record.id;
                        draft.files.insert(id, attachment);
                        draft.doc.add_attachment(block, record);
                        Ok(Some((id, block)))
                    })
                    // The draft is gone (sent, or its thread was closed).
                    .unwrap_or(Ok(None))
                });
                let result = result.map(|added| {
                    if let Some((id, block)) = added {
                        self.attachment_added(draft_id, id, cx);
                        if matches!(target, AttachmentTarget::NewBlock(_)) {
                            new_block = Some(block);
                        }
                    }
                });
                if let Err(error) = result {
                    self.attachment_errors.push(AttachmentError {
                        draft_id,
                        message: error.to_string(),
                    });
                }
            }
        }
        // Also sent from render, but a hidden window may not render, and
        // others cannot submit while a read is announced.
        self.publish_presence(cx);
        cx.notify();
        new_block
    }

    /// Gets a file the local user just attached to whoever needs it: the host
    /// announces it, as it already holds the bytes; a collaborator uploads
    /// it to the host, which announces it once it has every byte.
    fn attachment_added(&mut self, draft_id: Uuid, id: AttachmentId, cx: &mut Context<Self>) {
        let thread = self.draft_thread(draft_id, cx);
        let joined = thread.as_ref().is_some_and(|thread| {
            matches!(thread.read(cx).sharing, ThreadSharing::Connected { .. })
        });
        match thread {
            Some(thread) if joined => self.start_upload(thread, id, cx),
            Some(thread) => thread.update(cx, |thread, cx| {
                let uploader = thread.participant_id.into_bytes();
                thread.emit(
                    protocol::HostMessage::AttachmentStored {
                        id: id.as_uuid().into_bytes(),
                        uploader,
                    },
                    cx,
                );
            }),
            None => {
                self.update_draft(draft_id, cx, |draft| draft.stored.insert(id));
            }
        }
    }

    /// Sends a file of a joined thread's draft to its host, one chunk at a
    /// time. Stops early once the file is removed.
    fn start_upload(&mut self, thread: Entity<Thread>, id: AttachmentId, cx: &mut Context<Self>) {
        let ThreadSharing::Connected { uploads, .. } = &thread.read(cx).sharing else {
            return;
        };
        let uploads = uploads.clone();
        let thread = thread.downgrade();
        cx.spawn(async move |this, cx| {
            let mut offset = 0;
            loop {
                let Ok(chunk) = thread.update(cx, |thread, _| {
                    thread.draft.chunk(id, offset).filter(|_| {
                        thread
                            .draft
                            .attachment_records()
                            .iter()
                            .any(|record| record.id == id)
                    })
                }) else {
                    return;
                };
                let Some(chunk) = chunk else {
                    // Removed: lets the host discard what it has, which the
                    // bulk queue delivers after every chunk sent before.
                    _ = uploads
                        .send(protocol::CollaboratorMessage::AttachmentCancelled(
                            id.as_uuid().into_bytes(),
                        ))
                        .await;
                    return;
                };
                let next = chunk.offset + chunk.bytes.len() as u64;
                let total = chunk.total;
                if uploads
                    .send(protocol::CollaboratorMessage::AttachmentData(chunk))
                    .await
                    .is_err()
                {
                    return;
                }
                let updated = thread.update(cx, |thread, _| {
                    // Until the host confirms it has every byte.
                    if !thread.draft.stored.contains(&id) {
                        thread.draft.uploads.insert(id, next);
                    }
                });
                if updated.is_err() || this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
                if next >= total {
                    return;
                }
                offset = next;
            }
        })
        .detach();
    }

    /// The thread whose draft is `draft_id`, if it has one yet.
    fn draft_thread(&self, draft_id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        self.thread_store
            .read(cx)
            .threads
            .iter()
            .find(|thread| thread.read(cx).draft.id == draft_id)
            .cloned()
    }

    /// Removes an attachment, and its block too if that leaves the block
    /// empty while nobody is typing in it.
    pub(crate) fn remove_attachment(
        &mut self,
        draft_id: Uuid,
        block: ItemId,
        attachment: AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let block_focused = self
            .read_draft(draft_id, cx, |draft| {
                draft
                    .editor(EditorSlot::Prompt(block))
                    .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
            })
            .unwrap_or(false);
        self.update_draft(draft_id, cx, |draft| {
            draft.doc.remove_attachment(attachment);
            draft.drop_removed_file(attachment);
            if !block_focused {
                draft.remove_if_unattended(block);
            }
        });
        self.attachment_errors
            .retain(|error| error.draft_id != draft_id);
        cx.notify();
    }

    pub(crate) fn pick_attachments(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach text or images".into()),
        });
        let target = self.attachment_target_at_focus(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = selected.await;
            _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(Some(paths))) => this.add_attachments(
                        draft_id,
                        target,
                        paths.into_iter().map(AttachmentSource::Path).collect(),
                        Some(window.window_handle()),
                        cx,
                    ),
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(error)) => {
                        this.attachment_errors.push(AttachmentError {
                            draft_id,
                            message: format!("Could not choose files: {error}"),
                        });
                        cx.notify();
                    }
                }
                this.ensure_composer_focus(window, cx);
            });
        })
        .detach();
    }

    pub(crate) fn drop_attachments(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        // Drops onto a block are handled by the block itself; elsewhere,
        // attach to the block the user is editing when there is one.
        let target = self.attachment_target_at_focus(window, cx);
        self.drop_attachments_on(draft_id, target, paths, window, cx);
    }

    pub(crate) fn drop_attachments_on(
        &mut self,
        draft_id: Uuid,
        target: AttachmentTarget,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_attachments(
            draft_id,
            target,
            paths
                .paths()
                .iter()
                .cloned()
                .map(AttachmentSource::Path)
                .collect(),
            Some(window.window_handle()),
            cx,
        );
        if let AttachmentTarget::Block(id) = target {
            if !matches!(
                self.focused_draft_editor(window, cx),
                Some((focused_draft, EditorSlot::Prompt(focused_id), _))
                    if focused_draft == draft_id && focused_id == id
            ) {
                self.focus_draft_editor(draft_id, EditorSlot::Prompt(id), None, window, cx);
            }
        } else {
            self.ensure_composer_focus(window, cx);
        }
    }

    /// Runs before an editor's own paste so images and copied files become
    /// attachments of the block being typed in; plain text, and anything
    /// pasted into a comment, falls through to the text input.
    pub(crate) fn paste_attachments(
        &mut self,
        _: &Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((draft_id, slot, _)) = self.focused_draft_editor(window, cx) else {
            return;
        };
        let target = match slot {
            EditorSlot::Prompt(id) => AttachmentTarget::Block(id),
            EditorSlot::DraftPosition => AttachmentTarget::NewBlock(Uuid::new_v4()),
            EditorSlot::CommentInline(_) | EditorSlot::CommentComposer(_) => return,
        };
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let sources = clipboard_attachment_sources(&item);
        if sources.is_empty() {
            return;
        }
        // Files are not pasted as text even where they cannot be attached.
        cx.stop_propagation();
        self.add_attachments(draft_id, target, sources, Some(window.window_handle()), cx);
    }

    pub(crate) fn render_pending_attachment(pending: &PendingAttachment) -> Attachment {
        let icon = if pending.is_image {
            AssetIconName::Image
        } else {
            AssetIconName::FileText
        };
        let description = pending.progress.map_or_else(
            || "Preparing".to_owned(),
            |progress| format!("Reading · {progress:.0}%"),
        );
        Attachment::new()
            .xsmall()
            .status(if pending.progress.is_some() {
                AttachmentStatus::Uploading
            } else {
                AttachmentStatus::Pending
            })
            .media(AttachmentMedia::new().child(Icon::new(icon)))
            .content(
                AttachmentContent::new()
                    .title(
                        AttachmentTitle::new(pending.name.clone())
                            .status(AttachmentStatus::Complete),
                    )
                    .description(AttachmentDescription::new(description))
                    .child(
                        Progress::new(format!("attachment-progress-{}", pending.id))
                            .xsmall()
                            .w(px(140.))
                            .loading(pending.progress.is_none())
                            .value(pending.progress.unwrap_or(0.))
                            .accessibility_label(format!("Reading {}", pending.name)),
                    ),
            )
    }

    /// An attachment card, with a remove button when `removal` names the
    /// draft and block it can be removed from.
    pub(crate) fn render_attachment(
        &self,
        attachment: &AttachmentCard,
        removal: Option<(Uuid, ItemId, AttachmentId)>,
        cx: &mut Context<Self>,
    ) -> Attachment {
        let size = format_bytes(attachment.size);
        let label = match attachment.kind {
            AttachmentKind::Text => "Text",
            AttachmentKind::Png => "PNG",
            AttachmentKind::Jpeg => "JPEG",
        };
        let media = match (&attachment.image, attachment.kind) {
            (Some(image), _) => AttachmentMedia::new().src(image.clone()),
            (None, AttachmentKind::Text) => {
                AttachmentMedia::new().child(Icon::new(AssetIconName::FileText))
            }
            (None, _) => AttachmentMedia::new().child(Icon::new(AssetIconName::Image)),
        };
        let (description, progress) = match attachment.transfer {
            None => (format!("{label} · {size}"), None),
            Some(Transfer::Uploading(progress)) => {
                (format!("Uploading · {progress:.0}%"), Some(progress))
            }
            Some(Transfer::Downloading(progress)) => {
                (format!("Downloading · {progress:.0}%"), Some(progress))
            }
        };
        let mut content = AttachmentContent::new()
            .title(AttachmentTitle::new(attachment.name.clone()))
            .description(AttachmentDescription::new(description));
        if let Some(progress) = progress {
            content = content.child(
                Progress::new(format!("attachment-transfer-{}", attachment.id))
                    .xsmall()
                    .w(px(140.))
                    .value(progress)
                    .accessibility_label(format!("Transferring {}", attachment.name)),
            );
        }
        let mut card = Attachment::new()
            .xsmall()
            .when_some(attachment.transfer, |card, transfer| {
                card.status(match transfer {
                    Transfer::Uploading(_) => AttachmentStatus::Uploading,
                    Transfer::Downloading(_) => AttachmentStatus::Pending,
                })
            })
            .media(media)
            .content(content);
        if let Some((draft_id, block, attachment_id)) = removal {
            card = card.actions(
                AttachmentActions::new().child(
                    Button::new(format!("remove-attachment-{attachment_id}"))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(AssetIconName::X))
                        .accessibility_label(format!("Remove {}", attachment.name))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_attachment(draft_id, block, attachment_id, window, cx);
                            this.ensure_composer_focus(window, cx);
                        })),
                ),
            );
        }
        card
    }
}
