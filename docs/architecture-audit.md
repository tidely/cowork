# Architecture audit

A snapshot of how the workspace is put together, and the cleanups that are
worth doing. It describes the code as it is. The last section is a sequence of
reviewable changes; nothing in "Tradeoffs to reconsider" is proposed as work
to start without a decision.

The agent stack (`language-model`, the provider crates, `tool-runtime`,
`agent-input`, `agent-runtime`) is already close to the design in
`docs/agent-architecture.md`: one owner for a conversation, typed errors at
the library boundary, and phases instead of flags. The cost that does not pay
for itself sits in the app view, in a few duplicated edges, and in library
surface Cowork links but does not use.

## Architecture map

### Crates

| Crate | Owns | Must not own |
| --- | --- | --- |
| `draft` | One Yrs document: ordered prompt blocks and comments, local-update capture, `verify_change` | GPUI, networking, the agent |
| `language-model` | In-memory messages (`latest`, no Serde), frozen `v1` envelopes, `LanguageModelProvider` | HTTP, GPUI, a loop |
| `provider-sse` | Byte framing of `data:` lines | HTTP, model types |
| `providers/*/types` | One protocol's wire structs and the conversion to `latest` | Sockets |
| `providers/*/client` | One connection: discovery, `stream`, status and `Retry-After` mapping | Conversation state |
| `tool-runtime` | `Tool`, prepare-then-execute, `ToolRegistry<C, A>` | Who may run a call |
| `agent-input` | `Inbox`: queue, steer, interject, interrupt | Rendering, persistence |
| `agent-runtime` | `AgentSession`: one conversation, one run, context sections, tool execution, optional JSONL log | GPUI, sandbox policy |
| `agent` | Re-exports of the stack plus `Ollama`, `OpenAiCompatible`, `Anthropic` | New behavior |
| `sandbox` | mxc command sandbox, path checks, file tools, per-thread project and run grant | The model loop |
| `tools` | `calculate`, comment replies, Firecrawl tools | Sandbox policy |
| `firecrawl` | HTTP client for a local Firecrawl | Tools, GPUI |
| `cowork` | The GPUI app: window, chats, sharing, SQLite, scheduler, host of `AgentSession` | A second copy of draft or session rules |

`cowork` is a library with a thin `main` (`run` in `crates/cowork/src/lib.rs`).
Examples reach the same components. Inside `cowork`, visibility is `pub(crate)`
except `run`, `component_preview`, and `share_preview`.

### Dependency graph

Arrows point at a crate the dependent compiles against.

```text
cowork ──► agent ──► agent-runtime ──► agent-input
   │         │            │
   │         │            ├──► language-model ◄── providers/*/types
   │         │            └──► tool-runtime ──► language-model
   │         ├──► ollama, openai-compatible, anthropic
   │         └──► reqwest
   ├──► draft ──► yrs
   ├──► sandbox ──► agent, mxc-sdk
   ├──► tools ──► agent, firecrawl
   ├──► gpui, gpui-component, sqlx, iroh, …
   └──► firecrawl
```

`sandbox` and `tools` use `agent` only for `agent::tool` and `agent::model`
(`crates/sandbox/Cargo.toml`, `crates/tools/Cargo.toml`). Those paths are
re-exports of `tool-runtime` and `language-model`
(`crates/agent/src/lib.rs`). Depending on the facade also links every provider
client into those crates.

Ollama frames NDJSON itself (`crates/providers/ollama/client/src/streaming.rs`).
Anthropic and OpenAI-compatible share `provider-sse`. That split matches the
wires. The HTTP helpers do not: `error_chain` and `retry_after` are the same
function in all three clients (`crates/providers/anthropic/client/src/lib.rs:195`
and `:211`, `crates/providers/openai-compatible/client/src/lib.rs:205` and
`:221`, `crates/providers/ollama/client/src/lib.rs:214` and `:231`).

### GPUI: who owns state

`Cowork` (`crates/cowork/src/lib.rs:144`) is the window view. It implements
`Render` and holds both chrome and domain state: the draft being composed
before a chat exists, providers, the SQLite handle, the scheduler, every
thread's `ActiveGeneration` and `ThreadAgent`, and the timeline caches.

Entities (GPUI models, not views):

| Entity | Role |
| --- | --- |
| `ThreadStore` (`crates/cowork/src/chat_store.rs:184`) | Sidebar identity. Each `Chat` is `Unloaded`, `Loading { _load }`, or `Loaded(Entity<Thread>)` (`chat_store.rs:29`). The load task lives in the state, so dropping the chat cancels it. |
| `Thread` (`crates/cowork/src/thread.rs:126`) | One chat: timeline, `Transcript`, Yrs `ThreadDraft`, sharing, `active_run`. No `Render`. |
| `ModelPickerState`, `JoinDialog`, settings editors | Widgets the window subscribes to. Subscriptions are fields on `Cowork` (`_model_picker_subscription`, `_thread_store_subscription`, `profile_name_subscription`). |

Timeline rows are elements, built in prepaint from a borrowed `Thread`, not
entities (`lib.rs:381`, `docs/timeline-performance.md`). Layout and selection
are `Rc<RefCell<_>>` (`lib.rs:155`) because prepaint mutates them while `Cowork`
is borrowed. That is the usual GPUI escape, and it stays on the UI thread.
`ChatDb` tracks in-flight saves the same way (`crates/cowork/src/chat_db.rs:112`).

`Cowork` keeps a `WeakEntity<Self>` (`lib.rs:195`) for tooltips built outside
`render`. It does not store `Entity<Self>`.

Cross-thread state is `parking_lot` (clippy denies `std::sync::Mutex`,
`clippy.toml`) or an atomic:

| State | Why a lock |
| --- | --- |
| `Draft` local updates (`crates/draft/src/lib.rs:91`) | Yrs observer vs `take_local_update` |
| `Sandboxes` projects and run grants (`crates/sandbox/src/lib.rs:128`) | UI sets them; Tokio runs commands. The module says the lock is never held across `.await`. |
| `CoworkHost::unattended` (`crates/cowork/src/tool_approval.rs:137`) | Session driver and the UI both touch it |
| `WebAccess` (`lib.rs:201`) | Settings vs tool calls |
| JSONL error slot (`crates/agent-runtime/src/agent.rs:262`) | Logger task vs `close` |

### Async, streaming, and tool calls

One Tokio runtime is created before the window and stored in a process-local
`OnceLock` (`lib.rs:113`, `lib.rs:842`). `ChatDb::open` uses `block_on` on that
runtime (`crates/cowork/src/chat_db.rs:231`) and returns before
`gpui_platform::application().run` (`lib.rs:856`). Other `block_on` calls are
in tests. Command and file work uses `spawn_blocking`
(`crates/sandbox/src/lib.rs:300`, `crates/sandbox/src/files.rs:59`).

A run:

1. The UI submits through `Cowork`, which keeps its own pending list on
   `Thread` and posts one `RequestKind::Input` at a time
   (`crates/cowork/src/generation.rs:147`, `:543`).
2. `thread_agent` (`generation.rs:572`) starts `AgentSession::run` on Tokio
   (`generation.rs:228`) or reuses the thread's session. Events come back on an
   unbounded channel to a `cx.spawn` task (`generation.rs:630`).
3. The session actor applies control requests, asks the inbox for a safe
   point, diffs context sections, commits, and calls
   `LanguageModelProvider::stream` (`crates/agent-runtime/src/session.rs`).
   `Phase` (`session.rs:39`) is the run's state.
4. Provider tasks parse SSE or NDJSON into `StreamEvent`s. The session emits
   `Preview`, then one `MessageCommitted`.
5. Each tool call is prepared (decode arguments, `Tool::access`) and then
   `CoworkHost::authorize` (`tool_approval.rs:259`) returns `Allow`, `Deny`, or
   `Ask`. `Ask` becomes `ApprovalRequested`. The UI answers with
   `ResolveApproval` (`generation.rs:325`). Execution streams `ToolProgress`
   and exactly one finish. Results commit in call order.
6. `Cowork::agent_event` (`generation.rs:681`) projects that onto
   `protocol::RunEvent`s. The host folds them with `Thread::apply`.
   Collaborators receive the same messages over iroh and fold them with
   `try_apply` (`crates/cowork/src/sharing.rs:908`).

Yrs transactions stay on the GPUI thread. Only encoded update bytes are sent
to Tokio. `Draft::apply_update` is not re-emitted as a local update
(`crates/draft/src/lib.rs:76`).

SQLite writes are a Tokio task fed by an unbounded channel
(`chat_db.rs:254`). The UI does not wait for them. Sharing uses
`async_channel` for control and bulk attachment bytes (`protocol.rs:617`).

### Errors

| Boundary | Type | Where it surfaces |
| --- | --- | --- |
| Provider HTTP and streams | `language_model::Error` (`crates/language-model/src/lib.rs:416`) | `RunError::Model` (`session.rs:49`) |
| Session requests | `RequestError`, `PromptError` (`crates/agent-runtime/src/request.rs:114`) | `SessionHandle` replies |
| Session build | `SessionError` (`session.rs:78`), a newtype over `String` | `thread_agent`'s `anyhow` |
| Inbox | `StateError` (`crates/agent-input/src/lib.rs:167`) | `RequestError` |
| Tools | `ToolError` (`crates/tool-runtime/src/error.rs:22`) | Model-facing tool result |
| Commands and files | `SandboxError` (`sandbox/src/lib.rs:75`), plus `Result<_, String>` in `grant.rs:32`, `permissions.rs:48`, `environment.rs:79` | `ToolError` |
| Draft | `anyhow::Result` (`draft/src/lib.rs:49`, `validate.rs`) | UI and protocol checks |
| Host admission | `Result<(), String>` (`crates/agent-runtime/src/host.rs:81`) | `RequestError::Rejected` |
| Protocol and SQLite | `anyhow` in `cowork`; `Mismatch` is a private `thiserror` (`chat_db.rs:204`) | Logs, failed runs, disconnect |
| Firecrawl | `firecrawl::Error` | Tool error when web tools run |

`cowork`'s `run` returns `anyhow::Result` (`lib.rs:819`). That is the right
edge. Libraries that already use `thiserror`: `language-model`, `agent-input`,
`agent-runtime`, `tool-runtime`, `firecrawl`.

A panic inside `session.run` drops the event channel. The UI task treats that
as the session ending and fails the open run
(`generation.rs:649`, "The agent stopped unexpectedly."). A panic on the GPUI
thread does not get that recovery. `Thread::apply` is one of those:
`try_apply(...).expect("a valid thread event")` (`thread.rs:1573`).
Collaborators call `try_apply` and drop the connection (`sharing.rs:908`).

## Ranked findings

Severity is about how much the current shape costs a later change, not about
known user-facing bugs. Line numbers are the sites to read.

### 1. The window view owns the domain

`Cowork` (`lib.rs:144`–`254`) is the session host, the database client, the
scheduler, the provider registry, and the timeline. Every event of the active
chat calls `thread_updated`, which notifies that one view
(`generation.rs:1000`). GPUI coalesces notifications, and
`docs/timeline-performance.md` keeps row construction to the viewport. The
remaining cost is structural: a change to sharing, tools, or storage edits the
same type the frame renders, and almost every regression test builds a
`Cowork`.

**Fix.** A model entity for "this launch's chats, agents, and store", with
`Cowork` rendering it. Notify the timeline from generation events without
waking settings and the sidebar.

**Blast radius.** Most of `crates/cowork/src`. **Effort.** L.

### 2. A run is stored in two places

`Thread::active_run` (`thread.rs:152`) answers `is_generating`
(`thread.rs:209`). `Cowork::active_generations` (`lib.rs:202`) holds the
session handle, cancellation flag, approval map, and clock.
`Cowork::agents` (`lib.rs:206`) holds the session for the next run. The
scheduler refuses to start when `is_generating` is true (`scheduler.rs:197`),
which reads the thread, not the map the stop button uses.

The two are updated from the same events, and tests cover each side. Nothing
asserts they match after stop, failure, and a dropped session task.

**Fix.** One struct owns "this thread is running". The other side stores an id
or reads it.

**Blast radius.** `generation.rs`, `thread.rs`, `scheduler.rs`, timeline
header. **Effort.** M.

### 3. Host-applied events panic the UI thread

`Thread::apply` (`thread.rs:1573`) expects `try_apply` to succeed. The host
calls `apply` for events it just built. `try_apply` is the checked function,
and tests already use it for illegal sequences (`thread/tests.rs:470`). An
event that fails validation aborts the process. The collaborator path logs
and closes the chat (`sharing.rs:908`).

**Fix.** On the host, log the error and fail the run the way
`agent_session_ended` does (`generation.rs:671`). Do not `expect`.

**Blast radius.** `thread.rs` and the host emit sites. The success path stays.
The failure path stops crashing. **Effort.** S. This is a behavior change on
that failure path, so it is listed under decisions below rather than done here.

### 4. Library errors are untyped at two boundaries that callers branch on

`draft` returns `anyhow` (`draft/src/lib.rs:49`). Callers cannot match
"malformed update" against "edit rejected by `verify_change`" without reading
the string.

`SandboxError` (`sandbox/src/lib.rs:75`) implements `Error` by hand and still
carries `String` payloads. Path and grant resolution return `Result<_, String>`
(`grant.rs:32`, `permissions.rs:48`). `AgentHost::admit` does the same
(`host.rs:81`). `SessionError` (`session.rs:78`) is a `String` newtype.

`cowork` using `anyhow`, and the provider clients returning
`language_model::Error`, already match the rule.

**Fix.** `thiserror` enums at those boundaries. Keep `anyhow` in `run`, the
SQLite jobs, and protocol I/O.

**Blast radius.** `draft` callers, sandbox tools, `admit` impls. **Effort.** M
for draft plus sandbox, S for `admit` alone.

### 5. `sandbox` and `tools` depend on the provider facade

`agent` re-exports the runtime and every provider
(`crates/agent/src/lib.rs:24`). `sandbox` and `tools` only need `Tool`,
`ToolError`, and the model content types. A change to the Anthropic client
rebuilds the sandbox tests.

**Fix.** Depend on `tool-runtime` and `language-model`. Leave `cowork` on
`agent`.

**Blast radius.** The two `Cargo.toml`s and their `use` lines. **Effort.** S.

### 6. The same HTTP helpers are copied three times

`retry_after` and `error_chain` match in the three clients (sites in the map
above). Each client has its own tests, so a `Retry-After` fix in one does not
fail the others.

**Fix.** One function next to code that already depends on `reqwest` and
`httpdate`. Not `provider-sse`: its module comment keeps it free of HTTP
(`crates/provider-sse/src/lib.rs:4`). Not `language-model`: that crate has no
HTTP types on purpose (`language-model` README).

**Blast radius.** Three client crates. **Effort.** S, after choosing a home.

### 7. Thread ids inside the sandbox are strings

`Sandboxes` maps `String` to projects and grants (`sandbox/src/lib.rs:128`).
Cowork inserts `thread_id.to_string()` (`generation.rs:604`). Tests use
`"thread"` (`sandbox/src/tool.rs:175`). A mismatched key silently uses the
default project (`sandbox/src/lib.rs:160`).

**Fix.** Key the maps on `Uuid`, or a newtype the tests can construct.

**Blast radius.** `sandbox` public methods and Cowork's call sites.
**Effort.** M.

### 8. Approval audience is a `bool` in a map

`ActiveGeneration.approvals` is `HashMap<ToolCallId, bool>`
(`generation.rs:273`). The bool means "host only", documented on the field and
read in `approval_is_host_only` (`generation.rs:319`). `decide` discards the
flag (`generation.rs:326`). `false` is also what a missing call becomes
(`unwrap_or(false)`), so "not waiting" and "any admin may answer" are the same
bit at the read site. The write site (`generation.rs:950`) always inserts, so
today the missing-key case is "not waiting" and `decide` checks presence
first. The next caller of `approval_is_host_only` can get that wrong.

**Fix.** `enum ApprovalAudience { HostOnly, Admins }` and `Option` only for
"not waiting".

**Blast radius.** `generation.rs`, `tool_approval.rs:349`. **Effort.** S.

### 9. Streaming clones the partial reply

`ReplyPreview::partial` clones every part into a new `Message`
(`transcript.rs:291`). The timeline calls it while applying previews
(`transcript.rs:517`, `:570`, `:622`). `docs/timeline-performance.md` already
names this, next to reparsing the last Markdown block, and says it has not
been measured as the bottleneck. It is still the obvious per-token copy.

`thread_agent` also clones the selected model twice
(`generation.rs:579` and `:583`): once to own it, again to use as the map key.

`Range<usize>` is `Copy`, and several sites call `.clone()` on it
(`transcript.rs:140`, `transcript.rs:820`, `timeline_view.rs:1332` and
`:1349`). Those copies are cheap. They are noise in the hot render path.

**Fix.** Build the agent key once. Pass ranges by copy. Leave `partial` until
someone profiles a long reply; replacing it changes how preview state is
borrowed.

**Blast radius.** `generation.rs`, `transcript.rs`, `timeline_view.rs` for the
cheap copies. **Effort.** S for those, M for `partial`.

### 10. Panics whose invariant is only "the other half of this function"

These are safe if the surrounding lines stay put, and they crash the UI
thread or the provider task if a later edit breaks the pairing.

| Site | Assumption |
| --- | --- |
| `scheduler.rs:231` | `next_queued` (`scheduler.rs:89`) only returns an index whose `queued()` is `Some`, and `ready` (`scheduler.rs:190`) does not clear it. True today. |
| `anthropic` `stream.rs:167` and `:257` | A `ToolUse` block always stored `Some(call_id)` at `:100`. A tool delta with no id is already rejected at `:101`. |
| `openai-compatible` `conversion.rs:334` | `kind == "text"` parts are only pushed from `Content::Text` with `text: Some` (`conversion.rs:222`). |
| `thread.rs:266` | `take_active_preview` checks the id, then `unwrap`s the same `Option`. |
| `generation.rs:122` | Tool names registered in that function are unique. Startup only. |
| `session.rs` (for example `:675`, `:705`, `:732`) | Inbox and `active` agree. A panic here is recovered as a failed run, because it is on the Tokio task. |

Not worth wrapping the ones that are local and tested. The host `apply`
expect in finding 3 is the one that is not local.

### 11. Little dead code; the extra surface is live and unused by the app

There is no `todo!`, `unimplemented!`, or `allow(dead_code)` in the workspace.
Private unused items would fail a warnings-denied build. JSONL persistence,
dynamic tools, and `AllowForRun` are called by `agent-runtime` tests and by
the managed `Agent`. They are not dead. They are the tradeoffs below.

`EnvironmentSection::render` ignores its previous snapshot
(`generation.rs:142`) and always returns the current note. That is not a
bug: `ContextSection::update` only calls `render` when the snapshots differ
(`crates/agent-runtime/src/context.rs:74`).

### 12. Tests cover the behavior that has already gone wrong, and miss the dual run state

Present and worth keeping: draft convergence (`crates/draft/src/tests.rs`),
inbox delivery (`agent-input`), session phases, compaction, and JSONL
(`agent-runtime`), protocol round-trip and the stable `Join` / `Rejected`
bytes (`protocol.rs`), collaborator disconnect on a bad host
(`tests/collaboration.rs`), permissions and tool approval, SQLite reopen
(`chat_db/tests.rs`), sandbox containment (`sandbox/tests/containment.rs`),
timeline culling (`tests/timeline_virtualization.rs`), and a scripted
`AgentSession` that asks, refuses, and compacts
(`generation.rs:1132`). Firecrawl's error mapping has a loopback test
(`crates/firecrawl/tests/local_api.rs`). Live Ollama tests are `#[ignore]`.

Gaps that would miss a real regression:

- Nothing checks `active_run` against `active_generations` after stop, a
  failed start (`generation.rs:561`), and `agent_session_ended`.
- `Thread::apply`'s panic is untested because the tests call `try_apply`.
- The three `retry_after` copies do not share a test vector.

Screenshot and timeline bench tests are ignored on purpose
(`docs/timeline-performance.md`).

### Categories in good shape

- **`Rc<RefCell>` / `Arc<Mutex>`.** The locks in the map are cross-thread or
  the prepaint escape. None is a single-threaded mutex hiding in a library
  that could take `&mut self`.
- **Enums for phases.** `Phase`, `ThreadSharing`, `ChatState`, `MainStage`,
  `CallAccess`, and `RunOutcome` already make the illegal combination
  unrepresentable. The bool in finding 8 is the one that does not.
- **GPUI tasks and subscriptions.** Refresh tasks are stored and replaced
  (`ollama_watch`, `caret_label_refresh`, `working_refresh`, the idle sweep),
  so the previous task is dropped and cancelled. Subscriptions that must
  outlive the frame are fields. `theme.rs` detaches a process-lifetime
  theme observer. No foreground `block_on` on the UI path after startup.
- **Dependencies that look heavy and are used.** `chrono` exists for `rrule`'s
  types (`Cargo.toml` workspace comment, `scheduled_task.rs:141`). `pdf_oxide`,
  `syntect`, `similar`, `nucleo-matcher`, `postcard`, and `image` are the PDF
  tool, highlighting, the diff view, the palette, and the protocol.

## Tradeoffs to reconsider

These are working designs. Each costs more structure than Cowork uses. Do not
start them as drive-by cleanups.

### JSONL log beside SQLite

**Current.** `agent-runtime` can append `updates.jsonl` and rebuild a model
view (`crates/agent-runtime/src/persistence/mod.rs`, wired in `agent.rs`).
Cowork stores chats in SQLite and reuses only `StoredEntry` and
`StoredCompaction` (`docs/sqlite-storage.md`, `docs/agent-architecture.md`).

**Cost.** Two recovery stories. The JSONL log does not persist queued inputs,
approvals, or delivered context snapshots. SQLite persists a different cut
(turn boundaries, not a live run). Anyone changing an entry touches both the
record types and the SQL.

**Alternative.** Keep the record types. Leave the log as a feature of the
managed `Agent` only, or delete it if that `Agent` is not a product.

**Lost.** `Agent::create_at` / `resume_from`, and the torn-line tests in
`agent-runtime/src/tests/persistence.rs`.

### Cowork's queue beside the inbox

**Current.** `Thread::pending` is what collaborators see and cancel.
`agent_input::Inbox` also queues. Steers use `Fallback::Discard`
(`generation.rs:157`) so a steer that misses the run cannot start one behind
the UI. The UI then starts that submission itself.

**Cost.** Cancel, reorder, and "did this commit?" each have a Cowork path and
an inbox path (`InputsDiscarded` handling at `generation.rs:694`). The
follow-up in `docs/agent-architecture.md` already says to move Cowork's
pending list into the inbox.

**Alternative.** Render the inbox. One admission path.

**Lost.** A queue the UI can show before a session exists, unless the inbox
gains a way to hold work with no run. Discard-on-miss is what makes the
current split safe; deleting it without moving the list would let the session
start runs the UI does not know about.

### Runtime `AllowForRun` beside sandbox grants

**Current.** `ResolveApproval` with `AllowForRun` pushes a `Grant` on the
run (`session.rs:676`). Cowork sends `AllowOnce` or `Deny`
(`generation.rs:332`) and records command grants in `Sandboxes::run_grants`
(`sandbox/src/lib.rs:129`). `docs/agent-architecture.md` says the runtime
grants are not used.

**Cost.** Two meanings of "for the rest of this run", and tests for a
decision the app never produces (`agent-runtime/src/tests.rs`).

**Alternative.** Either Cowork answers `AllowForRun` for tools that do not
need a sandbox grant, or the session stops storing grants and every tool
keeps its own.

**Lost.** A host that wants run-scoped allow without a side table. The
managed `Agent` example uses `AllowForRun` today (`crates/agent/examples/agent.rs`).

### Dynamic tools in the session driver

**Current.** `search_tools` and `call_tool` are installed unless the host
calls `without_dynamic_tools` (`session.rs:303`). Cowork does
(`generation.rs:214`). The section, the snapshot, and the request variants
remain in the driver.

**Cost.** The prompt-prefix rules, `call_tool` rewriting, and the
`dynamic_tools` context section (`context.rs:157`) are maintained for a
feature the app turns off. They are not a second execution path for Cowork's
tools, which is what the design was avoiding.

**Alternative.** Build the session without that code when the flag is off,
or drop the feature until a host needs it.

**Lost.** A portable tool catalog (MCP-style) without a protocol change.
`ROADMAP.md` does not schedule that.

### One `Cowork` view instead of a model and a view

**Current.** Findings 1 and 2. The view is the app, which makes examples and
tests able to construct a window and drive it.

**Cost.** Render, persistence, networking, and the agent loop share a borrow.
`cx.notify` on generation redraws the window because there is nothing smaller
to notify.

**Alternative.** `Entity` for the launch state. `Cowork` renders pieces of it.

**Lost.** The ability to read "what the app is doing" in one struct, and the
current test style that mutates `Cowork` fields directly.

### Three provider clients, two of which share only a framer

**Current.** Separate type crates and client crates per protocol. Stream
state machines differ and should stay separate. The duplicated piece is
finding 6.

**Cost.** Parallel client layouts, and copy-paste for headers and error
chains. Ollama's NDJSON decoder is necessarily its own.

**Alternative.** Share the two helpers. Leave the stream machines.

**Lost.** Nothing about protocol behavior, once the helpers have one home.
Merging the stream machines would lose the per-protocol rejection tests.

### Stringly Yrs document

**Current.** Item kinds and fields are document keys (`draft/src/lib.rs:64`).
`validate.rs` is what makes a bad map a rejected update instead of a crash.
Attachment records and comment targets are a single `Any` so concurrent
writers cannot mix fields.

**Cost.** Strings instead of enums at the storage layer, and a validator that
must move with every schema change (`docs/collaboration.md`).

**Alternative.** A different CRDT, or a single editor with operational
transforms.

**Lost.** Concurrent editing with Yrs' existing merge, which is the product.
This cost is doing its job. Listed so it is not "fixed" by hiding the strings
in getters while the document stays the same.

## Sequenced plan

Each unit is one PR, behavior-preserving unless the note says the failure
path changes. Verify with `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace --all-targets`,
and `cargo test --workspace`.

### Quick wins

No product decision. Safe to merge in any order.

1. **Redundant clones.** Own the `(ModelRef, ModelInfo)` key once in
   `thread_agent` (`generation.rs:579`). Stop calling `Clone` on `Range<usize>`
   in `transcript.rs` (`:140`, `:820`) and `timeline_view.rs` (`:1332`,
   `:1349`). Check the session test `a_thread_session_asks_refuses_and_compacts`
   and a timeline virtualization test.
2. **Facade dependency.** Point `sandbox` and `tools` at `tool-runtime` and
   `language-model`. `cargo test -p sandbox` and `cargo test -p tools`.
3. **Approval audience.** Replace the bool in `approvals` with a two-variant
   enum (finding 8). Existing approval tests should not change their
   expectations.
4. **Clippy, as it comes up.** Fix warnings in the crate that emits them.
   Do not weaken `clippy.toml`.

### Needs a decision

5. **Host `apply` on failure** (finding 3). Confirm the UI: log and fail the
   run, or keep the crash as a tripwire. S, and it changes the failure path.
6. **Where `retry_after` lives** (finding 6). A tiny shared module that
   depends on `reqwest` and `httpdate`, or leave the copies. Do not put it in
   `provider-sse` or `language-model`.
7. **Sandbox thread key** (finding 7) and **typed draft/sandbox errors**
   (finding 4). API changes. Do draft's error type on its own; it has its
   own tests.
8. **One owner for a run** (finding 2). After that, the test that the two
   sides cannot disagree.
9. **Split `Cowork`** (finding 1). Only after 8, or the split preserves the
   duplication.
10. **Shed or keep** JSONL, dynamic tools, and runtime `AllowForRun`
    (tradeoffs). Each is its own PR, and only if the managed `Agent` and the
    examples are in or out together.

Not scheduled: replacing `ReplyPreview::partial`, a step cap
(`max_steps: NonZeroUsize::MAX` at `generation.rs:203`), or merging provider
stream parsers. The step cap is a product limit, and `ROADMAP.md` already
tracks completion policy.
