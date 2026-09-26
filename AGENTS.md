# Repository Guidelines

Cowork is a GPUI desktop app for collaborating on LLM agent threads (Ollama via Rig), with peer-to-peer sharing over iroh. Rust 2024 Cargo workspace.

## Project Structure

- `crates/cowork` — the app binary. `src/main.rs` holds the `Cowork` view struct, app startup, and top-level rendering; the rest of `Cowork`'s methods are `impl Cowork` blocks in the module for their area.
  - State: `thread.rs` (`Thread`, sharing state, applying host events), `thread_draft.rs` (the draft, its editors, and presence), `timeline.rs` (messages and their wire form), `transcript.rs` (the transcript and agent events on the wire, and folding them into a thread), `models.rs` (providers and model catalogs).
  - Sharing: `protocol.rs` (wire messages; postcard, length-delimited frames, control vs. bulk queues), `sharing.rs` (hosting, joining, and handling collaborators' requests), `participant.rs` (`ParticipantId` and derived names/colors), `profile.rs`.
  - Agent runs: `submission.rs` (which submission wins, and the prompt), `generation.rs` (running the agent), `prompt.rs`. The system prompt is `prompts/system.md`, compiled in with `include_str!`.
  - UI: `composer.rs`, `draft_editing.rs`, `composer_attachments.rs`, `timeline_view.rs`, `model_picker.rs`, `top_bar.rs`, `sidebar.rs`, `search_palette.rs`, `profile_page.rs`, `avatars.rs`, `caret.rs`, `highlight.rs`, `assets.rs`.
  - Other: `attachments.rs` (reading and classifying files), `usage.rs` (token usage and the activity chart).
- `crates/draft` — the collaborative draft as a Yrs CRDT document (prompt blocks, comments, attachment records) plus `verify_change` validation. No GPUI or networking dependencies; keep it that way.
- `crates/agent` — minimal multi-turn agent loop on Rig's low-level completion stream (see its `README.md` for why it exists instead of Rig's agent). Its `AgentEvent`s fully describe a run, and `TurnFold` folds them into history; the host forwards them and every participant folds them.
- `crates/tools` — Rig tools exposed to the model (`respond_to_comment`).
- `docs/collaboration.md` — the spec for collaborative drafts, the document layout, protocol, and host validation. Read the relevant section before touching draft sync, presence, submission, or attachment transfer, and update it (including its "Status" section) when behavior changes.
- `TODO.md` — the owner's feature notes; don't edit unless asked.

## Build, Test, and Development Commands

- `cargo run -p cowork` — run the app. Agent runs need a local Ollama server.
- `cargo test --workspace` — all tests.
- `cargo test -p draft` / `cargo test -p cowork <name>` — narrower runs.
- `cargo fmt --all` — default rustfmt settings; the tree is currently fmt-clean.
- `cargo clippy --workspace --all-targets` — the tree has some existing warnings; don't add new ones.

There is no CI; run fmt, clippy, and the tests yourself before finishing.

For Python scripting, use the `uv` package manager.

## Code Style and Conventions

- Dependencies are declared once in the root `Cargo.toml` `[workspace.dependencies]` and used as `foo.workspace = true`. Every crate has `[lints] workspace = true`.
- Visibility inside the `cowork` binary uses `pub(crate)`.
- Doc comments explain intent, invariants, and why (see `protocol.rs`, `draft/src/lib.rs`). Keep that style when adding types and constants.
- Text offsets in the draft API and in comment targets are UTF-8 byte offsets on char boundaries.

## Architecture Constraints

- The host is authoritative. All traffic goes through it; collaborators never talk to each other. The host's own edits, submissions, stops, and model changes go through the same code paths as a collaborator's.
- Local and shared threads use the same Yrs-backed draft. A local thread just has no peers.
- Yrs transactions are synchronous, live in the GPUI `Thread` entity, and must never be held across an `await`. Only encoded bytes cross to the Tokio runtime (`TOKIO_RUNTIME` / `tokio_handle.spawn`). Results come back to GPUI via `cx.spawn`/`spawn_in`.
- Draft invariants (in `docs/collaboration.md` "Validation" and `draft/src/validate.rs`): `creator` and `target` are write-once, attachment records and comment targets are atomic `Any` values, and `order`/`items` change together in one transaction. Changing the document layout means updating `validate.rs` and the spec as well.
- Updates merged with `Draft::apply_update` are never re-emitted as local updates. Don't break that; it prevents echoing other participants' edits.

## Making Changes

- **Protocol**: bump `PROTOCOL_VERSION` in `crates/cowork/src/protocol.rs` on any change to wire messages or to model identifier semantics, and on every Rig upgrade: transcript messages and agent events travel in Rig's serde encoding. Never change the encoding of `CollaboratorMessage::Join` or `HostMessage::Rejected`: keep their variant index, and keep `Join`'s version as its only field. `version_handshake_encoding_is_stable` checks this. Nothing is persisted, so there's no backward compatibility to maintain beyond that.
- **GPUI pins**: `gpui`/`gpui_platform` are pinned `=0.3.6` to match the snapshot the gpui-kit fork (`tidely/gpui-kit`, branch `text-view-source-range-highlights`) uses. Upgrade them together, never one alone.

## Testing

- Unit tests live next to the code in `#[cfg(test)] mod tests`. `draft` keeps its larger suite in `crates/draft/src/tests.rs`, grouped by numbered section comments and using its helpers (`replica_of`, `sync`, and so on) to simulate multiple replicas.
- In `cowork`, UI and entity tests use `#[gpui::test]` (with gpui's `test-support` dev feature), and async protocol tests use `#[tokio::test]`. Tests don't need a running Ollama server or network.
- Tests that drive the whole app (a `Cowork`, threads, or a host and collaborator pair) are in `crates/cowork/src/tests/`, one file per area, with the shared helpers (`test_cowork`, `composer_test_cowork`, `Collaboration`, and so on) in `tests/mod.rs`. Test helpers needed by modules outside `tests/` are in `test_support.rs`.
- Add a regression test for bug fixes where feasible, especially convergence cases in `draft` and round-trip tests in `protocol.rs` for new messages.
