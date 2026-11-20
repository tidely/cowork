# Repository Guidelines

Cowork is a GPUI desktop app for collaborating on LLM agent threads (Ollama, Anthropic, OpenAI) with peer-to-peer sharing over iroh.

Behavior and design reasons live in `docs/`; read the relevant section rather than the whole file.

## Where to read more

| Topic                                                                          | Read                                                                                                                       |
| ------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| Collaborative drafts, presence, submission, sharing, protocol, host validation | `docs/collaboration.md`                                                                                                    |
| Agent sessions, providers, tools, compaction, how Cowork uses the runtime      | `docs/agent-architecture.md`, `crates/agent/README.md`                                                                     |
| Command scopes, approval, file tools, `request_permissions`                    | `docs/permissions.md`; sandbox settings in `crates/sandbox/src/settings.rs`, path mapping in `crates/sandbox/src/paths.rs` |
| Timeline virtualization, its tradeoffs, how to measure                         | `docs/timeline-performance.md`                                                                                             |
| Saved chats in SQLite, lazy loading, unloading, palette search                 | `docs/sqlite-storage.md`                                                                                                   |

## Project structure

The main crates; the rest of `crates/` is supporting libraries.

- `crates/cowork` — the app, a library with a thin binary (`src/main.rs` calls `cowork::run`) so examples can reach its components. `lib.rs` holds the `Cowork` view, startup, actions and shortcuts; other `impl Cowork` blocks live in the module for their area.
- `crates/draft` — the collaborative draft as a Yrs CRDT plus `verify_change`. No GPUI or networking dependencies; keep it that way.
- `crates/agent` — the agent stack, re-exporting `agent-runtime`, `language-model` (`agent::model`), `tool-runtime` (`agent::tool`), and the provider clients in `crates/providers/*`. Cowork depends only on `agent`.
- `crates/sandbox` — the command and file tools, contained by the OS sandbox through mxc-sdk.
- `ROADMAP.md` — planned work that is not built yet. `TODO.md` — the owner's notes; don't edit unless asked.

## Commands

Use `uv` for Python commands.

- `cargo run -p cowork` — run the app (agent runs need a provider, e.g. a local Ollama).
- `cargo run -p cowork --example <name>` — previews of components in states the app can't easily reach; see `crates/cowork/examples`.
- `cargo test --workspace`; narrower: `cargo test -p draft`, `cargo test -p cowork <name>`. `cargo test -p sandbox` runs real contained commands.
- `cargo test -p cowork ui_screenshots -- --ignored --nocapture --test-threads=1` renders UI states to PNGs offscreen; see the `ui-screenshots` skill (`.agents/skills/ui-screenshots/SKILL.md`) before looking at UI any other way.

There is no CI. Before finishing, run `cargo fmt --all` (the tree is fmt-clean), `cargo clippy --workspace --all-targets` (it has some existing warnings; add none), and the tests.

## Conventions

- Dependencies are declared once in the root `[workspace.dependencies]` and used as `foo.workspace = true`; every crate has `[lints] workspace = true` and inherits `edition` and `rust-version`.
- Inside `cowork`, use `pub(crate)`; only `run`, `component_preview`, and `share_preview` are public.
- User-facing text says "chats", not threads, in sentence case (the macOS menu bar uses title case).
- Text offsets in the draft API and comment targets are UTF-8 byte offsets on char boundaries.

## Docs and comments

- Docs describe how things work today and why; update the relevant `docs/` file when that changes. Progress logs, completed checklists, measurements from finished investigations, and "used to" history belong in git; what isn't built yet goes in the doc's short "Not yet" list or `ROADMAP.md`.
- Don't copy what the code already documents (fields, message lists, schema); point to the file. Each topic has one home; link to it from elsewhere.
- Comments say what the code can't: why, invariants, ordering and concurrency requirements (see `protocol.rs`, `draft/src/lib.rs`). Don't restate a name or signature, and don't narrate the change you are making.
- Doc comments on `JsonSchema` tool-argument structs become the tool descriptions the model reads; edit them as prompt text.

## Architecture constraints

- The host is authoritative: all traffic goes through it, and its own actions use the same code paths as a collaborator's. Local threads use the same draft, just without peers.
- Yrs transactions are synchronous, live in the GPUI `Thread` entity, and never cross an `await`. Only encoded bytes go to Tokio (`tokio_handle.spawn`); results return via `cx.spawn`/`spawn_in`.
- Updates merged with `Draft::apply_update` are never re-emitted as local updates.
- Draft invariants are in `docs/collaboration.md` "Validation" and `draft/src/validate.rs`; a layout change updates both.
- Tool definitions and the system prompt never vary within a session, keeping the prompt prefix cacheable; what changes goes in the `environment` context section.
- API keys stay on the host, in the OS keychain, and never travel to peers.

## Making changes

- **Protocol**: bump `PROTOCOL_VERSION` (`protocol.rs`) on any change to wire messages, model identifier semantics, or the agent records that travel, and add new wire messages to `protocol.rs`'s existing round-trip test. Never change the encoding of `CollaboratorMessage::Join` or `HostMessage::Rejected` (`version_handshake_encoding_is_stable` checks it). No other backward compatibility is needed.
- **Saved state** (chats, projects, settings, statistics, schedules) is in SQLite, independent of the protocol: change its schema with a new migration in `crates/cowork/migrations/`, never by editing an applied one, comments included (sqlx checks each applied file's checksum).
- **GPUI pins**: `gpui`/`gpui_platform` match the snapshot gpui-kit (`gpui-component`) pins; upgrade them together.

## Testing

- `draft`'s larger suite is `crates/draft/src/tests.rs`, using its replica helpers.
- In `cowork`, UI and entity tests use `#[gpui::test]`, async protocol tests `#[tokio::test]`; no test needs Ollama or the network. Whole-app tests are in `src/tests/`, one file per area, with helpers in `tests/mod.rs` and `test_support.rs` (`ScriptedHost` scripts a host's run events).
- Before adding a test, name the observable behavior, the plausible regression, and why existing tests would miss it; if they wouldn't, add nothing. Prefer a case in an existing test over a new test. Test at the lowest level that observes the behavior; app tests cover wiring, one case each. Derive expected values independently, not by recomputing them the way the code does.
- Add a regression test for subtle bugs, especially `draft` convergence, and check that it fails for the expected reason before the fix when practical.
- Don't test constants, labels, defaults, or library guarantees, and don't pin cosmetic layout; layout tests are for real bugs (overlap, clipping, jumping).
