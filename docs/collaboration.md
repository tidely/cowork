# Collaborative drafts

How Cowork participants prepare, edit, and submit agent requests together:
the user-facing behavior, the shared Yrs document, and the protocol between
host and collaborators. The wire messages themselves are documented in
`crates/cowork/src/protocol.rs`, and the draft document's API and structural
checks in `crates/draft/src/lib.rs` and `crates/draft/src/validate.rs`.

## Terminology

- **Participant**: someone connected to a shared thread, including the host. The
  **host** runs the agent and holds the authoritative copy of all shared state;
  collaborators mirror it. Local threads use the same draft without peers.
- **Item**: one prompt block (an independently editable prompt with its own
  attachments) or one comment (an editable note targeting an excerpt of an
  assistant message) in the draft. The **creator** is attribution, not a
  permission.
- **Draft position**: the empty spot below the last prompt block. Typing there
  creates a new prompt block.
- **Submission**: an ordered immutable snapshot of the draft's non-empty items,
  which starts a run, steers the running one, or is queued for a later run.

Live cursors, selections, focus, and attachment reads are ephemeral presence,
outside the document. Permissions restrict actions, not visibility: `ReadOnly`
peers still receive the live draft, attachments, presence, models, and output.

## Participants and identity

- The host assigns each collaborator a random participant UUID when it joins
  and returns it in `Welcome`. The host generates its own UUID once per app
  launch and uses it for its local threads too.
- Reconnecting is a fresh join with a new participant UUID. Items created
  earlier keep their original creator.
- Each participant has a **profile**: an optional display name, an optional
  profile picture, and an **appearance** UUID. Without a name or picture, a
  participant shows as two filler words (such as "Amber Otter") and initials,
  derived deterministically from the appearance, as is their color. Each app
  sends its own per-launch UUID as the appearance, so a participant looks the
  same in every thread even though the host assigns a new participant UUID on
  each join. Without an appearance, the participant UUID is used. Profiles are
  cosmetic, never authoritative, and may collide; they are not disambiguated.
- A collaborator sends its profile right after `Join` and again whenever it
  changes. The host validates it (`profile.rs`) and disconnects peers that send
  an invalid one. Profiles are kept after a participant leaves, so their
  messages still name them.
- Identity is not authenticated. A future account or key scheme would map an
  authenticated identity to a participant UUID at the host; nothing in the
  document depends on how identity is established.

### Peer permissions

The host owns the per-thread policy. The host is always `Admin`, independent
of the peer default, and only the host can manage access; an `Admin` peer
cannot delegate it.

| Peer action                                              | `ReadOnly` | `Write` | `Admin` |
| -------------------------------------------------------- | ---------- | ------- | ------- |
| Receive live draft, attachments, models, agent output    | Yes        | Yes     | Yes     |
| Create, edit, remove any draft item or attachment        | No         | Yes     | Yes     |
| Submit, queue, cancel pending submissions, or stop       | No         | No      | Yes     |
| Allow or deny tool calls (not commands, writes, grants)  | No         | No      | Yes     |
| Change the selected model                                | No         | No      | Yes     |
| Manage default access or individual overrides            | No         | No      | No      |

The default starts as `Write`. Overrides are sparse and keyed by the
host-assigned participant UUID for this connection. No override means inherit
the **current** default, so changing it immediately affects existing
inheriting peers as well as future joins. An explicit override remains
explicit even when it equals the default; removing it resumes inheritance.
Leaving removes the override, and reconnecting gets a new UUID that inherits
the then-current default.

Read-only editors stay live and selectable, but editing and attachment actions
are gated, Send and Stop are hidden without `Admin`, and the model picker is
disabled without `Admin`.

#### Sharing menu

`peer_access.rs` renders a nonmodal popover below the titlebar gateway. The host
sees a **Share chat** switch, **Copy link**, the default mode (which can be set
before sharing, materializing a local thread while preserving its draft), and a
row per connected peer whose menu sets their mode; **Use default** removes the
override, while choosing a mode creates one even if it equals the default. Modes
read **Can view** (`ReadOnly`), **Can edit** (`Write`), and **Can run** (`Admin`).
Collaborators see whose chat it is, **Leave**, and their own mode, never the
host's controls. `cargo run -p cowork --example share_preview` shows every state.

## The draft

The editing behavior below requires `Write` or `Admin`. Read-only peers see the
same live items without mutating them; host cleanup still removes unattended
empty items.

### Composer layout

Below the timeline every participant sees, in this order: all draft comments
under a collapsible "N comments" toggle (local state); the prompt blocks, each
with its attachment row above its text; and the draft position, rendered only
when there are no blocks or someone is at it. The composer groups by kind,
comments first, each group in the shared order, even though that order spans
all items.

### Avatars

Each prompt block has an avatar gutter. The creator's avatar is primary, even
when the creator is not editing; other current editors are layered beside it in
join order, which does not reshuffle on presence updates, up to the creator plus
two editors and then "+N". The draft position row shows the avatars of
participants at it. Comment cards show their creator's avatar inside the card.

Resting the pointer on an avatar, in the timeline, the composer, or the top
bar, shows a card about who is behind it (`avatar_card.rs`): a participant's
name, whether they are here and what they are doing in the draft, their access,
how many prompts and comments they have sent in this chat, and when they
joined; or, for a reply, its model, whose computer or API key ran it, and how
long it worked. The card is a tooltip and only informs: it closes as soon as
the pointer leaves the avatar, and anything it mentions can be done elsewhere.
Join and leave times are what this client saw, kept in memory only, so a
collaborator never sees when those already present joined.

### Creating prompt blocks

Independent prompts are the default. When the draft is empty, every editing
participant's caret sits at the same draft position, with overlapping carets in
participant colors. Typing there always creates a new block appended to the end
of the shared order; the typist becomes its creator and stays in it, while
everyone else remains at the draft position, now below the new block. Two
participants typing concurrently create two blocks, which Yrs orders identically
on every replica. Adding an attachment at the draft position also creates a new
block, with an empty body and that attachment.

### Navigating

Clicking into an item places the caret there; participants then edit
concurrently with character-level merging. Up and Down move through one chain in
visual order: expanded composer comments, prompt blocks, then the draft
position. They leave an item only from its first or last **visual** line, and
the caret enters the neighbor on its nearest visual line. A collapsed comment
group is skipped, and inline comment editors in the timeline keep Up and Down
within themselves.

### Removing items

Empty items disappear when nobody is using them. An item is **empty** when its
body has no non-whitespace content and, for prompt blocks, it has no
attachments.

- When a participant's focus leaves an empty item and presence shows nobody
  else focused in it, that participant removes it. If others are focused in it,
  it stays and the last to leave removes it. Switching to another window or
  application does not count as leaving.
- **Escape** in an empty item removes it (if nobody else is in it) and moves
  the caret to the draft position. **Backspace** does the same but moves the
  caret to the end of the previous stop in the navigation chain.
- Because two participants leaving at once would each still see the other
  there, the host also removes an empty item as soon as presence shows everyone
  has left it. When a participant disconnects, the host removes empty items
  that participant was in and nobody else is. Read-only sanitization and
  downgrade cleanup (see Presence) do not trigger this removal.

A removal can race with someone else entering the item; their concurrent edits
are lost. This is accepted.

### Comments

- Typing while an excerpt of an assistant message (or an agent reply to a
  comment) is selected creates a comment targeting that excerpt, with the typed
  character as its first content. Comments can target messages that are still
  generating, since streaming only appends to a message's source.
- A comment's target is fixed at creation. Comments have no attachments;
  pasting files into a comment editor inserts only text.
- Every participant sees every draft comment: highlighted in the target
  message, with an inline editor next to it, and as a card in the composer.
  Both editors edit the same shared text and show remote carets.

### Attachments

Attachments belong to a prompt block, and `Write` and `Admin` participants can
add or remove any attachment, regardless of creator. File bytes travel over the
protocol and are never stored in the document.

The target block is the one pasted or dropped onto; otherwise the focused
prompt block; otherwise (draft position, a comment, or no focus) a new block.

1. **Reading**: the adder reads the file locally. Presence advertises the
   pending read (name, whether it is an image, progress, target block), so
   everyone sees a placeholder chip with a progress bar.
2. **Uploading**: once read, the adder appends an attachment record to the
   target block and streams the bytes to the host. If the target block was
   removed meanwhile, a new block is created for it.
3. **Stored**: when the host has every byte, it announces the attachment as
   stored. Only the host needs the bytes to submit, so an `Admin` can submit
   once every attachment is stored even if others are still downloading.
4. **Downloading**: the host relays bytes to every other participant, whose
   chips show progress until the bytes are available locally.

Transfer details:

- Each connection has a control queue and a small bulk queue. Attachment bytes
  travel on the bulk queue in 64 KiB chunks and the writer always sends waiting
  control messages first, so a large file delays draft updates and presence by at
  most one chunk.
- Bytes and the draft record that announces them can arrive in either order.
  Every chunk names the file, so the host stores an upload once both are there.
- Removing an attachment while it uploads cancels it: the collaborator sends
  `AttachmentCancelled` on the bulk queue after its last chunk. The host
  discards a file when an update removes its record or its uploader leaves
  before finishing. Collaborators keep every file they have, even ones they
  remove, because a submission can include a file being removed at the same time
  and the host sends each file only once.
- Losing draft-write permission cancels pending local reads, and late disk
  callbacks cannot attach the result even if access is restored. An upload whose
  record the host accepted while the uploader could write may finish after
  revocation, but the grant covers only that connected uploader and that
  record's ID, name, kind, and size. Pre-record buffers are discarded on
  revocation and new uploads are denied; cancelling one's own accepted transfer
  stays allowed.
- Size limits (`attachments.rs`) are checked when a file is added, against the
  records already in the draft, and again by whoever accepts a submission.
  Collaborators cannot attach files until they have received the thread.

### Presence

Each participant publishes one presence state (`Presence` in `protocol.rs`):
focus, a selection as Yrs sticky indices in the focused body, and pending
reads. It is published whenever it changes, checked every frame and whenever a
file read starts or ends, and is not coalesced.

- Presence is sent through the host, which rebroadcasts it tagged with the
  participant UUID, and dropped when the connection closes, so no timeouts are
  needed. It is never stored in the document.
- Remote carets and selections are painted in the participant's color and never
  move local focus. A caret shows the participant's name briefly after it
  moves.
- The host replaces read-only peers' presence with an empty state before
  storing and rebroadcasting it, so readers cannot announce work that blocks
  submission or trigger empty-item deletion. On downgrade, the host clears and
  rebroadcasts stale focus, selection, and pending reads without waiting for
  the peer, and mirrors clear them when applying the policy.

## Thread controls

### Submission

Ctrl-Enter (Cmd-Enter on macOS) in any composer editor or at the draft
position, or clicking Send, submits the whole draft for everyone, provided the
submitter is `Admin`. `Write` permits editing, not consuming anyone's draft;
the host checks the requester's current permission, not the local viewer's
mode. While nobody else is in the chat, plain Enter sends too and Shift-Enter
starts a new line. Once anyone else is there, Enter starts a new line, because
a submission takes everyone's blocks and a habitual Enter must not send others'
unfinished text. The send button's tooltip names whichever key sends.

Send requires that the draft has at least one non-empty item (or the idle
thread has accepted pending messages to resume), that no participant has a
pending attachment read, and that the host has stored every attachment in
non-empty blocks. The selected model must also be available.

The host accepts a submission as follows:

1. The submitter sends `Submit` (steering) or `Queue` with the submission
   sequence it has seen, after sending all its pending draft updates on the
   same ordered stream so its own accepted edits are included. The host
   checks `ControlGeneration` before considering the submission.
2. The host ignores a stale sequence: someone else's submission was already
   accepted and everyone sees it.
3. From its replica, the host snapshots every non-empty comment and every
   non-empty prompt block (with attachments), in draft order, including blocks
   others are still typing in.
4. In one transaction, the host removes exactly the snapshotted items from the
   draft. Empty items stay, for example a comment someone has not started
   typing yet.
5. The host broadcasts that draft update and advances the submission sequence.
   While idle it publishes the user message and starts the agent; while
   generating it accepts the submission as a pending one.
6. Edits that arrive for removed items are discarded, and participants whose
   focused item was submitted move to the draft position.

A permission rejection is reported only to the submitter as `PermissionDenied`,
routed to the originating thread and its draft even if another thread is now
active. Other refusals (empty draft, unavailable model, attachments not
stored, size limit) are dropped silently for collaborators.

The host titles a new thread from the first prompt block, or from the first
comment when a submission has only comments.

#### Pending delivery

Each accepted draft is immutable, gets an ID, and records who submitted it. The
pending list is host-owned, mirrored in snapshots, and individually cancellable,
steering and queued alike.

- **Steering** is the default while a run is going. It uses the session's native
  `Steer` delivery: it joins after the current reply and its tool results,
  before the next request, without interrupting sampling or bypassing a waiting
  approval. The host hands a steer to the run as it is accepted, and every
  pending steer to a new run right after its prompt, never to a stopping run, so
  steers left from a stopped run join the next run (new draft or Resume) at its
  first safe point.
- A pending group stays pending until its input commits; then `PendingDelivered`
  publishes it before the committed entry. The input ID is the user group's ID:
  an input in a run's output whose ID names a user group of the timeline is that
  group's steer, the one rule live folding and replay share. Steering comments
  use aliases `comment_<first 8 hex digits of the group ID>_<n>`, which keep
  earlier comment replies stable without renumbering after cancellation, and
  join the run's comments only when the steer commits.
- **Queued** drafts start one run each in FIFO order with the model and project
  settings current when each starts; nothing reaches a running agent from the
  queue. User queues take precedence over the scheduler reusing the thread. The
  32 MB attachment limit applies to each submission, as each becomes a message of
  its own, and queued attachment bytes are included in late-join transfers.
- **Cancellation** (`CancelPending`) requires `ControlGeneration` and removes one
  pending draft by ID. A queued draft, or a steer the run does not have yet, is
  removed at once. A steer the run has is withdrawn through the session, and only
  its `Removed` receipt takes it off the list; the session commits a safe point's
  steers as it takes them, so a cancel that loses that race is answered by the
  commit and changes nothing.
- **Advancing**: a normally completed run starts the next pending run,
  undelivered steering before the FIFO. A steer that finds no run to join is
  discarded by the session's fallback but stays in Cowork's pending list, so it
  starts a run of its own. Stopping or failure keeps pending submissions visible
  and cancellable, and Stop wins over a completion already on its way to the UI
  so the queue does not advance. An idle composer with an empty draft shows a
  **Resume messages** button (or the submit shortcut) that resumes pending work
  without creating a submission. Archived threads do not start queued runs, and
  pending submissions are not saved across restarts.

The composer shows Stop while generating with an empty draft and Steer once the
draft has any non-empty item. Cmd/Ctrl+Enter (or Enter, while alone) and a click
steer; Cmd/Ctrl+Shift+Enter and a Shift-click queue.

#### Published message and agent prompt

A submission is published as one timeline entry: a collapsible comment group,
then one row per prompt block. The agent prompt is the same for shared and local
threads (`submission.rs`): comments first, each labeled with its creator's prompt
name and answered with one `respond_to_comment` call, then each prompt block in
order preceded by its creator's prompt name. A participant's **prompt name** is
their display name when their first item is submitted in the thread and never
changes afterwards, so renaming shows everywhere in the UI but the agent keeps
knowing them by one name and nothing already sent changes.

#### Transcript and agent output

The host keeps one `AgentSession` per thread and a **transcript** of its runs: an
append-only vector of entries (inputs, context notes, replies, tool results)
plus a ledger of accepted compactions. Between compactions each request extends
the previous one exactly, keeping the provider's prompt cache valid. A stopped or
failed run keeps its prompt and every turn it completed.

- The host forwards what the session reports as `AgentEvent`s carrying a
  `RunEvent`. Everyone, the host included, folds them the same way into a replica
  of the session's conversation, which refuses an entry the session could not
  have committed at that index. A reply joins the transcript whole; previews only
  show it while it streams.
- What an agent message shows is never sent. It is a function of the transcript
  entries and compactions after the run's prompt, followed by the **pending
  events**, the previews since the last reply joined the transcript; a run that
  stops mid-reply keeps them, since that output never joins the transcript.
  Comment replies come from `respond_to_comment` calls with IDs derived from the
  message and call, so every participant names them alike.
- Only the **response**, the text after the agent's last tool call, can be
  commented on, and comment ranges are offsets into it. Everything before it is
  work, shown under "Working for" while the run goes and collapsed under "Worked
  for" once it ends. When steering extends a run after an answer, the earlier
  answer stays visible before its steering message even with work collapsed.
- The run's prompt joins the transcript with its files' content inline (images
  as base64), so attachment bytes travel both inline and as `AttachmentData`. A
  `Welcome` carries the whole transcript as one `Json<TranscriptRecord>` (the
  agent's versioned records, which postcard cannot represent directly) plus each
  agent message's run start, pending events, and whether the run ended, and the
  joiner folds the pending events to rebuild the message exactly.

#### Context compaction

Compaction is the agent runtime's (`docs/agent-architecture.md`), run at a safe
boundary: before a run's first request, or between two requests once every tool
call has its result, including for queued and scheduled runs. The host asks for
it when a request's reported usage reaches the configured percentage of the
context window (default 80%), or before a run starts when the last measured
context plus estimates for unmeasured output already does.

- The Context settings page controls the threshold and the default Ollama window
  (131,072 tokens). A window change updates the Ollama models in every local
  thread catalog and reaches collaborators through `ModelCatalogChanged`; runs in
  progress keep the window and policy they captured.
- A rejected summary (empty, failed, tool-bearing, or not smaller in encoded bytes
  than the history it replaces) leaves the run going on its old context.
- An accepted summary changes only what later requests send; the transcript stays
  complete. It leaves a "Context compacted" divider in the run's output that
  collapsed work keeps visible, and failed or canceled summaries leave none. The
  next request carries the environment note in full.

#### Onboarding chat

After first-time model setup, the host creates a "Welcome to Cowork" thread with
a canned greeting and a host-authored onboarding prompt. There is no
participant submission: the host seeds one run through `AgentStarted`, the
committed prompt (`Host` provenance, no user group in the timeline), `Preview`s
of the greeting, its committed reply, and `AgentEnded`. The thread is first saved
once that run has ended, no model request runs until the user replies, and a
reply sent while the greeting streams waits as a pending submission.

### Stopping

Submit and stop share the `ControlGeneration` permission, so the host and
`Admin` peers see the Stop button while the agent is generating. `Stop` names
the running agent message. The host checks current permission, cancels that run
if it is still active, and ignores an authorized request otherwise. The run
ends with `AgentEnded` marked as stopped.

### Tool approval

See [Permissions](permissions.md) for command scope, auto-approval, file tools,
and run grants. Reads span the host filesystem except denied credentials, so a
submitting collaborator can have the agent read other host files into the
transcript.

The host emits `ToolApprovalRequested` for a waiting call. Only the host may
decide commands, file writes, and permission grants; `Admin` peers may decide
other tools. Local clicks and `DecideToolCall` use the same checked path. The
first decision wins and emits `ToolApprovalResolved`; stale requests change
nothing. A denial commits a refused tool result and the run continues, and
ending a run settles outstanding calls as not run. Participants reject requests
for calls the reply did not make or that already returned.

`write_file` and `edit_file` calls show a diff on the approval card and in the
timeline. Every participant computes it from the call's arguments alone, so
nothing extra travels: an edit shows `old_string` becoming `new_string`, and a
write shows its whole content as added, whatever the file held before.

### Model selection

- Model selection is per thread; new local threads start with the last model
  selected locally. A run uses the model selected when its submission is
  accepted and keeps it to the end. Each agent message records its run's model
  (sent in `AgentStarted`), so a thread that switched models shows each
  message's own provider logo.
- Every thread carries a catalog of the models of whoever runs its agent, grouped
  by provider (`models.rs`). A local or hosted thread uses this app's discovered
  catalog; a mirrored thread uses the host's, which arrives in `Welcome` and is
  replaced by `ModelCatalogChanged` after each successful discovery, even an
  empty one. Failed discovery keeps the previous catalog.
- The providers are a fixed enum (Ollama, Anthropic, OpenAI) and the provider is
  part of a model's identity (`ModelRef`). API keys stay in the host's OS
  keychain and never travel; each provider's discovery replaces only its own part
  of the catalog.
- The host and `Admin` peers can pick a model. The client sends `SelectModel`;
  the host checks `ChangeModel`, applies it in arrival order if its catalog
  offers the model, ignores it otherwise, and broadcasts `ModelSelected`. A peer
  waits for that confirmation before showing the selection, since the host's
  catalog may have changed. Picking a host model does not change the default for
  new local threads.
- When a catalog stops offering the selected model, it stays selected but is
  shown grayed out, nobody can send, and the host refuses submissions for it too,
  until another model is picked or the catalog offers it again.

### Project folders

- A thread's project is a list of folders on the host's machine, in the order
  added. Only the host adds or removes them, from the bottom bar; a folder added
  twice is kept once. Folders and the level below chosen before the thread exists
  move into it with the draft.
- Each folder is named by its last path component, with path and list separators
  (`/ \ : ; ,`) and control characters replaced by `_`, and `root` for a
  filesystem root. A name already in the project gets a `-2`, `-3`, ... suffix,
  and names never change once given, so removing a folder leaves the others
  where they are.
- Collaborators see only the names, which arrive in `Welcome` and are replaced as
  a whole by `ProjectFoldersChanged`. The paths do reach collaborators through
  the transcript: the agent's environment note and its commands name them, along
  with the host's user name where a path holds it.
- The project has an auto-approval level, `Read`, `Write`, or `Write + Network`,
  chosen by the host from the **Auto-approve** menu (every thread starts in
  `Read`) and mirrored through `Welcome` and `ProjectModeChanged`. It is the
  default access commands run with unless they ask for their own; see
  [Tool approval](#tool-approval).
- Commands use the host's shell and environment in an OS sandbox, starting in the
  first project folder or scratch. Each command is contained by the project as it
  is when it starts, so a change applies from the next command while running ones
  keep their access. Read scope and write limits are in
  [Permissions](permissions.md).
- The agent learns the folders' paths, the level, where commands start, and the
  scratch folder from an `<environment>` note the host puts first in a run's
  prompt whenever it differs from the latest one in the transcript. The note, not
  the tool definitions, carries what changes, so the tools and system prompt stay
  a stable prompt prefix.

## Sharing lifecycle

- **Starting**: the host's existing draft becomes the shared draft unchanged;
  its items keep the host as creator. Only the active thread can be shared.
- **Joining**: the collaborator sends `Join` with its protocol version, then its
  `Profile`. The host rejects mismatched versions; otherwise it replies with
  `Welcome`, then streams the bytes of every attachment in the thread. A
  collaborator that lags the host's event buffer receives a new `Welcome`; see
  [Validation](#validation) for how it is installed.
- **Collaborator disconnects**: its presence and permission override disappear,
  and the host removes the empty items and unfinished uploads described above.
- **Host stops sharing**: while membership and the broadcast channel still exist,
  the host runs departure cleanup for every peer (unfinished uploads, presence,
  overrides, unattended empty items), then clears membership and ends hosting.
  Accepted content remains local.
- **Host archives the thread**: the host stops its agent run and stops sharing.
  A thread archived while its sharing is still starting closes the endpoint once
  bound instead of hosting, and a restored thread is not shared. Collaborators
  cannot archive their copies; they disconnect instead.
- **Host disconnects**: the session ends and collaborators' copies are removed.
  Host migration is out of scope.
- **Invalid host data**: an invalid draft snapshot, an inconsistent transcript or
  run snapshot, or invalid agent records are protocol errors; the collaborator
  removes its mirrored thread and the application stays open. `Welcome` is
  validated before installation, rebuilding the transcript with the checks made
  when each entry was first applied and checking that run starts increase,
  timeline UUIDs are unique, and only the last run can still be generating. A
  frame over 1 GiB cannot be sent, which disconnects the collaborator.

Drafts are not persisted: a saved thread comes back with an empty one.

### Saved threads

The host saves its own threads, archived ones included, in one SQLite database,
`~/.cowork/cowork.db` (`crates/cowork/src/chat_db.rs`, schema in
`crates/cowork/migrations/`, design in `docs/sqlite-storage.md`). Collaborators
save nothing of a thread they joined. The sidebar project a thread is filed
under is local organization and never sent to collaborators (`docs/projects.md`).

- Unless launched with `--ephemeral`, the host lists threads by their rows at
  startup and loads each when it is opened or a scheduled task continues it. A
  loaded thread goes back to its row after 5 idle minutes once everything it
  holds is confirmed saved; a shared thread is always loaded.
- A thread is saved at turn boundaries: when a run ends, when it is archived or
  restored, before it is unloaded, and when the app quits; never during a run, so
  every tool call in a saved transcript has its result. A crash during a run
  loses what that run did since the last save. A save appends what is new in one
  transaction and carries the head it was built on, which the database checks; a
  save holding less than is stored is refused.
- The draft, pending submissions, sharing, and per-peer permissions are not
  saved. A thread comes back unshared, where it was in the sidebar or archive,
  with an empty draft and the profiles kept so earlier messages still name their
  authors. Loading checks a saved thread as a collaborator checks a `Welcome`.
- The schema has its own migrations, independent of `PROTOCOL_VERSION`.
- One app instance owns the saved state, holding `~/.cowork/.lock`; another
  normal launch exits with an already-running error before loading anything.
  `--ephemeral` opens an independent instance that neither reads nor writes
  `~/.cowork` nor takes the lock, so nothing is persisted or unloaded, and its
  scratch files go in a temporary folder removed on orderly shutdown. It does not
  restrict otherwise permitted agent changes to project files.
- Deleting a thread from the archive removes its rows and its folder.

## Document layout

One Yrs document per thread holds the draft, collaborative content only: an
`order` array of item IDs and an `items` map. A prompt item has `kind`,
`creator`, a `body` text, and an `attachments` array; a comment item has `kind`,
`creator`, `target`, and `body`. Field names and the API are in
`crates/draft/src/lib.rs`. `creator` and `target` are write-once. Attachment
records and comment targets are atomic Yrs `Any` values rather than nested maps,
because their fields describe one coherent value and must never merge field by
field.

- **Order separate from content.** Items have stable UUIDs independent of
  position, so carets, presence, and creator records survive concurrent inserts
  and removals. Creating or removing an item changes `order` and `items` in one
  transaction.
- **One document per thread.** Accepting a submission removes the submitted items
  rather than replacing the document, so unsubmitted items and the carets in
  them keep their identity and late updates need no routing, since they land in
  removed items. The submission sequence, not the document, prevents duplicate
  submissions.
- **Creator in the item.** It arrives atomically with the item, so no item is
  ever shown without a creator, and clients never write it afterwards. Future
  authentication belongs at the host, not in the document.
- **Bodies are Yrs `Text`** holding plain unformatted text; the composer does not
  render markdown. Mentions (inline embeds holding a participant UUID) and inline
  formatting (Yrs attributes) would fit without a layout change, but validation
  rejects embeds and attributes until they are supported.

## Protocol

All traffic goes through the host; collaborators never talk to each other.
`protocol.rs` defines every message and `PROTOCOL_VERSION`, which must match
exactly. `Join` keeps its variant index and version as its only field, and
`Rejected` keeps its variant index and string payload, so a version mismatch can
still be reported; runtime permission denials use the separate
`PermissionDenied`, never this handshake rejection.

The host is itself a participant: its local edits, submissions, stops, and model
changes go through the same authorization paths as a collaborator's, with the
host always authorized. Permission defaults and overrides are host-authoritative
state in `ThreadSnapshot`; the host applies and broadcasts their change events
(`DefaultPeerModeChanged`, `PeerModeOverrideChanged`), and `ParticipantLeft`
removes the departing override. There is no collaborator command to manage
access.

Each collaborator request is checked against current permission before the host
acts (see the permissions table above). A stale submission sequence and a
`SelectModel` for a model the catalog does not offer are ignored, a reader's
presence is sanitized to empty, and `AttachmentCancelled` is always allowed for
the peer's own upload. Only the host may decide a `DecideToolCall` for commands,
file writes, and permission grants.

Host messages are thread-wide events that the host applies locally and
broadcasts unchanged, except the peer-specific `Welcome`, `Rejected`,
`PermissionDenied`, and `DraftReset`. A `PermissionDenied` carries the
participant, the operation, and a reason (`NotParticipant`,
`InsufficientMode`, `HostOnly`, or `StaleDraftGeneration`) and leaves the
connection open. Draft updates from the host and from other participants are
broadcast untagged; generations tag only collaborator requests.

## Validation

Authorization and structural validation are separate. For a collaborator's
`DraftUpdate { generation, update }`, the host checks membership, the peer's
current generation, and `EditDraft` permission **before decoding, applying, or
broadcasting** bytes. A denial sends `PermissionDenied` followed by
`DraftReset { generation, state }` only to that peer, without changing the host
draft or disconnecting it.

Each connection starts at generation 0. Rejecting its **current** epoch for
revoked edit permission advances only that peer's epoch once. A mismatched epoch
is denied as `StaleDraftGeneration`, even after regrant, without advancing
again, so queued optimistic updates from the rejected replica stay fenced out.
The generation is a replica fence, not a thread-wide revision, submission
sequence, or counter of permission changes.

`Welcome` and `DraftReset` use the same installation rules: an older generation
is ignored; a matching one merges, retaining fresh unsent edits; only a newer one
replaces the document with a fresh Yrs client ID, invalidating editor handles and
sync baselines so old editors cannot replay rejected input after regrant.

For an **authorized** update, the host applies and broadcasts it first, then
checks (`validate.rs`, `verify_change`, and `thread.rs`):

- the document has only the `order` and `items` roots;
- every `order` entry is unique and has an item, and every item is in `order`;
- every item has a supported kind and exactly the fields listed for it;
- `creator` and `target` never change, and a new item's creator is the sending
  participant;
- a new item does not reuse the ID of an already submitted item;
- bodies contain only plain text, and attachment records are well-formed, unique
  by ID, and created by the sender when new (size limits are the caller's).

Any violation is a protocol error and the host disconnects the peer. The update
has already been applied and broadcast, with no rollback. Permission enforcement
prevents unauthorized draft writes but does **not** make malicious writes by
authorized peers safe: structural validation is a defensive check, not a
security boundary, and isolating such writes would need validating against a
scratch copy before applying and broadcasting.

### Constraints for editor code

- Yrs transactions are synchronous, live in the `Thread` entity, and never cross
  an `await`; the Tokio side moves only encoded bytes.
- `thread/permissions.rs` checks membership and policy on every operation through
  sealed operation types (`EditDraft`, `ControlGeneration`, `ChangeModel`,
  `ManageAccess`) that select closure-scoped `Authorized` guards, which callers
  cannot construct or retain and which expose neither `Thread` nor the Yrs `Doc`.
- `DraftEditorState` is separate from the canonical draft. Mutations and
  asynchronous attachment callbacks recheck the access of the draft they target,
  not of whichever thread is active.
- Authorized local edits never wait for the network. Remote edits in a focused
  editor must preserve the local caret and selection and are deferred while IME
  composition is active.
- Text offsets are UTF-8 bytes, converted to Yrs offsets at the boundary; carets
  and selections that must survive concurrent edits use sticky indices.
- Undo must never revert another participant's edits. Each replica uses a fresh
  random Yrs client ID, and a rejoin creates a new replica.

## Not yet implemented

Gaps in what this document describes:

- explaining non-permission submission refusals to collaborators (empty draft,
  unavailable model, attachments loading, size limit), which are dropped
  silently;
- showing names when hovering a remote caret (names show only briefly after
  a caret moves);
- fully validating writable peers' presence at the host (focus, selections,
  announced file reads); only read-only presence is sanitized;
- showing others' selections in agent messages, which needs a gpui-kit
  addition.

Ideas that fit the design without changing the document layout: authentication
(the host maps authenticated identities to participant UUIDs and enforces
`creator`), contributor history derived from Yrs events, mentions and inline
formatting, and reordering blocks (no UI is planned).

Out of scope: host migration, persisting drafts, keeping identity across
reconnects, and an invitation format beyond the endpoint ID.
