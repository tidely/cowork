# Collaborative drafts

This is the specification for how Cowork participants prepare, edit, and
submit agent requests together. It defines user-facing behavior, the shared
Yrs document, and the protocol between host and collaborators.

## Status

All five steps of the [implementation order](#implementation-order) are done:
participant identity, the protocol version handshake, shared model selection
from the host's catalog (grouped by provider), stopping from any participant,
Yrs drafts with prompt blocks, comments as items, per-block attachments,
navigation, and empty-item removal, the draft
synced through the host with host-coordinated submission, presence, and
attachment transfer.

Not yet implemented:

- telling a collaborator why the host rejected their submission (generating,
  attachments loading, size limit); the request is currently dropped
  silently;
- showing names when hovering a remote caret or an avatar (names show only
  briefly after a caret moves, and on the top bar's avatars);
- validating collaborators' presence at the host (e.g. announced file reads);
- showing others' selections in agent messages, which needs a gpui-kit
  addition; see [gpui-kit-text-view-highlights.md](gpui-kit-text-view-highlights.md).

Implementation notes on presence:

- Presence is published whenever it changes, checked on every frame and
  whenever a file read starts or ends. It is not coalesced.
- Besides each participant removing an empty item they leave while nobody
  else is in it, the host removes an empty item as soon as presence shows
  everyone has left it, since two people leaving at once would each still see
  the other there.

Implementation notes on attachment transfer:

- Each connection has a control queue and a small bulk queue. Attachment
  bytes travel on the bulk queue in 64 KiB chunks, and the writer always
  sends waiting control messages first, so a large file delays draft updates
  and presence by at most one chunk. Reading runs separately from writing,
  so two sides sending files at once never wait on each other.
- A collaborator whose upload's record disappears stops uploading and sends
  `AttachmentCancelled` on the bulk queue, after its last chunk, so the host
  discards what it received. Participants ignore pieces of files they have
  discarded.
- Bytes and the draft record that announces them travel separately and can
  arrive in either order. Every chunk names the file, so it can be put
  together without the record; the host stores an upload once both are there.
- The host discards a file when an update removes its record, or when its
  uploader leaves before finishing. Collaborators keep every file they have,
  even ones they remove themselves: a submission can include a file that is
  being removed at the same time, and the host sends each file only once.
- The per-message size limit is checked when a file is added, against the
  records already in the draft, and again by whoever accepts a submission.

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
- Each participant has a **profile**: a display name and a profile picture,
  both optional and chosen by the participant, plus an **appearance** UUID.
  Without a name or picture, a participant shows as two filler words (e.g.
  "Amber Otter") and initials, derived deterministically from the
  appearance; so is their color. Each app sends its own per-launch UUID as
  the appearance, so a participant looks the same in every thread even though
  the host assigns a new participant UUID on each join. Without an
  appearance, the participant UUID is used. Profiles are cosmetic, never
  authoritative, and may collide; they are not disambiguated.
- A collaborator sends its profile right after `Join`, and again whenever it
  changes. The host validates it (a trimmed name of at most 40 characters; a
  JPEG of exactly 256×256 pixels and at most 128 KiB) and disconnects peers
  that send an invalid one. Profiles travel in `ParticipantJoined`,
  `ProfileChanged`, and every snapshot, and are kept after a participant
  leaves, so their messages still name them.
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

- Comments come first, as today, each labeled with its creator's prompt
  name. The agent must still call `respond_to_comment` once per comment.
- Each prompt block follows, in order, preceded by its creator's prompt
  name.
- A participant's **prompt name** is their display name when their first
  item is submitted in the thread, and never changes afterwards. Renaming
  shows everywhere in the UI, but the agent keeps knowing them by one name,
  and nothing it was already sent changes.
- The host keeps a **transcript** of everything the agent was sent and
  replied, including reasoning, tool calls, and tool results, and sends it
  verbatim as the history of the next run. Each request therefore extends
  the previous one exactly, keeping the provider's prompt cache valid. A
  run that is stopped or fails keeps its prompt and every turn it
  completed.
- Every participant mirrors the transcript and the prompt names. The
  host forwards what its agent loop reports, as it happens, in
  `AgentEvent`: Rig's stream events (deltas and block boundaries), the end
  of each model turn with its usage, and each tool's result. Everyone, the
  host included, folds them the same way (the agent crate's `TurnFold`)
  into the transcript, which comes out exactly as the agent loop recorded
  it. Each piece of output travels once: the stream's terminal record and
  provider payloads Rig does not model are not forwarded, and a block's end
  drops the completed block, which folding rebuilds.
- What an agent message shows (its thinking, text, tool calls with their
  results, and replies to comments) is never sent. It is a function of Rig
  messages alone: the transcript entries its run added (those after its
  prompt, up to the next run's prompt), followed by the message the run is
  folding, as far as it has come, as Rig's stream accumulator has it. That
  message is the fold of the run's **pending events**: those folded since
  its output last joined the transcript. A run that is stopped or fails
  mid-turn keeps its pending events, since that output never joins the
  transcript. Comment replies
  come from `respond_to_comment` calls, with ids derived from the message
  and call, so every participant names them alike. A failure message is
  kept apart from the agent's output, and shown only when there is none.
- The transcript is kept as Rig messages; they and the agent events travel
  as JSON, which postcard cannot represent directly. Their encoding is
  Rig's, so upgrading Rig bumps the protocol version. A prompt joins the
  transcript with `AgentStarted`, including its files' content, images as
  base64, so attachment bytes also travel inline besides as
  `AttachmentData`. A `Welcome` carries the transcript and, for each agent
  message, where its prompt is, its pending events, and whether its run
  has ended. The joiner folds each run's pending events, which rebuilds the
  message it was folding exactly, and so shows what everyone else does. For
  the running message, it keeps that fold, so someone joining mid-turn
  folds the rest of it. Before that, the
  joiner checks that the snapshot fits its transcript: every prompt is a
  user message there, in order, only the last run can still be
  generating, and no run's pending events complete a message the
  transcript lacks. A frame larger than 1 GiB cannot be sent; the
  collaborator is then disconnected.
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
- Every thread carries a model catalog: the models of whoever runs its agent,
  grouped by provider. Local and hosted threads use this app's discovered
  catalog; a mirrored thread uses the host's, which arrives in the
  `Welcome` snapshot and is replaced by `ModelCatalogChanged` after each
  successful discovery (including with an empty catalog). Failed discovery
  leaves the previous catalog in place.
- Providers are a fixed enum known at compile time, and the provider is part
  of a model's identity (`ModelRef` is a provider and the provider's model
  ID). The catalog is keyed by provider and then by model ID, so neither can
  repeat. Provider names and icons are fixed per provider and never sent.
- Anyone can pick a model the thread's catalog offers. The client sends
  `SelectModel`. The host applies it in arrival order if its catalog offers
  the model, ignores it otherwise, and broadcasts `ModelSelected`. A peer
  waits for that confirmation before showing the new selection, since the
  host's catalog may have changed. Picking a host model does not change the
  default for new local threads.
- When a catalog stops offering the selected model, the model stays
  selected but is shown grayed out, with a tooltip saying it is unavailable,
  and nobody can send until another model is picked. The host refuses
  submissions for it as well. Once another model is picked, the unavailable
  one is no longer listed. It becomes available again if the catalog offers
  it again.
- A run uses the model selected when its submission is accepted. Changing
  the model during a run affects the next run.

This is independent of the draft document and is the first feature to build.

## Sharing lifecycle

- **Starting**: the host's existing draft becomes the shared draft
  unchanged. Its items keep the host as creator.
- **Joining**: the collaborator sends `Join` with its protocol version,
  then its `Profile`. The host rejects mismatched versions. Otherwise it
  replies with `Welcome`
  (see [Protocol](#protocol)), then streams the bytes of every attachment
  in the thread.
- **Falling behind**: a collaborator that lags the host's event buffer
  receives a new `Welcome`. It **merges** the draft state into its existing
  replica instead of replacing it, so its unsent local edits survive.
- **Collaborator disconnects**: its presence disappears. The host applies the
  empty-item and incomplete-upload cleanup described above.
- **Host disconnects**: the session ends and collaborators' copies of the
  thread are removed, as today. Host migration is out of scope.
- **Invalid host data**: if a transcript message or agent event from the host
  is not valid Rig JSON, the collaborator treats it as a protocol error and
  removes its mirrored thread. The application stays open.

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
| Participants and profiles           | Host session, synced by protocol; colors from UUIDs      |
| Prompt names and agent transcript   | Thread state at the host, mirrored by protocol           |
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

| Message          | Purpose                                                |
| ---------------- | ------------------------------------------------------ |
| `Join`           | First message; carries the protocol version            |
| `Profile`        | Second message, and again whenever the profile changes |
| `DraftUpdate`    | Encoded Yrs update from a local transaction            |
| `Presence`       | Replaces this participant's presence state             |
| `AttachmentData` | Bytes of an attachment this participant added          |
| `Submit`         | Requests a submission at the given sequence            |
| `Stop`           | Stops the named agent run                              |
| `SelectModel`    | Selects a model by provider and model ID               |

**Host to collaborators**

| Message                                      | Purpose                                                                                                                                                                                                                                                                                    |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Welcome`                                    | Participant UUID, timeline snapshot (including the model catalog and selected model, transcript, each agent message's prompt position, pending events and run state, and prompt names), draft state, participants, their profiles and presence, submission sequence, stored attachment IDs |
| `Rejected`                                   | Join refused (e.g. protocol version) or a request refused; peer-specific                                                                                                                                                                                                                   |
| `DraftUpdate`                                | Yrs update from another participant or the host                                                                                                                                                                                                                                            |
| `ParticipantJoined` / `Left`                 | Membership changes; joining carries the participant's profile                                                                                                                                                                                                                              |
| `ProfileChanged`                             | A participant's new profile                                                                                                                                                                                                                                                                |
| `Presence`                                   | A participant's latest presence                                                                                                                                                                                                                                                            |
| `AttachmentData`                             | Relayed attachment bytes                                                                                                                                                                                                                                                                   |
| `AttachmentStored`                           | The host holds every byte of an attachment                                                                                                                                                                                                                                                 |
| `ModelSelected`                              | The thread's model changed                                                                                                                                                                                                                                                                 |
| `ModelCatalogChanged`                        | Replaces the thread's model catalog (including with an empty one)                                                                                                                                                                                                                          |
| `UserMessage`                                | An accepted submission, now carrying its sequence, creators, and attachment references instead of bytes                                                                                                                                                                                    |
| `ThreadTitled`, `AgentStarted`, `AgentEnded` | The title; a run starting, with its prompt; and ending, with its duration and any failure                                                                                                                                                                                                  |
| `AgentEvent`                                 | What the host's agent loop reported during a run, which everyone folds into the agent message and transcript                                                                                                                                                                               |
| `PromptNamed`                                | A participant's prompt name, fixed on their first submitted item                                                                                                                                                                                                                           |

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
   presence-aware removal (`ThreadDraft::remove_if_unattended`), replacing
   the creator-only removal rule used during step 3.
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
