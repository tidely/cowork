# Agent runtime

The provider-independent session driver above `language-model`, `tool-runtime`,
and `agent-input`. It owns one conversation and one active run at a time. The
Cowork app still uses Rig; this is the stack it will move to. The architecture
behind it, and why state lives where it does, is described in
[docs/agent-architecture.md](../../docs/agent-architecture.md).

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

The state is split by what a change costs:

- **`AgentDefinition`** is what the prompt cache depends on: the model, the
  instructions, the tools offered (by name), thinking, and provider options.
  Plain data, shareable by many conversations. It is never patched:
  `ReplaceDefinition` swaps it whole and starts a new `DefinitionRevision`,
  which each model reply records in its origin.
- **`Configuration`** is everything else: the host's permissions, sampling
  controls (`max_output_tokens`, `temperature`, ...), steps per run, and the
  compaction policy. `Configure` requests change one field at a time, so
  independent sources never undo each other's changes. Changes apply to calls
  not yet authorized and to the next request.

Each model request is assembled from the definition, the configuration, the
tools, and the conversation, then handed to the provider; it is never stored.
The conversation holds only user, assistant, and tool messages; instructions
travel in `CompletionRequest::instructions`.

## The host

`AgentHost` is the application side of a session:

- `Source`: who sent a request (a participant, a schedule, a parent agent).
  Recorded on every entry and on the run it starts.
- `Input`: model-visible input kept typed until it is committed, then rendered
  as user content (`RenderInput`), so no source can make its text privileged.
- `Access`: what one tool call needs. Each tool's own `Access` converts into it.
- `Permissions`: the live permission state, part of the configuration.

`admit` sees every request with its source before the session acts on it.
`authorize` decides one call: `Allow`, `Deny`, or `Ask`. It receives the real
tool, the call's access, the source that started the run, the permissions, and
the run's grants. `describe_permissions` puts the policy in words for the model.
Tool contexts come from the host too: a tool whose `Context` is `T` registers
when the host implements `Provides<T>`.

`BasicHost` covers single-user programs: string sources and inputs, tools that
need no context, and `ToolPermissions` by tool name. Its default refuses every
call; `ToolPermissions::allow([...])`, `allow_all()`, and `.ask_otherwise()`
open it up. `Agent::builder` uses it.

## Requests

Everything that changes a session is a `Request { source, kind }`, sent through
a `SessionHandle`:

- Inputs, with the delivery semantics of [agent-input](../agent-input/README.md):
  `Queue`, `Steer`, `Interject`, `Interrupt`, plus editing, removing,
  redirecting, and reordering queued inputs, and `SourceClosed` to withdraw a
  closed source's producer-bound work.
- Run lifecycle: `Stop { run }` (pauses the queue until `Resume`), `Compact`,
  `Shutdown`.
- State: `ReplaceDefinition`, `Configure`, `DynamicTools`.
- Answers: `ResolveApproval { call, decision }`.

`handle.send(request)` answers once the session has acted (`Compact` once the
summary is applied); `handle.run(...)` queues an input and waits for the reply
of the run that consumes it; `handle.post(request)` sends without waiting.
`Agent::prompt` and `Agent::send` do the same with the agent's own source.

Closing every handle drains accepted runnable work and returns the conversation
plus any paused pending inputs. `Shutdown` stops the active run and returns
retained work. Aborting the driver is hard teardown, without a terminal event.
A dropped `send` future does not withdraw a request already sent.

## Tools, approvals, and dynamic tools

The definition's base tools are offered directly in every request. A call is
resolved, its arguments decoded and its access computed, then authorized. On
`Ask` the session emits `ApprovalRequested` and parks the call until a
`ResolveApproval` request answers: `AllowOnce`, `AllowForRun` (adds a run grant
`authorize` sees for later calls), or `Deny`. Stop and shutdown settle parked
calls like any unfinished call, and a late answer finds nothing to resolve. The
tool then runs with exactly the access that was authorized.

Dynamic tools are reached through two built-ins included in every definition by
default: `search_tools` returns matching tools with their parameter schemas,
and `call_tool { name, arguments }` runs one. `DynamicTools` requests add or
remove them and only ever append to the conversation, so the request's `tools`
never change and the prompt cache holds with every provider. `call_tool` is
resolved to the real tool before authorization, so permissions, approvals,
events (`ToolStarted { tool, .. }`), and output all see that tool; it refuses
base tools, which are called directly. A definition without `call_tool` has no
dynamic tools. Each step takes a snapshot of the tools, and the calls in its
reply run against it.

Calls run one at a time. Every accepted call gets one correlated result,
including denied and undispatched calls; a stopped in-flight call's result says
execution may have started. No call is retried.

## Context sections

At each safe point (every call settled, before sampling) the session compares
each `ContextSection`'s snapshot with what the model last received and commits
a note for the sections that changed, each in its own `<section_id>` tags, in
one user entry with `Origin::Context`, before any inputs committed with it.
Notes are committed rather than injected into one request, which would change
the next request's prefix. The built-in sections are `tool_policy` (the host's
`describe_permissions`), `dynamic_tools` (added and removed tools), and `model`
(a replaced definition's model). Hosts add their own with `with_section` or
`AgentBuilder::section`.

What the model was told is conversation state: a compaction that summarizes the
point where a section was delivered makes it render in full again. Delivered
snapshots are not persisted, so a resumed session renders every section again.

## Conversation and events

`Conversation` owns accepted entries, each a message and its `Origin` (an
input and its source, a model reply and its revision, a tool result, or context
notes). Appends validate a whole batch: message and call IDs are unique, results
match outstanding calls, and no conversational message precedes the remaining
results. Complete messages, including native replay data, are kept verbatim.

The event callback receives monotonic `AgentEvent`s: admissions, run phases,
previews, committed entries, usage, approvals, tool progress and results,
state changes, and terminal outcomes. It is synchronous; forward events rather
than doing expensive work in it. `MessageCommitted` carries the entry and its
index; a replica applies them with `Conversation::apply`. Model and tool streams
must end cleanly after exactly one final event before their content counts.

## Compaction without transcript deletion

`model_messages()` is the current model context: the latest summary, then the
entries after it. Compaction never rewrites entries or drops their replay data;
`Compaction` records say which entries a summary replaces for inference.

```text
before:  [previous summary][older entries ... | newer entries]
                                              ^ cut
after:   [summary][newer entries]
```

`summarized_fraction` (default 0.7) is the share of the context the summary
replaces. The cut is the last point at or before that size where every tool call
has its result, or the first after it, so one large exchange still compacts. The
last entry always stays. `Compact` requests it; `trigger_bytes` (serialized
context size, off by default) compacts automatically at settled boundaries.

The summary request is a step's request with the context up to the cut as its
messages, followed by the summary instruction, so the provider serves it from
the prompt cache the last step left. Truncated, empty, tool-calling, stale, or
non-reducing summaries are rejected without changing either view; failure is
reported as `CompactionFailed`, not a failed run. Summaries are user-role
context notes, never privileged instructions.

## Asynchronous session files

`persistence::JsonlLog` is an optional Tokio-backed writer fed from the event
callback. Sources need `Serialize` and `Deserialize` only at this boundary.
No operation waits for its writes; `finish().await` joins the writer and
reports background failures.

* `updates.jsonl` keeps the initial conversation and every recorded event:
  committed entries with their origins, previews, tool progress, structured
  outputs, and compactions. It is append-only, including across compaction.
* `chat_history.jsonl` is a derived model-context view, replaced on compaction
  and rebuilt from the transcript on open.

`persistence::load` returns the full conversation, the initial conversation,
and stored updates for UI replay. A torn unterminated last line is ignored on
load and removed on reopen; complete corrupt lines or unknown schemas are
errors and stay untouched. Queued inputs and pending approvals are not
persisted. After a crash, `with_interrupted_conversation` settles unresolved
calls the log never saw start (`LoadedSession::started_calls`) as not run, since
authorization comes before dispatch; a call that may have started is refused
and needs explicit recovery. The runtime never re-executes a call. The managed
builder does this when it resumes. Only one writer may own a directory.

The managed builder opts in with `.create_at(path)`, `.resume_from(path)`, or
`.open_or_create(path)`.

## Examples

```text
cargo run -p language-models --example prompt   # prompting
cargo run -p language-models --example agent    # host, tools, approvals, dynamic tools
```

Both use Ollama at `http://localhost:11434` with `gemma4:e4b`.
