# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                          # debug build
cargo build --release                # optimized build
cargo run -p cowork                  # run the TUI app
cargo test -p cowork                 # run all tests
cargo test -p cowork <test_name>     # run a single test (e.g. test_typing_and_backspace_edit_the_input)
cargo test -p cowork -- --nocapture  # show println output during tests
cargo clippy                         # lint
cargo fmt                            # format
```

## Architecture

`cowork` is an async Rust TUI application that runs a multi-agent AI assistant with hierarchical agent delegation. The user interacts with a main agent in a terminal; that agent can spawn subagents and workers to parallelize tasks.

### Module relationships

```
main.rs       — initializes InMemoryConversationMemory, starts TUI
tui.rs        — terminal setup (alternate screen, raw mode), event loop, TerminalGuard
app.rs        — AppState reducer: receives RuntimeEvents, updates state; all unit tests live here
ui.rs         — Ratatui render functions (sidebar, main pane, input bar); reads AppState, no mutations
agent.rs      — Rig client setup, stream pumping, hooks; spawns tokio tasks that emit AgentEvents
tools.rs      — tool implementations: read_file, list_directory, edit_file, subagent, worker_agent
```

### Event-driven data flow

A dedicated input thread polls `crossterm::event::poll` and forwards `KeyEvent`s. Agent tasks run on the Tokio runtime. Both send into a single `mpsc::channel<RuntimeEvent>` that the TUI loop drains:

```
crossterm (input thread) ──┐
                           ├──> mpsc::Sender<RuntimeEvent> ──> tui.rs loop ──> app.rs reducer ──> ui.rs render
tokio tasks (agents)     ──┘
```

`RuntimeEvent` wraps terminal events, a `Tick` for cursor blinking, and `AgentEvent`s. `AgentEvent` variants: `Started`, `Spawned`, `AssistantDelta`, `ReasoningDelta`, `ToolCall`, `ToolResult`, `Status`, `Usage`, `Finished`, `Error`.

### Agent hierarchy and routing

- The main agent is always `AgentId(0)` / `AgentAddr::Main`.
- `subagent` tool spawns first-level task agents; `worker_agent` spawns under a subagent.
- Each spawned agent gets an atomic runtime key (`AgentAddr::Runtime(key)`); events carry this key so `app.rs` can route them to the correct node in the thread tree.
- Parent–child relationships are preserved in `AppState.threads[].subagents` (each subagent has its own `workers` list).

### Focus model

Three focus states cycle with Tab; Esc returns to `Input`:
- **Input** — prompt bar active, Enter submits (disabled while an agent is running)
- **Sidebar** — navigate threads/agents with arrow keys
- **Conversation** — browse messages, toggle collapsibles (reasoning blocks and tool calls) with Space

### Message roles and collapsibles

Roles: `System`, `User`, `Assistant`, `Reasoning`, `ToolCall`, `ToolResult`, `Status`, `Error`.  
`Reasoning` and `ToolCall` messages are collapsible (▾/▸) and hidden by default.

### Rig dependency

`rig-core` and `rig-derive` come from a custom fork (`tidely` branch `feat/streaming-tool-concurrency`) pinned in `Cargo.lock`. This adds streaming concurrent tool support not yet merged upstream. The agent uses an Ollama backend (`gemma4:31b`).

### Retry logic

Transient errors (network, timeout, DNS) retry with backoff: 1 s → 3 s → 10 s → 30 s. Deterministic errors (tool errors, schema errors, max turns) fail immediately.

### Path handling in tools

`tools.rs` resolves `~`, relative, and absolute paths and normalizes separators. It falls back through `HOME` → `USERPROFILE` → `HOMEDRIVE+HOMEPATH` for the home directory.
