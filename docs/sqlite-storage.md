# Saving chats in SQLite

Chats are saved in one SQLite database, `~/.cowork/cowork.db`,
through sqlx (`crates/cowork/src/chat_db.rs`), replacing the per-chat JSON
folders of `thread_files.rs`. Chats saved in that format are not read. The
rest of what Cowork saves is in the same database (see "The rest of the
app's state"), replacing `projects.json`, `settings.json`,
`statistics.json` and `schedules.json`, which are not read either. This
document is the design and the reasons for it; what a saved chat holds, from
the collaboration side, is in `docs/collaboration.md` "Saved threads".

## Status

Implemented as described below, except:

- **Statistics still read `token_activity` at startup**, with the `threads`
  rows. Querying it when the profile page opens needs the page's statistics
  to become async, and deleting an unloaded chat to read its activity before
  its rows go.
- Attachment bytes and timeline windows load with the whole chat (see "What
  loads when").

## Why

Startup reads every saved chat in full, archived ones included: the whole
timeline, every transcript record, and every attachment's bytes, on the main
thread before the window opens. Timeline virtualization limits only what is
drawn. With a database, startup reads only the sidebar's rows and a chat
loads when it is opened, and a save is one transaction instead of the
`messages_bytes` ordering trick.

## What loads when

| When | Reads |
|---|---|
| Startup | every `threads` row (10,000 chats read in under 20 ms), and for now every `token_activity` row |
| Opening a chat, or a scheduled task continuing it | the rest of its rows, attachment bytes included |
| Typing in the search palette | matching `search_texts` of unloaded chats, through `chat_search` |

Attachment bytes stay with the chat for now: saving, serving them to peers,
the join snapshot and prompts all expect them in `draft().files`. Loading
them on demand is a later step, and so is loading the timeline in windows.
The schema allows both (`messages.seq` is the row index; a run's output is
`transcript_entries` from its `transcript_start` to the next run's), but a
chat that runs, or that someone joins, needs its whole transcript.

## Schema

Every table is `STRICT` and keyed by its thread, so deleting a `threads` row
removes the chat. Encodings:

- UUIDs: 16-byte `BLOB` (sqlx `uuid` feature).
- Timestamps: `chrono::DateTime<Utc>` through sqlx's `chrono` feature, which
  writes RFC 3339 `TEXT` (`2026-11-15T09:46:40.123456789+00:00`) and reads it
  back with nanoseconds intact. sqlx writes whatever offset it is given, so
  the columns `CHECK` for `+00:00`. It writes 0, 3, 6 or 9 fraction digits,
  whichever is exact, so sort with `unixepoch(x, 'subsec')`, never by the
  text.
- Durations: `INTEGER` nanoseconds (292 years fit).
- `u64` and `usize`: `INTEGER` through `i64::try_from` both ways, never `as`;
  sqlx can't bind a `u64`.
- Enums: snake_case `TEXT`, `CHECK`ed, written by an explicit `match`, not
  serde: `ModelProvider` and `ProjectMode` serialize as `"OpenAi"`, `"Write"`.
- Agent records (`StoredEntry`, `StoredCompaction`, `Envelope`): `TEXT`, the
  agent's versioned JSON, unchanged from `messages.jsonl`. They keep their own
  versioning through `type_id`, so the schema doesn't model them.

The schema is `crates/cowork/migrations/0001_schema.sql`, with a comment on
every table. Change it with a new migration, never by editing an applied
one.

## The rest of the app's state

The same schema holds projects, settings, what statistics
count of deleted chats, and scheduled tasks; `chat_db/app_state.rs` reads
them all at startup with the chats' rows and writes them as jobs on the same
writer, so they are ordered with chat saves (a project is created before a
chat filed under it is saved). Values become rows on the GPUI side, through
a cloneable `StateSaver` that `Projects` and the `Scheduler` hold.

- **Projects** change by steps: one is inserted or deleted.
  `threads.project_id` is a foreign key with `ON DELETE SET NULL`, so
  deleting a project unfiles its chats in the same transaction. Writes of
  `project_id` go through `(SELECT id FROM projects WHERE id = ?)`, so a chat
  saved after its project was removed is saved unfiled rather than failing.
- **Deleted chats' statistics** are added to, never rewritten: deleting a chat
  adds its counts (`Statistics::of_chat`) in the transaction that deletes its
  rows, and a run ending after that adds its usage on its own. The longest
  chat is the larger of the two, the rest sums (`Statistics::add`).
- **Settings** are one row, upserted whenever one changes; no row reads as
  the defaults, and values are brought into range when read.
- **Scheduled tasks** are written whole on every change, as the file was:
  there are few, each with at most 50 runs. A task's schedule (`When`, seven
  shapes) is a JSON column; everything else has columns. A task or run that
  can't be read is left out, and so is gone at the next save.

## Saving

**The database is the truth about what is saved.** Three designs were
prototyped against the real schema with the same failure suite
(`/tmp/cowork-persist-designs`): GPUI-side bookkeeping like today's, the
database as the truth, and acknowledged extents. Only the second passed
everything without parallel state: the first let a placeholder save wipe a
saved chat, and after a failure it rewrote the chat whole at twice the cost.

A save is one transaction, run at the same turn boundaries as now. The job
carries the extent it was built from (`from`: messages, entries,
compactions) and the chat's full extent (`end`). Inside the transaction the
writer reads the chat's actual extent (three indexed `max()` lookups, about
5 µs) and:

- refuses with `Behind(actual)` if the database holds less than `from`: an
  earlier save failed. The GPUI side resends from `actual`; no poisoned
  state, and no rewrite.
- refuses with `Ahead(actual)` if it holds more than `end`: the chat in
  memory isn't the whole chat, such as a placeholder.
- otherwise writes, skipping rows the database already has:

1. Upsert the `threads` row with `INSERT … ON CONFLICT (id) DO UPDATE`.
   Never `INSERT OR REPLACE`: it deletes the row first, and the cascade wipes
   the chat. Replace its `thread_folders`, `thread_profiles`, `prompt_names`
   and `token_activity` rows.
2. Insert the messages after the database's count, with their blocks,
   attachments, comments and runs. The timeline is only appended to: runs
   are saved once ended, comments are fixed at submission. Rewriting it
   instead cost about 300 ms per save at 2,000 messages.
3. Insert the transcript entries, then the compactions, after the database's
   extent. The two tables map directly onto `Transcript` (`entries()[n..]`,
   and `compacted_at` as `at_entries`; reading back is `to_transcript`), so
   `TranscriptLine` and `lines_after` go away.
4. Insert the new messages' `search_texts`.
5. `INSERT OR IGNORE` the new attachments' bytes.

The GPUI side's extent is only a hint for what to encode: too low costs
re-encoding, too high is refused. The gap triggers back this up.

**The API.** One handle owns a writer task draining one channel; the job
enum is private. Rows are built from app data by pure functions (unit-tested
by round trip), and all `u64`/`usize`/`Duration`/time and enum conversions
live in one checked module, so a job holds only values its columns accept.

`ChatDb` offers:

- `save` (through `Cowork::save_chat`): a loaded chat whole, built into
  rows on the GPUI side; a chat that isn't loaded saves only its placement.
- `save_meta`: where a chat is listed (archived, position, project). An
  `UPDATE` only, so it works for an unloaded chat and never creates an empty
  one.
- `is_saved`: whether the writer confirmed writing everything a loaded chat
  holds now, compared by extent and head; what unloading asks.
- `loader`: the store's loader, queued behind every save and delete.
- `delete`, `search` (on its own connection, on Tokio), `forget` (as a chat
  unloads), and `quit`, which consumes the handle so nothing is queued after.

`ChatRows`, a chat as rows, is made only by a load, so no code builds a
`Thread` with a saved chat's id out of less; a chat in memory holding less
than is saved is refused anyway. Ephemeral instances have no `ChatDb`.

## Loading lazily

Each chat in the store is an `Entity<Chat>`, a stable handle whose state is
`Unloaded(ChatMeta)`, `Loading { meta, task }` or `Loaded(Entity<Thread>)`.
`ChatMeta` is the `threads` row; a loaded chat derives it from its `Thread`,
so there is one copy. Three designs were prototyped on the real code with
the same tests (branches `lazy-a-enum`, `lazy-b-index`, `lazy-c-handle` on
the shared `lazy-base`):

| | Enum per entry | Index plus cache of loaded chats | Handle per chat |
|---|---|---|---|
| Two loads of one chat | checked in the store | checked | impossible: `load` acts only on `Unloaded` |
| Load finishing after its chat was deleted | re-checked on completion | re-checked | impossible: the task lives in `Loading` and drops with the chat |
| Metadata | one copy | synced copy, which failed 7 existing tests (5 after adding notifies) | one copy |
| Cost | readers need two code paths | `Context` on every insert | `cx` to iterate at ~22 sites |

The handle won: it rules out two concurrent loads and late results by
construction, and outside code can hold a cheap `Chat` handle while the
store keeps the only strong `Thread`.

- **The store's entries are private.** Only its transitions change it:
  `push_front`, `archive`, `restore`, `remove`, `set_project`, `open`,
  `unload(Idle<'_>)`, `activate`. With the lists private, the whole app
  compiles; the compiler found 37 direct uses and 57 lookups by id.
- **The active chat lives in the store**, set only by `activate`, which
  accepts only a loaded chat. Sharing, submitting and `active_thread` fall
  back to a new chat when the active one is missing (`sharing.rs:66-90`,
  `submission.rs:123-127`, `lib.rs:265-268`), so this has to hold. The
  latest open wins, matched by handle identity.
- **Lookups split in two, by name.** `thread(id)` is removed rather than kept
  as "loaded only", which would have compiled silently and been wrong at the
  scheduler, `generation.rs`'s deleted-chat checks (double-counted tokens),
  `shown_title`, `request_archive_thread` and the schedules page.
  `listed(id)` finds any chat; `loaded(id)` only a loaded one. The scheduler
  uses `ready()`, `Result<Option<Entity<Thread>>, Blocked>`, giving
  `Blocked::Loading` for an unloaded chat and retrying when the load
  finishes, as `set_models` does.
- **Loads come only from the loader**, which alone can build a `Thread` with
  an existing id, so a placeholder can't be built and saved.
- **Still to do:** handles outside the store become `WeakEntity` (`loaded()`,
  `active()`, the lookups, and `start_sharing`'s task still hand out strong
  ones); the real loader; showing the loaded chat's composer on activation.
- Archiving, restoring, deleting and removing a project work on metadata
  (`save_meta`). Deleting an archived chat still adds its tokens and
  `generation_ns` to statistics.
- `set_profile` and `set_models` apply to loaded chats; a chat gets both when
  it loads.
- Search matches every chat's full text; see "Search".

### The sidebar

Startup reads every chat's `threads` row: titles are cheap, contents are
not. The sidebar draws about 20 Recents rows and adds more as it is
scrolled, as projects already do with "Show more". Paging the rows from the
database instead would break statistics, fuzzy title search, per-project
ordering, schedules' lookups by id, and `position` as a dense index.
`token_activity` isn't read at startup; statistics query it when the
profile page opens.

### Unloading

An open chat that has been idle for 5 minutes goes back to its `threads`
row, freeing its timeline, transcript and attachments. Drafts, pending
submissions and sharing aren't saved, so only an idle chat can be unloaded.

- **The check is derived, not tracked.** `Thread::unload_blocker()` and
  `ThreadDraft::unload_blocker()` destructure every field without `..`, each
  commented as persisted, derived or blocking, so a new field doesn't
  compile until it is classified. `Cowork::unload_blocker()` adds the
  per-chat state outside `Thread`. Both return a typed `UnloadBlocker`.
- **Blockers:** the active chat; a run, compaction or approval wait (including
  a stopped run whose session hasn't ended, `active_generations`); pending
  submissions; a scheduled run; sharing, or a joined chat; a draft with text
  or with file bytes in transit; pending attachment reads; anything not yet
  confirmed saved.
- **Only the check can unload.** `ThreadStore::unload` takes an `Idle<'_>`
  token whose field is private to the unload module, made in the same
  update as the check. The store's entries must be private for this to hold.
- **Idle time is measured by the sweep**, once a minute: the clock starts at
  the first sweep that finds no blocker, and any blocker resets it. Nothing
  else has to remember to touch a timestamp.
- **Confirmed saved, then dropped.** Mode, folder and model changes aren't
  saved when made, and a save may fail. A chat otherwise eligible that
  `ChatDb::is_saved` doesn't confirm is saved by the sweep, and unloaded by
  a later sweep if it is still eligible and nothing changed since; a failed
  save just leaves it loaded. Without a database nothing is unloaded.
- **Nothing outside the store holds a chat strongly.** Holders keep a
  `WeakEntity<Thread>` and treat a failed upgrade as "gone", never as "no
  chat" (the folder and mode menus used to apply a change for a deleted chat
  to the new chat's draft). After an unload a deferred `debug_assert!` checks
  the weak handle no longer upgrades.
- Loads are queued behind saves, so a reload sees the last save. Unloading
  drops the agent session, sandbox project, presence, segment views and
  what is recorded as saved; all are rebuilt on load. Opening a chat always
  scrolls to the bottom, so there is no scroll position to keep.

Pins (RAII leases held by anything needing the chat resident) were
prototyped and rejected: only runs are scopes, everything else would need a
pin taken and released at every state change, and a new field nobody pins is
silently unloaded and lost.

## Sharing

The host loads a chat whole, never part of it, so a shared chat is always
fully loaded and peers get what they get today: the join snapshot
(`ThreadSnapshot`) carries the whole timeline and transcript. Only the
active chat can be shared (`prepare_thread_for_sharing`), and a chat is
active only once loaded.

That doesn't change if the host later loads timelines in windows: a peer
can't take part with less than everything. Each participant keeps a replica
of the agent's conversation and checks every entry the host commits against
it (`Transcript::commit`, which needs `index == len`), and the snapshot is
validated as a whole (`prepare_snapshot`). Paging history to peers would mean
a protocol where replicas trust the host for history they haven't seen, and
the host tracking what each peer has. So sharing a chat loads it whole first;
windowed loading, if it comes, is for chats only the host is viewing.

## Search

The palette becomes async. Titles stay in memory (the `threads` rows) and
are matched synchronously as now. Message text comes from two places:

- **Loaded chats**, including joined chats, which aren't in the database:
  searched in memory, synchronously, as now. Memory is newer than the last
  save.
- **Unloaded chats**: `chat_search` queried on Tokio as the user types.

Each keystroke shows the synchronous rows at once and starts a query tagged
with a generation number; a result for an older generation is dropped. When
the current one arrives its rows are merged, and the palette refreshes. Rows
matched only by text already rank after rows matched by title (`section`),
so arriving results add to the end of the group instead of reshuffling what
is under the pointer.

FTS5 narrows; Rust decides. The query is `chat_search MATCH '"<phrase>"'`
joined to `search_texts` on `rowid`, for unloaded chats, ordered by
`thread_id, seq`. Every returned text then
goes through `PaletteQuery::row` as now, which applies smart case and smart
diacritics (both stricter than the trigram's folding, so the database
returns a superset) and builds the excerpt from the first text that matches
in each chat. Matching semantics stay exactly as they are.

- The trigram tokenizer needs three characters. Shorter queries match titles
  and loaded chats only; a one- or two-letter substring of every message is
  noise anyway.
- `LIKE` against the trigram index doesn't fold diacritics; use `MATCH`.
- Cap the rows a query returns, or stop reading after a match per chat, so a
  common phrase doesn't ship every message.
- Searches read through their own pool connection, not the ordered writer
  task: WAL lets them run during a save, and a result a save behind is
  harmless.
- The index costs a few times the searchable text's size on disk.


## Startup and threading

1. Build the Tokio runtime, then `ChatDb::open` takes `~/.cowork/.lock` (SQLite's own
   locks don't stop a second instance from writing, and the error is
   clearer) and blocks on opening the pool (`journal_mode = WAL`,
   `synchronous = NORMAL`, `foreign_keys = ON`), running migrations, and
   reading `threads` and `token_activity`, before the window opens. Blocking
   keeps `needs_onboarding` from briefly seeing no chats. Errors return from
   `run` as a held lock does.
2. `start_chat_db` lists `Unloaded` entries; a chat's sandbox project is
   synced when it runs (`start_generation`).

Saves and loads go through one Tokio task draining an unbounded channel, one
job at a time. That keeps today's guarantees: saves apply in order, a load
never races a delete queued before it, and `Job::Quit` answers after every
earlier job. Spawning a task per save would not keep order. The task gets and
returns plain row data; the GPUI side builds jobs and, in `cx.spawn`, the
`Transcript` and `Thread`.

Ephemeral instances save nothing, so they open no database; they still
remove their temporary root on quit. Deleting a chat removes `threads/<id>/`
after the database delete, since scratch folders stay on disk for the
sandbox. Folders the old format left (`thread.json`, `messages.jsonl`,
`attachments/`) are left alone.

## Timestamps

Saved chats hold two timestamps, `AgentMessage::started_at` and
`TokenActivity::at`, both `SystemTime` in the app. Convert at the database
boundary with chrono, which the app already uses for local calendar math:
`DateTime::<Utc>::from(SystemTime)` and back are lossless (prototyped,
including nanoseconds and times before the epoch). Switching the app's types
isn't needed: `started_at` travels on the wire, so that would bump
`PROTOCOL_VERSION` for no gain.

## Reused ids from collaborators

`verify_change` checks only the creator of a new draft item, and `validate`
checks uniqueness within the draft, so a peer could re-add an item with the
id of an already submitted block or comment. The JSON format tolerated that;
the `(thread_id, id)` keys would make the chat unsavable.
`apply_collaborator_update_authorized` refuses new item ids already
submitted, pending ones included.

## sqlx

Prototyped with sqlx 0.9.0 (bundled SQLite 3.51.3, features `sqlite`,
`runtime-tokio`, `chrono`, `uuid`, `migrate`, `macros`, no default features):
the schema migrates as written, timestamps round-trip exactly, deletes
cascade, a failing save rolls back, and FTS5 with the trigram tokenizer is
compiled in. It adds no TLS, no `sqlite3` link
conflict, and no duplicate major versions; chrono, uuid and tokio match the
workspace's.

- Use runtime `query` / `query_as`. The `query!` macros need `DATABASE_URL`
  or a committed `.sqlx/` from `cargo sqlx prepare`, and infer weak types
  (`Vec<u8>`, `String`, `i64`) that each need an `AS "x: T"` override; none
  infer `u64`.
- sqlx 0.9 rejects `format!`-built SQL unless wrapped in `sqlx::AssertSqlSafe`.
- Migrations live in `crates/cowork/migrations/`, embedded by
  `sqlx::migrate!()`, replacing the threads' `FILE_VERSION`. A database
  migrated by a newer app fails to open, and Cowork doesn't start, as for a
  held lock.
- Tests use a temporary database (`chat_db::TestDb`) with a multi-thread
  runtime driving the writer. A test awaiting the database from GPUI calls
  `cx.executor().allow_parking()`, as the answer comes from a Tokio thread.

## Open questions

- sqlx is async-only; a synchronous library (rusqlite) on a writer thread
  would avoid the runtime at startup. The plan above works with sqlx.
