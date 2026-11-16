# Repository Guidelines

Cowork is a GPUI desktop app for collaborating on LLM agent threads (Ollama, Anthropic, OpenAI) with peer-to-peer sharing over iroh. Rust 2024 Cargo workspace.

Every module starts with a `//!` doc saying what it owns; read it before changing the module. Design and behavior live in `docs/`; read the relevant section rather than the whole file.

## Where to read more

| Topic | Read |
|---|---|
| Collaborative drafts, presence, submission, sharing, protocol, saved threads, host validation | `docs/collaboration.md` (keep its "Status" section current) |
| Agent sessions, providers, tools, compaction, how Cowork uses the runtime | `docs/agent-architecture.md`, `crates/agent/README.md` |
| Command scopes, approval, file tools, `request_permissions` | `docs/permissions.md`; sandbox settings in `crates/sandbox/src/settings.rs`, path mapping in `crates/sandbox/src/paths.rs` |
| Sidebar projects | `docs/projects.md` |
| Scheduled tasks | `docs/schedule.md` |
| Timeline virtualization and performance | `docs/timeline-performance.md` |
| Saved chats in SQLite, lazy loading, unloading, palette search | `docs/sqlite-storage.md` |

## Project structure

- `crates/cowork` — the app, a library with a thin binary (`src/main.rs` calls `cowork::run`) so examples can reach its components. `lib.rs` holds the `Cowork` view, startup, actions and shortcuts; other `impl Cowork` blocks live in the module for their area.
  - State: `thread.rs` (+ `thread/`), `thread_draft.rs`, `timeline.rs`, `timeline_projection.rs`, `transcript.rs`, `models.rs`, `providers.rs`.
  - Sharing: `protocol.rs`, `sharing.rs`, `participant.rs`, `profile.rs`, `peer_access.rs`.
  - Agent runs: `submission.rs`, `generation.rs`, `tool_approval.rs`, `prompt.rs`; prompts in `prompts/`.
  - UI: `composer*.rs`, `draft_editing.rs`, `timeline_*.rs`, `turn_navigation.rs`, `sidebar.rs`, `settings.rs`, `provider_setup.rs`, `welcome.rs`, …
  - Persistence: everything is in one SQLite database, `~/.cowork/cowork.db`, through `chat_db.rs` (chats; schema in `migrations/`) and `chat_db/app_state.rs` (projects, settings, deleted chats' statistics, scheduled tasks). What uses that state: `projects.rs`, `saved_settings.rs`, `statistics.rs`, `scheduler.rs`.
  - Lazy chats: `chat_store.rs` (each chat unloaded, loading, or loaded), `chat_meta.rs`, `unload.rs`.
  - Dev previews: `component_preview.rs`, `share_preview.rs`, and `examples/`.
- `crates/draft` — the collaborative draft as a Yrs CRDT plus `verify_change`. No GPUI or networking dependencies; keep it that way.
- `crates/agent` — the agent stack, re-exporting `agent-runtime`, `language-model` (`agent::model`), `tool-runtime` (`agent::tool`), and the provider clients in `crates/providers/*`. Cowork depends only on `agent`.
- `crates/tools` — `respond_to_comment` and `calculate`.
- `crates/sandbox` — `run_command`, `request_permissions`, and the file tools, contained by the OS sandbox through mxc-sdk.
- `TODO.md` — the owner's notes; don't edit unless asked.

## Commands

- `cargo run -p cowork` — run the app (agent runs need a provider, e.g. a local Ollama).
- `cargo run -p cowork --example component_preview` — components in states the app can't easily reach; also `compaction_preview`, `turn_navigation_preview`, `share_preview`.
- `cargo test --workspace`; narrower: `cargo test -p draft`, `cargo test -p cowork <name>`. `cargo test -p sandbox` runs real contained commands.
- `cargo fmt --all` (tree is fmt-clean) and `cargo clippy --workspace --all-targets` (has some existing warnings; add none).

There is no CI: run fmt, clippy, and the tests before finishing. Use `uv` for Python scripts.

## Conventions

- Dependencies are declared once in the root `[workspace.dependencies]` and used as `foo.workspace = true`; every crate has `[lints] workspace = true` and inherits `edition` and `rust-version`.
- Inside `cowork`, use `pub(crate)`; only `run`, `component_preview`, and `share_preview` are public.
- Doc comments explain intent, invariants, and why (see `protocol.rs`, `draft/src/lib.rs`).
- User-facing text says "chats", not threads, in sentence case (the macOS menu bar uses title case).
- Text offsets in the draft API and comment targets are UTF-8 byte offsets on char boundaries.

## Architecture constraints

- The host is authoritative: all traffic goes through it, and its own actions use the same code paths as a collaborator's. Local threads use the same draft, just without peers.
- Yrs transactions are synchronous, live in the GPUI `Thread` entity, and never cross an `await`. Only encoded bytes go to Tokio (`tokio_handle.spawn`); results return via `cx.spawn`/`spawn_in`.
- Updates merged with `Draft::apply_update` are never re-emitted as local updates.
- Draft invariants are in `docs/collaboration.md` "Validation" and `draft/src/validate.rs`; a layout change updates both.
- Tool definitions and the system prompt never vary within a session, keeping the prompt prefix cacheable; what changes goes in the `environment` context section.
- API keys stay on the host, in the OS keychain, and never travel to peers.

## Making changes

- **Protocol**: bump `PROTOCOL_VERSION` (`protocol.rs`) on any change to wire messages, model identifier semantics, or the agent records that travel. Never change the encoding of `CollaboratorMessage::Join` or `HostMessage::Rejected` (`version_handshake_encoding_is_stable` checks it). No other backward compatibility is needed.
- **Saved state** (chats, projects, settings, statistics, schedules) is in SQLite, independent of the protocol: change its schema with a new migration in `crates/cowork/migrations/`, never by editing an applied one.
- **GPUI pins**: `gpui`/`gpui_platform` match the snapshot gpui-kit (`gpui-component`) pins; upgrade them together.
- Update the relevant `docs/` file when behavior it describes changes.

## Testing

- Unit tests sit next to the code in `#[cfg(test)] mod tests`; `draft`'s larger suite is `crates/draft/src/tests.rs`, using its replica helpers.
- In `cowork`, UI and entity tests use `#[gpui::test]`, async protocol tests `#[tokio::test]`; no test needs Ollama or the network. Whole-app tests are in `src/tests/`, one file per area, with helpers in `tests/mod.rs` and `test_support.rs` (`ScriptedHost` scripts a host's run events).
- Test each behavior once, at the lowest level that observes it; prefer extending an existing test. App tests cover wiring, one case each.
- Don't test constants, labels, defaults, or library guarantees, and don't pin cosmetic layout; layout tests are for real bugs (overlap, clipping, jumping).
- Add a regression test for subtle bugs, especially `draft` convergence. New wire messages go in `protocol.rs`'s existing round-trip test.
