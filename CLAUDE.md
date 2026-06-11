# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo run -p cowork
cargo test -p cowork [test_name]
cargo clippy && cargo fmt
```

## What this project is

`cowork` is a TUI-first AI assistant where the main agent can delegate to a recursive tree of subagents in parallel. The TUI makes this hierarchy visible: the sidebar shows spawned agents live as they run, and their message streams are individually inspectable.

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

## Agent hierarchy and event routing

Agents form a tree: one main agent at depth 0, with each `subagent` call spawning a child one level deeper. The recursive `SubagentTool` in `runtime.rs` carries an event sink, parent runtime key, depth, and shared `ConversationStore` — this is how nested agents emit events that surface in the TUI. A child can itself delegate until `MAX_AGENT_DEPTH`, where agents become leaves with no `subagent` tool. Each spawned agent gets a runtime key used to route its events to the correct node in the tree.

## Design decisions already made

- **TUI-only**: no CLI fallback mode. Direct `println!` anywhere corrupts the alternate screen.
- **Submit disabled during active run**: simplest way to avoid concurrent state issues. No queuing.
- **Reasoning shown inline but collapsed**: visible for agent debugging, not noisy by default.
- **One thread for now**: the data model (`threads: Vec<ThreadState>`) is built for multiple threads, but only one is used currently.
- **No persistence**: all state is in-memory. `ConversationStore` and visible UI history are not serialized yet.

## Open areas

`docs/tui-design.md` documents the full design rationale including unresolved decisions (cancellation, mouse support, persistence, multi-thread UX). Read it before making architectural changes.
