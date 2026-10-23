# Collaborative drafts

This is the specification for how Cowork participants prepare, edit, and
submit agent requests together. It defines user-facing behavior, the shared
Yrs document, and the protocol between host and collaborators.

## Status

The LLM transcript is the host's agent session's conversation: an append-only
vector of entries (inputs, context notes, replies, and tool results) and a
ledger of accepted compactions, each replacing the entries before its end for
later requests, and kept with the number of entries it was accepted at.
Indices never change. UUIDs, run starts, and unfinished previews belong to
shared timeline run records. A run's first input entry is its prompt, possibly
after a context note. Each resident timeline entry holds its shared record and
local presentation in separate fields; records and views share a lifetime while
all history stays in memory. Only records cross the sharing boundary. Welcome
snapshots carry the whole transcript as one `Json<TranscriptRecord>` value, its
entries and compactions in the agent's versioned records, plus the run records
and their pending previews. All history and local views remain
in memory; there is no transcript offloading or history paging. The host saves
its own threads at turn boundaries and reads them back when it starts, unless
launched with `--ephemeral`. Normal launches require exclusive ownership of
the saved state; see
[Saved threads](#saved-threads).

The visible timeline is a local, pure projection of these records, pending
submissions, and active run activity. Compactions fold into output steps in the
same traversal as messages; there is no separate boundary-to-step translation.
The projection owns placement and collapse behavior; synthetic rows never enter the agent transcript or the wire
history. Each folded step has one local view at the same index, including
compaction steps. Streaming only rewinds the uncommitted suffix; collapsing or
rendering a subset does not change those indices. In-run dividers use their run
UUID and step index; no transcript positions flow through folded output. A thread
owns at most one active execution, whose activity and event fold end together;
stopped and failed run records still retain their unfinished replay events.

Context compaction is the agent runtime's, run at a safe boundary of the host's
session: before a run's first request, or between two requests of a run once
every tool call has its result. This includes queued and scheduled runs. The host
asks for it when a request's reported usage reaches the configured percentage of
the context window (default: 80%), and before a run starts when the last measured
context plus unmeasured tool-result or streamed-output estimates already does,
with no minimum-used-token floor. The Context settings page also controls the default Ollama window
(131,072 tokens), accepting custom counts down to 1,024 tokens and stepping through
powers of two. Window changes update all local thread catalogs,
including archived threads, and reach collaborators through `ModelCatalogChanged`.
Discovery uses the current setting. The indicator and examples update immediately;
runs already in progress keep their captured window and compaction policy until
the next run. Settings are local to the host for this session.
A single summary request sends the older part of the model context (about 70%,
cut where every tool call has its result) with the runtime's summary instruction,
preserving the ordinary system prompt, tools, and model options; it never
executes tool calls. Empty, failed, or tool-bearing summaries are rejected, and
the run goes on with its old context. Stop cancels the whole run.
A successful summary (including its wrapper) must also be smaller than the
history it replaces in encoded bytes. Only model request history replaces the earlier
transcript prefix with a summary user message, followed by the entries after the
cut; the complete transcript and timeline stay intact. Host events and snapshots mirror accepted summaries and
compacting indicator. Usage includes successful summary requests. The context
indicator is also shown inline as “Compacting context…” while a summary runs.
Every accepted summary appends a compaction entry and leaves a “Context compacted”
divider at that position in the run's output. Collapsed work keeps its dividers
visible above the response. Failed, skipped, and canceled summaries leave no divider;
snapshots restore all accepted dividers from the ordered transcript entries.
The context estimate immediately after compaction counts the encoded bytes of the
summary and the entries kept after it, excluding system and tools, until normal
provider usage arrives.
The request after a summary always carries the environment note in full, so
project access information remains available. A single request whose context
already exceeds the window can still fail. Failed summaries are logged to the
host's standard error without message content.

All five steps of the [implementation order](#implementation-order) are done:
participant identity, the protocol version handshake, shared model selection
from the host's catalog (grouped by provider), shared stopping,
Yrs drafts with prompt blocks, comments as items, per-block attachments,
navigation, and empty-item removal, the draft
synced through the host with host-coordinated submission, presence, and
attachment transfer. Submissions made while a run is going steer it by default,
joining at the agent session's next safe boundary. Explicitly queued
submissions start runs one at a time, oldest first, after normal completion. Stop pauses pending delivery, including
when completion races Stop; an idle composer with an empty draft offers an
explicit Resume messages button. Peer permissions are also implemented: `ReadOnly`, `Write`,
and `Admin`, defaulting to `Write`.
The right-aligned titlebar gateway opens a nonmodal sharing popover with sharing
and link actions; the host also manages live defaults before sharing and per-connection
overrides there. Snapshots and host events sync the unchanged policy.
Permission denials keep the session open. Per-peer draft generations fence rejected optimistic history,
including after regrant; snapshots and resets merge only at a matching epoch.
The host sanitizes read-only presence and clears stale announcements on
downgrade. Stopping sharing cleans peers and unfinished uploads before
clearing membership. Denial feedback is routed to the originating thread.
Committed entries are checked on arrival as the session's own conversation
checked them.
Tool calls outside a short always-allowed list wait for the host or an
`Admin` peer to allow or deny them, and `run_command`, `write_file`, and `request_permissions` for the
host alone; commands within the project auto-approval level run without asking, as does
`write_file` in `Write` and `Write + Network`, a command may
ask for more access than the mode, or for folders outside the project, for
itself alone, and `request_permissions` asks for either for the rest of the
run. See [Tool approval](#tool-approval).
The agent's commands run on the host's computer, contained by its operating
system's sandbox. The project auto-approval level provides the default
access; each command may request narrower access or approval for more.
Collaborators see the folders by name with that level, and
the agent learns their paths from an environment note before its prompt; see
[Project folders](#project-folders).

Not yet implemented:

- explaining non-permission submission refusals to collaborators (
  empty draft, unavailable model, attachments loading, size limit); these
  requests are still dropped silently;
- showing names when hovering a remote caret or an avatar (names show only
  briefly after a caret moves, and on the top bar's avatars);
- fully validating writable peers' presence at the host (e.g. focus,
  selections, and announced file reads); read-only presence is sanitized;
- showing others' selections in agent messages, which needs a gpui-kit
  addition; see [gpui-kit-text-view-highlights.md](gpui-kit-text-view-highlights.md).

Implementation notes on presence:

- Presence is published whenever it changes, checked on every frame and
  whenever a file read starts or ends. It is not coalesced.
- Besides each participant removing an empty item they leave while nobody
  else is in it, the host removes an empty item as soon as presence shows
  everyone has left it, since two people leaving at once would each still see
  the other there. Read-only sanitization and downgrade cleanup do not trigger
  this empty-item removal path.

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
- `Write` and `Admin` participants can create, edit, and remove every item
  and every prompt block's attachments. The creator is attribution, not a
  permission.
- Participants write independent prompt blocks by default and co-edit a block
  by deliberately moving into it.
- Live cursors, selections, focus, and attachment reads are shared as
  ephemeral presence, outside the document.
- `Admin` participants can submit, stop the agent, and change the model;
  submit and stop share one permission. Only the host manages peer access.
  The host coordinates so each submission happens exactly once.
- `ReadOnly` peers still receive the live draft, attachments, presence,
  models, and agent output. Permissions restrict actions, not visibility.

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
- **Submission**: an ordered immutable snapshot of the draft's non-empty items.
  It starts a run immediately, or, while a run is going, is queued for a
  later run.

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

### Peer permissions

The host owns the per-thread policy. The host is always `Admin`, independent
of the peer default, and only the host can manage access; an `Admin` peer
cannot delegate it.

| Peer action                                           | `ReadOnly` | `Write` | `Admin` |
| ----------------------------------------------------- | ---------- | ------- | ------- |
| Receive live draft, attachments, models, agent output | Yes        | Yes     | Yes     |
| Create, edit, remove any draft item or attachment     | No         | Yes     | Yes     |
| Submit the whole draft or stop generation             | No         | No      | Yes     |
| Allow or deny the agent's tool calls                  | No         | No      | Yes     |
| Change the selected model                             | No         | No      | Yes     |
| Manage default access or individual overrides         | No         | No      | No      |

The default starts as `Write`. Overrides are sparse and keyed by the
host-assigned participant UUID for this connection. No override means
inherit the **current** default, so changing it immediately affects existing
inheriting peers as well as future joins. An explicit override remains
explicit even when it equals the default. Removing it resumes inheritance.
Leaving removes the override; reconnecting gets a new UUID and inherits the
then-current default. Profiles and creators do not grant authority.

`peer_access.rs` renders a nonmodal `gpui_component::Popover`, approximately
300 px wide, below the right-aligned titlebar gateway beside the participant avatars.
The 28 px button sits in the existing 40 px header; there is no extra access bar.
The host menu has the same sharing header, copy-link control, default access track,
and sharing action before and after sharing. Copy link is disabled until shared;
Share thread becomes Stop sharing, and starting or retrying sharing keeps the
menu open so the link is immediately accessible. Defaults can be configured before sharing,
materializing a local thread if necessary while preserving its draft.
Connected peers add override rows. Share, copy link, and stop sharing use this
same menu rather than separate titlebar buttons. Collaborators see **Your access** with their mode icon and Disconnect,
never the host's permission controls.

For the host, **Default** and connected peer names label fixed-width,
three-mode icon tracks: Eye (`ReadOnly`), Pencil (`Write`), and Shield (`Admin`).
Hover tooltips explain each mode, and the selected pill shows the effective
mode, including for inheriting peers. Each peer has a separate RotateCcw reset
outside the track. Its tooltip describes inheritance of the current default;
it is disabled when already inherited. Resetting removes the override, while
choosing a mode explicitly creates one even if it equals the default. No
long, changing inheritance label widens the controls.

Changes apply immediately and keep the popover open; policy changes and peer
joins/leaves redraw it live. Escape or an outside click dismisses it without a
modal backdrop or moving the page. The popover's element identity is scoped
to the active draft, so switching threads closes any stale menu while materializing
a new thread for sharing preserves the open menu. Read-only
editors stay live and selectable, editing and attachment actions are gated,
Send/Stop are hidden without `Admin`, and the model picker is disabled without
`Admin`.

## The draft

The editing behavior below requires `Write` or `Admin`. Read-only peers can
view the same live items without mutating them; host cleanup still removes
unattended empty items.

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
the draft is empty, all editing participants' avatars are layered on that
single row.

Comment cards show their creator's avatar inside the card, as today, with
editors layered beside it. Ownership of every comment must be clear.

### Creating prompt blocks

Independent prompts are the default.

1. When the draft is empty, every editing participant's caret sits at the
   same draft position. Overlapping carets use participant colors and never create
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

Attachments belong to a prompt block. `Write` and `Admin` participants can
add or remove any attachment, regardless of creator. File bytes are sent
over the protocol, never stored in the document.

**Choosing the target block**:

- Pasting or dropping onto a block attaches to that block.
- Pasting or dropping elsewhere attaches to the focused prompt block.
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

Only the host needs the bytes to send. An `Admin` participant can submit
once the host has stored every attachment, even if others are still
downloading.

**Failure and removal**:

- A failed local read removes the placeholder. The error is shown only to
  the adder, as today.
- If the adder disconnects before the upload finishes, the host removes the
  record and discards the partial bytes.
- Removing an attachment while it uploads cancels the upload. The host
  discards bytes for attachments no longer referenced by the draft or the
  timeline.
- Losing draft-write permission cancels pending local reads and removes
  their placeholders. Disk I/O may finish, but late callbacks cannot attach
  the result, even if access is restored meanwhile.
- An upload whose record the host accepted while the uploader could write
  may finish after revocation. This grant is limited to that connected
  uploader and the record's ID, name, kind, and size; removal or disconnect
  ends it. Pre-record buffers are discarded on revocation and new uploads
  are denied. Cancelling one's accepted transfer remains allowed, without
  granting permission to remove its draft record.
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
- Presence is never stored in the document. The host checks membership and
  replaces read-only peers' presence with an empty state before storing and
  rebroadcasting it. Readers cannot announce work that blocks submission or
  trigger empty-item deletion by reporting that they left an item.
- On downgrade, the host clears and rebroadcasts stale focus, selection, and
  pending reads without waiting for peer cooperation. Mirrors also clear
  them when applying the policy. This cleanup does not remove empty items.
- Writable peers' focus, selections, and read announcements still need full
  host validation; sanitizing read-only presence is not that validation.

## Thread controls

### Submission

Pressing Ctrl-Enter (Cmd-Enter on macOS) in any composer editor or at the
draft position, or clicking Send, submits the whole draft for everyone,
provided the submitter is `Admin`. `Write` permits editing, not consuming
anyone's draft. The host checks the requester's current permission, not the
local viewer's mode.

Send is available only to the host and `Admin` peers, and requires:

- the draft contains at least one non-empty item, or the idle thread has
  accepted pending messages to resume;
- no participant has a pending attachment read; and
- the host has stored every attachment in non-empty blocks.

The host accepts a submission as follows:

1. The submitter sends `Submit` (steering) or `Queue` with the submission sequence it has seen. It
   sends all its pending draft updates first, on the same ordered stream, so
   its own accepted edits are included. The host checks `ControlGeneration`
   before considering the submission.
2. The host ignores a stale sequence: someone else's submission was already
   accepted and everyone sees it.
3. From its replica, the host snapshots every non-empty comment and every
   non-empty prompt block (with attachments), in draft order. This includes
   blocks others are still typing in.
4. In one transaction, the host removes exactly the snapshotted items from
   the draft. Empty items stay, for example a comment someone has not started
   typing yet.
5. The host broadcasts that draft update and advances the explicit submission
   sequence. While idle it publishes the user message and starts the agent.
   While generating it accepts the submission as a pending one, without adding
   it to the timeline or transcript yet.
6. Edits that arrive for removed items are discarded. Participants whose
   focused item was submitted move to the draft position.

A permission rejection is reported only to the submitter as
`PermissionDenied`. Feedback uses the originating local `thread_id` and its
draft, not the currently active thread, including after switching threads.
Other refusals (empty, unavailable model, attachments not stored,
size limit) still lack collaborator feedback, as noted in Status.

**Pending delivery**: each accepted draft is immutable and receives an ID,
and records who submitted it, the source of its input. The pending list is
host-owned, mirrored in snapshots, and individually cancellable, steering and
queued alike. Steering uses the agent session's native `Steer` delivery: it
joins after the current reply and its tool results, before the next request.
It does not interrupt sampling or bypass a waiting approval. The host hands a
steer to the run as it is accepted, and every pending steer to a new run
right after its prompt, never to a stopping run. Steers left from a stopped
run therefore join the next run (whether started by a new draft or by Resume)
at its first safe point, right after its prompt and before its first request. The canonical group remains
pending until its input commits; then `PendingDelivered` publishes it before
the committed entry. The input ID is the user group's ID: an input in a run's
output whose ID names a user group of the timeline is that group's steer, the
one rule both live folding and replay use, so the group shows inline in the
run's progress without another saved index. Steering comments use
submission-namespaced aliases, `comment_<first 8 hex digits of the group
ID>_<n>`, preserving earlier comment replies without renumbering after
cancellation; they join the run's comments only when the
steer commits, so a withdrawn steer names nothing the model can answer.

Queued drafts remain separate and start one run each in FIFO order, using the
selected model and project settings when each run starts. Accepted user queues
take precedence over the scheduler trying to reuse that thread. Nothing reaches
a running agent from the ordinary queue: each queued draft is the prompt of a
run of its own.

Cancellation (`CancelPending`) requires current `ControlGeneration` permission
and removes one pending draft by its ID. A queued draft, or a steer the run
doesn't have, is removed at once. A steer the run has is withdrawn through the
session, with the canceller as the request's source, until it joins the run;
only the session's `Removed` receipt takes it off the mirrored list. The
session commits a safe point's steers as it takes them, so a cancel that loses
that race is answered by the commit: the steer shows inline in the run as
sent. A late cancel changes nothing.
`PendingDelivered` removes a queued draft as its run starts and appends its user group to
the timeline, followed by the run's `AgentStarted`. Queued comments are named
in that run's prompt like any submission's. Queued attachment bytes stay stored
and are included in late-join transfers. The 32 MB message attachment limit
applies to each submission, as each becomes a message of its own.

Normally completed runs automatically start the next pending run, with
undelivered steering before the ordinary FIFO. A steer that finds no run to
join (one racing completion, or a run stopping) is discarded by the session's
fallback but stays in Cowork's pending list, so it starts a run of its own
rather than an untracked one. Stopping keeps steers. Stopping or
failure keeps queued submissions visible and cancellable. Stop also wins over a
completion already on its way to the UI, preventing automatic queue advancement.
An idle composer with an empty draft shows a labelled **Resume messages** button
with a play icon. Clicking it (or pressing the submit shortcut) resumes the
pending work (steering first, then FIFO), without creating a new submission. Sending a new draft
while idle starts its run at once; the queue continues after it completes.
Archived threads do not start queued runs. Queues are session state, and are
not saved across app restarts, though the host's threads are (see
[Saved threads](#saved-threads)).

The composer shows the red Stop control while generating with an empty draft.
With any non-empty block, comment, or attachment it shows the blue Steer
control. Cmd/Ctrl+Enter steers; Cmd/Ctrl+Shift+Enter queues. Clicking steers,
and Shift-clicking queues. The tooltip names both choices and the Stop shortcut.
Idle Send uses the same blue enabled accent. Pending blocks have muted centered
Steering or Queued labels in dashed
dividers, cancellation at the right, and a solid Draft divider before the
editable draft.

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
- The host keeps a **transcript** of ordinary agent runs, including prompts,
  reasoning, tool calls, and tool results. Between compactions each request
  extends the previous one exactly, keeping the provider's prompt cache
  valid. After compaction, requests use the latest summary followed by the
  entries after the point it covers; the session's context notes and
  steering never change an earlier request. All accepted summaries are
  retained, and no entry is removed. The full transcript remains available
  for display and sharing. A run that is stopped or fails keeps its prompt and
  every turn it completed.
- Every participant mirrors the transcript and the prompt names. The
  host runs one agent session per thread (the `agent` crate's
  `AgentSession`) and forwards what it reports for a run, as it happens, in
  `AgentEvent`s carrying a `RunEvent`: previews of the reply's text and
  reasoning, each entry the session commits with its index (the run's
  input and context notes, each complete reply, each tool's result as soon
  as the tool returns), the tokens each request used, and compactions
  starting, being accepted, or failing. Everyone, the host included, folds
  them the same way into a replica of the session's conversation, which
  refuses an entry the session's conversation could not have committed at
  that index, such as a result no call waits for. A reply joins the
  transcript whole, as the provider completed it; previews only show it
  while it streams, and tool calls show once it is complete.
- What an agent message shows (its thinking, text, tool calls with their
  results, and replies to comments, in the order the agent produced them)
  is never sent. It is a function of transcript entries and compactions:
  the entries after its prompt, up to the next run's start, with the
  compactions accepted among them, followed by the reply being previewed,
  as far as it has come. That reply is the fold of the run's **pending
  events**: the previews since its last reply joined the transcript. A run
  that is stopped or fails mid-reply keeps its pending events, since that
  output never joins the transcript. Comment replies
  come from `respond_to_comment` calls, with ids derived from the message
  and call, so every participant names them alike. Only the **response**,
  the text after the agent's last tool call, can be
  commented on; comment
  ranges are offsets into it. Everything before it is the agent's work,
  shown under a "Working for" line while the run goes and collapsed under
  "Worked for" once it ends, with whether it was stopped or failed. That
  line and whether it is open are local. A failure message is kept apart
  from the agent's output, and shown only when there is none.
- The transcript's entries and compactions are sent in the agent's
  versioned records (message envelopes), together as one JSON value;
  individual committed entries and compactions also travel as JSON, which
  postcard cannot represent directly. `AgentStarted` creates the run's
  message; its prompt joins the transcript as the run's first input entry,
  including its files' content, images as base64, so attachment bytes also
  travel inline besides as `AttachmentData`. Summary requests and their
  replies never become entries; an accepted summary joins the compaction
  ledger, at a point where every call has its result. The session compacts
  when a request reports the context past the user's threshold, or before
  a run's first request when the run starts on a full context. A `Welcome`
  carries the whole `Json<TranscriptRecord>` and, for each agent
  message, where its run starts, its pending events, and whether its run
  has ended. The joiner folds each run's pending events, which rebuilds the
  message it was folding exactly, and so shows what everyone else does. For
  the running message, it keeps that fold, so someone joining mid-turn
  folds the rest of it. Before that, the
  joiner rebuilds the transcript, checking every entry and compaction as
  they were checked when first applied, and checks that the snapshot fits
  it: run starts increase within it, all timeline UUIDs are unique, only the
  last run can still be generating (and only that run may still await its
  prompt, with no output), and the running run's previews, waiting call,
  and compaction are ones its conversation allows. A frame larger than 1 GiB cannot be sent; the
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

Submitting while the agent is generating steers or queues as described above. `Write` and `Admin`
participants can keep editing the draft during generation.

### Stopping

The host and `Admin` peers see the Stop button while the agent is generating.
Submit and stop use the same `ControlGeneration` permission; there is no
separate stop setting. `Stop` names the running agent message. The host
checks current permission, cancels that run if it is still active, and
ignores an authorized request otherwise. The run then ends with `AgentEnded`
marked as stopped, so everyone can tell it apart from one that completed.

### Tool approval

See [Permissions](permissions.md) for command scope, auto-approval, file
tools, and run grants. Reads span the host filesystem on every platform,
except denied credentials; a submitting collaborator can therefore have
the agent read other host files into the transcript.

The host emits `ToolApprovalRequested` for a waiting call. Only the host
may decide commands, file writes, and permission grants; Admin peers may
decide other tools. Local clicks and `DecideToolCall` use the same checked
path. The first decision wins and emits `ToolApprovalResolved`; stale
requests change nothing. A denial commits a refused tool result and the
run continues. Ending a run settles outstanding calls as not run. Snapshots include
the waiting call; participants reject requests for calls the reply did not
make or that already returned.

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
- The host and `Admin` peers can pick a model the thread's catalog offers.
  The client sends `SelectModel`. The host checks `ChangeModel`, then applies
  it in arrival order if its catalog offers the model, ignores it otherwise,
  and broadcasts `ModelSelected`. A peer
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

### Project folders

- A thread's project is a list of folders on the host's machine, in the
  order they were added. Only the host adds or removes them, from the
  bottom bar; a folder added twice is kept once. Folders added before the
  thread exists move into it with the draft.
- Each folder is named by its last path component, with path and list
  separators (`/ \ : ; ,`) and control characters replaced by `_`, and
  `root` for a filesystem root. A name already in the project gets a
  `-2`, `-3`, ... suffix (`src`, `src-2`), and names never change once
  given, so removing a folder leaves the others where they are.
- The project UI shows collaborators only each folder's name. The names
  arrive in the `Welcome` snapshot and are replaced as a whole by
  `ProjectFoldersChanged` after every change, so a removal needs no message
  of its own. The paths do reach collaborators in the transcript: the
  agent's environment note and its commands name them, along with the
  host's user name where a path holds it.
- Collaborators see the names but cannot change them; there is no
  collaborator message for it.
- The project has an auto-approval level, `Read`, `Write`, or `Write + Network`, picked from a menu
  in the bottom bar between the context indicator and the model picker.
  `Write + Network` is `Write` with network access. Every thread starts in `Read`.
  Only the host can change it; collaborators see it in the `Welcome`
  snapshot and follow `ProjectModeChanged`. A mode picked before the thread
  exists moves into it with the folders. Hovering `Write` or `Write + Network` in
  the menu warns that allowed commands change the folders at once (and, for
  `Write + Network`, can download code and send out what they read), and that a
  malicious agent may be able to escape the sandbox. The control is labelled
  `Auto-approve`, and its level is the default access
  commands run with unless they ask for their own; see
  [Tool approval](#tool-approval).
- Commands use the host's shell and environment in an OS sandbox. They
  start in the first project folder or scratch, with the real HOME and
  scratch as TMPDIR. File tools are app-managed; `paths.rs` maps their
  paths. Read scope and write limits are described in
  [Permissions](permissions.md); `sandbox/src/settings.rs` is the low-level
  policy reference.
- Each command is contained by the project as it is when the command
  starts, with the access it was granted, so a change applies from the next
  command, with nothing to
  restart or confirm; commands already running keep the access they started
  with.
- The agent learns the folders' paths, the mode, where commands start, and
  the scratch folder from an `<environment>` note the host puts first in a
  run's prompt whenever it differs from the latest one in the transcript.
  The note, not the tool definitions, carries what changes, so the tools
  and system prompt stay a stable prompt prefix. The system prompt
  names the host's platform, which decides the shell.

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
  receives a new `Welcome` with its per-peer `draft_generation`. A matching
  generation merges draft state, preserving unsent edits; a newer one
  replaces the replica with a fresh Yrs client ID.
- **Denied optimistic edits**: rejecting a peer's current epoch for revoked
  edit permission advances only that peer's generation. Queued updates from
  that rejected epoch stay denied after regrant. `DraftReset` installs the
  assigned generation and authoritative state; repeated resets at that same
  epoch merge, preserving fresh authorized edits instead of erasing them.
- **Collaborator disconnects**: its presence and permission override disappear.
  The host applies the empty-item and incomplete-upload cleanup described above.
- **Host stops sharing**: while membership and the broadcast channel still
  exist, the host runs departure cleanup for every peer, removing unfinished
  upload records and buffers, presence, overrides, and unattended empty items.
  It then clears membership and draft generations and ends hosting. Repeated
  serving-task departure cleanup is harmless; accepted content remains local.
- **Host archives the thread**: the host stops its agent run and stops
  sharing as above, so collaborators' copies are removed as when the host
  disconnects. A thread archived while its sharing is still starting closes
  the endpoint once it is bound instead of hosting. A restored thread is no
  longer shared. Collaborators can't archive their copies; they disconnect
  instead.
- **Host disconnects**: the session ends and collaborators' copies of the
  thread are removed, as today. Host migration is out of scope.
- **Invalid host data**: invalid draft snapshots, inconsistent transcript/run
  snapshots, or invalid agent records in transcript entries or run events are
  protocol errors. The collaborator removes its mirrored thread; the
  application stays open. `Welcome` validates draft and transcript/run state
  before installation.

Drafts are not persisted. A draft lives as long as its thread exists in the
host's app; a saved thread comes back with an empty one.

### Saved threads

The host saves its own threads, archived ones included, each in a folder of
its own, `~/.cowork/threads/<thread id>/`: `thread.json` (title, model,
project folders and mode, the profiles and prompt names of everyone who took
part, token usage, and the timeline's messages and run records),
`messages.jsonl` (the transcript, one of the agent's versioned entry or
compaction records per line, only appended to), `attachments/` (the bytes of
the files its messages attach), and `scratch/` (its commands' scratch
folder). `crates/cowork/src/thread_files.rs` has the format.

- Only the host saves a thread. Collaborators save nothing of a thread they
  joined.
- A thread is saved at turn boundaries: when a run ends, when it is archived
  or restored, and when the app quits. Never during a run, so every tool call
  in a saved transcript has its result; a crash during a run loses what that
  run did since the last save.
- The draft, pending submissions, sharing, and per-peer permissions are not
  saved. A thread comes back unshared, in the sidebar or archive where it
  was, with an empty draft and the profiles kept so earlier messages still
  name their authors.
- Loading checks a saved thread as a collaborator checks a host's `Welcome`
  snapshot: the files are converted to one in memory. The files have their
  own format version, independent of `PROTOCOL_VERSION`; a file of another
  version is neither read nor overwritten.
- One app instance keeps the threads, holding `~/.cowork/threads/.lock`.
  Another normal launch exits with an already-running error before loading
  saved state. There is no automatic activation of the first instance.
  `--ephemeral` opens an independent instance without reading or writing
  `~/.cowork` or acquiring its lock: threads, statistics, and schedules are
  not persisted. Scratch files use a unique temporary folder removed on
  orderly shutdown (a crash may leave it behind). Ephemeral mode does not
  restrict otherwise permitted agent changes to project files.
- Deleting a thread from the archive removes its folder.

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
├── start                           inclusive UTF-8 byte offset
├── end                             exclusive UTF-8 byte offset
└── quote                           display text captured at creation
```

The stored comment target's keys are `message_id`, `start`, `end`, and
`quote`; the Rust `CommentTarget` API exposes `start..end` as `range`.
Attachment metadata uses `id`, `name`, `kind`, `size`, and `creator`.
Both are atomic Yrs `Any` values, not nested collaborative maps.

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
  host checks this through the applies-first structural validation below.
  Future authentication belongs at the host, not in the document.
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

| State                               | Where it lives                                                           |
| ----------------------------------- | ------------------------------------------------------------------------ |
| Participants and profiles           | Host session, synced by protocol; colors from UUIDs                      |
| Prompt names and agent transcript   | Thread state at the host, mirrored by protocol                           |
| Presence                            | Host-relayed protocol messages                                           |
| Selected model                      | Thread state at the host, synced by protocol                             |
| Project folders                     | Paths at the host; names only mirrored by snapshots and events           |
| Project mode                        | Thread state at the host, mirrored by snapshots and events               |
| Peer default and sparse overrides   | Host-owned thread state, mirrored by snapshots and events                |
| Per-peer draft generation           | Host session per connection; receiving peer's `Welcome` and `DraftReset` |
| Submission sequence                 | Thread state at the host                                                 |
| Attachment bytes and stored status  | Host byte store keyed by attachment ID, relayed to peers                 |
| Published timeline, agent runs      | Thread state, synced by the existing host events                         |
| Folding, scroll, local errors, etc. | Local UI state                                                           |

## Protocol

All traffic goes through the host. Collaborators never talk to each other.
`protocol.rs` defines the wire messages; versions must match exactly.
`Join` keeps its variant index and version as its only field;
`Rejected` keeps its variant index and string payload so a version mismatch
can still be reported. Runtime permission denials use the separate
`PermissionDenied`, never this stable handshake rejection.

**Collaborator to host**

| Message               | Purpose                                                                                                |
| --------------------- | ------------------------------------------------------------------------------------------------------ |
| `Join`                | First message; carries the protocol version                                                            |
| `Profile`             | Second message, and again whenever the profile changes                                                 |
| `DraftUpdate`         | `{ generation, update }`: encoded Yrs update; requires the peer's current epoch and `Write` or `Admin` |
| `Presence`            | Replaces this participant's presence; host sanitizes readers to empty                                  |
| `AttachmentData`      | Upload chunks; requires draft-write access or an already accepted record                               |
| `AttachmentCancelled` | Cancels this peer's upload, including after revocation                                                 |
| `Submit`              | Requests a submission at the given sequence, steering while a run is going; requires `Admin`           |
| `Queue`               | Requests a submission at the given sequence for a later run; same permission as `Submit`               |
| `CancelPending`       | Cancels the named pending submission; same permission as `Submit`                                      |
| `Stop`                | Stops the named agent run; same permission as `Submit`                                                 |
| `SelectModel`         | Selects a model by provider and model ID; requires `Admin`                                             |
| `DecideToolCall`      | Allows or denies the named run's waiting tool call; requires `Admin`; ignored once decided             |

**Host to collaborators**

| Message                                      | Purpose                                                                                                                                                                                                                                                                                                                                                      |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Welcome`                                    | Participant UUID, timeline and transcript snapshot (models, run state, compacted context and status, prompt names, project folder names and mode), draft and receiving peer's `draft_generation`, participants, profiles, presence, stored attachment IDs, and peer permissions; explicit submission sequence and pending groups are included; peer-specific |
| `Rejected`                                   | Join refused (e.g. protocol version); stable handshake encoding, then disconnect; peer-specific                                                                                                                                                                                                                                                              |
| `PermissionDenied`                           | Runtime denial with participant UUID, operation, and reason (`NotParticipant`, `InsufficientMode`, `HostOnly`, or `StaleDraftGeneration`); peer-specific, connection stays open                                                                                                                                                                              |
| `DraftReset`                                 | `{ generation, state }`: peer-specific authoritative draft; matching epoch merges, newer epoch replaces, older epoch is ignored                                                                                                                                                                                                                              |
| `DefaultPeerModeChanged`                     | Host-owned live default changed; inheriting peers immediately follow it                                                                                                                                                                                                                                                                                      |
| `PeerModeOverrideChanged`                    | Host-owned per-connection override changed; `None` resumes inheritance                                                                                                                                                                                                                                                                                       |
| `DraftUpdate`                                | Untagged Yrs update from another participant or the host; per-peer generations tag only collaborator requests                                                                                                                                                                                                                                                |
| `ParticipantJoined` / `ParticipantLeft`      | Membership changes; joining carries the participant's profile                                                                                                                                                                                                                                                                                                |
| `ProfileChanged`                             | A participant's new profile                                                                                                                                                                                                                                                                                                                                  |
| `Presence`                                   | A participant's latest presence                                                                                                                                                                                                                                                                                                                              |
| `AttachmentData`                             | Relayed attachment bytes                                                                                                                                                                                                                                                                                                                                     |
| `AttachmentStored`                           | The host holds every byte of an attachment                                                                                                                                                                                                                                                                                                                   |
| `ModelSelected`                              | The thread's model changed                                                                                                                                                                                                                                                                                                                                   |
| `ModelCatalogChanged`                        | Replaces the thread's model catalog (including with an empty one)                                                                                                                                                                                                                                                                                            |
| `ProjectFoldersChanged`                      | Replaces the names of the project's folders, in order; paths are never sent                                                                                                                                                                                                                                                                                  |
| `ProjectModeChanged`                         | The project's mode is now `Read`, `Write`, or `Write + Network`                                                                                                                                                                                                                                                                                              |
| `PendingSubmissions`                         | Full accepted pending groups and current submission sequence; IDs fence cancellation                                                                                                                                                                                                                                                                         |
| `PendingDelivered`                           | Removes a pending ID and publishes its user group; followed by its steering input commit or a new run's `AgentStarted` |
| `UserMessage`                                | A delivered immediate submission with creators and attachment references instead of bytes; advances the submission sequence                                                                                                                                                                                                                                  |
| `ThreadTitled`, `AgentStarted`, `AgentEnded` | The title; a run starting, whose prompt follows as its first committed input; and ending, with its duration and whether it completed, was stopped, or failed (with a message)                                                                                                                                                                                                                  |
| `AgentEvent`                                 | A `RunEvent` of the host's session during a run (a preview, a committed entry, usage, or a compaction starting, accepted, or failing), which everyone folds into the agent message and transcript |
| `PromptNamed`                                | A participant's prompt name, fixed on their first submitted item                                                                                                                                                                                                                                                                                             |
| `ToolApprovalRequested`                      | A run waits for the named call of its last reply to be allowed or denied                                                                                                                                                                                                                                                                                     |
| `ToolApprovalResolved`                       | The waiting call was decided; its result follows as an `AgentEvent`                                                                                                                                                                                                                                                                                          |

The host is itself a participant. Its local edits, submissions, stops, and
model changes go through the same authorization paths as a collaborator's,
with the host always authorized. Permission defaults and overrides are
host-authoritative state in `ThreadSnapshot`; the host applies and broadcasts
their change events, and `ParticipantLeft` removes the departing override.
There is no collaborator command to manage access.

Attachment bytes travel in bounded chunks, or on their own stream, so a
large file never delays draft updates or presence.

## Validation

Authorization and structural validation are separate. For a collaborator's
`DraftUpdate { generation, update }`, the host checks membership, the peer's
current generation, and `EditDraft` permission **before decoding, applying,
or broadcasting** bytes. A denial sends `PermissionDenied` followed by
`DraftReset { generation, state }` only to that peer, without changing the
host draft or disconnecting it.

Each connection starts at generation 0. Rejecting its **current** epoch for
revoked edit permission advances only that peer's epoch once. A mismatched
epoch is denied as `StaleDraftGeneration`, even after regrant, without
advancing again. Constructing or repeating a reset never advances the epoch.
The generation is a replica fence, not a thread-wide revision, submission
sequence, or counter of permission changes.

`Welcome` and `DraftReset` use the same installation rules: older draft
generations are ignored; matching generations merge, retaining fresh unsent
edits; only a newer generation replaces the document with a fresh Yrs client
ID. Replacement invalidates item and draft-position editor handles and sync
baselines, retaining comment folding and pruning obsolete upload bookkeeping.
Queued events from old editors therefore cannot replay rejected input after
regrant.

For an **authorized** update, the existing structural path still applies and
broadcasts it first, then checks:

- the document has only the `order` and `items` roots;
- every `order` entry is unique and has an item, and every item is in
  `order`;
- every item has a supported kind and exactly the fields listed for it;
- `creator` and `target` never change, and a new item's creator is the
  sending participant;
- bodies contain only version-1 content;
- attachment records are well-formed, unique by ID, and within size limits.

Any structural violation is a protocol error, and the host disconnects the
peer. The update has already been applied and broadcast, with no rollback.
Permission enforcement prevents unauthorized draft writes; it does **not**
make malicious writes by authorized peers safe. Structural validation is
still a defensive check, not a security boundary. Validating against a
scratch copy before applying and broadcasting would be needed to isolate
such writes.

Protocol versions must match exactly and drafts are not persisted, so the
document needs no schema version or compatibility with older clients.

## Implementation notes

- **One code path.** Local and shared threads both back their draft with
  Yrs. A local thread simply has no peers.
- **Threading.** Each client's replica lives in its GPUI `Thread` entity.
  Transactions are synchronous and never cross an `await`. The Tokio side
  moves only encoded bytes. The host's replica is authoritative.
- **Authorization.** `thread/permissions.rs` checks current actor membership
  and policy on each operation. Sealed operation types (`EditDraft`,
  `ControlGeneration`, `ChangeModel`, `ManageAccess`) select closure-scoped
  `Authorized` guards. Callers cannot construct or retain guards, and guards
  expose neither raw `Thread` nor Yrs `Doc` access.
- **Thread subtree.** `ThreadDraft`, sharing, and submission are nested as
  `thread::draft`, `thread::sharing`, and `thread::submission`; generation is
  under `thread::submission::generation`. Their files remain at their original
  paths. Participant identity/membership, model catalog, and ownership are
  private, as are general host-event `apply`/`try_apply`, `emit`, and `publish`.
  Outside callers use read accessors, authorized operations, and narrow
  maintenance APIs for hosting, catalogs, profiles, and locally stored files,
  not arbitrary event injection. Test-only helpers provide trusted setup.
- **Editor cache.** `DraftEditorState` is separate from the canonical draft.
  Rendering can update editor caches without edit permission. Policy changes
  reconcile read-only inputs, typing presence, and pending reads immediately;
  rendering reconciles text and IME with a window. Mutations and asynchronous
  attachment callbacks recheck the destination draft's access, not whichever
  thread is currently active.
- **Optimistic local edits.** Authorized local edits never wait for the
  network. Queued outgoing updates may be merged while the channel is busy;
  if the host rejects their epoch, a newer-generation `DraftReset` discards
  the optimistic history. Updates from that epoch stay fenced after regrant;
  matching-generation resets merge fresh edits.
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

- **Authentication**: the host maps authenticated identities to participant
  UUIDs and enforces `creator`.
- **Contributor history**: the host can derive which items each participant's
  updates touched from Yrs events, without changing the document.
- **Mentions and inline formatting**: see
  [Document layout](#document-layout).
- **Reordering blocks**: possible because order is separate from content. No
  UI is planned.

Out of scope: host migration, persisting drafts, keeping identity across
reconnects, collaborative undo semantics beyond undoing local changes, and an
invitation format beyond today's endpoint ID.

## Acceptance scenarios

Unless stated otherwise, peers inherit the initial `Admin` default.

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

**Live peer access**

1. The host changes the default from `Admin` to `Write`. Inheriting peers
   keep editing everyone's items and attachments but cannot submit, stop, or
   change the model. An explicit `Admin` override stays unchanged.
2. Downgrading a peer to `ReadOnly` cancels pending reads, not uploads whose
   records were already accepted. The host clears and rebroadcasts stale
   editing presence. Draft, model, and agent updates stay live.
3. An edit racing revocation is denied before host apply/broadcast; only its
   sender's epoch advances, and it receives `PermissionDenied` and `DraftReset`.
   Old-epoch updates remain denied after restoring `Write`, without advancing
   again. Fresh-epoch edits converge; repeated same-epoch resets preserve them.
4. A same-epoch `Welcome` preserves unsent edits; a newer-epoch snapshot or
   reset invalidates old editor caches and rejected history.
5. Leaving removes the override; a rejoin inherits the current default.
   Stopping sharing cleans every peer's unfinished uploads before membership
   is cleared, leaving the host's draft usable locally.

**Late join**

1. Dave joins mid-generation. He receives the timeline, the draft with
   everyone's items, presence, the model, and then attachment bytes.
2. His replica converges with the host, and his first edit is visible to
   everyone.
