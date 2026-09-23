//! Structural validation of a draft document, and checks on what one participant's update may
//! change. The host runs both after applying a collaborator's update.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use uuid::Uuid;
use yrs::{
    Array, GetString, Map, MapRef, Out, ReadTxn, Text, TextRef, Transact,
    branch::{Branch, BranchPtr},
};

use crate::{
    ATTACHMENTS, AttachmentId, AttachmentRecord, BODY, CREATOR, CommentTarget, Draft, DraftItem,
    DraftItemKind, ITEMS, ItemId, KIND, KIND_COMMENT, KIND_PROMPT, ORDER, TARGET, get_string,
    item::has_exact_fields, parse_item, parse_order_entry,
};

const PROMPT_FIELDS: [&str; 4] = [KIND, CREATOR, BODY, ATTACHMENTS];
const COMMENT_FIELDS: [&str; 4] = [KIND, CREATOR, TARGET, BODY];

impl Draft {
    /// Checks the structural invariants of the document. Returns a description of the first
    /// violation.
    ///
    /// A document that passes shows every item in [`Self::items`], in `order`, with every
    /// attachment record. Attachment size limits are not checked; they are the caller's policy.
    pub fn validate(&self) -> Result<()> {
        let txn = self.doc.transact();

        for (name, _) in txn.root_refs() {
            ensure!(name == ORDER || name == ITEMS, "unexpected root {name:?}");
        }
        // Content written into the "other half" of a shared type (map entries on an array or
        // sequence content on a map) is invisible through the typed API, so check for it
        // explicitly.
        ensure!(
            map_len(&txn, self.order.as_ref()) == 0,
            "the `order` root has map entries"
        );
        ensure!(
            self.items.as_ref().len() == 0,
            "the `items` root has sequence content"
        );

        let mut ordered = HashSet::new();
        for (index, entry) in self.order.iter(&txn).enumerate() {
            let id = parse_order_entry(&entry)
                .ok_or_else(|| anyhow!("order entry {index} is not an item id: {entry:?}"))?;
            ensure!(ordered.insert(id), "item {id} is in order more than once");
            ensure!(
                self.items.contains_key(&txn, &id.to_string()),
                "order entry {id} has no item"
            );
        }

        let mut attachment_ids = HashSet::new();
        for (key, value) in self.items.iter(&txn) {
            // Lookups use the canonical id string, so only keys in that form can be ordered.
            let id = Uuid::parse_str(key)
                .ok()
                .map(ItemId::from_uuid)
                .filter(|id| id.to_string() == key && ordered.contains(id))
                .ok_or_else(|| anyhow!("item {key:?} is not in order"))?;
            let Out::YMap(map) = value else {
                bail!("item {id} is not a map");
            };
            validate_item(&txn, id, &map, &mut attachment_ids)
                .with_context(|| format!("item {id}"))?;
        }

        Ok(())
    }
}

fn validate_item<T: ReadTxn>(
    txn: &T,
    id: ItemId,
    map: &MapRef,
    attachment_ids: &mut HashSet<AttachmentId>,
) -> Result<()> {
    ensure!(map.as_ref().len() == 0, "has sequence content");

    let kind = get_string(txn, map, KIND).context("kind is missing or not a string")?;
    let fields = match kind.as_str() {
        KIND_PROMPT => PROMPT_FIELDS,
        KIND_COMMENT => COMMENT_FIELDS,
        _ => bail!("unsupported kind {kind:?}"),
    };
    let mut keys: Vec<&str> = map.keys(txn).collect();
    keys.sort_unstable();
    let mut expected = fields.to_vec();
    expected.sort_unstable();
    ensure!(
        keys == expected,
        "has fields {keys:?}, expected {expected:?}"
    );

    let creator = get_string(txn, map, CREATOR).context("creator is not a string")?;
    Uuid::parse_str(&creator).with_context(|| format!("creator {creator:?} is not a UUID"))?;

    let Some(Out::YText(body)) = map.get(txn, BODY) else {
        bail!("body is not a text");
    };
    validate_body(txn, &body)?;

    if kind == KIND_PROMPT {
        let Some(Out::YArray(attachments)) = map.get(txn, ATTACHMENTS) else {
            bail!("attachments is not an array");
        };
        ensure!(
            map_len(txn, attachments.as_ref()) == 0,
            "attachments has map entries"
        );
        for (index, value) in attachments.iter(txn).enumerate() {
            let Out::Any(any) = value else {
                bail!("attachment {index} is not an atomic value");
            };
            ensure!(
                has_exact_fields(&any, &AttachmentRecord::FIELDS),
                "attachment {index} does not have exactly the fields {:?}",
                AttachmentRecord::FIELDS
            );
            let record = AttachmentRecord::from_any(&any)
                .ok_or_else(|| anyhow!("attachment {index} is malformed: {any}"))?;
            ensure!(
                attachment_ids.insert(record.id),
                "attachment {} is in the document more than once",
                record.id
            );
        }
    } else {
        let Some(Out::Any(target)) = map.get(txn, TARGET) else {
            bail!("target is not an atomic value");
        };
        ensure!(
            has_exact_fields(&target, &CommentTarget::FIELDS),
            "target does not have exactly the fields {:?}",
            CommentTarget::FIELDS
        );
        CommentTarget::from_any(&target).ok_or_else(|| anyhow!("target is malformed: {target}"))?;
    }

    // The checks above are stricter than what `items()` requires; make sure the two never
    // disagree about an item that passes.
    ensure!(
        parse_item(txn, id, map).is_some(),
        "is not readable as an item"
    );
    Ok(())
}

/// Version 1 bodies are plain unformatted text.
fn validate_body<T: ReadTxn>(txn: &T, body: &TextRef) -> Result<()> {
    ensure!(map_len(txn, body.as_ref()) == 0, "body has map entries");
    // Embeds count towards the length but are omitted from the string.
    ensure!(
        body.len(txn) as usize == body.get_string(txn).len(),
        "body contains embeds"
    );
    let formatted = body.diff(txn, |_| ()).iter().any(|chunk| {
        chunk
            .attributes
            .as_ref()
            .is_some_and(|attrs| !attrs.is_empty())
    });
    ensure!(!formatted, "body has formatting attributes");
    Ok(())
}

/// The number of live map entries on `branch`, whatever its type.
fn map_len<T: ReadTxn>(txn: &T, branch: &Branch) -> u32 {
    MapRef::from(BranchPtr::from(branch)).len(txn)
}

/// Checks that going from `before` to `after` (both from [`Draft::items`], taken just before and
/// after applying one participant's update) is a change `author` was allowed to make.
///
/// - Every item that is new in `after` is created by `author`.
/// - Items present in both keep their creator, their kind (prompt vs comment) and, for comments,
///   their target.
/// - Every attachment record that is new in `after` (by attachment id, across all blocks) has
///   `author` as its creator, and records present in both are unchanged.
///
/// Removing items and editing bodies is always allowed.
pub fn verify_change(before: &[DraftItem], after: &[DraftItem], author: Uuid) -> Result<()> {
    let before_items: HashMap<ItemId, &DraftItem> =
        before.iter().map(|item| (item.id, item)).collect();
    for item in after {
        let Some(old) = before_items.get(&item.id) else {
            ensure!(
                item.creator == author,
                "new item {} has creator {} instead of {author}",
                item.id,
                item.creator
            );
            continue;
        };
        ensure!(
            item.creator == old.creator,
            "item {} changed creator from {} to {}",
            item.id,
            old.creator,
            item.creator
        );
        match (&old.kind, &item.kind) {
            (DraftItemKind::Prompt { .. }, DraftItemKind::Prompt { .. }) => {}
            (DraftItemKind::Comment { target: old }, DraftItemKind::Comment { target: new }) => {
                ensure!(old == new, "comment {} changed target", item.id);
            }
            _ => bail!("item {} changed kind", item.id),
        }
    }

    let before_records: HashMap<AttachmentId, &AttachmentRecord> = attachment_records(before)
        .map(|record| (record.id, record))
        .collect();
    for record in attachment_records(after) {
        match before_records.get(&record.id) {
            None => ensure!(
                record.creator == author,
                "new attachment {} has creator {} instead of {author}",
                record.id,
                record.creator
            ),
            Some(old) => ensure!(*old == record, "attachment {} was modified", record.id),
        }
    }

    Ok(())
}

fn attachment_records(items: &[DraftItem]) -> impl Iterator<Item = &AttachmentRecord> {
    items.iter().flat_map(|item| match &item.kind {
        DraftItemKind::Prompt { attachments } => attachments.as_slice(),
        DraftItemKind::Comment { .. } => &[],
    })
}
