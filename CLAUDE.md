# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo run -p cowork
cargo test -p cowork [test_name]
cargo clippy && cargo fmt
```

## What this project is

`cowork` is a TUI-first AI assistant where the main agent can delegate to a recursive tree of subagents. The TUI makes this hierarchy visible: the sidebar shows spawned agents live as they run, and their message streams are individually inspectable. Subagents run one at a time, not concurrently — see the Ollama serialization constraint below.

It uses a local Ollama model (`gemma4:12b-it-qat`) through the in-workspace `ollama` provider with reasoning enabled (`think: true`).

## Core architectural tension

LLM/agent streams are async; Ratatui rendering is synchronous. The solution is a single `mpsc` channel that everything funnels into:

- A dedicated blocking thread polls `crossterm` for keyboard input
- Tokio tasks run agent streams and emit `AgentEvent`s
- Both send `RuntimeEvent`s into one channel
- The TUI loop drains that channel, updates `AppState`, then re-renders

`app.rs` is the reducer: it receives events and mutates state. `ui.rs` only reads state — it never mutates. This separation is intentional and should be preserved.

## Dual memory model

`llm::ConversationStore` stores the model's conversation context (what the LLM sees). `AppState` stores the visible message history (what the user sees). These are separate on purpose — the UI history can include things like status messages, structured tool call display, and per-agent views that don't map cleanly to model turns.

## Key traits (the extension seams)

Five traits are the abstraction boundaries; almost everything else is concrete. Know these before changing behavior:

- **`llm::Provider`** — `stream_chat(ChatRequest) -> LlmStream` (a stream of `StreamEvent`s). The LLM backend. Only impl is `ollama::OllamaProvider`; swap this to change models/backends. Lives behind `Arc<dyn Provider>` in `AgentRuntime`.
- **`llm::ConversationMemory`** — `load` / `append` / `replace` a conversation's model context. Only impl is `llm::ConversationStore` (in-memory `HashMap`); `persistence.rs` snapshots it via `export`/`from_conversations`. The async methods return `BoxFuture` because it is used as `Arc<dyn ConversationMemory>` (not dyn-compatible as `async fn`). The intended write-through persistence seam.
- **`llm::Tool`** — `name` / `description` / `parameters_schema` / `call(Value) -> ToolOutput`. Every agent capability. Impls: the `agent_tools::fs` tools (`ReadFile`, `WriteFile`, `EditFile`, `ListDirectory`, `ReadPdf`) and the app-coupled recursive `SubagentTool`. Held by name in `ToolRegistry` (a `BTreeMap`, so the model sees a stable tool order).
- **`agent::EventSink`** — `emit(AgentEvent) -> BoxFuture`. How a run streams progress out without knowing about the TUI. Impls: `ChannelEventSink`/`AgentEventSink` (forward to the `mpsc` channel as `RuntimeEvent`s) and `NoopEventSink` (tests).
- **`agent::ToolPermissionPolicy`** — `decide(&ToolCall) -> ToolPermission` (`Allow` / `Deny{reason}`), consulted once per call before execution. Impls: `AllowAll` (default) and the app's `UiPermissionPolicy` (applies the `AgentProfile`, then routes to the permission UI). A denied call is fed back to the model as an error tool result, not a hard failure.

## Agent hierarchy and event routing

Agents form a tree: one main agent at depth 0, with each `subagent` call spawning a child one level deeper. The recursive `SubagentTool` in `runtime.rs` carries an event sink, parent runtime key, depth, and shared `ConversationStore` — this is how nested agents emit events that surface in the TUI. A child can itself delegate until `MAX_AGENT_DEPTH`, where agents become leaves with no `subagent` tool. Each spawned agent gets a runtime key used to route its events to the correct node in the tree.

## Design decisions already made

- **TUI-only**: no CLI fallback mode. Direct `println!` anywhere corrupts the alternate screen.
- **Subagents/tool calls run sequentially, never concurrently**: the only provider is local Ollama, which serves one prompt at a time. Because the `subagent` tool runs a nested agent turn, parallel tool execution would issue overlapping Ollama requests that thrash its shared KV cache and serialize behind its global lock anyway — slower, not faster. The sequential `for` loop in `agent::AgentRuntime::execute_tools` is load-bearing; do **not** switch it to `join_all`/concurrent execution (nor lift the per-app submit gate to per-thread) without an explicit decision that accounts for the single-instance Ollama backend. The same constraint is why submission is gated per app, not per thread.
- **Mid-run messages are queued, not rejected**: a message typed while an agent runs (the main agent or a selected subagent) is enqueued on that agent's `AgentControl.queue` and drained by the agent loop at the next turn boundary — never mid-stream, so an in-flight provider stream or tool call is not interrupted. There is no separate "subagent guidance" concept: it is the same outgoing-message queue for every agent, keyed by `AgentAddr` in `AppState::agent_controls` (which also carries the run's cancel token). The queued message is **not** shown in the transcript when typed; the agent loop emits `AgentStreamEvent::UserMessageInjected` → `AgentNodeEvent::UserMessage` when it actually injects it, and the reducer appends the visible bubble then, so it lands between turns rather than splitting a streaming reasoning/assistant block. Delivery is best-effort: a message sent after the run ended is dropped.
- **Reasoning shown inline but collapsed**: visible for agent debugging, not noisy by default.
- **One thread for now**: the data model (`threads: Vec<ThreadState>`) is built for multiple threads, but only one is used currently.
- **Persistence is a single JSON session snapshot** (`persistence.rs`): the visible thread tree *and* every conversation's `ConversationStore` context are written together to `session.json` under the platform data dir (`$COWORK_DATA_DIR` overrides). Neither store is a superset of the other — the tree topology lives only in `AppState`, the resumable model turns only in `ConversationStore` — so both are persisted. Writes are atomic (temp+rename), debounced onto the 250ms tick, and skipped while a run is in flight so a half-finished turn is never saved; a corrupt/old-version file is ignored, not clobbered. On load, `AppState::restored` settles any interrupted run (main→Idle, subagent→Cancelled, in-flight tool calls→Failed). Model-context write-through (crash durability mid-run) and de-duplicating message text by re-deriving display messages from `ChatMessage`s are deferred follow-ups; see `docs/tui-design.md` Decision 6.

## Open areas

`docs/tui-design.md` documents the full design rationale including unresolved decisions (cancellation, mouse support, persistence, multi-thread UX). Read it before making architectural changes.
