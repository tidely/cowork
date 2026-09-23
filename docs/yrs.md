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
