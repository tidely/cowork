# Saving chats in SQLite

Everything Cowork saves lives in one SQLite database, `~/.cowork/cowork.db`,
reached through sqlx (`crates/cowork/src/chat_db.rs`): chats, projects,
settings, what statistics count of deleted chats, and scheduled tasks. The
schema, with a comment on every table and the column encodings, is
`crates/cowork/migrations/`; change it with a new migration, never by editing an
applied one. What a saved chat holds, from the collaboration side, is in
`docs/collaboration.md` "Saved threads".

Every table is keyed by its chat, so deleting a `threads` row deletes the chat.
Enums are written by explicit `match`, never serde, and numbers and times go
through the checked conversions in `chat_db/columns.rs`, so a row holds only
values its columns accept.

## What loads when

| When | Reads |
|---|---|
| Startup | every `threads` row, every `token_activity` row, and the rest of the app's state |
| Opening a chat, or a scheduled task continuing it | the rest of the chat's rows, attachment bytes included |
| Typing in the search palette | matching `search_texts` of unloaded chats, through `chat_search` |

Startup does not read chat contents, so it stays fast however many chats are
saved. The sidebar draws about 20 recents and adds more as it is scrolled
(`RECENTS_PAGE`), but every chat's row is in memory: paging the rows from the
database would break statistics, fuzzy title search, per-project ordering,
schedules' lookups by id, and `position` as a dense index.

A chat loads whole. Attachment bytes stay with it because saving, serving them
to peers, the join snapshot and prompts all expect them in `draft().files`. The
schema would allow loading attachments or timeline windows on demand
(`messages.seq` is the row index), but a chat that runs, or that someone joins,
needs its whole transcript.

## Saving

**The database is the truth about what is saved.** The module doc of
`chat_db.rs` describes the protocol: a save carries the extent it was built from
(`from`) and the chat's whole extent (`end`); inside its transaction the writer
reads what the database holds and refuses with `Behind` if that is less than
`from` (an earlier save failed, so the next one starts from what is there) or
`Ahead` if it is more than `end` (the chat in memory is not the whole chat, such
as a placeholder). Otherwise it writes only the rows past what is stored. The
alternative, GPUI-side bookkeeping of what was saved, let a placeholder save wipe
a saved chat and, after a failure, rewrote the chat whole.

Two rules in the writer exist for reasons that are easy to break:

- The `threads` row is upserted with `INSERT … ON CONFLICT (id) DO UPDATE`, never
  `INSERT OR REPLACE`: replace deletes the row first, and the cascade wipes the
  chat.
- The timeline is only appended to (runs are saved once ended, comments are fixed
  at submission). Rewriting it cost about 300 ms per save at 2,000 messages. The
  transcript entries and compactions are likewise append-only, which triggers in
  the schema enforce.

One Tokio task writes, draining one unbounded channel, so saves apply in order, a
load never races a delete queued before it, and `Job::Quit` answers after every
earlier job. Spawning a task per save would not keep order. The task gets and
returns plain row data; the GPUI side builds jobs and, in `cx.spawn`, the
`Transcript` and `Thread`. App state (projects, settings, tasks, deleted-chat
statistics) is written as jobs on the same writer, so a project is created before
a chat filed under it is saved. Searches use their own pool connection: WAL lets
them run during a save, and a result a save behind is harmless.

Chats are saved at turn boundaries, when archived or restored, before unloading,
and on quit. `ChatDb::save_meta` updates only where a chat is listed (archived,
position, project) and works for an unloaded chat. `ChatDb::is_saved` reports
whether the writer confirmed everything a loaded chat holds, which is what
unloading asks. `ChatRows`, a chat as rows, is made only by a load, so nothing
builds a `Thread` with a saved chat's id out of less.

### The rest of the app's state

- **Projects** change by single inserts and deletes. `threads.project_id` is a
  foreign key with `ON DELETE SET NULL`, so deleting a project unfiles its chats
  in the same transaction, and a chat saved after its project was removed is saved
  unfiled rather than failing.
- **Deleted chats' statistics** are only added to. Deleting a chat adds its counts
  (`Statistics::of_chat`) in the transaction that deletes its rows, and a run
  ending after that adds its own usage. The longest chat is the larger of the
  two; the rest sums.
- **Settings** are one row, upserted on every change. No row reads as the
  defaults, and values are brought into range when read.
- **Scheduled tasks** are written whole on every change: there are few, each with
  at most 50 runs. A task's schedule is a JSON column. A task or run that cannot be
  read is left out, and so is gone at the next save.

## Chats in the store

Each listed chat is an `Entity<Chat>` (`chat_store.rs`), a stable handle whose
state is `Unloaded(ChatMeta)`, `Loading`, or `Loaded(Entity<Thread>)`. Its
invariants are in the module doc: one load at a time, which dropping the chat
cancels, and one copy of the metadata. The store's entries are private and change
only through its transitions, which is what makes the unload guarantee below
hold.

Lookups are split by name: `listed(id)` finds any chat, `loaded(id)` only a loaded
one. There is deliberately no `thread(id)` meaning "loaded only", because an
unloaded chat read as a missing one is wrong for the scheduler, for the
deleted-chat checks in `generation.rs` (tokens would be counted twice) and for the
schedules page. The scheduler uses `ready()`, which returns `Blocked::Loading` for
an unloaded chat and retries when the load finishes. Loads come only from the
loader, which alone can build a `Thread` with an existing id.

Archiving, restoring and removing a project work on metadata through `save_meta`.
Search matches every chat's full text, loaded or not.

### Unloading

An open chat idle for 5 minutes goes back to its `threads` row, freeing its
timeline, transcript and attachments. Drafts, pending submissions and sharing are
not saved, so only an idle chat can be unloaded.

- **The check is derived, not tracked.** `Thread::unload_blocker()` and
  `ThreadDraft::unload_blocker()` destructure every field without `..`, each
  commented as persisted, derived or blocking, so a new field does not compile
  until it is classified. `Cowork::unload_blocker()` adds the per-chat state
  outside `Thread`. Pins (RAII leases held by whatever needs a chat resident) were
  rejected: a new field nobody pins would be silently unloaded and lost.
- **Blockers** include the active chat; a run, compaction or approval wait; pending
  submissions; a scheduled run; sharing or a joined chat; a draft with text or file
  bytes in transit; pending attachment reads; and anything not yet confirmed saved.
  `unload.rs` has the full list.
- **Only the check can unload.** `ThreadStore::unload` takes an `Idle<'_>` token
  that only `unload.rs` makes, in the same update as the check.
- **Idle time is measured by the sweep**, once a minute: the clock starts at the
  first sweep that finds no blocker, and any blocker resets it.
- **Confirmed saved, then dropped.** Mode, folder and model changes are not saved
  when made, and a save may fail. An otherwise eligible chat that `is_saved` does
  not confirm is saved by the sweep and unloaded by a later one if still eligible;
  a failed save leaves it loaded. Without a database nothing is unloaded.
- **Nothing outside the store holds a chat strongly.** Holders keep a
  `WeakEntity<Thread>` and treat a failed upgrade as "gone", never as "no chat",
  so a menu cannot apply a change for a deleted chat to the new chat's draft.
  After an unload a deferred `debug_assert!` checks the weak handle no longer
  upgrades.

Loads are queued behind saves, so a reload sees the last save. Unloading drops the
agent session, sandbox project, presence, segment views and what is recorded as
saved; all are rebuilt on load. Opening a chat always scrolls to the bottom, so
there is no scroll position to keep.

## Sharing

The host loads a chat whole, so a shared chat is always fully loaded and the join
snapshot (`ThreadSnapshot`) carries the whole timeline and transcript. Only the
active chat can be shared (`prepare_thread_for_sharing`), and a chat is active only
once loaded.

A peer cannot take part with less than everything. Each participant keeps a replica
of the agent's conversation and checks every entry the host commits against it
(`Transcript::commit` needs `index == len`), and the snapshot is validated as a
whole (`prepare_snapshot`). Paging history to peers would need a protocol where
replicas trust the host for history they have not seen. So if timelines are ever
loaded in windows, that is for chats only the host is viewing.

## Search

Titles stay in memory and are matched synchronously. Message text comes from two
places: loaded chats, including joined chats that are not in the database, are
searched in memory because memory is newer than the last save; unloaded chats are
searched with `chat_search`, an FTS5 trigram index over `search_texts`, queried on
Tokio as the user types (`Cowork::search_saved_chats`). A result for a phrase that
is no longer the current one is dropped; a current one joins the group after the
rows matched by title, so nothing under the pointer moves.

FTS5 narrows; Rust decides. Every text the database returns goes through
`PaletteQuery::row`, which applies smart case and smart diacritics (stricter than
the trigram's folding, so the database returns a superset) and builds the excerpt,
so matching semantics are identical for loaded and unloaded chats.

- The trigram tokenizer needs three characters; shorter queries match titles and
  loaded chats only.
- Use `MATCH`, not `LIKE`: `LIKE` against the trigram index does not fold
  diacritics.
- A query returns at most `SEARCH_LIMIT` rows, so a common phrase does not ship
  every message.
- The index costs a few times the searchable text's size on disk.

## Startup

`ChatDb::open` runs after the Tokio runtime exists and before the window opens. It
takes `~/.cowork/.lock` (SQLite's own locks do not stop a second instance from
writing, and the error is clearer), opens the pool (`journal_mode = WAL`,
`synchronous = NORMAL`, `foreign_keys = ON`), runs migrations, and reads `threads`,
`token_activity` and the app state. Blocking keeps `needs_onboarding` from briefly
seeing no chats. Failures, a held lock or a database migrated by a newer app,
return from `run`, and Cowork does not start. `start_chat_db` then lists the
`Unloaded` chats.

Ephemeral instances save nothing, so they open no database; they still remove their
temporary root on quit. Deleting a chat removes `threads/<id>/` after the database
delete, since scratch folders stay on disk for the sandbox.

## Conversions that are easy to get wrong

- Timestamps in chats (`AgentMessage::started_at`, `TokenActivity::at`) are
  `SystemTime` in the app and chrono `DateTime<Utc>` in the database, converted at
  the boundary, which is lossless. The app's types stay `SystemTime` because
  `started_at` travels on the wire and changing it would bump `PROTOCOL_VERSION`.
- A collaborator could re-add a draft item with the id of an already submitted block
  or comment, which would make the chat unsavable under the `(thread_id, id)` keys.
  `apply_collaborator_update_authorized` refuses new item ids already submitted,
  pending ones included.

## sqlx

Use runtime `query` and `query_as`, not the `query!` macros, which need
`DATABASE_URL` or a committed `.sqlx/` and infer weak types that each need an
override; none infer `u64`. sqlx 0.9 rejects `format!`-built SQL unless wrapped in
`sqlx::AssertSqlSafe`. Migrations are embedded by `sqlx::migrate!()`.

Tests use a temporary database (`chat_db::TestDb`) with a multi-thread runtime
driving the writer. A test awaiting the database from GPUI calls
`cx.executor().allow_parking()`, because the answer comes from a Tokio thread.
