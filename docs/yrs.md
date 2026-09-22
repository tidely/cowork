    pub(crate) fn apply_remote_edit(
        &mut self,
        edit: SharedAuthoredEdit,
        mut edit: SharedAuthoredEdit,
    ) -> Result<(), SharedError> {
        self.replica.validate_authored_edit(&edit)?;
        self.contributions.validate(&edit)?;
        self.replica.apply_authored_edit(&edit)?;
        self.replica.apply_attributed_edit(&mut edit)?;
        self.contributions.record(edit)?;
        Ok(())
    }
    fmt,
    ops::Range,
    sync::{Arc, Mutex},

};

use serde::{Deserialize, Serialize};
use yrs::{
Any, Array, ArrayRef, Assoc, BranchID, Doc, GetString, IndexedSequence, Map, MapPrelim, MapRef,
OffsetKind, Options, Out, ReadTxn, SharedRef, StateVector, StickyIndex, Text, TextPrelim,
TextRef, Transact, Update, updates::encoder::Encode,
Observable, OffsetKind, Options, Out, ReadTxn, SharedRef, StateVector, StickyIndex, Text,
TextPrelim, TextRef, Transact, Update,
types::{Change, DeepObservable, Event, PathSegment},
updates::encoder::Encode,
};

use super::{
///
/// `touched_items` and `creation` are unauthenticated provenance claims. They
/// are deliberately not inferred from checkpoints or decoded update internals.
/// are deliberately not inferred from checkpoints. At a live synchronization
/// boundary, `touched_items` is replaced with item IDs observed by applying the
/// update to a disposable, causally complete replica before it is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedAuthoredEdit {
pub draft_id: DraftId,
&self.update
}

    /// Replaces the untrusted wire hint with item IDs observed while applying
    /// this update to a complete replica.
    fn set_attributed_items(&mut self, mut items: Vec<InputItemId>) -> Result<(), SharedError> {
        items.sort_unstable();
        items.dedup();
        if items.is_empty() {
            return Err(SharedError::InvalidEnvelope(
                "authored update did not affect an input item".into(),
            ));
        }
        self.touched_items = items;
        Ok(())
    }

}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
}

    /// Validates an authored update in a disposable copy, derives the input
    /// items it actually changes, and only then integrates it into this replica.
    ///
    /// This is the trust-boundary entry point for live authored traffic. The
    /// probe must be causally complete: otherwise a pending update could become
    /// visible during a later actor's transaction and be attributed incorrectly.
    pub fn apply_attributed_edit(
        &mut self,
        edit: &mut SharedAuthoredEdit,
    ) -> Result<(), SharedError> {
        self.validate_authored_edit(edit)?;

        let checkpoint = self.checkpoint();
        let mut probe = Self::new(self.descriptor, self.context)?;
        probe.apply_state_transfer(&checkpoint)?;
        if probe.doc.transact().has_missing_updates() {
            return Err(SharedError::MissingDependencies);
        }

        let affected = probe.apply_update_observed(edit.update_bytes())?;
        if probe.doc.transact().has_missing_updates() {
            return Err(SharedError::MissingDependencies);
        }
        // Validate the complete post-update schema before touching the live Doc.
        probe.draft_snapshot()?;
        if let Some(claim) = edit.creation.as_deref()
            && !affected.contains(&claim.item_id)
        {
            return Err(SharedError::InvalidEnvelope(
                "creation claim does not describe an item changed by the update".into(),
            ));
        }
        edit.set_attributed_items(affected.into_iter().collect())?;

        // The exact bytes validated above are applied to the equivalent live
        // state. An integration error is still fatal, as documented by the
        // lower-level method.
        self.apply_authored_edit(edit)
    }

    /// Applies unattributed checkpoint/diff state.
    ///
    /// Routing and decode errors leave this replica unchanged. A Yrs integration
        }
    }

    fn apply_update_observed(
        &mut self,
        bytes: &[u8],
    ) -> Result<BTreeSet<InputItemId>, SharedError> {
        #[derive(Default)]
        struct Observation {
            items: BTreeSet<InputItemId>,
            error: Option<SharedError>,
        }

        fn record_id(observation: &mut Observation, value: &str) {
            match parse_item_id(value) {
                Ok(id) => {
                    observation.items.insert(id);
                }
                Err(error) => {
                    observation.error.get_or_insert(error);
                }
            };
        }

        let before_order = {
            let txn = self.doc.transact();
            self.order
                .iter(&txn)
                .map(parse_order_id)
                .collect::<Result<Vec<_>, _>>()?
        };
        let observation = Arc::new(Mutex::new(Observation::default()));

        let item_observation = Arc::clone(&observation);
        let _items_subscription = self.items.observe_deep(move |txn, events| {
            let mut observation = item_observation
                .lock()
                .expect("attribution observer poisoned");
            for event in events.iter() {
                let path = event.path();
                if let Some(PathSegment::Key(item_id)) = path.front() {
                    record_id(&mut observation, item_id);
                } else if path.is_empty()
                    && let Event::Map(event) = event
                {
                    for item_id in event.keys(txn).keys() {
                        record_id(&mut observation, item_id);
                    }
                }
            }
        });

        let order_observation = Arc::clone(&observation);
        let _order_subscription = self.order.observe(move |txn, event| {
            let mut observation = order_observation
                .lock()
                .expect("attribution observer poisoned");
            let mut old_index = 0usize;
            for change in event.delta(txn) {
                match change {
                    Change::Retain(len) => old_index += *len as usize,
                    Change::Removed(len) => {
                        let end = old_index.saturating_add(*len as usize);
                        if let Some(removed) = before_order.get(old_index..end) {
                            observation.items.extend(removed.iter().copied());
                        } else {
                            observation.error.get_or_insert_with(|| {
                                SharedError::MalformedSchema(
                                    "input order delta exceeds its previous length".into(),
                                )
                            });
                        }
                        old_index = end;
                    }
                    Change::Added(values) => {
                        for value in values {
                            match value {
                                Out::Any(Any::String(item_id)) => {
                                    record_id(&mut observation, item_id)
                                }
                                _ => {
                                    observation.error.get_or_insert_with(|| {
                                        SharedError::MalformedSchema(
                                            "input order contains a non-string value".into(),
                                        )
                                    });
                                }
                            }
                        }
                    }
                }
            }
        });

        let update = decode_exact::<Update>(bytes).map_err(SharedError::MalformedUpdate)?;
        self.doc
            .transact_mut()
            .apply_update(update)
            .map_err(|error| SharedError::UpdateIntegration(error.to_string()))?;

        let mut observation = observation.lock().expect("attribution observer poisoned");
        if let Some(error) = observation.error.take() {
            Err(error)
        } else {
            Ok(std::mem::take(&mut observation.items))
        }
    }

}

fn validate_plain_text<T: ReadTxn>(
}
if let Some(claim) = edit.creation.as_deref()
&& (!edit.touched_items.contains(&claim.item_id) || claim.draft_id != edit.draft_id)
&& claim.draft_id != edit.draft_id
{
return Err(SharedError::InvalidEnvelope(
"creation claim is not routed to a touched item".into(),
"creation claim targets another draft".into(),
));
}
let update =
&mut self,
replica: &mut SharedDraftReplica,
edit: SharedAuthoredEdit,
mut edit: SharedAuthoredEdit,
) -> Result<SyncMessage, SessionError> {
self.ensure_replica(replica)?;
self.ensure_draft(edit.draft_id)?;
self.claims.validate(claim).map_err(SessionError::Model)?;
}
replica.apply_authored_edit(&edit).map_err(fatal_apply)?;
replica
.apply_attributed_edit(&mut edit)
.map_err(fatal_apply)?;
self.outbound_authored(edit)
}

        }
        match message {
            SyncMessage::AuthoredEdit { edit, .. } => {
            SyncMessage::AuthoredEdit { mut edit, .. } => {
                match self
                    .contributions
                    .validate(&edit)
                            self.claims.validate(&claim).map_err(SessionError::Model)?;
                        }
                        replica.apply_authored_edit(&edit).map_err(fatal_apply)?;
                        replica
                            .apply_attributed_edit(&mut edit)
                            .map_err(fatal_apply)?;
                        self.contributions
                            .record(edit.clone())
                            .map_err(SessionError::Model)?;
                }
            }
            SyncMessage::StateVector { vector, .. } => Ok(vec![SyncMessage::StateTransfer {
                thread_id: self.descriptor.thread_id,
                purpose: TransferPurpose::Diff,
                transfer: replica.diff_from(&vector).map_err(SessionError::Model)?,
            }]),
            SyncMessage::StateVector { vector, .. } => {
                // Provenance must arrive before an unattributed diff containing
                // the same CRDT blocks. Otherwise a later envelope would have
                // no observable effect from which to derive its item IDs.
                let mut replies = self
                    .contributions
                    .edits()
                    .iter()
                    .cloned()
                    .map(|edit| SyncMessage::AuthoredEdit {
                        thread_id: self.descriptor.thread_id,
                        edit,
                    })
                    .collect::<Vec<_>>();
                replies.push(SyncMessage::StateTransfer {
                    thread_id: self.descriptor.thread_id,
                    purpose: TransferPurpose::Diff,
                    transfer: replica.diff_from(&vector).map_err(SessionError::Model)?,
                });
                Ok(replies)
            }
            SyncMessage::StateTransfer { transfer, .. } => {
                apply_transfer(replica, &transfer)?;
                Ok(Vec::new())

}

#[test]
fn contribution_items_are_derived_from_the_update_not_the_wire_hint() {
let descriptor = descriptor();
let mut alice = replica(descriptor);
let (first, first_creation) = alice.create_prompt("first").unwrap();
let (second, second_creation) = alice.create_prompt("second").unwrap();
let alice_actor = first_creation.actor_id();

    let mut bob = replica(descriptor);
    bob.apply_authored_edit(&first_creation).unwrap();
    bob.apply_authored_edit(&second_creation).unwrap();
    let actual_edit = bob.replace(second, 6..6, "!").unwrap().unwrap();
    let bob_actor = actual_edit.actor_id();
    let misleading = SharedAuthoredEdit::from_parts(SharedAuthoredEditParts {
        draft_id: actual_edit.draft_id,
        actor_id: actual_edit.actor_id(),
        session_id: actual_edit.session_id(),
        edit_id: actual_edit.edit_id(),
        session_sequence: actual_edit.session_sequence(),
        touched_items: vec![first],
        creation: None,
        update: actual_edit.update_bytes().to_vec(),
    })
    .unwrap();

    let mut target = replica(descriptor);
    let mut session = SyncSession::new(descriptor);
    for creation in [first_creation, second_creation] {
        session
            .receive(
                &mut target,
                SyncMessage::AuthoredEdit {
                    thread_id: descriptor.thread_id,
                    edit: creation,
                },
            )
            .unwrap();
    }
    session
        .receive(
            &mut target,
            SyncMessage::AuthoredEdit {
                thread_id: descriptor.thread_id,
                edit: misleading,
            },
        )
        .unwrap();

    assert_eq!(target.item_text(first).unwrap(), "first");
    assert_eq!(target.item_text(second).unwrap(), "second!");
    assert_eq!(
        session
            .contributions()
            .contributors(first)
            .collect::<Vec<_>>(),
        vec![alice_actor]
    );
    let mut expected = vec![alice_actor, bob_actor];
    expected.sort_unstable();
    assert_eq!(
        session
            .contributions()
            .contributors(second)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        session
            .contributions()
            .edits()
            .last()
            .unwrap()
            .touched_items(),
        &[second]
    );

}

fn deliver(
sender: &mut SyncSession,
sender_replica: &mut SharedDraftReplica,

#[test]
fn periodic_checkpoint_repairs_interleaved_pending_update() {
fn unattributed_state_cannot_be_retroactively_claimed_by_an_unseen_envelope() {
let descriptor = descriptor();
let mut source = replica(descriptor);
let (item, creation) = source.create_prompt("state first").unwrap();

    let mut target = replica(descriptor);
    let mut session = SyncSession::new(descriptor);
    session
        .receive(
            &mut target,
            SyncMessage::StateTransfer {
                thread_id: descriptor.thread_id,
                purpose: TransferPurpose::Checkpoint,
                transfer: source.checkpoint(),
            },
        )
        .unwrap();
    assert_eq!(target.item_text(item).unwrap(), "state first");

    assert!(
        session
            .receive(
                &mut target,
                SyncMessage::AuthoredEdit {
                    thread_id: descriptor.thread_id,
                    edit: creation,
                },
            )
            .is_err()
    );
    assert!(session.contributions().edits().is_empty());
    assert!(session.contributions().contributors(item).next().is_none());

}

#[test]
fn authored_updates_with_missing_dependencies_are_rejected_before_live_mutation() {
let descriptor = descriptor();
let mut source = replica(descriptor);
let mut source_session = SyncSession::new(descriptor);
let mut pending = replica(descriptor);
let mut pending_session = SyncSession::new(descriptor);
pending_session
.receive(
&mut pending,
SyncMessage::AuthoredEdit {
thread_id: descriptor.thread_id,
edit: later,
},
)
.unwrap();
assert!(pending.submission_snapshot().is_err());

    // A later periodic cycle includes a checkpoint as well as vectors, so the
    // missing dependency is repaired even if vectors alone are insufficient.
    deliver(
        &mut source_session,
        &mut source,
        &mut pending_session,
        &mut pending,
    );
    assert_eq!(pending.item_text(item).unwrap(), "ab");
    assert_eq!(
        pending.submission_snapshot().unwrap(),
        source.submission_snapshot().unwrap()
    let before = pending.checkpoint();
    assert!(
        pending_session
            .receive(
                &mut pending,
                SyncMessage::AuthoredEdit {
                    thread_id: descriptor.thread_id,
                    edit: later,
                },
            )
            .is_err()
    );
    assert_eq!(pending.checkpoint(), before);
    assert!(pending_session.contributions().edits().is_empty());
    assert!(pending.item_text(item).is_err());

}

#[test]
This document is the authoritative product behavior for Cowork collaboration.
Implementation status and storage/transport details live in the
[Yrs implementation plan](yrs-composer-plan.md). The application now backs its
ordered prompt and inline-comment editors with Yrs and projects live edits over
iroh. Presence and coordinated cross-client submission remain incomplete.
[Yrs implementation plan](yrs-composer-plan.md), with the document concepts
summarized in [collaborative document data layout](yrs-data-layout.md). The
application now backs its ordered prompt and inline-comment editors with Yrs and
projects live edits over iroh. Presence and coordinated cross-client submission
remain incomplete.

The user reconfirmed these semantics on 2026-09-19: participants may create
multiple blocks and collaborate on every mutable input; Ctrl/Cmd-Enter collects
all apply. Creator identity is attribution, not an editing permission. The
reference to each user's composer does not impose one permanent field per user.
The conceptual layout and storage boundaries are summarized separately in
[yrs-data-layout.md](yrs-data-layout.md).

This confirmation revises the initial storage recommendation: use one Yrs Doc
for the entire active submission generation, containing its ordered prompt
network replica; the older `outbound_authored` still requires its supplied
edit already to exist in the associated replica. Neither authenticates peers
nor persists session state.
nor persists session state. Live authored integration applies each update to
a disposable complete replica first, derives affected item IDs from Yrs map,
text, and order events, and replaces the untrusted wire hint before recording
contributors. Authored updates with missing dependencies are rejected at this
boundary; retained authored envelopes are sent before unattributed diffs so a
diff cannot erase the observable effects needed for item-level attribution.

- Iroh framing transport: `cowork-iroh::{DraftStream, connect, listener_alpns}`
  opens/accepts and repeatedly exchanges those frames with bounded allocation
  and direct backpressure. `DraftRuntime` runs one draft/session worker with

# Collaborative document data layout

This document describes the conceptual layout of a collaborative submission.
It defines what belongs in the shared document, how the pieces relate, and why
the data is divided this way.

Product behavior is defined in [collaboration.md](collaboration.md). Broader
design considerations are recorded in
[yrs-composer-plan.md](yrs-composer-plan.md).

## Document boundary

One shared document represents one active submission generation: the prompts
and comments participants are preparing for the next agent request.

The document does not contain the full thread. Published messages remain
immutable and live outside the active draft. Once a submission is accepted, the
whole generation can be retired and replaced with a fresh one.

Every participant in a shared thread may edit every item. Ownership records who
created an item; it does not restrict who may change it.

## Layout

The document has two top-level collections:

```text
Shared submission
├── order
│   └── [item ID, item ID, ...]
│
└── items by ID
    ├── item ID
    │   ├── kind: prompt
    │   └── body: collaborative text
    │
    └── item ID
        ├── kind: comment
        ├── body: collaborative text
        └── target: immutable-message reference
```

Each item receives a globally unique ID when it is created. The same ID appears
in the order and identifies the item's content.

### Order

The order is a collaborative sequence of item IDs. It determines how pending
prompts and comments appear in the composer and how they are enumerated for a
submission.

The order contains references only, not item bodies. Separating order from
content gives every item a stable identity independent of its current position.
It also allows an item to be reordered without recreating its text or changing
the identity used by cursors, ownership records, and activity history.

Every item must occur exactly once in the order:

- duplicate order entries are invalid;
- an order entry without corresponding content is invalid; and
- item content absent from the order is invalid.

Creating or removing an item changes both the order and the item collection as
one logical operation.

### Items

The item collection associates each stable item ID with its mutable content.
Each item has a kind and its own collaborative text body.

A prompt contains:

```text
Prompt
├── kind: prompt
└── body: collaborative text
```

A comment contains:

```text
Comment
├── kind: comment
├── body: collaborative text
└── target: immutable-message reference
```

Each body is an independent collaborative text surface. Participants editing
different items do not write into one shared string, while participants editing
the same item receive character-level merging.

The initial format is plain text. Formatting, embedded content, attachments,
and additional properties require an explicit schema revision rather than being
silently interpreted by older clients.

### Comment targets

A comment points to an immutable published message. Its target contains:

```text
Message reference
├── thread identity
├── message identity
├── fingerprint of the canonical message
├── optional range in the canonical source
└── optional quote preview
```

The target is one atomic value because its fields describe one coherent
reference and must change together. Independently combining a message identity
from one edit with a fingerprint or source range from another could create a
reference that never existed.

The referenced message itself is not copied into the collaborative document.
Editing a comment never edits the published message.

The content fingerprint identifies the authoritative message version. A source
range is interpreted only against that exact version. A quote preview is a
bounded display and diagnostic aid, not authoritative content.

## Ownership and contributors

Every prompt and comment is tied to one creator through its stable item ID:

```text
Item creation record
├── submission generation
├── item ID
├── creator
└── initial item kind and comment target, when applicable
```

Creator identity is kept outside the freely editable collaborative document.
Otherwise any participant with document write access could rewrite ownership as
an ordinary content edit.

Participants who later change an item become contributors to that item. The
creator remains the owner; contributors are a separate, derived set.

For each authored change, the surrounding collaboration record contains:

```text
Attributed change
├── submission generation
├── change identity
├── participant and session
├── affected item IDs
└── collaborative change
```

Affected item IDs are derived from what the collaborative change actually
modifies. They are not accepted as an assertion from the sender. This makes it
possible to answer “who changed this prompt or comment?” without maintaining
character-by-character authorship.

State snapshots and reconciliation data carry document state, not authorship.
They must never be attributed to the participant who happened to forward them.
Original attributed changes must be retained separately when contributor
history matters.

## State outside the document

The shared document contains collaborative draft content only. Related state
with different authority or lifetime belongs elsewhere:

| State                                      | Why it is separate                                        |
| ------------------------------------------ | --------------------------------------------------------- |
| Submission generation and thread identity  | Routes the document and controls its lifetime             |
| Creator ownership                          | Must not be rewritable as ordinary collaborative content  |
| Contributor history                        | Must remain tied to authenticated authored changes        |
| Published messages                         | Immutable history must not be changed through a draft     |
| Cursor, selection, focus, and connectivity | Ephemeral presence expires with a session                 |
| Synchronization state                      | Transport and recovery machinery are not content          |
| File bytes                                 | Large immutable data belongs in content-addressed storage |
| Downloads, viewport, and local focus       | Local presentation state is not shared content            |

This separation prevents the collaborative document from becoming an authority
for identity, permissions, immutable history, or local UI state.

## Validation invariants

A document is valid only when:

- it contains exactly the order and item collections expected by its schema;
- every order entry is a valid item ID;
- every item appears exactly once in the order;
- every ordered item has corresponding content;
- no unreferenced item content remains;
- every item has a supported kind;
- prompts contain only their kind and body;
- comments contain only their kind, body, and target;
- bodies contain only content supported by the schema version; and
- comment targets identify the same thread and have a valid reference shape.

Unknown roots, properties, item kinds, formatting, or embedded values are
reported as unsupported. Older clients must not flatten, discard, or silently
reinterpret content they do not understand.

## Draft and submission views

The same document supports two conceptual views.

The live draft view includes every integrated item, including empty and
whitespace-only bodies. This preserves empty composers and lets participants
clear and retype an item without changing its identity.

The submission view includes only meaningful pending input. Empty or
whitespace-only prompts and comments are omitted. A target alone does not make
an empty comment authored content.

A submission must not be produced while the document is known to be incomplete
or while an item lacks its external creator record. These conditions could
otherwise produce an apparently valid but incomplete or unattributed agent
request.

Both views follow the shared order and combine document content with external
owner and contributor information.

## Future extensions

Additional collaborative fields should follow the same division:

- independently editable values use independently mergeable structures;
- values whose fields must remain coherent use atomic references;
- ordered attachment instances use one canonical placement representation;
- attachment bytes remain outside the document;
- full block editing uses one canonical block-tree representation rather than a
  writable text body and block tree in parallel; and
- schema changes use an explicit version and account for older connected
  clients and stored positions.
