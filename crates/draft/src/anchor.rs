//! Anchors: positions in an item's body that stay on the same place in the text across concurrent
//! edits, for showing other participants' carets and selections.
//!
//! An anchor is a v1-encoded Yrs [`StickyIndex`] (the same format as a Yjs relative position).
//! Yrs 0.28 computes sticky indices wrongly for multi-byte text when the document counts in bytes:
//! item clocks count UTF-16 code units, but `StickyIndex::at` and `StickyIndex::get_offset` mix in
//! byte offsets within an item. So this module maps between byte offsets and item clocks itself,
//! and only relies on Yrs where its result doesn't depend on offsets within an item.

use std::sync::Arc;

use yrs::{
    Any, Assoc, ClientID, ID, IndexScope, Out, ReadTxn, Snapshot, StickyIndex, Text, TextRef,
    Transact, TransactionMut,
    branch::{Branch, BranchPtr},
    encoding::read::{Cursor, Read},
    types::text::YChange,
    updates::encoder::Encode,
};

use crate::{Draft, ItemId};

impl Draft {
    /// An anchor at byte `offset` of `item`'s body that stays on the same place in the text
    /// across concurrent edits, encoded for sending to other replicas. `None` if the item doesn't
    /// exist or `offset` is beyond the body or not on a char boundary.
    ///
    /// The anchor sticks to the character after `offset` (Yjs' default association for cursors):
    ///
    /// - Text inserted right at the anchor, by anyone, lands before it, so the anchor moves
    ///   right past the insertion.
    /// - An anchor at the end of the body (including an empty body) sticks to the end itself: it
    ///   keeps resolving to the end of the body as text is appended, whether by the anchor's owner
    ///   typing on or by someone else.
    /// - If the character after the anchor is deleted, the anchor resolves to where it was.
    pub fn anchor(&self, item: ItemId, offset: usize) -> Option<Vec<u8>> {
        let mut txn = self.doc.transact_mut();
        let parsed = self.visible_item(&txn, item)?;
        let body = &parsed.item.body;
        if !body.is_char_boundary(offset) || !is_plain_text(&txn, &parsed.body, body) {
            return None;
        }

        let index = if offset == body.len() {
            // No character follows; stick to the end of the text instead.
            StickyIndex::from_type(&txn, &parsed.body, Assoc::After)
        } else {
            let segments = segments(&mut txn, &parsed.body)?;
            let segment = segments
                .iter()
                .find(|segment| (segment.start..segment.end()).contains(&offset))?;
            let units = segment.text[..offset - segment.start]
                .encode_utf16()
                .count();
            let clock = segment.id.clock.checked_add(u32::try_from(units).ok()?)?;
            StickyIndex::from_id(ID::new(segment.id.client, clock), Assoc::After)
        };
        Some(index.encode_v1())
    }

    /// Resolves an encoded anchor against this replica: the byte offset in `item`'s current body,
    /// always on a char boundary. `None` if the item doesn't exist (anymore), the anchor doesn't
    /// belong to that item's body (e.g. it was made for another item), or it can't be decoded or
    /// resolved (e.g. its text hasn't arrived yet).
    pub fn resolve_anchor(&self, item: ItemId, anchor: &[u8]) -> Option<usize> {
        let index = decode_anchor(anchor)?;
        let mut txn = self.doc.transact_mut();
        let parsed = self.visible_item(&txn, item)?;
        let body = &parsed.item.body;
        if !is_plain_text(&txn, &parsed.body, body) {
            return None;
        }

        let resolved = index.get_offset(&txn)?;
        let body_branch: &Branch = parsed.body.as_ref();
        if resolved.branch != BranchPtr::from(body_branch) {
            return None;
        }
        // Anchors on live text are resolved from the segments, where Yrs' index may be off. For
        // deleted text and the ends of the text, Yrs' index only sums whole items and is exact.
        let offset = match index.id() {
            Some(id) => segments(&mut txn, &parsed.body)?
                .iter()
                .find_map(|segment| segment.offset_of(id, index.assoc)),
            None => None,
        }
        .unwrap_or(resolved.index as usize);
        Some(body.floor_char_boundary(offset.min(body.len())))
    }
}

/// Whether `text`'s Yrs length matches its string `content`. Embeds count as length 1 in Yrs but
/// are omitted from the string; such text never comes from this crate, and offsets into it
/// wouldn't line up.
fn is_plain_text<T: ReadTxn>(txn: &T, text: &TextRef, content: &str) -> bool {
    text.len(txn) as usize == content.len()
}

/// One live item of a text: its string and the id of its first UTF-16 code unit.
struct Segment {
    id: ID,
    /// Byte offset of the segment in the whole text.
    start: usize,
    text: Arc<str>,
}

impl Segment {
    fn end(&self) -> usize {
        self.start + self.text.len()
    }

    /// The byte offset in the whole text that an anchor at `id` resolves to, if `id` lies in this
    /// segment. An id in the middle of a surrogate pair (never produced by [`Draft::anchor`]) is
    /// treated as pointing at the whole char.
    fn offset_of(&self, id: &ID, assoc: Assoc) -> Option<usize> {
        if id.client != self.id.client {
            return None;
        }
        let units = id.clock.checked_sub(self.id.clock)? as usize;
        let mut seen = 0;
        for (byte, c) in self.text.char_indices() {
            seen += c.len_utf16();
            if units < seen {
                // `id` is this char: `After` sits right before it, `Before` right after it.
                return Some(match assoc {
                    Assoc::After => self.start + byte,
                    Assoc::Before => self.start + byte + c.len_utf8(),
                });
            }
        }
        None
    }
}

/// The live string items of `text` in order, each with its id. `None` if the text contains
/// anything but strings.
///
/// Yrs doesn't expose items directly, but diffing the current state against an empty snapshot
/// reports every live item as a separate "added" chunk carrying its id. This only reads: splitting
/// by the current snapshot finds nothing to split, and no content changes, so nothing is recorded
/// as a local update.
fn segments(txn: &mut TransactionMut, text: &TextRef) -> Option<Vec<Segment>> {
    let now = txn.snapshot();
    let mut start = 0;
    text.diff_range(
        txn,
        Some(&now),
        Some(&Snapshot::default()),
        YChange::identity,
    )
    .into_iter()
    .map(|chunk| {
        let (Out::Any(Any::String(content)), Some(change)) = (chunk.insert, chunk.ychange) else {
            return None;
        };
        let segment = Segment {
            id: change.id,
            start,
            text: content,
        };
        start = segment.end();
        Some(segment)
    })
    .collect()
}

/// Decodes a v1-encoded [`StickyIndex`] exactly like its `Decode` impl, except that it rejects
/// input that impl would panic on (client ids beyond 53 bits trip a debug assertion in
/// [`ClientID::new`]) or only partly consume.
pub(crate) fn decode_anchor(bytes: &[u8]) -> Option<StickyIndex> {
    fn read_id(cursor: &mut Cursor) -> Option<ID> {
        let client: u64 = cursor.read_var().ok()?;
        if client >> 53 != 0 {
            return None;
        }
        let clock: u32 = cursor.read_var().ok()?;
        Some(ID::new(ClientID::new(client), clock))
    }

    let mut cursor = Cursor::new(bytes);
    let scope = match cursor.read_var::<u8>().ok()? {
        0 => IndexScope::Relative(read_id(&mut cursor)?),
        1 => IndexScope::Root(cursor.read_string().ok()?.into()),
        2 => IndexScope::Nested(read_id(&mut cursor)?),
        _ => return None,
    };
    let assoc = if cursor.read_var::<i8>().ok()? >= 0 {
        Assoc::After
    } else {
        Assoc::Before
    };
    (!cursor.has_content()).then(|| StickyIndex::new(scope, assoc))
}
