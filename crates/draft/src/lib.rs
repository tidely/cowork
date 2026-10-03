//! The collaborative draft document of a chat thread, built on the Yrs CRDT.
//!
//! One Yrs document holds one thread's draft: an ordered collection of prompt blocks and comments
//! that several participants edit concurrently. Replicas exchange v1-encoded updates and converge
//! regardless of delivery order.
//!
//! ```text
//! Draft document
//! ├── order: Array<String>        item ids (hyphenated UUID strings), shared order across all kinds
//! └── items: Map<String, Map>     item id -> item
//!
//! Prompt item (Map)
//! ├── kind: "prompt"
//! ├── creator: String             participant UUID, write-once
//! ├── body: Text
//! └── attachments: Array<Any>     each element is one atomic AttachmentRecord value
//!
//! Comment item (Map)
//! ├── kind: "comment"
//! ├── creator: String             write-once
//! ├── target: Any                 atomic CommentTarget value, write-once
//! └── body: Text
//! ```
//!
//! Attachment records and comment targets are stored as single `Any` values so concurrent writers
//! can never produce a record mixing fields from different versions. Attachment file bytes are not
//! part of the document.
//!
//! Text positions throughout the API are UTF-8 byte offsets on char boundaries, matching Rust
//! strings; the document is configured to count text in bytes so no conversion is needed.
//!
//! Changes made through a [`Draft`]'s own methods are recorded as they commit and handed out by
//! [`Draft::take_local_update`]; changes merged in with [`Draft::apply_update`] never are, so a
//! replica never echoes other participants' content back as its own.

mod anchor;
mod item;
mod text_edit;
mod validate;

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeSet, HashSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use anyhow::{Context as _, Result};
use uuid::Uuid;
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Doc, GetString, Map, MapPrelim, MapRef, OffsetKind, Options,
    Out, ReadTxn, StateVector, Text, TextPrelim, TextRef, Transact, TransactionMut, Update,
    updates::{decoder::Decode, encoder::Encode},
};

pub use item::{
    AttachmentId, AttachmentKind, AttachmentRecord, CommentTarget, DraftItem, DraftItemKind, ItemId,
};
pub use text_edit::TextEdit;
pub use validate::verify_change;

const ORDER: &str = "order";
const ITEMS: &str = "items";

const KIND: &str = "kind";
const CREATOR: &str = "creator";
const BODY: &str = "body";
const ATTACHMENTS: &str = "attachments";
const TARGET: &str = "target";

const KIND_PROMPT: &str = "prompt";
const KIND_COMMENT: &str = "comment";

/// Transaction origin of [`Draft::apply_update`], which the local update recorder skips.
const REMOTE_ORIGIN: &str = "draft:remote";
/// Key of the document's update observer that records local updates.
const LOCAL_UPDATE_OBSERVER: &str = "draft:local-updates";

/// One replica of a thread's draft.
///
/// All methods take `&self`; each call runs in its own Yrs transaction.
pub struct Draft {
    doc: Doc,
    order: ArrayRef,
    items: MapRef,
    /// The v1 update of every local transaction since the last [`Self::take_local_update`],
    /// pushed by the document's update observer. The observer lives in `doc`, which this struct
    /// owns exclusively, so it is dropped along with it.
    local_updates: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Default for Draft {
    fn default() -> Self {
        Self::new()
    }
}

impl Draft {
    pub fn new() -> Self {
        let doc = Doc::with_options(Options {
            offset_kind: OffsetKind::Bytes,
            ..Options::default()
        });
        let order = doc.get_or_insert_array(ORDER);
        let items = doc.get_or_insert_map(ITEMS);

        let local_updates = Arc::new(Mutex::new(Vec::new()));
        let recorder = local_updates.clone();
        // Yrs only invokes update observers for transactions that inserted or deleted something,
        // so no-op transactions record nothing.
        doc.observe_update_v1(LOCAL_UPDATE_OBSERVER, move |txn, event| {
            let remote = txn
                .origin()
                .is_some_and(|origin| origin.as_ref() == REMOTE_ORIGIN.as_bytes());
            if !remote {
                lock(&recorder).push(event.update.clone());
            }
        })
        .expect("a new document has no active transaction");

        Self {
            doc,
            order,
            items,
            local_updates,
        }
    }

    /// The v1 update of every local change since the previous call, merged into one update, or
    /// `None` when nothing changed locally.
    ///
    /// Local changes are those made through this `Draft`'s own mutating methods
    /// ([`Self::create_prompt`], [`Self::create_comment`], [`Self::remove_items`],
    /// [`Self::set_body`], [`Self::edit_body`], [`Self::add_attachment`],
    /// [`Self::remove_attachment`]). Changes merged in with [`Self::apply_update`] are never
    /// reported, though local changes made afterwards may depend on them.
    pub fn take_local_update(&self) -> Option<Vec<u8>> {
        let updates = std::mem::take(&mut *lock(&self.local_updates));
        if updates.is_empty() {
            return None;
        }
        let updates = updates.iter().map(|update| {
            // These bytes were just encoded by Yrs itself; failing to decode them is a Yrs bug.
            Update::decode_v1(update).expect("yrs produced an undecodable update")
        });
        let merged = Update::merge_updates(updates);
        (!merged.is_empty()).then(|| merged.encode_v1())
    }

    /// Appends a prompt block. `body` may be empty.
    pub fn create_prompt(&self, creator: Uuid, body: &str) -> ItemId {
        let id = ItemId::new();
        let mut txn = self.doc.transact_mut();
        let item = self.insert_item(&mut txn, id, KIND_PROMPT, creator, body);
        item.insert(&mut txn, ATTACHMENTS, ArrayPrelim::default());
        id
    }

    /// Appends a comment.
    pub fn create_comment(&self, creator: Uuid, target: CommentTarget, body: &str) -> ItemId {
        let id = ItemId::new();
        let mut txn = self.doc.transact_mut();
        let item = self.insert_item(&mut txn, id, KIND_COMMENT, creator, body);
        item.insert(&mut txn, TARGET, target.to_any());
        id
    }

    /// Inserts the item and its order entry. Both happen in the caller's transaction so peers
    /// never observe an order entry without its item or vice versa.
    fn insert_item(
        &self,
        txn: &mut TransactionMut,
        id: ItemId,
        kind: &str,
        creator: Uuid,
        body: &str,
    ) -> MapRef {
        let key = id.to_string();
        let item = self.items.insert(txn, key.as_str(), MapPrelim::default());
        item.insert(txn, KIND, kind);
        item.insert(txn, CREATOR, creator.to_string());
        item.insert(txn, BODY, TextPrelim::new(body));
        self.order.push_back(txn, key);
        item
    }

    /// Removes the given items (and their order entries) in one transaction. Unknown ids are
    /// ignored. Returns how many of the ids had an item or order entry to remove.
    pub fn remove_items(&self, ids: &[ItemId]) -> usize {
        let targets: HashSet<ItemId> = ids.iter().copied().collect();
        if targets.is_empty() {
            return 0;
        }

        let mut txn = self.doc.transact_mut();
        let mut removed = HashSet::new();

        let entries: Vec<(usize, ItemId)> = self
            .order
            .iter(&txn)
            .enumerate()
            .filter_map(|(index, entry)| {
                let id = parse_order_entry(&entry)?;
                targets.contains(&id).then_some((index, id))
            })
            .collect();
        // Back to front so the remaining indices stay valid.
        for (index, id) in entries.into_iter().rev() {
            self.order.remove(&mut txn, index as u32);
            removed.insert(id);
        }

        for id in targets {
            if self.items.remove(&mut txn, &id.to_string()).is_some() {
                removed.insert(id);
            }
        }

        removed.len()
    }

    /// All well-formed items in draft order.
    ///
    /// Malformed items (missing/invalid fields, unknown kind, order entries without an item,
    /// duplicate order entries) are skipped: a peer running a different version must not be able
    /// to break the draft for everyone. Individual malformed attachment records are skipped
    /// without hiding their block.
    pub fn items(&self) -> Vec<DraftItem> {
        let txn = self.doc.transact();
        let mut seen = HashSet::new();
        self.order
            .iter(&txn)
            .filter_map(|entry| parse_order_entry(&entry))
            .filter(|id| seen.insert(*id))
            .filter_map(|id| self.read_item(&txn, id))
            .map(|parsed| parsed.item)
            .collect()
    }

    /// The item, if [`Self::items`] would include it.
    pub fn item(&self, id: ItemId) -> Option<DraftItem> {
        let txn = self.doc.transact();
        self.visible_item(&txn, id).map(|parsed| parsed.item)
    }

    pub fn contains(&self, id: ItemId) -> bool {
        self.item(id).is_some()
    }

    pub fn body(&self, id: ItemId) -> Option<String> {
        self.item(id).map(|item| item.body)
    }

    /// Replaces the item's whole body with `new_body`, applying only the minimal
    /// [`TextEdit::diff`] so concurrent edits elsewhere in the text survive merging. Returns the
    /// edit applied (`None` if unchanged or the item doesn't exist).
    pub fn set_body(&self, id: ItemId, new_body: &str) -> Option<TextEdit> {
        let mut txn = self.doc.transact_mut();
        let parsed = self.visible_item(&txn, id)?;
        let edit = TextEdit::diff(&parsed.item.body, new_body)?;
        apply_text_edit(&mut txn, &parsed.body, &parsed.item.body, &edit).then_some(edit)
    }

    /// Applies one edit at byte offsets. Returns `false` if the item doesn't exist or the range
    /// is invalid / not on char boundaries.
    pub fn edit_body(&self, id: ItemId, edit: &TextEdit) -> bool {
        let mut txn = self.doc.transact_mut();
        let Some(parsed) = self.visible_item(&txn, id) else {
            return false;
        };
        apply_text_edit(&mut txn, &parsed.body, &parsed.item.body, edit)
    }

    /// Appends an attachment record to a prompt block. Returns `false` if `block` is not a
    /// prompt.
    pub fn add_attachment(&self, block: ItemId, record: AttachmentRecord) -> bool {
        let mut txn = self.doc.transact_mut();
        let Some(ParsedItem {
            attachments: Some(attachments),
            ..
        }) = self.visible_item(&txn, block)
        else {
            return false;
        };
        attachments.push_back(&mut txn, record.to_any());
        true
    }

    /// Removes an attachment record from whichever prompt block holds it. Returns `false` if not
    /// found.
    pub fn remove_attachment(&self, attachment: AttachmentId) -> bool {
        let mut txn = self.doc.transact_mut();
        let mut removed = false;
        for array in self.attachment_arrays(&txn) {
            let indices: Vec<u32> = array
                .iter(&txn)
                .enumerate()
                .filter(|(_, value)| attachment_id(value) == Some(attachment))
                .map(|(index, _)| index as u32)
                .collect();
            for index in indices.into_iter().rev() {
                array.remove(&mut txn, index);
                removed = true;
            }
        }
        removed
    }

    /// Every attachment id referenced by the draft, sorted and deduplicated.
    ///
    /// This deliberately includes blocks that [`Self::items`] hides as malformed, so callers
    /// garbage-collecting attachment bytes never drop a file the document still points at.
    pub fn attachment_ids(&self) -> Vec<AttachmentId> {
        let txn = self.doc.transact();
        let ids: BTreeSet<AttachmentId> = self
            .attachment_arrays(&txn)
            .iter()
            .flat_map(|array| array.iter(&txn).filter_map(|value| attachment_id(&value)))
            .collect();
        ids.into_iter().collect()
    }

    /// The full document state as a v1 update.
    pub fn encode_state(&self) -> Vec<u8> {
        self.doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    /// The document's state vector, v1 encoded.
    pub fn state_vector(&self) -> Vec<u8> {
        self.doc.transact().state_vector().encode_v1()
    }

    /// Everything the holder of `state_vector` is missing, as a v1 update.
    pub fn encode_diff(&self, state_vector: &[u8]) -> Result<Vec<u8>> {
        let state_vector = StateVector::decode_v1(state_vector).context("invalid state vector")?;
        Ok(self.doc.transact().encode_state_as_update_v1(&state_vector))
    }

    /// Merges a v1 update (idempotent, commutative).
    ///
    /// Parts of the update whose causal dependencies haven't arrived yet are kept pending inside
    /// the document and integrated once they do. Nothing merged here is reported by
    /// [`Self::take_local_update`].
    pub fn apply_update(&self, update: &[u8]) -> Result<()> {
        let update = Update::decode_v1(update).context("invalid draft update")?;
        self.doc
            .transact_mut_with(REMOTE_ORIGIN)
            .apply_update(update)
            .context("failed to apply draft update")
    }

    /// The item if it has an order entry and is well-formed, i.e. if [`Self::items`] shows it.
    /// Mutations go through this too, so they never touch hidden items.
    fn visible_item<T: ReadTxn>(&self, txn: &T, id: ItemId) -> Option<ParsedItem> {
        let in_order = self
            .order
            .iter(txn)
            .any(|entry| parse_order_entry(&entry) == Some(id));
        if !in_order {
            return None;
        }
        self.read_item(txn, id)
    }

    fn read_item<T: ReadTxn>(&self, txn: &T, id: ItemId) -> Option<ParsedItem> {
        let Out::YMap(map) = self.items.get(txn, &id.to_string())? else {
            return None;
        };
        parse_item(txn, id, &map)
    }

    /// The `attachments` array of every item in the map, visible or not.
    fn attachment_arrays<T: ReadTxn>(&self, txn: &T) -> Vec<ArrayRef> {
        self.items
            .iter(txn)
            .filter_map(|(_, item)| match item {
                Out::YMap(item) => match item.get(txn, ATTACHMENTS)? {
                    Out::YArray(array) => Some(array),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

/// A well-formed item along with the shared refs mutations need.
struct ParsedItem {
    item: DraftItem,
    body: TextRef,
    /// Present exactly for prompts.
    attachments: Option<ArrayRef>,
}

fn parse_item<T: ReadTxn>(txn: &T, id: ItemId, map: &MapRef) -> Option<ParsedItem> {
    let kind = get_string(txn, map, KIND)?;
    let creator = Uuid::parse_str(&get_string(txn, map, CREATOR)?).ok()?;
    let Out::YText(body_ref) = map.get(txn, BODY)? else {
        return None;
    };
    let body = body_ref.get_string(txn);

    let (kind, attachments_ref) = match kind.as_str() {
        KIND_PROMPT => {
            let Out::YArray(attachments_ref) = map.get(txn, ATTACHMENTS)? else {
                return None;
            };
            let attachments = attachments_ref
                .iter(txn)
                .filter_map(|value| match value {
                    Out::Any(any) => AttachmentRecord::from_any(&any),
                    _ => None,
                })
                .collect();
            (DraftItemKind::Prompt { attachments }, Some(attachments_ref))
        }
        KIND_COMMENT => {
            let Out::Any(target) = map.get(txn, TARGET)? else {
                return None;
            };
            let target = CommentTarget::from_any(&target)?;
            (DraftItemKind::Comment { target }, None)
        }
        _ => return None,
    };

    Some(ParsedItem {
        item: DraftItem {
            id,
            creator,
            body,
            kind,
        },
        body: body_ref,
        attachments: attachments_ref,
    })
}

fn get_string<T: ReadTxn>(txn: &T, map: &MapRef, key: &str) -> Option<String> {
    match map.get(txn, key)? {
        Out::Any(Any::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// Locks `mutex`, ignoring poisoning: the guarded list of updates is valid after any panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn parse_order_entry(entry: &Out) -> Option<ItemId> {
    match entry {
        Out::Any(Any::String(s)) => Uuid::parse_str(s).ok().map(ItemId::from_uuid),
        _ => None,
    }
}

fn attachment_id(value: &Out) -> Option<AttachmentId> {
    match value {
        Out::Any(any) => AttachmentRecord::id_of(any),
        _ => None,
    }
}

/// Applies `edit` to `text`, whose current content is `current`. Returns `false` without
/// touching the document if the edit doesn't fit `current`.
fn apply_text_edit(
    txn: &mut TransactionMut,
    text: &TextRef,
    current: &str,
    edit: &TextEdit,
) -> bool {
    let range = &edit.range;
    if range.start > range.end
        || !current.is_char_boundary(range.start)
        || !current.is_char_boundary(range.end)
    {
        return false;
    }
    // Yrs counts embeds as length 1 while `get_string` omits them. Such text never comes from
    // this crate; refuse rather than edit at the wrong offsets.
    if text.len(txn) as usize != current.len() {
        return false;
    }
    let (Ok(start), Ok(len)) = (u32::try_from(range.start), u32::try_from(range.len())) else {
        return false;
    };
    if len > 0 {
        text.remove_range(txn, start, len);
    }
    text.insert(txn, start, &edit.insert);
    true
}
