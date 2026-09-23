# Collaborative drafts

This is the specification for how Cowork participants prepare, edit, and
submit agent requests together. It defines user-facing behavior, the shared
Yrs document, and the protocol between host and collaborators.

## Status

Steps 1 to 3 of the [implementation order](#implementation-order) are done:
participant identity, the protocol version handshake, shared model selection,
stopping from any participant, Yrs drafts with prompt blocks, comments as
items, per-block attachments, navigation, and empty-item removal, and the
draft synced through the host with host-coordinated submission.

Two interim rules apply until later steps replace them; see
[Implementation order](#implementation-order). Not yet implemented: telling a
collaborator why the host rejected their submission (generating, attachments
loading); the request is currently dropped silently.

## Summary

- Every thread has one draft, backed by one Yrs document, whether or not the
  thread is shared.
- The draft is an ordered collection of **items**: prompt blocks and comments.
  Prompt blocks carry their own attachments.
- Every participant can create, edit, and remove every item. The creator is
  attribution, not a permission.
- Participants write independent prompt blocks by default and co-edit a block
  by deliberately moving into it.
- Live cursors, selections, focus, and attachment reads are shared as
  ephemeral presence, outside the document.
- Anyone can submit, stop the agent, or change the model. The host
  coordinates so each submission happens exactly once.

## Terminology

- **Participant**: someone connected to a shared thread, including the host.
- **Host**: the participant whose endpoint serves the thread. The host runs
  the agent and holds the authoritative copy of all shared state.
- **Item**: one prompt block or one comment in the draft.
- **Prompt block**: an independently editable prompt with its own
  attachments.
- **Comment**: an editable note targeting an excerpt of an assistant message.
- **Creator**: the participant who created an item.
- **Editor**: a participant whose focus is currently inside an item.
- **Draft position**: the empty spot below the last prompt block. Typing
  there creates a new prompt block.
- **Submission**: an ordered snapshot of the draft's non-empty items, published
  as one immutable user message and sent to the agent as one request.

## Participants and identity

- The host assigns each collaborator a random participant UUID when it joins,
  and returns it in `Welcome`. The host generates its own UUID once per app
  launch and uses it for its local threads too.
- Reconnecting is a fresh join with a new participant UUID. Items created
  earlier keep their original creator.
- Display names are two filler words (e.g. "Amber Otter") and a color, both
  derived deterministically from the UUID. Every client computes the same
  result, so names are never transmitted. Names are not authoritative and
  may collide.
- Identity will later be tied to real accounts or keys by mapping an
  authenticated identity to a participant UUID at the host. Nothing in the
  document depends on how identity is established.

## The draft

### Composer layout

The composer column is shown below the timeline for every participant:

1. **Comments**: all draft comments, in draft order, under a collapsible
   "N comments" toggle. Collapsing is local UI state.
2. **Prompt blocks**: in draft order. Each block shows its attachment row
   above its text, like the current composer.
3. **Draft position**: an empty row below the last block. It is rendered only
   when there are no blocks or when at least one participant is there.
   Clicking the empty space below the blocks moves the local participant
   there.

The shared order spans all items, but the composer always groups by kind:
comments first, then prompt blocks. Relative order within each group follows
the shared order.

### Avatars

Each prompt block has an avatar gutter, like timeline messages:

- The creator's avatar is primary, even when the creator is not editing.
- Other current editors are shown as smaller avatars layered beside it, in
  a deterministic order (join order) that does not reshuffle on presence
  updates.
- At most the creator plus two editors are shown, then "+N".
- The layout reserves space so avatar changes never shift text horizontally.
- Hovering an avatar shows the participant's name.

The draft position row shows the avatars of participants who are at it. When
the draft is empty, everyone's avatar is layered on that single row.

Comment cards show their creator's avatar inside the card, as today, with
editors layered beside it. Ownership of every comment must be clear.

### Creating prompt blocks

Independent prompts are the default.

1. When the draft is empty, every participant's caret sits at the same draft
   position. Overlapping carets use participant colors and never create
   separate rows.
2. Typing at the draft position creates a prompt block. The typist becomes
   its creator and stays in it.
3. Everyone else at the draft position remains at the draft position, which
   now renders below the new block.
4. Typing at the draft position always creates a new block, even if that
   participant already created others.
5. New blocks are appended to the end of the shared order.
6. If two participants type at the draft position concurrently, two blocks
   are created. Yrs orders the concurrent inserts identically on every
   replica.

Adding an attachment while at the draft position also creates a new block,
with an empty body and that attachment.

### Joining and navigating

- Clicking into any item places the caret there.
- Up and Down move through one navigation chain in visual order: expanded
  composer comments, then prompt blocks, then the draft position.
- Up/Down leave an item only from its first or last **visual** line
  (respecting wrapping). The caret enters the adjacent item on its nearest
  visual line, keeping horizontal position where possible.
- Down from the last line of the last block moves to the draft position. Up
  from the draft position moves into the last block.
- A collapsed comment group is skipped. Inline comment editors in the
  timeline keep Up/Down within themselves.
- Once in an item, participants edit concurrently with character-level
  merging. Native Textarea behavior (selection, IME, replace, delete) is
  preserved.
- Leaving an item never changes its creator.

### Removing items

Empty items disappear naturally when nobody is using them:

- An item is **empty** when its body has no non-whitespace content and, for
  prompt blocks, it has no attachments.
- When a participant's focus leaves an empty item and presence shows nobody
  else focused in it, that participant removes it.
- **Escape** in an empty item removes it (if nobody else is focused in it)
  and moves the caret to the draft position.
- **Backspace** in an empty item removes it (if nobody else is focused in it)
  and moves the caret to the end of the previous stop in the navigation
  chain.
- If others are focused in the empty item, it stays. The last to leave
  removes it.
- When a participant disconnects, the host removes empty items that the
  participant was focused in and nobody else is.
- Switching to another window or application does not count as leaving.

A removal can race with someone else entering the item. Their concurrent
edits are lost. This is accepted.

### Comments

- Typing while an excerpt of an assistant message (or an agent reply to a
  comment) is selected creates a comment targeting that excerpt, with the
  typed character as its first content.
- Comments can target messages that are still generating. Streaming only
  appends to a message's source, so a target range stays valid.
- Every participant sees every draft comment: highlighted in the target
  message, with an inline editor next to it, and as a card in the composer.
  Both editors edit the same shared text and show remote carets.
- A comment's target is fixed at creation.
- Comments have no attachments. Pasting files into a comment editor inserts
  only text.

### Attachments

Attachments belong to a prompt block. Anybody can add or remove any
attachment. File bytes are sent over the protocol, never stored in the
document.

**Choosing the target block**:

- Pasting or dropping onto a block attaches to that block.
- The paperclip button attaches to the focused prompt block.
- Otherwise (draft position, a comment, or no focus), a new block is created.

**Lifecycle**:

1. **Reading**: the adding participant reads the file locally. Presence
   advertises the pending read (name, whether it is an image, progress,
   target block), so everyone sees a placeholder chip with a progress bar.
2. **Uploading**: once read, the adder appends an attachment record to the
   target block and streams the bytes to the host. If the target block was
   removed in the meantime, a new block is created for it.
3. **Stored**: when the host has every byte, it announces the attachment as
   stored.
4. **Downloading**: the host relays bytes to every other participant. Each
   participant's chip shows a progress bar until the bytes are available
   locally.

Only the host needs the bytes to send. A participant can submit as soon as
the host has stored every attachment, even if other participants are still
downloading.

**Failure and removal**:

- A failed local read removes the placeholder. The error is shown only to
  the adder, as today.
- If the adder disconnects before the upload finishes, the host removes the
  record and discards the partial bytes.
- Removing an attachment while it uploads cancels the upload. The host
  discards bytes for attachments no longer referenced by the draft or the
  timeline.
- Existing size limits apply: per file when adding, and the per-message total
  across all submitted blocks when submitting.

### Presence

Each participant publishes one presence state:

```text
Presence
├── focus: none | draft position | item ID
├── selection: anchor and head as Yrs sticky indices in the focused body
└── pending reads: [name, is image, progress, target block or new block]
```

- Presence is sent through the host, which rebroadcasts it tagged with the
  participant UUID. Only the latest state matters, so updates may be
  coalesced.
- The host drops a participant's presence when its connection closes, so no
  timeouts are needed.
- Remote carets and selections are painted in the participant's color and
  never move local focus. A caret shows the participant's name when hovered
  or briefly after it moves.
- Presence is never stored in the document.

## Thread controls

### Submission

Pressing Ctrl-Enter (Cmd-Enter on macOS) in any composer editor or at the
draft position, or clicking Send, submits the whole draft for everyone.

Send is enabled for everyone when:

- the agent is not generating;
- the draft contains at least one non-empty item;
- no participant has a pending attachment read; and
- the host has stored every attachment in non-empty blocks.

The host accepts a submission as follows:

1. The submitter sends `Submit` with the submission sequence it has seen. It
   sends all its pending draft updates first, on the same ordered stream, so
   its own edits are always included.
2. The host ignores a stale sequence: someone else's submission was already
   accepted and everyone sees it.
3. From its replica, the host snapshots every non-empty comment and every
   non-empty prompt block (with attachments), in draft order. This includes
   blocks others are still typing in.
4. In one transaction, the host removes exactly the snapshotted items from
   the draft. Empty items stay, for example a comment someone has not started
   typing yet.
5. The host broadcasts that draft update and the published user message with
   the next sequence number, then starts the agent run.
6. Edits that arrive for removed items are discarded. Participants whose
   focused item was submitted move to the draft position.

Other rejections (generating, empty, attachments not stored, size limit) are
reported only to the submitter.

**Published user message**: one timeline entry, rendered like today:

- a collapsible comment group, each card showing its creator's avatar; then
- one row per prompt block, with the creator's avatar in the gutter, its
  attachment row, and its text.

A single-block submission looks like the current user message.

**Agent prompt**: the same format for shared and local threads.

- Comments come first, as today, each labeled with its creator's name. The
  agent must still call `respond_to_comment` once per comment.
- Each prompt block follows, in order, preceded by its creator's name.
- Each block's attachments stay with that block, using today's encoding.
- Mentions (once supported) render as `@Name`.

Illustrative, not normative:

```text
The user attached the following inline comments ...

1. <comment id> — Amber Otter, on an excerpt from assistant message 3:
> quoted excerpt
Comment: Why is this unsafe?

Amber Otter:
Investigate the crash.

Brisk Heron:
Also check the attached log.
<attachment name="crash.log">...</attachment>
```

The host titles a new thread from the first prompt block, or from the first
comment when a submission has only comments.

Submitting while the agent is generating is rejected. Participants can keep
editing the draft during generation.

### Stopping

Everyone sees the Stop button while the agent is generating. `Stop` names
the running agent message. The host cancels that run if it is still active
and ignores the request otherwise. The message ends as it does today.

### Model selection

- Model selection is per thread. New local threads start with the last model
  selected locally.
- Anyone can pick a model. The client sends `SelectModel` with the catalog
  ID. The host applies it in arrival order, ignores unknown IDs, and
  broadcasts `ModelSelected`. Every picker updates live.
- A run uses the model selected when its submission is accepted. Changing
  the model during a run affects the next run.

This is independent of the draft document and is the first feature to build.

## Sharing lifecycle

- **Starting**: the host's existing draft becomes the shared draft
  unchanged. Its items keep the host as creator.
- **Joining**: the collaborator sends `Join` with its protocol version. The
  host rejects mismatched versions. Otherwise it replies with `Welcome`
  (see [Protocol](#protocol)), then streams the bytes of every attachment
  in the thread.
- **Falling behind**: a collaborator that lags the host's event buffer
  receives a new `Welcome`. It **merges** the draft state into its existing
  replica instead of replacing it, so its unsent local edits survive.
- **Collaborator disconnects**: its presence disappears. The host applies the
  empty-item and incomplete-upload cleanup described above.
- **Host disconnects**: the session ends and collaborators' copies of the
  thread are removed, as today. Host migration is out of scope.

Nothing is persisted. A draft lives as long as its thread exists in the
host's app.

## Document layout

One Yrs document per thread holds the draft. It holds collaborative content
only.

```text
Draft document
├── order: Array<ItemId>
└── items: Map<ItemId, Item>

Prompt item (Map)
├── kind: "prompt"
├── creator: ParticipantId          write-once
├── body: Text
└── attachments: Array<AttachmentRecord>

Comment item (Map)
├── kind: "comment"
├── creator: ParticipantId          write-once
├── target: CommentTarget           atomic, write-once
└── body: Text

AttachmentRecord (atomic value)
├── id
├── name
├── kind: text | png | jpeg
├── size
└── creator: ParticipantId

CommentTarget (atomic value)
├── message_id                      agent message or comment reply
├── range                           byte range in the message's markdown source
└── quote                           display text captured at creation
```

Design decisions:

- **Order separate from content.** Items have stable UUIDs independent of
  position, so carets, presence, and creator records survive concurrent
  inserts and removals. Creating or removing an item changes `order` and
  `items` in one transaction.
- **One document per thread.** Accepting a submission removes the submitted
  items rather than replacing the document. Unsubmitted items (and the carets
  in them) keep their identity. Late updates need no routing, since they land
  in removed items. The submission sequence, not the document, prevents
  duplicate submissions.
- **Creator in the item.** It arrives atomically with the item, so no item is
  ever shown without a creator. Clients never write it after creation. The
  host enforces this. Future authentication is enforced at the host, not in
  the document.
- **Atomic targets and attachment records.** Their fields describe one
  coherent value and must never merge field by field.
- **Bodies are Yrs `Text`.** Version 1 writes plain unformatted text. The
  composer does not render markdown, so typed markdown is literal text.
  Planned additions fit `Text` without changing the layout:
  - mentions as inline embeds `{ "mention": <participant UUID> }`, so the
    participant reference is one unit instead of editable characters; and
  - inline formatting as Yrs formatting attributes.

  Clients must never flatten embeds or attributes into plain text.

### State outside the document

| State                               | Where it lives                                           |
| ----------------------------------- | -------------------------------------------------------- |
| Participants, names, colors         | Host session; names derived from UUIDs                   |
| Presence                            | Host-relayed protocol messages                           |
| Selected model                      | Thread state at the host, synced by protocol             |
| Submission sequence                 | Thread state at the host                                 |
| Attachment bytes and stored status  | Host byte store keyed by attachment ID, relayed to peers |
| Published timeline, agent runs      | Thread state, synced by the existing host events         |
| Folding, scroll, local errors, etc. | Local UI state                                           |

## Protocol

All traffic goes through the host. Collaborators never talk to each other.
The messages below are conceptual. Names and shapes will follow the existing
`protocol.rs` style.

**Collaborator to host**

| Message            | Purpose                                                         |
| ------------------ | --------------------------------------------------------------- |
| `Join`             | First message; carries the protocol version                     |
| `DraftUpdate`      | Encoded Yrs update from a local transaction                     |
| `Presence`         | Replaces this participant's presence state                      |
| `AttachmentData`   | Bytes of an attachment this participant added                   |
| `Submit`           | Requests a submission at the given sequence                     |
| `Stop`             | Stops the named agent run                                       |
| `SelectModel`      | Selects a model by catalog ID                                   |

**Host to collaborators**

| Message                      | Purpose                                                                                                                                  |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `Welcome`                    | Participant UUID, timeline snapshot, draft state, participants and their presence, model, submission sequence, stored attachment IDs |
| `Rejected`                   | Join refused (e.g. protocol version) or a request refused; peer-specific                                                                 |
| `DraftUpdate`                | Yrs update from another participant or the host                                                                                          |
| `ParticipantJoined` / `Left` | Membership changes                                                                                                                       |
| `Presence`                   | A participant's latest presence                                                                                                          |
| `AttachmentData`             | Relayed attachment bytes                                                                                                                 |
| `AttachmentStored`           | The host holds every byte of an attachment                                                                                               |
| `ModelSelected`              | The thread's model changed                                                                                                               |
| `UserMessage`                | An accepted submission, now carrying its sequence, creators, and attachment references instead of bytes                                  |
| existing agent events        | Unchanged: title, agent start, streamed text, comment replies, end                                                                       |

The host is itself a participant. Its local edits, submissions, stops, and
model changes go through the same logic as a collaborator's.

Attachment bytes travel in bounded chunks, or on their own stream, so a
large file never delays draft updates or presence.

## Validation

After applying a collaborator's `DraftUpdate`, the host checks:

- the document has only the `order` and `items` roots;
- every `order` entry is unique and has an item, and every item is in
  `order`;
- every item has a supported kind and exactly the fields listed for it;
- `creator` and `target` never change, and a new item's creator is the
  sending participant;
- bodies contain only version-1 content;
- attachment records are well-formed, unique by ID, and within size limits.

Any violation is a protocol error, and the host disconnects the peer. The
update has already been applied by then. With no authentication this is a
defensive check, not a security boundary. If authentication makes this
insufficient, the host can validate each update against a scratch copy
before applying it.

Protocol versions must match exactly and nothing is persisted, so the
document needs no schema version or compatibility with older clients.

## Implementation notes

- **One code path.** Local and shared threads both back their draft with
  Yrs. A local thread simply has no peers.
- **Threading.** Each client's replica lives in its GPUI `Thread` entity.
  Transactions are synchronous and never cross an `await`. The Tokio side
  moves only encoded bytes. The host's replica is authoritative.
- **Optimistic local edits.** Local edits never wait for the network. Queued
  outgoing updates may be merged while the channel is busy.
- **Offsets.** Application-facing text offsets are UTF-8 bytes, converted to
  Yrs offsets at the boundary. Carets and selections that must survive
  concurrent edits use sticky indices.
- **Remote edits in focused editors.** Remote edits must preserve the local
  caret and selection, and are deferred while IME composition is active.
- **Comment editors.** The inline and composer editors of a comment bind to
  the same `Text`. The current copy-on-change sync goes away.
- **Undo.** Undo must never revert another participant's edits. Scope it to
  local-origin changes, for example with a Yrs `UndoManager`.
- **Client IDs.** Each replica uses a fresh random Yrs client ID. A rejoin
  creates a new replica.

## Implementation order

1. **Shared thread controls, no Yrs**: participant UUIDs and derived names,
   protocol version in `Join`, per-thread model selection with
   `SelectModel`, and `Stop` from any participant.
2. **Local Yrs drafts**: move drafts onto Yrs for all threads. Add
   multi-block composing, comments as items, per-block attachments,
   navigation, empty-item removal, and the new prompt format.
3. **Draft sync**: sync the draft through the host, make collaborators
   writable, and submit through the host with sequence numbers.
4. **Presence**: remote carets and selections, avatar gutters, and
   presence-aware removal.

   **Replace the interim removal rule.** Without presence, nobody can tell
   whether someone else is typing in an item. Until this step, an empty item
   is removed automatically (on blur, or when its last attachment is
   removed) only by its creator. Escape and Backspace still remove any empty
   item. This step replaces the creator check with "nobody else is focused in
   it" (`ThreadDraft::remove_if_abandoned`), and adds the host's cleanup of
   empty items a disconnecting participant was focused in.
5. **Attachment transfer**: send bytes separately with progress and stored
   status, and reference attachments by ID in published messages and
   `Welcome`.

   **Enable attaching in joined threads.** Until this step, collaborators
   cannot attach files: the attach button is disabled, and pasting or
   dropping files does nothing (`Cowork::draft_accepts_attachments`).

## Future work

These fit the design without changing the document layout:

- **Queued messages**: accepting a submission (snapshot and remove) is
  already separate from starting a run. Queueing means accepting while
  generating, showing the accepted message as pending, and running it after
  the current run.
- **Permissions**: binary edit/view. The host drops draft updates, presence
  edits, `Submit`, `Stop`, and `SelectModel` from viewers. Viewers get the
  current read-only rendering. Nothing changes in the document.
- **Authentication**: the host maps authenticated identities to participant
  UUIDs and enforces `creator`.
- **Contributor history**: the host can derive which items each participant's
  updates touched from Yrs events, without changing the document.
- **Mentions and inline formatting**: see
  [Document layout](#document-layout).
- **Reordering blocks**: possible because order is separate from content. No
  UI is planned.

Out of scope: host migration, persistence, keeping identity across
reconnects, collaborative undo semantics beyond undoing local changes, and an
invitation format beyond today's endpoint ID.

## Acceptance scenarios

**Two participants start from empty**

1. Alice and Bob join an empty shared thread. Both carets and avatars share
   one draft row.
2. Alice types. A block is created with Alice as creator, and she stays in
   it.
3. Bob sees Alice's text and caret live. His caret is at the draft position
   below her block.

**Independent and multiple blocks**

1. Alice creates block A, and Bob types at the draft position, creating B.
2. Alice moves to the draft position and types, creating C.
3. Every replica shows A, B, C, and the agent prompt lists them in that
   order under their creators' names.

**Co-editing**

1. Bob presses Up from the draft position into A.
2. Bob's avatar layers beside Alice's on A.
3. Both edit concurrently. Replicas converge, and each caret stays anchored
   through the other's edits.

**Empty block removal**

1. Bob clears A while Alice's caret is still in A. A stays.
2. Alice clicks away. A is removed for everyone.

**Submission while typing**

1. Alice and Bob each have a non-empty block. Bob is mid-sentence.
2. Alice presses Ctrl-Enter. One user message with both blocks appears for
   everyone, and exactly one agent run starts.
3. Bob's characters typed after the host's snapshot are discarded, and his
   caret moves to the draft position.
4. Bob pressing Ctrl-Enter at the same moment starts no second run.

**Comments**

1. While the agent is still streaming, Bob selects part of its answer and
   types, creating a comment.
2. Alice sees the highlight, the inline editor, and the composer card with
   Bob's avatar, and can edit the comment.
3. After submission, the agent replies to the comment, and the reply shows
   under the comment in the response.

**Attachments**

1. Bob pastes an image into his block. Everyone sees a loading chip, and
   Send is disabled.
2. Once the host has stored the image, Alice can send even though Carol is
   still downloading it.
3. The published message shows the image in Bob's block. Carol's chip
   finishes loading.

**Thread controls**

1. Carol, a collaborator, changes the model. Every picker updates, and the
   next run uses it.
2. During the run, Bob presses Stop. The host cancels the run for everyone.

**Late join**

1. Dave joins mid-generation. He receives the timeline, the draft with
   everyone's items, presence, the model, and then attachment bytes.
2. His replica converges with the host, and his first edit is visible to
   everyone.
