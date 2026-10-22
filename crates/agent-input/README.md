# Agent input

A reusable input lifecycle for the provider-independent agent loop. The
[agent-runtime](../agent-runtime/README.md) driver uses this layer; Cowork still
keeps its own pending-submission queue and hands its session one input at a
time. This crate owns pending work and the handoff into a conversation; it has
no GPUI, provider, tool, async-runtime, or persistence dependency.

`Inbox<Source, Payload>` is owned by the session actor. Source and payload are
application types: adding a scheduler, parent agent, collaborator, background
task, or extension requires no new queue implementation. For example:

```rust
use agent_input::*;

#[derive(Debug)]
enum Source {
    Participant(String),
    Task(String),
    Schedule(String),
    Parent(String),
    Extension(String),
}

let mut inbox = Inbox::default();
inbox.admit(Input {
    id: InputId::new(),
    source: Source::Participant("alice".into()),
    payload: "Check the failing test first",
    policy: Policy {
        access: QueueAccess::Editable,
        fallback: Fallback::Queue,
        lifetime: Lifetime::Retain,
    },
}, Delivery::Steer).unwrap();
```

The host authorizes a request before admission. A source is provenance, not
authority; a model-authored parent message cannot grant itself human privileges.
Queue edit APIs check editability but do not replace participant permissions or
optimistic version checks. This crate deliberately does not parse commands,
expand skills/files, generate prompt envelopes, or infer policy from ID prefixes.

## Delivery and control

| Intent      | While running                                | While idle                       |
| ----------- | -------------------------------------------- | -------------------------------- |
| Queue       | Ordinary FIFO for later runs                 | Starts on the next `begin_next`  |
| Steer       | Joins the next safe-point batch              | Queued or returned, per fallback |
| Interject   | Like Steer, with a `YieldWait(run)` signal   | Queued or returned, per fallback |
| Interrupt   | `Cancel(run)` plus a priority FIFO follow-up | Priority FIFO follow-up          |
| `stop(run)` | `Cancel(run)` and pause accepted work        | Stale run error                  |

Interject means waking a cooperative wait, not forcibly killing a tool. It has
priority over Steer within a safe-point snapshot; each class preserves admission
order. Interrupt follow-ups precede ordinary future runs, preserving their own
FIFO. Ordinary Queue work never automatically becomes steering; a host's live
follow-up setting can select Steer on admission, or `redirect` an editable queued
row atomically. `move_before` reorders the ordinary lane. There is no implicit
prompt combining or source-dependent priority.

Stop is separate control with no model-visible payload. It pauses the queue;
`resume` allows future work, and a newer explicit Interrupt also resumes handoff.
A failed run pauses too. Merely enqueueing a prompt does not resume a stopped
session. Signals are level-triggered, run-scoped observations: always recheck
the current signal before performing delayed cancellation or wait-wakeup work.

Policy axes stay independent:

- `QueueAccess`: editable, protected, hidden. This does not set priority.
- `Fallback`: queue or return a steer/interjection that has no accepting run.
- `Lifetime`: retain, producer-bound, ephemeral. Producer closure withdraws the
  latter two; session shutdown returns them to the host, which cancels
  producer-bound producers and discards ephemeral context. Retained work stays
  in memory. None of these policies retract already committed messages.

## Reservation and commitment

Every model-visible handoff uses a reservation, including a run's first prompt:

1. `begin_next` creates a run and reserves one queued input. Or, on a running
   conversation, `begin_safe_point(run)` reserves the current interjection and
   steering snapshot. Neither operation relinquishes input ownership.
2. Inspect `batch(id)`, clone the preparation data needed by async work, and
   release the actor borrow/lock. Expand attachments, skills, and application
   context outside this crate. Later arrivals remain separate from this batch.
3. Return to the owner and `commit(id, callback)`. The callback synchronously
   commits the prepared messages and their journal record. It must be atomic
   on error. An error leaves the reservation available for retry; a cancelled
   run or invalidated reservation prevents the callback from running.
4. Only a successful commit returns the owned batch. The host can now publish
   delivery receipts and start the next completion request.

There is one preparation per session. Cancelling an async preparation future
does not remove its reserved inputs. The owner must retry it or settle its run;
there is no lease expiry and no hidden drop callback. `producer_closed` can
invalidate a snapshot while it is being prepared: its return value identifies
the invalidated delivery and any replacement containing the surviving sources.
Old preparation must be discarded, never committed under the replacement ID.
Withdrawal of an uncommitted initial prompt requests cancellation of that run.

`finish_run(run, Completed)` refuses completion while admitted work is still
unconsumed. The owner must serialize that final check with admission: a message
either joins the still-active run or observes an idle session and takes its
fallback. On Stop/Failure, unconsumed active work follows fallback in delivery
order ahead of ordinary queued work, behind explicit Interrupt follow-ups.
An uncommitted initial prompt is recovered regardless of its mid-run fallback.
Returned discarded/withdrawn inputs remain available for host receipts and
producer bookkeeping. Late finish/stop/commit calls cannot mutate a successor.

The ownership guarantee is in-process only. Successful commitment prevents this
inbox from delivering that input twice. Duplicate IDs are rejected only while
pending; reconnect/replay deduplication, crash recovery, event encoding, capacity
limits and backpressure belong to the session owner/journal. IDs and policies
here introduce no new wire or storage format.

## Placement in the stack

The [agent-runtime](../agent-runtime/README.md) session uses this layer above
`LanguageModelProvider` and `tool-runtime`. The integration boundaries are:

- Commit the initial input before building `CompletionRequest.messages`.
- Keep `CompletionEvent` deltas provisional. Steering does not modify a request
  already streaming, and cancelled previews do not become accepted messages.
- After accepting an assistant reply, settle its tool calls and append correlated
  results before committing a safe-point batch. Preserve complete assistant
  messages, including provider-native signed thinking/replay data.
- Before returning a final answer, drain once more, and continue inference if
  inputs arrived. Serialize the final `finish_run` with new admission.
- On `YieldWait`, wake only a tool implementing that cooperative host service.
  On `Cancel`, cancel local inference and cooperate with tools, then settle the
  run. `ToolCapabilities.supports_cancel` does not promise rollback; dropping
  HTTP work does not prove remote computation stopped. The loop must resolve
  outstanding tool-call correlation before reusing that history.

`Payload` is application data; agent-runtime uses its host's typed `Input` and
renders it to user content only when it is committed, so untrusted source text
never becomes a privileged message. Application history, permissions,
formatting and event journaling remain with the host. Providers still execute
one request; tool-runtime still executes one call. This crate makes no tools or
model requests itself.
