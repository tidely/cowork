# Roadmap

Planned work that is deliberately out of scope for now. `TODO.md` holds the
owner's own feature notes.

## Tool permissions

- **Tools that always run in `Read` mode.** Let chosen tools (such as
  `run_command`) run without asking while the project is read-only, as they
  did before commands ran on the host. mxc-sdk enforces read-only folders, so
  this is a policy choice rather than a missing capability.
- **Allow-all override.** A per-thread switch letting every tool call run
  without asking, for people who accept the risk.
- **Permission page.** One place to review and change what the agent may do:
  the per-mode tool rules above, the allow-all override, and network access.
  Network is denied for now. On macOS Seatbelt can only allow or deny all
  outbound traffic, not filter by host.

## Commands

- **Cancel Command button.** A button on a running `run_command` card that
  ends just that command, without stopping the whole run. Today a command runs
  until it exits or the run is stopped, with no time limit.

## Provider-independent agent runtime

Build the replacement for the Rig integration around the new `language-model`,
`language-models`, provider, and `tool-runtime` crates. These are responsibility
boundaries, not a requirement to create a separate crate for every item. Existing
Cowork behavior should inform the implementation; some of these mechanisms
already exist in the app but have not moved into the new stack.

The [agent-input layer](crates/agent-input/README.md) covers source-independent
queueing, steering, interjection, and interrupt follow-ups. It is integrated into
the new [agent-runtime](crates/agent-runtime/README.md) session driver. The runtime
is exercised by `language-models/examples/agent_session.rs`; Cowork still uses
the Rig integration. Input queues own pending delivery; the runtime owns running
work, conversation history, and execution-policy coordination.

The managed `Agent` API now owns driver/log tasks and input-bound completion
receipts. Its builder defaults to denied tools, bounded runs, protected queueing,
and manual compaction, with explicit policy and storage opt-ins. The hardcoded
session example uses this API; custom hosts retain the lower-level driver and
handle for typed input sources, controls, and event observation.

### First: execution ownership and observation

- [x] **Session and run lifecycle.** Introduce an `AgentSession` owning execution
      phases: input preparation, sampling, tool execution, approval waits, stopping,
      and completion. Scope cancellation and cleanup to run identities. Settle races
      between cancellation and normal completion with exactly one terminal outcome.
      Integrate `agent-input` admission and delivery at safe conversation boundaries.
      Implemented in `agent-runtime`, with command handling during model startup,
      streaming, approval waits, and tool execution. Stop/interrupt settle outstanding
      calls; normal completion rechecks input admission before reporting its outcome.
- [x] **Authoritative conversation and agent events.** Own ordered commitment of
      accepted model replies, correlated tool results, steering, and compaction.
      Expose a run event stream covering provisional previews, committed messages,
      tool lifecycle, usage, and terminal outcomes. Use it for UI observation and
      collaborator replay. The full-history owner, atomic batch validation, context
      compaction transitions, indexed replay, and ordered runtime events are
      implemented. Optional versioned JSONL logging retains transcript events while
      maintaining a separate replaceable model-context file. Writes are asynchronous;
      agent operations do not require durable disk acknowledgement. Cowork UI/network
      integration, queued-input persistence, and recovery of interrupted runs remain
      application work; unresolved tool calls are never automatically re-executed.
- [x] **Tool execution policy.** Wrap `ToolDispatch` with approval and grant
      handling, concurrency limits, conflicting-operation serialization, cooperative
      cancellation, and result settlement. `ToolCapabilities` describes execution
      properties; the executor applies policy. Give every accepted call a defined
      outcome, including denial or cancellation, and preserve valid tool correlation
      before reusing conversation history.
      The initial `ToolExecutor` uses an explicit host policy, supports pending
      approval and per-call grant extensions, and serializes full calls across
      sessions sharing an executor. It signals cooperative cancellation and records
      uncertainty for stopped in-flight calls. Parallel/resource-specific scheduling
      and optional timeout enforcement are later extensions.

### Next: resilient progression and context

- [ ] **Completion acceptance and recovery.** Separate transport completion from
      the decision to execute tools, continue inference, or finish a run. Define
      bounded policies for truncation, refusal, malformed calls, context overflow,
      transient failures, rate-limit waits, and retry exhaustion. Keep model-request
      retries separate from retries of tools that may already have had side effects.
- [ ] **Request assembly and context maintenance.** Assemble a stable prompt/tool
      prefix, changing environment context, selected history, and token reservations.
      Support compaction through an explicit conversation transition that preserves
      concurrent additions, tool correlation, and provider-native replay data. Reuse
      lessons from Cowork's existing request prefix and compaction implementation.
      Manual and automatic compaction now run at safe boundaries and preserve full
      transcript/UI records. The initial trigger measures serialized context bytes;
      token reservations, model-aware thresholds, and richer context selection remain.

### Extend as callers appear

- [ ] **Typed extension points and completion gates.** Support context contributors,
      lifecycle observers, and gates that can request continued work before a run
      finishes. Distinguish observation from permission to change execution. Define
      hook ordering, outputs, cancellation behavior, and continuation budgets; keep
      loop control with the session owner. Queued inputs and pulled context
      contributions serve different purposes.
- [ ] **Background work supervision.** Own background commands and child-agent
      identities, status, cancellation scopes, completion delivery, and cleanup.
      Feed completion notifications through `agent-input`; the inbox does not own
      producer execution. Add this boundary when background execution or subagents
      become supported.
- [x] **Agent definition and construction.** Separate reusable agent configuration
      (prompt, tool selection, permissions, and execution policies) from a built
      session's live state and services. `AgentDefinition` now holds identity, prompt,
      tool selection, permission defaults, and generation controls. `AgentBuilder`
      binds runtime services and explicit per-instance overrides; the managed
      `Agent` owns execution. Reusing a definition never shares conversation state.
