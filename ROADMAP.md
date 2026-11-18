# Roadmap

Planned work that is deliberately out of scope for now. `TODO.md` holds the
owner's own feature notes.

## Tool permissions

- **Tools that always run in `Read` mode.** Let chosen tools run without asking
  while the project is read-only. mxc-sdk enforces read-only folders, so this is
  a policy choice rather than a missing capability.
- **Allow-all override.** A per-thread switch letting every tool call run
  without asking, for people who accept the risk.
- **Permission page.** One place to review and change what the agent may do: the
  per-mode tool rules, the allow-all override, and network access. Network is
  all or nothing per command (the Write + Network scope); on macOS Seatbelt can
  only allow or deny all outbound traffic, not filter by host.

## Forking chats

- **Fork, edit and resend, retry.** A fork is a new chat whose history is
  another's up to a settled turn boundary (after an ended run, or the empty
  root). Storage already holds it (`docs/sqlite-storage.md` "Chats as a tree"):
  a `threads` row whose head is an existing message, with the source's per-chat
  rows (folders, profiles, prompt names) copied. What a fork does with the
  scratch folder and project folders waits for something like worktrees.
- **A `/compact` command.** Its own run in the open chat: an agent message with
  no entries and one compaction at its start, compacting with
  `Compact { run: None }` while the message is open. Needs
  `validate_agent_runs` to accept a run without a prompt that starts where the
  next run does.
- **Stable identities for blocks and comments**, so a collaborator reusing an
  id is a non-issue; until then such a reuse fails that chat's save.
- Two loaded forks each hold a full `Transcript`; sharing the common prefix in
  memory is possible later. A branch switcher inside a chat, and merging
  branches, are not planned: a fork is its own chat.

## Commands

- **Cancel Command button.** A button on a running `run_command` card that ends
  just that command, without stopping the whole run. Today a command ends when
  it exits, when its `timeout_ms` expires, or when the run is stopped.

## Agent runtime

- **Completion acceptance and recovery.** Separate transport completion from the
  decision to execute tools, continue inference, or finish a run. Define bounded
  policies for truncation, refusal, malformed calls, context overflow, transient
  failures, rate-limit waits, and retry exhaustion. Keep model-request retries
  separate from retries of tools that may already have had side effects.
- **Model-aware compaction.** The runtime triggers automatic compaction on
  serialized context bytes only. Token reservations, thresholds from the model's
  context window, and richer context selection remain.
- **Typed extension points and completion gates.** Context contributors,
  lifecycle observers, and gates that can request continued work before a run
  finishes, with defined hook ordering, outputs, cancellation behavior, and
  continuation budgets. Loop control stays with the session owner.
- **Background work supervision.** Own background commands and child-agent
  identities, status, cancellation scopes, completion delivery, and cleanup,
  feeding completion notifications through `agent-input`. Add this when
  background execution or subagents become supported.
