//! A thread's pending request: the shared draft document, the bytes of
//! its files, the editors showing its items, and everyone's presence.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    time::{Duration, Instant},
};

use draft::{AttachmentId, AttachmentRecord, Draft, DraftItemKind, ItemId, TextEdit};
use gpui::{Entity, EntityId};
use gpui_base::input::TextareaState;
use uuid::Uuid;

use crate::{
    attachments::{
        FileAttachment, IncomingFile, MAX_MESSAGE_ATTACHMENT_BYTES, kind_from_protocol,
        kind_to_protocol, max_attachment_size,
    },
    participant::ParticipantId,
    protocol,
    timeline::{CommentReference, UserComment, UserCommentBody},
};

/// A thread's pending request: the shared draft document, the bytes of the
/// thread's files, and the local editors showing its items.
///
/// Editors are created lazily by [`Cowork::prepare_draft`] because they need
/// a window, and they are kept in sync with the document there too.
pub(crate) struct ThreadDraft {
    /// Routes asynchronous work, such as reading attachments, to this draft
    /// even after the user switched threads.
    pub(crate) id: Uuid,
    /// Who the local user is in this draft.
    pub(crate) author: ParticipantId,
    pub(crate) doc: Draft,
    /// The bytes of every file this participant has of the thread: the
    /// draft's, and those of messages submitted from it. Kept with the draft
    /// because a new thread's draft becomes its thread's.
    pub(crate) files: HashMap<AttachmentId, FileAttachment>,
    /// Files whose bytes are still arriving from another participant.
    pub(crate) incoming: HashMap<AttachmentId, IncomingFile>,
    /// The files the host holds every byte of, which is what submitting
    /// them needs.
    pub(crate) stored: HashSet<AttachmentId>,
    /// How many bytes of each of the local user's files have been sent to
    /// the host so far, while they are being sent.
    pub(crate) uploads: HashMap<AttachmentId, u64>,
    /// Files discarded here, whose pieces still in flight are ignored.
    pub(crate) discarded: HashSet<AttachmentId>,
    /// Whether removing a file from the draft keeps its bytes. Joined
    /// threads do: a removal can race a submission that includes the file,
    /// and the host sends every file only once.
    pub(crate) keeps_removed_files: bool,
    pub(crate) editors: HashMap<ItemId, ItemEditors>,
    /// The text each item editor last agreed on with the document. An editor
    /// catches up with others' edits only when it is next drawn, so a
    /// keystroke can arrive first; the change it makes is then its difference
    /// from this text, not from the document, which would revert those edits.
    pub(crate) synced_text: HashMap<EntityId, String>,
    /// The empty spot below the prompt blocks; typing there creates a block.
    pub(crate) draft_position: Option<Entity<TextareaState>>,

    /// Blocks created for attachments picked together, so they share one.
    attachment_batches: HashMap<Uuid, ItemId>,
    pub(crate) comments_folded: bool,
    /// Every participant's presence in this draft, including the host's echo
    /// of the local user's own, with when it last changed.
    pub(crate) presence: HashMap<ParticipantId, (protocol::Presence, Instant)>,
}

pub(crate) enum ItemEditors {
    Prompt(Entity<TextareaState>),
    /// A comment is edited both next to its excerpt and in the composer.
    Comment {
        inline: Entity<TextareaState>,
        composer: Entity<TextareaState>,
    },
}

impl ItemEditors {
    pub(crate) fn all(&self) -> Vec<&Entity<TextareaState>> {
        match self {
            Self::Prompt(editor) => vec![editor],
            Self::Comment { inline, composer } => vec![inline, composer],
        }
    }
}

/// Which part of a draft an editor edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditorSlot {
    DraftPosition,
    Prompt(ItemId),
    CommentInline(ItemId),
    CommentComposer(ItemId),
}

impl EditorSlot {
    pub(crate) fn item(self) -> Option<ItemId> {
        match self {
            Self::DraftPosition => None,
            Self::Prompt(id) | Self::CommentInline(id) | Self::CommentComposer(id) => Some(id),
        }
    }
}

impl ThreadDraft {
    pub(crate) fn new(author: ParticipantId) -> Self {
        Self {
            id: Uuid::new_v4(),
            author,
            doc: Draft::new(),
            files: HashMap::new(),
            incoming: HashMap::new(),
            stored: HashSet::new(),
            uploads: HashMap::new(),
            discarded: HashSet::new(),
            keeps_removed_files: false,
            editors: HashMap::new(),
            synced_text: HashMap::new(),
            draft_position: None,
            attachment_batches: HashMap::new(),
            comments_folded: false,
            presence: HashMap::new(),
        }
    }

    /// Records a participant's presence. The time it last changed only moves
    /// when their caret did, since that is what shows their name for a moment.
    pub(crate) fn set_presence(
        &mut self,
        participant: ParticipantId,
        presence: protocol::Presence,
    ) {
        let moved_at = match self.presence.get(&participant) {
            Some((current, moved_at))
                if current.focus == presence.focus && current.selection == presence.selection =>
            {
                *moved_at
            }
            _ => Instant::now(),
        };
        self.presence.insert(participant, (presence, moved_at));
    }

    /// Whether anyone other than `except` is focused in the item.
    pub(crate) fn is_attended(&self, id: ItemId, except: Option<ParticipantId>) -> bool {
        let focus = protocol::PresenceFocus::Item(id.as_uuid().into_bytes());
        self.presence.iter().any(|(participant, (presence, _))| {
            Some(*participant) != except && presence.focus == Some(focus)
        })
    }

    /// Participants focused in the item, in join order.
    pub(crate) fn editors_of(&self, id: ItemId, order: &[ParticipantId]) -> Vec<ParticipantId> {
        let focus = protocol::PresenceFocus::Item(id.as_uuid().into_bytes());
        self.participants_where(order, |presence| presence.focus == Some(focus))
    }

    /// Participants other than the local user at the draft position, in
    /// join order.
    pub(crate) fn others_at_draft_position(&self, order: &[ParticipantId]) -> Vec<ParticipantId> {
        self.participants_where(order, |presence| {
            presence.focus == Some(protocol::PresenceFocus::DraftPosition)
        })
        .into_iter()
        .filter(|participant| *participant != self.author)
        .collect()
    }

    /// Participants whose presence matches, in join order. Anyone missing
    /// from `order` comes last, sorted, so the order never reshuffles.
    fn participants_where(
        &self,
        order: &[ParticipantId],
        matches: impl Fn(&protocol::Presence) -> bool,
    ) -> Vec<ParticipantId> {
        let mut participants = self
            .presence
            .iter()
            .filter(|(_, (presence, _))| matches(presence))
            .map(|(participant, _)| *participant)
            .collect::<Vec<_>>();
        participants.sort_by_key(|participant| {
            (
                order
                    .iter()
                    .position(|joined| joined == participant)
                    .unwrap_or(usize::MAX),
                participant.as_uuid(),
            )
        });
        participants
    }

    /// The carets and selections of everyone else in the item, resolved
    /// against this replica's text of it.
    pub(crate) fn remote_carets(&self, id: ItemId, order: &[ParticipantId]) -> Vec<RemoteCaret> {
        let others = self
            .editors_of(id, order)
            .into_iter()
            .filter(|participant| *participant != self.author)
            .collect::<Vec<_>>();
        if others.is_empty() {
            return Vec::new();
        }
        let Some(body) = self.doc.body(id) else {
            return Vec::new();
        };
        others
            .into_iter()
            .filter_map(|participant| {
                let (presence, changed) = self.presence.get(&participant)?;
                let selection = presence.selection.as_ref()?;
                let resolve = |anchor: &[u8]| {
                    self.doc
                        .resolve_anchor(id, anchor)
                        .map(|offset| body.floor_char_boundary(offset))
                };
                let head = resolve(&selection.head)?;
                let tail = resolve(&selection.anchor).unwrap_or(head);
                Some(RemoteCaret {
                    participant,
                    selection: head.min(tail)..head.max(tail),
                    head,
                    moved_at: *changed,
                })
            })
            .collect()
    }

    /// Everyone else at the draft position, which has no text to place a
    /// caret in, so all of them sit at its start.
    pub(crate) fn draft_position_carets(&self, order: &[ParticipantId]) -> Vec<RemoteCaret> {
        self.others_at_draft_position(order)
            .into_iter()
            .filter_map(|participant| {
                let (_, changed) = self.presence.get(&participant)?;
                Some(RemoteCaret {
                    participant,
                    selection: 0..0,
                    head: 0,
                    moved_at: *changed,
                })
            })
            .collect()
    }

    /// Whether anyone's presence announces files being read into the block.
    pub(crate) fn has_announced_reads_into(&self, id: ItemId) -> bool {
        let block = id.as_uuid().into_bytes();
        self.presence.values().any(|(presence, _)| {
            presence
                .pending_reads
                .iter()
                .any(|read| read.block == Some(block))
        })
    }

    /// Whether a participant other than the local user is reading files
    /// into the draft.
    pub(crate) fn others_are_reading_files(&self) -> bool {
        self.presence.iter().any(|(participant, (presence, _))| {
            *participant != self.author && !presence.pending_reads.is_empty()
        })
    }

    pub(crate) fn slot_of(&self, editor: EntityId) -> Option<EditorSlot> {
        if self
            .draft_position
            .as_ref()
            .is_some_and(|draft_position| draft_position.entity_id() == editor)
        {
            return Some(EditorSlot::DraftPosition);
        }
        self.editors
            .iter()
            .find_map(|(&id, editors)| match editors {
                ItemEditors::Prompt(prompt) => {
                    (prompt.entity_id() == editor).then_some(EditorSlot::Prompt(id))
                }
                ItemEditors::Comment { inline, composer } => {
                    if inline.entity_id() == editor {
                        Some(EditorSlot::CommentInline(id))
                    } else {
                        (composer.entity_id() == editor).then_some(EditorSlot::CommentComposer(id))
                    }
                }
            })
    }

    pub(crate) fn editor(&self, slot: EditorSlot) -> Option<Entity<TextareaState>> {
        match (slot, slot.item().and_then(|id| self.editors.get(&id))) {
            (EditorSlot::DraftPosition, _) => self.draft_position.clone(),
            (EditorSlot::Prompt(_), Some(ItemEditors::Prompt(editor)))
            | (EditorSlot::CommentInline(_), Some(ItemEditors::Comment { inline: editor, .. }))
            | (
                EditorSlot::CommentComposer(_),
                Some(ItemEditors::Comment {
                    composer: editor, ..
                }),
            ) => Some(editor.clone()),
            _ => None,
        }
    }

    /// The editors Up and Down move between, top to bottom: the composer's
    /// comment editors unless folded, the prompt blocks, and the draft
    /// position.
    pub(crate) fn navigation_chain(&self) -> Vec<(EditorSlot, Entity<TextareaState>)> {
        let mut chain = Vec::new();
        let items = self.doc.items();
        if !self.comments_folded {
            chain.extend(
                items
                    .iter()
                    .filter(|item| item.is_comment())
                    .filter_map(|item| {
                        let slot = EditorSlot::CommentComposer(item.id);
                        Some((slot, self.editor(slot)?))
                    }),
            );
        }
        chain.extend(
            items
                .iter()
                .filter(|item| item.is_prompt())
                .filter_map(|item| {
                    let slot = EditorSlot::Prompt(item.id);
                    Some((slot, self.editor(slot)?))
                }),
        );
        if let Some(draft_position) = &self.draft_position {
            chain.push((EditorSlot::DraftPosition, draft_position.clone()));
        }
        chain
    }

    /// Applies what the user typed into an item's editor to the document.
    ///
    /// The editor may not show others' latest edits yet, so its change is
    /// taken relative to the text it last synced and moved past those edits
    /// before it is applied. Returns the item's resulting text.
    pub(crate) fn apply_typing(
        &mut self,
        id: ItemId,
        editor: EntityId,
        typed: &str,
    ) -> Option<String> {
        let body = self.doc.body(id)?;
        let synced = self
            .synced_text
            .get(&editor)
            .cloned()
            .unwrap_or_else(|| body.clone());
        if let Some(mut edit) = TextEdit::diff(&synced, typed) {
            if let Some(remote) = TextEdit::diff(&synced, &body) {
                edit.range = remote.map_offset(edit.range.start)..remote.map_offset(edit.range.end);
            }
            self.doc.edit_body(id, &edit);
        }
        // Everything the editor shows is in the document now.
        self.synced_text.insert(editor, typed.to_owned());
        self.doc.body(id)
    }

    /// Removes items together with their editors and file bytes.
    pub(crate) fn remove_items(&mut self, ids: &[ItemId]) {
        let attachments = self
            .doc
            .items()
            .into_iter()
            .filter(|item| ids.contains(&item.id))
            .flat_map(|item| match item.kind {
                DraftItemKind::Prompt { attachments } => attachments,
                DraftItemKind::Comment { .. } => Vec::new(),
            })
            .map(|record| record.id)
            .collect::<Vec<_>>();
        self.take_items(ids);
        for attachment in attachments {
            self.drop_removed_file(attachment);
        }
    }

    /// Forgets a file whose record was removed, unless this draft keeps
    /// removed files.
    pub(crate) fn drop_removed_file(&mut self, id: AttachmentId) {
        if !self.keeps_removed_files {
            self.drop_file(id);
        }
    }

    /// Removes items and their editors, keeping their files, as a
    /// submission needs them.
    pub(crate) fn take_items(&mut self, ids: &[ItemId]) {
        self.doc.remove_items(ids);
        for id in ids {
            if let Some(editors) = self.editors.remove(id) {
                for editor in editors.all() {
                    self.synced_text.remove(&editor.entity_id());
                }
            }
        }
        self.attachment_batches
            .retain(|_, block| !ids.contains(block));
    }

    /// Forgets everything about a file.
    pub(crate) fn drop_file(&mut self, id: AttachmentId) {
        self.files.remove(&id);
        self.incoming.remove(&id);
        self.stored.remove(&id);
        self.uploads.remove(&id);
        self.discarded.insert(id);
    }

    /// The attachment records of the draft's prompt blocks, in draft order.
    pub(crate) fn attachment_records(&self) -> Vec<AttachmentRecord> {
        self.doc
            .items()
            .into_iter()
            .flat_map(|item| match item.kind {
                DraftItemKind::Prompt { attachments } => attachments,
                DraftItemKind::Comment { .. } => Vec::new(),
            })
            .collect()
    }

    /// Whether a file of a non-empty block is not with the host yet, which
    /// holds submission back.
    pub(crate) fn has_unstored_attachments(&self) -> bool {
        self.doc
            .items()
            .into_iter()
            .filter(|item| !item.is_empty())
            .flat_map(|item| match item.kind {
                DraftItemKind::Prompt { attachments } => attachments,
                DraftItemKind::Comment { .. } => Vec::new(),
            })
            .any(|record| !self.stored.contains(&record.id))
    }

    /// Adds a received piece of a file. Returns the file once every byte
    /// has arrived, or an error when the piece breaks the rules for it.
    ///
    /// Pieces of files already complete, or that continue a file this
    /// participant has discarded or never started, are ignored: they can
    /// still be in flight after a removal.
    pub(crate) fn receive_chunk(
        &mut self,
        chunk: protocol::AttachmentChunk,
        uploader: Option<ParticipantId>,
    ) -> anyhow::Result<Option<AttachmentId>> {
        let id = AttachmentId::from_uuid(Uuid::from_bytes(chunk.id));
        let kind = kind_from_protocol(chunk.kind);
        anyhow::ensure!(
            chunk.total <= max_attachment_size(kind),
            "{} is larger than attachments may be",
            chunk.name
        );
        if self.files.contains_key(&id) || self.discarded.contains(&id) {
            return Ok(None);
        }
        if chunk.offset == 0 {
            if let Some(uploader) = uploader {
                let budget = 2 * MAX_MESSAGE_ATTACHMENT_BYTES;
                let buffered = |incoming: &HashMap<AttachmentId, IncomingFile>| {
                    incoming
                        .iter()
                        .filter(|(other, file)| **other != id && file.uploader == Some(uploader))
                        .map(|(_, file)| file.total)
                        .sum::<u64>()
                };
                if buffered(&self.incoming) + chunk.total > budget {
                    // Complete files still waiting for a record that may
                    // never come, e.g. attached to a block removed at the
                    // same time, make room first.
                    self.incoming.retain(|_, file| {
                        file.uploader != Some(uploader) || file.bytes.len() as u64 != file.total
                    });
                }
                anyhow::ensure!(
                    buffered(&self.incoming) + chunk.total <= budget,
                    "Too many attachment bytes in flight"
                );
            }
            self.incoming.insert(
                id,
                IncomingFile {
                    name: chunk.name,
                    kind,
                    total: chunk.total,
                    bytes: Vec::with_capacity(chunk.total as usize),
                    uploader,
                },
            );
        }
        let Some(file) = self.incoming.get_mut(&id) else {
            return Ok(None);
        };
        if file.uploader != uploader
            || file.kind != kind
            || file.total != chunk.total
            || file.bytes.len() as u64 != chunk.offset
        {
            // Out of step, e.g. restarted after a removal: start over.
            self.incoming.remove(&id);
            return Ok(None);
        }
        anyhow::ensure!(
            chunk.offset + chunk.bytes.len() as u64 <= file.total,
            "{} has more bytes than announced",
            file.name
        );
        file.bytes.extend_from_slice(&chunk.bytes);
        Ok((file.bytes.len() as u64 == file.total).then_some(id))
    }

    /// Turns a completely received file into one this participant has.
    pub(crate) fn complete_file(&mut self, id: AttachmentId) -> anyhow::Result<()> {
        let Some(file) = self.incoming.remove(&id) else {
            return Ok(());
        };
        let attachment = FileAttachment::from_bytes(file.name, file.kind, file.bytes)?;
        self.files.insert(id, attachment);
        Ok(())
    }

    /// The piece of a file this participant has that starts at `offset`,
    /// or `None` once past its end or when the file is gone.
    pub(crate) fn chunk(&self, id: AttachmentId, offset: u64) -> Option<protocol::AttachmentChunk> {
        let file = self.files.get(&id)?;
        let bytes = file.bytes();
        let start = usize::try_from(offset).ok()?;
        if start >= bytes.len() && !(start == 0 && bytes.is_empty()) {
            return None;
        }
        let end = (start + protocol::ATTACHMENT_CHUNK_SIZE).min(bytes.len());
        Some(protocol::AttachmentChunk {
            id: id.as_uuid().into_bytes(),
            name: file.name.clone(),
            kind: kind_to_protocol(file.kind()),
            total: bytes.len() as u64,
            offset,
            bytes: bytes[start..end].to_vec(),
        })
    }

    /// Removes the item if it is empty. Returns whether it was removed.
    pub(crate) fn remove_if_empty(&mut self, id: ItemId) -> bool {
        if !self.doc.item(id).is_some_and(|item| item.is_empty()) {
            return false;
        }
        self.remove_items(&[id]);
        true
    }

    /// Removes an empty item the local user is leaving, unless someone else
    /// is in it. Returns whether it was removed.
    ///
    /// Presence travels separately from the draft, so someone who has just
    /// entered the item can still lose it; that race is accepted.
    pub(crate) fn remove_if_unattended(&mut self, id: ItemId) -> bool {
        !self.is_attended(id, Some(self.author)) && self.remove_if_empty(id)
    }

    /// The block to attach a finished file to, creating it when needed.
    pub(crate) fn attachment_block(&mut self, target: AttachmentTarget) -> ItemId {
        let batch = match target {
            AttachmentTarget::Block(id)
                if self.doc.item(id).is_some_and(|item| item.is_prompt()) =>
            {
                return id;
            }
            AttachmentTarget::Block(_) => None,
            AttachmentTarget::NewBlock(batch) => Some(batch),
        };
        if let Some(id) = batch.and_then(|batch| self.attachment_batches.get(&batch).copied())
            && self.doc.contains(id)
        {
            return id;
        }
        let id = self.doc.create_prompt(self.author.as_uuid(), "");
        if let Some(batch) = batch {
            self.attachment_batches.insert(batch, id);
        }
        id
    }

    /// Where pending files for `target` are shown: the block they will land
    /// in, or `None` for the draft position.
    pub(crate) fn pending_block(&self, target: AttachmentTarget) -> Option<ItemId> {
        let id = match target {
            AttachmentTarget::Block(id) => id,
            AttachmentTarget::NewBlock(batch) => *self.attachment_batches.get(&batch)?,
        };
        self.doc
            .item(id)
            .filter(|item| item.is_prompt())
            .map(|item| item.id)
    }

    /// The draft's comments as the timeline and composer render them, with
    /// who is in them (`order` is the thread's join order).
    pub(crate) fn comment_views(&self, order: &[ParticipantId]) -> Vec<UserComment> {
        self.doc
            .items()
            .into_iter()
            .filter_map(|item| {
                let DraftItemKind::Comment { target } = item.kind else {
                    return None;
                };
                let body = match self.editors.get(&item.id) {
                    Some(ItemEditors::Comment { inline, composer }) => UserCommentBody::Editing {
                        inline: inline.clone(),
                        composer: composer.clone(),
                    },
                    _ => UserCommentBody::Submitted(item.body.into()),
                };
                let creator = ParticipantId::from_uuid(item.creator);
                Some(UserComment {
                    id: item.id.as_uuid(),
                    author: creator,
                    reference: CommentReference {
                        message_id: target.message_id,
                        range: target.range,
                        quote: target.quote,
                    },
                    body,
                    presence: ItemPresence {
                        editors: self
                            .editors_of(item.id, order)
                            .into_iter()
                            .filter(|editor| *editor != creator)
                            .collect(),
                        carets: self.remote_carets(item.id, order),
                    },
                })
            })
            .collect()
    }
}

/// Who is in a draft item besides its creator, and where their carets are.
#[derive(Clone, Default)]
pub(crate) struct ItemPresence {
    /// Participants focused in the item other than its creator, in join
    /// order.
    pub(crate) editors: Vec<ParticipantId>,
    /// Everyone else's carets in the item.
    pub(crate) carets: Vec<RemoteCaret>,
}

/// Another participant's caret and selection in an editor, as byte offsets
/// into the text it shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RemoteCaret {
    pub(crate) participant: ParticipantId,
    pub(crate) selection: Range<usize>,
    pub(crate) head: usize,
    /// When it last moved, which is when its name is shown for a moment.
    pub(crate) moved_at: Instant,
}

/// How long a remote caret shows its participant's name after moving.
pub(crate) const CARET_LABEL_DURATION: Duration = Duration::from_millis(1_500);

/// Where a file being read will be attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentTarget {
    Block(ItemId),
    /// A block created for the batch of files picked together.
    NewBlock(Uuid),
}
