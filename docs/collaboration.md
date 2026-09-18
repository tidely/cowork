# Collaborative prompts

## Status

This document describes the intended collaboration behavior for Cowork. It is a planning document, not an implementation description. Confirmed behavior is separated from open questions so the design can be refined without losing the original requirements.

## Goals

A shared thread should let several participants prepare prompts together without forcing everyone into one text field.

- Participants write independent prompts by default.
- A participant may create multiple prompt blocks.
- Any participant may deliberately join and co-edit another prompt block.
- Everyone sees text edits and collaborator cursors update live.
- Submitting combines the current prompt blocks and sends one request to the agent.
- The creator and current editors of each prompt block are visually identifiable.
- Existing single-user behavior should remain unchanged for unshared threads.

## Terminology

- **Participant**: A user connected to a shared thread.
- **Prompt block**: One independently editable text entry in the collaborative composer. A participant can own more than one block.
- **Creator**: The participant who caused a prompt block to be created.
- **Editor**: A participant whose cursor or selection is currently inside a prompt block. The creator may or may not be a current editor.
- **Draft position**: The empty composer position where a participant can begin a new independent prompt block.
- **Submission**: An ordered snapshot of the nonempty prompt blocks sent to the agent as one request.

## Composer behavior

### Empty composer

When all prompt blocks are empty:

1. Every participant sees one empty composer position.
2. Every participant's cursor blinks at the same visual position.
3. The overlapping cursors may use participant-specific colors, but they must not create separate visible rows while nobody has typed.
4. No participant owns visible content yet.

The shared empty position is primarily a visual and presence affordance. Whether empty drafts exist as CRDT records before typing remains an implementation decision.

### Creating independent prompt blocks

Independent prompts are the default behavior.

1. The first participant who types at the shared empty position creates a prompt block.
2. That participant remains in the newly created block and continues typing there.
3. The other participants' draft cursors move to a new empty composer position below the created block.
4. Typing at that lower position creates a separate prompt block owned by the participant who typed.
5. If a participant later moves below the existing blocks and starts typing again, a new prompt block is created even if that participant already owns an earlier block.
6. Prompt blocks appear in document order, matching their positions in the composer.

Example:

1. Alice types and creates block A.
2. Bob's cursor moves below A. Bob types and creates block B.
3. Alice moves below B and types again, creating block C.
4. The order submitted to the agent is A, B, C.

Ownership identifies who created a block; it does not grant exclusive editing rights.

### Joining an existing prompt block

A participant can intentionally edit an existing block instead of creating an independent one.

- Clicking text places the participant's cursor in that prompt block.
- Pressing Up from the draft position moves into the nearest appropriate prompt block above it.
- Once joined, participants can edit the same text concurrently.
- Selections, cursor movement, insertion, replacement, and deletion should retain the native text editing behavior provided by Cowork's GPUI text component.
- Leaving a block does not change its creator.
- A participant can return to the draft position to create another independently owned block.

Exact Up/Down behavior at wrapped visual lines and block boundaries must be specified before implementation. See [Open questions](#open-questions).

### Remote cursors and selections

- Each participant has a stable color for the duration of a collaboration session.
- Remote cursors and selections update without taking local keyboard focus.
- Cursor and selection positions use CRDT-relative anchors so concurrent edits do not corrupt their positions.
- Presence state is ephemeral and must not be stored as durable document content.
- Disconnecting removes a participant's live cursor and editor avatar after an appropriate presence timeout.

### Block avatars

Each prompt block has an avatar gutter consistent with the timeline's existing layout.

- The creator's avatar is the primary avatar.
- If other participants are editing the block, their avatars are layered beside or partially over the creator avatar.
- Layering must preserve recognizable colors or initials and should expose all active editors without making the text jump horizontally.
- Avatar order must be deterministic to avoid reshuffling on every presence update.
- Participants who are merely viewing the thread do not appear on a block; only the creator and current editors do.
- The UI should distinguish the creator from additional editors, even when the creator is not currently editing the block.

The exact maximum visible avatar count and overflow treatment are still open.

## Submission behavior

Pressing Ctrl-Enter, or Cmd-Enter on macOS, from any collaborative prompt block requests one shared submission.

1. Capture a consistent, ordered snapshot of all nonempty prompt blocks.
2. Combine the blocks into one prompt in document order.
3. Submit exactly one request to the agent.
4. Make the submitted user content immutable in the timeline.
5. Show the same submitted content and streaming agent response to every participant.
6. Deduplicate concurrent submission requests so simultaneous key presses cannot start duplicate agent runs for the same composer revision.

Empty blocks do not contribute to the combined prompt. Whitespace-only blocks should be treated as empty unless later requirements say otherwise.

The separator and attribution format used to combine blocks are intentionally unspecified. The agent may eventually need participant attribution, but that should not be assumed until the prompt format is chosen.

Existing behavior that allows composing the next response while an agent is generating should be preserved. The lifecycle of collaborative blocks after submission still needs a product decision.

## Shared document model

The model should be inspired by `irohproxy`:

- Yrs owns collaborative document content.
- Iroh transports encoded CRDT updates between peers.
- CRDT transactions remain synchronous and never cross an `await`.
- UI and transport replicas exchange encoded updates rather than sharing Yrs transactions or UI offsets across threads.
- Public text positions are UTF-8 byte offsets at the application boundary and are translated to Yrs's internal indexing.
- Cursor and selection preservation uses opaque CRDT-relative anchors.
- Bounded channels bridge GPUI and the Tokio-owned network session.
- Local edits remain optimistic and must not block on the network.
- Reconnection uses state vectors and diffs rather than assuming every acknowledgement was received.

### Proposed logical schema

The exact Yrs types may change during prototyping, but the document needs to represent at least:

```text
ThreadDocument
  prompt_order: ordered collection of PromptBlockId
  prompt_blocks: map PromptBlockId -> PromptBlock

PromptBlock
  id: stable unique identifier
  creator_id: ParticipantId
  text: collaborative text
  creation metadata needed for deterministic ordering
```

A prompt block is the unit of ownership, ordering, navigation, avatar display, and submission. It must not be modeled as one fixed field per participant because a participant may create multiple blocks.

Participant display names, avatar details, cursors, selections, focus, and connectivity belong to ephemeral presence state. They should not be mixed into the durable prompt text unless a small stable creator identifier is required for block attribution.

### Ordering and creation

Creating a block and inserting its identifier into document order must be one logical operation. Concurrent block creation must converge to the same deterministic order on every replica.

A block should have a stable identifier independent of its current vector index. This permits concurrent insertion and future insertion of comments or other timeline elements without invalidating identity.

### Submission coordination

CRDT convergence alone does not guarantee exactly-once agent submission. The collaboration protocol needs an authoritative coordinator, expected to be the participant hosting the shared Iroh endpoint initially.

A submission request should identify the document revision or submission generation it targets. The coordinator should:

1. Accept at most one submission for that generation.
2. Snapshot the converged ordered blocks.
3. Start the agent operation.
4. Broadcast the accepted submission identity and streaming response events.
5. Reject or coalesce duplicate requests.

Behavior when the host is missing updates at the instant another participant submits needs to be defined. A short synchronization handshake may be required before the snapshot is accepted.

## UI state and networking boundaries

Each collaborative thread will eventually need state beyond the current shared/unshared marker:

- Iroh endpoint or client connection lifecycle
- Local participant identity
- Peer membership and presence
- UI-side CRDT replica
- Transport-side CRDT replica
- Outbound update queue and acknowledgement state
- Submission coordinator state
- Agent stream state shared with peers
- Connection and synchronization errors visible in the UI

The Tokio runtime owns asynchronous Iroh transport. GPUI entities own renderable state and native text editor instances. Communication across that boundary uses bounded channels or coalescible snapshots so neither runtime blocks the other.

A prompt block's CRDT text and its GPUI editor state are related but distinct. Applying remote text must preserve local selection, IME composition, and focus. Remote updates should be deferred or reconciled safely while IME composition is active, following the approach demonstrated by `irohproxy`.

## Interaction scenarios

These scenarios form the initial acceptance checklist.

### Two users start from empty

1. Alice and Bob join an empty shared thread.
2. Both cursors appear at the same empty position.
3. Alice types `Investigate the crash`.
4. Alice remains in her block.
5. Bob sees Alice's text and cursor live.
6. Bob's draft cursor appears in an empty position below Alice's block.

### Independent prompts

1. Alice owns block A.
2. Bob types in the lower draft position and creates block B.
3. Alice and Bob can continue editing their own blocks without changing focus for the other participant.
4. Both replicas show A followed by B.

### Multiple blocks from one participant

1. Alice creates block A.
2. Bob creates block B below it.
3. Alice moves to the draft position below B and types.
4. A new block C is created with Alice as creator.
5. Blocks A and C remain separate and are submitted in their visual order.

### Co-editing one block

1. Alice creates block A.
2. Bob clicks text in A, or navigates upward into it.
3. Bob's avatar layers with Alice's avatar beside A.
4. Alice and Bob edit A concurrently.
5. Both replicas converge without losing either participant's valid edits.
6. Each participant's cursor and selection remain anchored through remote edits.

### Shared submission

1. Several nonempty blocks exist.
2. Bob presses Ctrl-Enter.
3. Every participant sees one immutable submitted user message containing the blocks in order.
4. Exactly one agent generation starts.
5. The response streams identically to every participant.
6. Simultaneous Ctrl-Enter presses do not duplicate the request.

### Disconnect and reconnect

1. Alice edits while Bob is temporarily disconnected.
2. Bob reconnects and synchronizes through state vectors and CRDT diffs.
3. Both replicas converge.
4. Bob's stale presence is removed while disconnected and recreated after reconnecting.
5. No stale cursor offset is applied directly to the reconciled text.

## Non-goals for the first collaboration increment

- Access control or user accounts
- Durable server-side persistence
- Collaborative undo/redo semantics
- Rich-text prompt blocks
- Tool execution controlled independently by multiple peers
- Host migration after the sharing participant disconnects
- Comments inserted into prior timeline content
- A polished invitation/link format beyond the endpoint identity

These may be added later, but the initial architecture should avoid making them impossible.

## Open questions

The following decisions must be made before considering the behavior complete:

1. **Participant identity:** Is identity ephemeral per connection, stable per installation, or tied to a future user account?
2. **Empty drafts:** Does each participant have an explicit empty CRDT block, or is the empty draft position represented only by presence until typing begins?
3. **First-writer races:** If two participants type into the shared empty position concurrently, do they create two independent blocks or co-edit one newly claimed block?
4. **Vertical navigation:** At what exact cursor positions does Up leave the draft or cross from one block into another? How should wrapped visual lines behave?
5. **Returning to independent mode:** What command or pointer target moves a participant from a co-edited block back to their empty draft position?
6. **Block deletion:** Is an empty block removed automatically, retained with its creator, or removed only through an explicit action?
7. **Submission format:** How are blocks separated, and should creator names be included in the prompt sent to the agent?
8. **Submission snapshot:** How does the host ensure it has incorporated a remote participant's latest update before accepting Ctrl-Enter?
9. **Post-submit lifecycle:** Are submitted blocks cleared, archived as a grouped timeline item, or retained while a fresh collaborative draft set is created?
10. **Edits during generation:** Are newly typed blocks always reserved for the next submission, and can participants edit the just-submitted snapshot?
11. **Host failure:** What happens to collaboration and an in-flight generation when the endpoint-owning participant disconnects?
12. **Avatar overflow:** How many editor avatars are shown before collapsing into a count?
13. **Awareness transport:** Should presence use a dedicated protocol message stream, a Yrs awareness implementation, or another ephemeral channel?
14. **Permissions:** Can every connected participant submit, edit every block, and unshare the thread?
15. **Invitation data:** Is an endpoint public key sufficient, or will peers require an endpoint address, relay information, thread identifier, and protocol version in a share link?

## Requirement summary

The core invariant is that the collaborative composer is an ordered collection of independently created CRDT prompt blocks, not one shared string and not one permanent field per user. Independence is the default, co-editing is deliberate, participant presence remains ephemeral, and Ctrl/Cmd-Enter creates one ordered, exactly-once agent submission visible to everyone.
