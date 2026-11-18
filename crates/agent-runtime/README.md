# Agent runtime

The provider-independent session driver above `language-model`, `tool-runtime`,
and `agent-input`. It owns one conversation and one active run at a time. The
Cowork app runs one session per thread on it, through the `agent` crate. Who
owns which state, and why, is in
[docs/agent-architecture.md](../../docs/agent-architecture.md); read it before
changing the session.

## Building an agent

```rust,no_run
use agent_runtime::{Agent, AgentDefinition, AgentError};
use language_model::{LanguageModel, LanguageModelProvider};

async fn example(provider: impl LanguageModelProvider + 'static) -> Result<(), AgentError> {
    let definition = AgentDefinition::new(LanguageModel::new("model"))
        .instructions("Answer concisely.");
    let agent = Agent::builder(provider, definition).build().await?;
    println!("{}", agent.prompt("Hello").await?.text());
    agent.close().await?;
    Ok(())
}
```

The managed `Agent` above is the simple API. Programs with their own sources,
inputs, tools, or approvals implement `AgentHost` and drive an `AgentSession`
through a `SessionHandle`; `crates/agent/examples/agent.rs` shows a host with
tools, approvals, a policy change, and a dynamic tool.

## The host

`AgentHost` is the application side of a session. Its associated types are the
request `Source`, the typed model-visible `Input`, the per-call `Access`, and the
live `Permissions`. `admit` sees every request with its source before the session
acts on it, `authorize` answers `Allow`, `Deny`, or `Ask` for one call, and
`describe_permissions` puts the policy in words for the model. A tool whose
`Context` is `T` registers when the host implements `AsRef<T>`.

`BasicHost` covers single-user programs, with `ToolPermissions` by tool name.
Its default refuses every call; `ToolPermissions::allow([...])`, `allow_all()`,
and `.ask_otherwise()` open it up. `Agent::builder` uses it.

## Requests

Everything that changes a session is a `Request { source, kind }` sent through a
`SessionHandle`. `handle.send(request)` answers once the session has acted
(`Compact` once the summary is applied); `handle.run(...)` queues an input and
waits for the reply of the run that consumes it; `handle.post(request)` sends
without waiting. `Agent::prompt` and `Agent::send` do the same with the agent's
own source. Inputs follow the delivery semantics of
[agent-input](../agent-input/README.md); `Stop { run }` pauses the queue until
`Resume`.

Closing every handle drains accepted runnable work and returns the conversation
plus any paused pending inputs. `Shutdown` stops the active run and returns
retained work. Aborting the driver is hard teardown, without a terminal event. A
dropped `send` future does not withdraw a request already sent.

Every accepted tool call gets one correlated result, including denied and
undispatched calls; a stopped in-flight call's result says execution may have
started. No call is retried.

## Events

The event callback receives monotonic `AgentEvent`s. It is synchronous; forward
events rather than doing expensive work in it. `MessageCommitted` carries the
entry and its index, and a replica applies them with `Conversation::apply`.
Conversation appends validate a whole batch: IDs are unique, results match
outstanding calls, and no conversational message precedes the remaining results.
Model and tool streams must end cleanly after exactly one final event before
their content counts.

## Compaction

`model_messages()` is the current model context: the latest summary, then the
entries after it. Compaction never rewrites entries or drops their replay data;
`Compaction` records say which entries a summary replaces for inference.
`summarized_fraction` (default 0.7) is the share of the context the summary
replaces, cut where every tool call has its result. `Compact` requests it (with
`run`, only during that run; a request read after the run ended is refused), and
`trigger_bytes` (serialized context size, off by default) compacts automatically
at settled boundaries.

The summary request is a step's request with the context up to the cut as its
messages, followed by the summary instruction, so the provider serves it from the
prompt cache the last step left. Truncated, empty, tool-calling, stale, or
non-reducing summaries are rejected without changing either view, and failure is
reported as `CompactionFailed`, not a failed run.

## Persistence

Cowork does not use the JSONL log: it saves chats in SQLite (see
`docs/sqlite-storage.md`) and reuses only the `persistence::record` types
(`StoredEntry`, `StoredCompaction`) as the stored and wire form of entries. The
managed `Agent` is the user of `persistence::JsonlLog`, opted into with
`.create_at(path)`, `.resume_from(path)`, or `.open_or_create(path)`.

The log is an optional Tokio-backed writer fed from the event callback; no
operation waits for its writes, and `finish().await` reports background failures.
`updates.jsonl` is the append-only transcript of every recorded event, including
across compaction. `chat_history.jsonl` is a derived model-context view, replaced
on compaction and rebuilt from the transcript on open. `persistence::load`
returns the full conversation and stored updates for replay. A torn last line is
ignored on load and removed on reopen, while complete corrupt lines or unknown
schemas are errors. Only one writer may own a directory.

Queued inputs, pending approvals, and delivered context snapshots are not
persisted. After a crash, `with_interrupted_conversation` settles calls the log
never saw start (`LoadedSession::started_calls`) as not run, since authorization
comes before dispatch; a call that may have started is refused and needs
explicit recovery. The runtime never re-executes a call.
