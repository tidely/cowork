# TUI Design Draft

This document captures the proposed terminal UI design for `cowork`, the engineering changes needed to support it, and open design decisions.

## Target UX

`cowork` should become an interactive terminal application with:

- A left sidebar containing conversation threads.
- Threads expandable inline to show the main agent and subagents beneath that thread.
- A main conversation pane showing messages for the selected agent/subagent.
- Message blocks/cards for system, user, assistant, tool-call, tool-result, reasoning/status events.
- A fixed prompt input bar at the bottom of the main pane.

Approximate layout:

```text
┌──────────────────────────────┬──────────────────────────────────────────────────────────────┐
│ Threads                      │ Conversation: Thread 1 / Main Agent                         │
│                              │                                                              │
│ ▾ Thread 1                   │ ┌─ System ─────────────────────────────────────────────────┐ │
│   ● Main Agent               │ │ You are the top-level user-facing assistant...           │ │
│   ├─ Subagent: research      │ └──────────────────────────────────────────────────────────┘ │
│   ├─ Subagent: tests         │                                                              │
│   └─ Subagent: docs          │ ┌─ User ───────────────────────────────────────────────────┐ │
│                              │ │ User prompt...                                           │ │
│ ▸ Thread 2                   │ └──────────────────────────────────────────────────────────┘ │
│                              │                                                              │
│ ▸ Thread 3                   │ ┌─ Assistant ──────────────────────────────────────────────┐ │
│                              │ │ Assistant response...                                    │ │
│                              │ └──────────────────────────────────────────────────────────┘ │
│                              │                                                              │
│                              │ ┌──────────────────────────────────────────────────────────┐ │
│                              │ │ > Type a prompt...                                      │ │
│                              │ └──────────────────────────────────────────────────────────┘ │
└──────────────────────────────┴──────────────────────────────────────────────────────────────┘
```

## Current architecture summary

`cowork` is now TUI-only. The runtime path is in-workspace rather than backed by an external agent framework:

- `main.rs` starts the TUI; there is no line-oriented CLI fallback.
- `tui.rs` owns terminal setup, the single runtime event channel, keyboard input forwarding, and prompt task spawning.
- `runtime.rs` builds `agent::AgentRuntime` with `ollama::OllamaProvider`, `llm::ConversationStore`, `agent_tools` filesystem/PDF tools, and the app-coupled recursive `subagent` tool.
- `app.rs` is the reducer/state owner. It mutates `AppState` in response to structured `AgentEvent`s and terminal actions.
- `ui.rs` renders `AppState` only; it does not mutate state.
- No runtime path writes directly to stdout, because that would corrupt the alternate screen.

The core architectural move is: convert streamed model/tool/subagent events into structured app events, store visible UI history in `AppState`, keep model context in `llm::ConversationStore`, and render from state repeatedly.

## Code organization

```text
crates/cowork/src/
├── main.rs            # starts the TUI
├── config.rs          # model, prompt, and retry constants
├── events.rs          # runtime event sink helper
├── permissions.rs     # agent profile and tool permission mode
├── runtime.rs         # provider/agent/tool runtime orchestration
├── app.rs             # app state, reducer/update logic, thread/agent/message models
├── tui.rs             # terminal setup, event loop, input channel, prompt task spawning
└── ui.rs              # Ratatui widgets/layout rendering
```

Supporting crates:

```text
crates/llm/          # Chat types, provider trait, tool trait/registry, ConversationStore
crates/ollama/       # Direct Ollama /api/chat streaming provider
crates/agent/        # Generic multi-turn agent loop and permission hook
crates/agent-tools/  # Reusable filesystem/PDF tools
```

## Key dependencies

```toml
ratatui = "0.29"
crossterm = "0.28"
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["json", "stream"] }
pdf-extract = "0.10"
```

## Data model draft

The TUI has an app-owned model separate from `llm::ConversationStore`.

```rust
struct AppState {
    threads: Vec<ThreadState>,
    selected: Selection,
    input: InputState,
    ui: UiState,
    running: bool,
}

struct ThreadState {
    id: ThreadId,
    title: String,
    expanded: bool,
    main_agent: AgentNode,
    subagents: Vec<AgentNode>,
}

struct AgentNode {
    id: AgentRunId,
    label: String,
    depth: AgentDepth,
    status: AgentStatus,
    messages: Vec<Message>,
}

enum Selection {
    ThreadMain(ThreadId),
    Agent(ThreadId, AgentRunId),
}

enum Message {
    System { content: String },
    User { content: String },
    Assistant { content: String },
    Reasoning { content: String },
    ToolCall { name: String, arguments: serde_json::Value },
    ToolResult { content: String },
    Error { content: String },
    Status { content: String },
}
```

In practice, assistant text and reasoning will arrive as deltas. The app should append deltas to the current in-progress message when possible instead of creating one message per token/chunk.

## Event flow draft

```text
Terminal keyboard/mouse events
        │
        ▼
TUI event loop ────────────────┐
        │                      │
        │ AppCommand           │ AppEvent
        ▼                      ▼
Agent task(s) ───────────▶ mpsc channel ───────────▶ AppState update/reducer
        │                                             │
        └──── llm/agent stream events                 ▼
                                                  Ratatui render
```

The TUI should own a `tokio::sync::mpsc` channel. Background async tasks send structured `AppEvent`s into the UI loop:

```rust
enum AppEvent {
    Input(KeyEvent),
    Mouse(MouseEvent),
    Tick,
    AgentStarted { thread_id: ThreadId, agent_id: AgentRunId },
    MessageStarted { thread_id: ThreadId, agent_id: AgentRunId, role: MessageRole },
    MessageDelta { thread_id: ThreadId, agent_id: AgentRunId, message_id: MessageId, delta: String },
    ToolCall { thread_id: ThreadId, agent_id: AgentRunId, name: String, args: serde_json::Value },
    ToolResult { thread_id: ThreadId, agent_id: AgentRunId, content: String },
    AgentFinished { thread_id: ThreadId, agent_id: AgentRunId },
    AgentErrored { thread_id: ThreadId, agent_id: AgentRunId, error: String },
}
```

## Main implementation complications

### 1. Async agent streaming vs synchronous terminal rendering

`ratatui` rendering itself is synchronous, while LLM streaming, agent loops, and tools are async.

Recommended pattern:

- Keep `#[tokio::main]`.
- Enter terminal raw mode in main.
- Run a synchronous-ish TUI loop inside async main.
- Use background `tokio::spawn` tasks for agent streams.
- Use `tokio::sync::mpsc` to move events from agent tasks into the UI loop.
- Poll terminal input using either:
  - blocking input in a dedicated thread, or
  - crossterm event polling on the main loop with short timeouts.

Open choice below.

### 2. No direct stdout printing

Runtime code must emit structured events, not `print!` / `println!`. The renderer decides how to display each event. Direct stdout printing corrupts the alternate screen.

### 3. Subagent tree visibility

Subagents are invoked from tool calls. The `subagent` tool in `runtime.rs` carries UI context and emits nested lifecycle/message events addressed by runtime key.

To show subagents in the sidebar, the app needs stable IDs and lifecycle events:

- subagent started
- subagent label/task
- subagent message deltas
- subagent finished/errored

This is implemented with custom tool structs carrying event sender, current `ThreadId`, parent runtime key, depth, and shared model memory.

### 4. Conversation memory vs visible message history

`llm::ConversationStore` stores model context, but it is not enough for UI rendering.

The app maintains its own visible history in `AppState`.

Potential issue: what exactly should count as a visible message?

Options:

- Show only system/user/assistant final text by default, with tool/reasoning collapsible or styled as status lines.
- Show everything: reasoning, tool calls, tool results, usage, subagent activity.
- Show assistant/user in main conversation, and expose tool/reasoning in an inspect/details panel later.

### 5. Reasoning display

The current app displays model reasoning because `think: true` is enabled and streamed reasoning is handled.

Possible TUI treatments:

- Show reasoning as its own dimmed `Thinking` block.
- Hide reasoning by default but show a live status line like `thinking...`.
- Make reasoning collapsible per assistant turn.

There is no universally best answer here because it depends on whether this tool is for debugging agent behavior or normal assistant use.

### 6. Input while agent is running

Options:

- Disable prompt submission while the selected thread has an active agent run.
- Allow typing but queue the prompt until the current run finishes.
- Allow concurrent prompts/runs in the same thread.

For a first implementation, disabling submission while a run is active is simplest and safest.

### 7. Cancellation

Long-running model/tool calls need a UX escape hatch.

Options:

- `Esc` cancels the active run.
- `Ctrl+C` cancels active run first; if no active run, exits app.
- No cancellation in first pass.

True cancellation requires plumbing cancellation into spawned tasks, likely using `tokio_util::sync::CancellationToken` or task abort handles.

### 8. Mouse support

The requested design includes clicking threads.

Implementing mouse support in `crossterm` is feasible, but it adds coordinate bookkeeping:

- Need to know row ranges for each sidebar item.
- Need to map click coordinates to thread/agent selection.
- Need to redraw hover/selection states if desired.

Keyboard navigation should probably exist too:

- Up/down: move sidebar selection or scroll conversation.
- Enter: select/expand.
- Tab: switch focus between sidebar and input/conversation.
- Ctrl+C: exit/cancel.

### 9. Scrolling

Main conversation can exceed terminal height.

Needed state:

- Scroll offset per selected agent or per thread.
- Auto-scroll-to-bottom while new output arrives unless user manually scrolls up.

First pass can implement simple global conversation scroll.

### 10. Thread naming

Currently there is one hardcoded conversation ID: `agent-thread-0`.

The sidebar implies multiple threads.

Choices:

- First pass: one thread only, architecture supports more later.
- Add new-thread support immediately, e.g. `Ctrl+N`.
- Auto-title threads from first user prompt.

### 11. Persistence

Currently all memory is in-process.

Choices:

- No persistence in first pass.
- Persist visible conversations as JSON under a config/data directory.
- Persist both visible history and enough agent context to resume conversations.

Full resume requires serializing both visible history and enough `llm::ChatMessage` context to reconstruct model state.

### 12. Terminal lifecycle/error recovery

The app must restore terminal state on panic/error:

- Leave alternate screen.
- Disable raw mode.
- Disable mouse capture.
- Show cursor.

This should be wrapped in a terminal guard type with `Drop`.

## Proposed first milestone

A practical first TUI milestone:

1. Add `ratatui` and `crossterm` dependencies.
2. Introduce `AppState` with one thread and main agent node.
3. Render the target layout with:
   - sidebar
   - message cards
   - input bar
4. Support keyboard input and prompt submission.
5. Stream top-level agent responses into `AppState` via channel.
6. Disable direct stdout printing.
7. Display tool calls/results as message blocks in the main agent conversation.
8. Show subagent tool calls initially as placeholder sidebar nodes if nested event plumbing is too invasive.

Second milestone:

1. Refactor `Subagent` / `WorkerAgent` tools to emit nested events.
2. Display real subagent message streams in selectable sidebar nodes.
3. Add mouse click selection/expansion.
4. Add cancellation and better scrolling.

## Open design decisions

### Decision 1: TUI-only or keep CLI mode?

Options:

A. Replace the current CLI entirely with the TUI.

- Simpler implementation.
- No need to maintain two output paths.
- Best if TUI is now the primary app.

B. Keep both modes, e.g. `cowork` starts TUI and `cowork --plain` starts old line CLI.

- More flexible.
- Requires preserving/refactoring stdout stream handling.
- Slightly more code and testing burden.

Recommendation: A for now, unless you specifically want scriptable/plain terminal usage.

### Decision 2: How visible should reasoning be?

Options:

A. Show reasoning blocks inline.

- Best for debugging and understanding agents.
- Can be noisy.

B. Hide reasoning and show only status like `thinking...`.

- Cleaner normal chat UX.
- Loses useful agent-debugging detail.

C. Show reasoning inline but visually dim/collapsible later.

- Good compromise.
- Collapsing can be added after the first pass.

Recommendation: C, with inline dimmed reasoning in first pass.

### Decision 3: Can users submit while an agent is running?

Options:

A. Disable submit while active.

- Simplest and safest.
- Avoids hard-to-reason-about concurrent turns.

B. Allow typing but queue submission.

- Better UX.
- Requires queue state and clear status.

C. Allow concurrent runs.

- Powerful but can conflict with shared conversation memory.

Recommendation: A for first pass.

### Decision 4: Thread support scope

Options:

A. First pass has exactly one thread, but sidebar is built as if multiple threads exist.

- Fastest path to working TUI.
- Matches current hardcoded conversation ID.

B. Add multiple threads immediately.

- Better matches final UI.
- Requires per-thread memory/conversation IDs and navigation.

Recommendation: A first, then B.

### Decision 5: Mouse support timing

Options:

A. Implement keyboard navigation first, mouse clicks second.

- Faster and less fragile.
- Still usable.

B. Implement mouse selection immediately.

- Matches the requested click behavior earlier.
- Requires sidebar hit-testing from the start.

Recommendation: A for first milestone, B in second milestone unless clicking is mandatory for your first review.

### Decision 6: Persistence

Options:

A. In-memory only.

- Matches current app.
- Lowest complexity.

B. Persist visible message history only.

- Useful for reviewing previous sessions.
- Does not necessarily restore model context.

C. Persist full resumable conversations.

- Most useful long term.
- Requires serializing both visible history and model conversation state.

Recommendation: A for first pass.

### Decision 7: Terminal input strategy

Options:

A. Poll `crossterm::event::poll` from the main async loop with short timeouts.

- Simple and common for Ratatui apps.
- Keeps most UI logic in one task.
- Slightly less elegant because it mixes blocking-ish polling with async channel receive.

B. Spawn a dedicated blocking thread for terminal input and forward input events into the async channel.

- Cleaner separation between terminal input and async agent streams.
- Avoids blocking the async runtime on terminal event reads.
- Adds one more moving piece and cross-thread shutdown coordination.

Recommendation: B if we want a robust architecture from the start; A if we want the fastest minimal TUI.

## Current preferred path

Unless directed otherwise, the safest implementation path is:

1. Replace the current line CLI with a TUI.
2. Use `ratatui` + `crossterm`.
3. Use `tokio::mpsc` for app events from async agent streams.
4. Keep one thread initially, but model state as multi-thread-capable.
5. Show reasoning inline but dimmed.
6. Disable prompt submission during an active run.
7. Implement keyboard navigation first.
8. Add real nested subagent event plumbing after the top-level TUI is stable.
