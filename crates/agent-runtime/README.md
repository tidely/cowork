# Agent runtime

The provider-independent session driver above `language-model`, `tool-runtime`,
and `agent-input`. This is groundwork alongside the Rig application, which has
not migrated. It owns one conversation and one active run at a time.

## Building an agent

`Agent` owns the driver task, prompt completion tracking, and optional log
writer. Ordinary callers only build, prompt, and close:

```rust,no_run
use agent_runtime::{Agent, AgentDefinition, AgentError};
use language_model::{LanguageModel, LanguageModelProvider};

async fn example(provider: impl LanguageModelProvider + 'static) -> Result<(), AgentError> {
    let definition = AgentDefinition {
        name: "assistant".into(),
        system_prompt: "Answer concisely.".into(),
        ..Default::default()
    };
    let agent = Agent::builder(provider, LanguageModel::new("model"))
        .definition(&definition)
        .build().await?;

    println!("{}", agent.prompt("Hello").await?.text());
    agent.close().await?;
    Ok(())
}
```

`AgentDefinition` is a cloneable blueprint containing identity, system prompt,
tool selection, permission defaults, generation limits, and thinking intent.
Reuse it across agents; each build creates independent conversation state.
The builder binds providers, concrete tools, host approval logic, storage, and
observers. Input and compaction policies also remain build-time settings.
`AgentSession` remains the lower-level execution owner behind the managed `Agent`.

`.definition(&definition)` applies a snapshot; later builder methods override its
settings for that instance. `agent.definition()` exposes the authored snapshot,
not an effective-policy report. `tools: None` exposes all registered tools;
`Some(names)` exposes exactly those names, and a missing registration fails build.
`.tools(names)` overrides this selection. Selection and authorization are separate:
`ToolPermissions::DenyAll` is the default, `AllowListed(names)` grants named tools,
and `AllowAll` explicitly grants all selected tools. Excluded tools cannot execute,
even under an allow-all policy. Tool implementations are supplied at build time
so definitions do not capture runtime resources or another agent's tool state.

Defaults deny every tool, serialize execution, cap each run at eight model turns
and each request at 4096 output tokens, protect queued inputs, and enable only
manual compaction. Registering `.tool(tool)?` does not grant permission:
`.allow_tools(["name"])` grants named tools, `.allow_all_tools()` grants all, and
`.tool_policy(policy)` installs host approval logic. Generation limits, input
policy, compaction policy, and the full request have explicit builder setters.
Conversation validity and tool correlation remain enforced under every policy.

Storage is opt-in: `.create_at(path)` refuses existing logs, `.resume_from(path)`
requires one, and `.open_or_create(path)` explicitly allows reuse. Saved leading
instructions must match configured instructions; `.use_saved_instructions()`
explicitly selects the saved ones instead. Only stored source types need
`Serialize`. `.on_event(callback)` adds observation without owning loop control.

`prompt()` queues a user message and awaits that input's run result, including
when other prompts are queued concurrently. Failed runs pause progression;
prompts submitted while paused return an error until the host resumes through
`handle()`. The handle also exposes steering, interruption, and typed inputs from
other sources. Use `AgentBuilder::new` for a shared provider and custom source
type. `compact()` returns `Unchanged` for short histories or non-reducing summaries;
transport and invalid-summary failures remain errors. `close()` stops execution,
joins the driver and log writer, and reports background errors. Dropping an
agent requests shutdown but cannot wait for those tasks.

## Driving a session directly

Construct `ToolExecutor` with a dispatcher and an explicit host `ToolPolicy`,
then pass it, a provider, a model, and an initial `CompletionRequest` to
`AgentSession::new`. This returns the owner and a cloneable `SessionHandle`.
The request's messages seed history; its other generation controls are retained.
Tool definitions are snapshotted from the executor, and conflicting definitions
are rejected. A nonzero model-turn budget defaults to 64 and is configurable.

Poll `session.run(callback)` while producers use the handle to submit authorized
`Input<Source, Message>` values. Sources are host-defined, cloneable, comparable,
and Send. Queue, Steer, Interject, and Interrupt retain the semantics documented
in [agent-input](../agent-input/README.md). Stop names a run; a late stop cannot
cancel its successor. `producer_closed` withdraws only producer-scoped pending
work. Snapshots expose the current run, phase, pause state, and counts.

Closing every handle drains accepted runnable work and returns the conversation
plus any paused pending inputs. Explicit `shutdown` stops the active run and
returns retained work without starting another run. Join the driver after
shutdown. Aborting/dropping it is hard teardown: cooperative tool cancellation
is signalled, but there can be no terminal event or delivery receipt afterwards.
Unconfirmed admission returns the original input identity to a waiting caller;
it does not prove the input never ran. Dropping a submission future does not
withdraw a command already sent.

The executable `language-models/examples/agent_session.rs` runs an arithmetic
tool through this driver without depending on Rig:

```text
cargo run -p language-models --example agent_session
```

## Conversation and observations

`Conversation` owns accepted messages and validates batch appends atomically:
message/call IDs are unique, results match outstanding calls, and a new
conversational message cannot precede the remaining results. Complete messages,
including native thinking/replay data, are retained without rewriting content.
Starting a session with unresolved historical calls is rejected; crash recovery
must decide their disposition rather than replay their side effects blindly.

The callback receives monotonic `AgentEvent`s for input admission, run phases,
previews, accepted messages, usage, tool progress/results, and terminal outcomes.
It is synchronous: forward to application channels rather than doing expensive
work inside it. Model and tool streams must end cleanly after exactly one final
event before their content becomes authoritative. Missing finals, trailing
events, and trailing errors never turn into accepted partial results.

`MessageCommitted` includes its conversation index and, for submitted input,
source and input identity. A replica seeded with the same initial history can
apply these events with `Conversation::apply`; duplicate indexes and gaps fail.
`Compacted` records apply through `Conversation::apply_compaction`. Callbacks run
after in-memory commitment and cannot veto it. The optional JSONL writer below
records these changes asynchronously; no database transaction or disk-commit
acknowledgement gates agent operations. Cowork's reconnect protocol has not
migrated to these events.

Admission and final completion share the same owner. The driver checks commands
again before finishing a model answer: a late steer either joins that run or
takes its idle fallback. Inputs are committed only after all accepted tool calls
are settled. Stop and interruption produce one terminal event per run; pending
queue entries and undelivered steering remain owned according to their policy.
`InputsDiscarded` carries the full inputs so the host can settle producer
receipts or cancel producer-bound work on shutdown.

## Compaction without transcript deletion

`Conversation::messages()` always returns the complete accepted history.
`model_messages()` returns the current model context: leading system/developer
instructions, the latest summary, and the retained recent tail. Compaction never
rewrites the original messages or drops their native replay data. Separate
`Compaction` records describe the original index range replaced for inference;
they are also UI events, not ordinary user messages in the full transcript.

Configure `.with_compaction(CompactionPolicy { ... })` to enable compaction.
Sizes are the serialized shared context, deliberately not an invented token
estimate. `trigger_bytes` is the context size that triggers automatic
compaction. Token-budget-aware triggers remain future work.

A compaction summarizes the older part of the model context and keeps the newer
part verbatim:

```text
before:  [instructions][previous summary][older messages ... | newer messages]
                                                            ^ cut
after:   [instructions][summary][newer messages]
```

`summarized_fraction` (default 0.7) is the share of the context the summary
replaces, counting the instructions and any previous summary. The cut is the
last point at or before that size where every tool call has its result, or the
first one after it when even the first exchange is larger, so one large exchange
still compacts. The last message always stays verbatim. Each compaction frees
about that share minus the summary's size, and the next one summarizes the
previous summary along with the messages after it.

Automatic compaction runs before sampling at settled conversation boundaries.
`handle.compact().await` requests it explicitly, including while idle. The owner
keeps processing controls while the provider summarizes; admitted steering waits
for the next safe point. A summary covers a fixed snapshot, so later messages
remain in the tail. Automatic attempts do not repeat before the next main model
response merely because steering arrived during summarization.

The summary request is a turn's request (system prompt, tools, thinking,
options, output cap) whose messages are the context up to the cut, followed by
the summary instruction. That is how the last turn's request began, so the
provider serves it from the prompt cache that turn left; nothing in it is
flattened or stripped, as any change there would miss the cache. The model sees
exactly what the summary replaces, so nothing past the cut is summarized twice,
and the instruction tells it the newer messages follow the summary. Summaries
are plain user-role context notes, never privileged instructions. Truncated, empty, tool-calling, stale, or
non-reducing summaries are rejected without changing either view. Failure is
reported as `CompactionFailed`, not a failed agent run. Stop/shutdown cancels
in-flight summarization; completed summaries emit `Compacted`.

## Asynchronous session files

`persistence::JsonlLog` is an optional Tokio-backed writer, fed from the event
callback with `log.record(&event)`. Sources need Serialize only at this logging
boundary. No operation waits for its queued writes. `finish().await` joins the
writer and reports background failures; `flush().await` is an ordered buffered
flush, not an fsync guarantee.

* `updates.jsonl` retains the initial full conversation and every recorded event:
  deltas, committed messages, tool progress, structured outputs, and compactions.
  The v1 record wrapper uses versioned message envelopes and preserves opaque UI
  payloads. It is append-only, including across compaction.
* `chat_history.jsonl` is a derived model-context view. New messages append;
  compaction replaces the file with prefix + summary + tail using a temporary
  file and rename. It is never the only copy of the original transcript.

`persistence::load` returns full history, its compacted context, the initial
conversation, and stored updates. A UI can replay from `initial_conversation`,
apply authoritative records with `StoredEvent::apply`, and render observations
such as previews and tool cards without rebuilding accepted messages from
deltas. `JsonlLog::open` reconstructs the context file from the transcript, so a
crash between writing the two files cannot make the derived file authoritative.
Use `with_conversation(loaded.conversation, loaded.next_sequence)` to continue a
session while preserving compaction and event ordering.

A syntactically torn unterminated last log line can be ignored on load and
removed on reopen; complete corrupt lines or unknown schemas are errors and
remain untouched. Buffered writes can lose their tail on a crash or power loss.
Unresolved historical tool calls require explicit recovery before continuation;
the runtime never re-executes them automatically. Queued unconsumed inputs are
not yet persisted. Only one writer may own a directory at a time.

The session example hardcodes Ollama at `http://localhost:11434`, model
`qwen3.8:27b`, disabled thinking, and storage at `/tmp/cowork-agent-session`.
It explicitly opts into saved-session reuse and allows only its `add` tool.
It requests manual compaction between arithmetic and a follow-up question;
a non-reducing summary leaves the context unchanged. It reads no environment
variables; edit the constants in the example to use different settings.

## Tools and permissions

`ToolPolicy::authorize` receives the run, call, and snapshotted descriptor before
`ToolDispatch::call` can execute. Decisions allow with `TypedExtensions`, deny
with a `ToolError`, or await a host decision. Approval waits keep the command loop
responsive and appear as `AwaitingApproval`. Approved extensions can carry exact
per-call grants; the executor adds `CallControl` after authorization. Read-only
metadata never grants permission. `AllowAll` is an explicit opt-in for trusted
examples, not a default policy.

The initial executor serializes the entire call stream across sessions sharing
that executor. This conservatively respects concurrency limits and prevents
conflicting operations from overlapping. Independent executor instances do not
share a lock. Parallel scheduling and argument-specific resource locks can be
added later. Capability timeouts are suggestions; no timeout or retry is imposed.

Tools may observe `CallControl.cancellation` for cooperative run cancellation
and `yield_wait` for an interjection that should wake a wait without stopping its
producer. The session also drops local pending futures/streams on stop. This
does not guarantee external processes or remote inference have stopped.

Every accepted tool call gets one correlated result, including denied and
undispatched calls. A stopped in-flight call receives a cancellation result
explicitly marking that execution may have started. That restores conversation
correlation; it does not assert rollback or a known external outcome. Tool
progress remains provisional. A malformed result stream yields an error result
instead of retaining its provisional success. No call is automatically retried.

## Initial scope

The driver accepts the supported providers' ordinary completion reasons and
rejects malformed tool arguments or unusable stops before executing any calls.
Failures pause queued work for explicit resumption. This deliberately carries
the minimal example's conservative acceptance policy; configurable recovery,
token-budget context maintenance, hooks, background supervision,
and Cowork integration remain roadmap work. The channels are currently
unbounded; capacity/backpressure must be designed before untrusted remote
producers are exposed directly.
