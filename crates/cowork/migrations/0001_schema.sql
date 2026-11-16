-- Everything Cowork saves: chats, projects, settings, what statistics count
-- of deleted chats, and scheduled tasks. Every table is STRICT. A chat's
-- tables are keyed by the owning thread, so its rows are deleted together by
-- `ON DELETE CASCADE` from `threads`.
--
-- Encodings:
--   UUIDs              BLOB, 16 bytes (sqlx `uuid` feature)
--   timestamps         TEXT, chrono::DateTime<Utc> (sqlx `chrono` feature
--                      writes RFC 3339 ending in +00:00). Sort with
--                      unixepoch(x, 'subsec'), never the text: the number of
--                      fraction digits varies.
--   local times        TEXT, chrono::NaiveDateTime (sqlx writes
--                      'YYYY-MM-DD HH:MM:SS.fff'); a schedule's times are
--                      wall-clock times in the user's time zone
--   durations          INTEGER nanoseconds
--   u64 and usize      INTEGER via i64::try_from; sqlx can't bind u64
--   enums              TEXT, snake_case names written by an explicit match
--                      (ModelProvider and ProjectMode serialize unrenamed)
--   paths              TEXT; a path that isn't UTF-8 can't be saved
--   agent records      TEXT, the agent's versioned JSON (StoredEntry,
--                      StoredCompaction, Envelope)
--   a schedule         TEXT, scheduled_task::When as serde JSON: seven
--                      shapes, only ever read and written whole

-- Projects: named groups of chats in the sidebar. See docs/projects.md.
CREATE TABLE projects (
    id          BLOB PRIMARY KEY CHECK (length(id) = 16),
    -- Trimmed and never blank. Names needn't be unique.
    name        TEXT NOT NULL CHECK (name <> '' AND name = trim(name)),
    -- Projects are listed newest first.
    created_at  TEXT NOT NULL CHECK (created_at LIKE '%+00:00' AND unixepoch(created_at) IS NOT NULL)
) STRICT, WITHOUT ROWID;

-- The folders a chat composed in the project starts with, in order.
CREATE TABLE project_folders (
    project_id  BLOB    NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL CHECK (seq >= 0),
    path        TEXT    NOT NULL,
    PRIMARY KEY (project_id, seq)
) STRICT, WITHOUT ROWID;

-- One row per chat: everything the sidebar, archive, schedules and statistics
-- need, so startup reads only this table and `token_activity`.
CREATE TABLE threads (
    id              BLOB    PRIMARY KEY CHECK (length(id) = 16),
    title           TEXT    NOT NULL,
    -- Who the user is in the thread, as its messages name them.
    participant_id  BLOB    NOT NULL CHECK (length(participant_id) = 16),
    archived        INTEGER NOT NULL CHECK (archived IN (0, 1)),
    -- Where it is in the sidebar's recents, or in the archive.
    position        INTEGER NOT NULL,
    model_provider  TEXT    CHECK (model_provider IN ('ollama', 'anthropic', 'open_ai')),
    model_id        TEXT,
    project_mode    TEXT    NOT NULL CHECK (project_mode IN ('write', 'read', 'network')),
    -- Writes go through `(SELECT id FROM projects WHERE id = ?)`, so a chat
    -- saved after its project was removed is saved unfiled.
    project_id      BLOB    REFERENCES projects (id) ON DELETE SET NULL,
    tokens_used     INTEGER NOT NULL CHECK (tokens_used >= 0),
    context_tokens  INTEGER CHECK (context_tokens >= 0),
    streamed_bytes  INTEGER NOT NULL CHECK (streamed_bytes >= 0),
    -- Derived from `agent_runs` on each save, so an unloaded chat has them:
    -- Thread::generation_time (statistics, deleting an archived chat) and
    -- Thread::last_activity (ordering chats within a project).
    generation_ns   INTEGER NOT NULL CHECK (generation_ns >= 0),
    last_run_at     TEXT    CHECK (last_run_at IS NULL OR (last_run_at LIKE '%+00:00' AND unixepoch(last_run_at) IS NOT NULL)),
    CHECK ((model_provider IS NULL) = (model_id IS NULL))
) STRICT;

-- Not unique: saving one chat leaves the others' positions stale.
CREATE INDEX threads_by_position ON threads (archived, position);

-- Removing a project unfiles its chats through it.
CREATE INDEX threads_by_project ON threads (project_id) WHERE project_id IS NOT NULL;

CREATE TABLE thread_folders (
    thread_id  BLOB    NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    name       TEXT    NOT NULL,
    path       TEXT    NOT NULL,
    PRIMARY KEY (thread_id, seq)
) STRICT, WITHOUT ROWID;

-- Everyone who joined while it was shared, so their messages keep their names.
CREATE TABLE thread_profiles (
    thread_id    BLOB NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    participant  BLOB NOT NULL CHECK (length(participant) = 16),
    name         TEXT,
    -- A square JPEG, raw rather than base64.
    picture      BLOB,
    appearance   BLOB CHECK (appearance IS NULL OR length(appearance) = 16),
    PRIMARY KEY (thread_id, participant)
) STRICT;  -- rowid: pictures are large

-- The names participants' prompts carry, by participant.
CREATE TABLE prompt_names (
    thread_id    BLOB NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    participant  BLOB NOT NULL CHECK (length(participant) = 16),
    name         TEXT NOT NULL,
    PRIMARY KEY (thread_id, participant)
) STRICT, WITHOUT ROWID;

-- Read at startup with `threads`: statistics sum it across every chat.
CREATE TABLE token_activity (
    thread_id    BLOB    NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    at           TEXT    NOT NULL CHECK (at LIKE '%+00:00' AND unixepoch(at) IS NOT NULL),
    duration_ns  INTEGER NOT NULL CHECK (duration_ns >= 0),
    tokens       INTEGER NOT NULL CHECK (tokens >= 0),
    PRIMARY KEY (thread_id, seq)
) STRICT, WITHOUT ROWID;

-- The timeline, in order. `seq` is the row's index in the timeline, so a
-- window of rows is a range query. The timeline is only appended to.
CREATE TABLE messages (
    thread_id  BLOB    NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    id         BLOB    NOT NULL CHECK (length(id) = 16),
    seq        INTEGER NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('user', 'agent')),
    PRIMARY KEY (thread_id, id),
    UNIQUE (thread_id, seq)
) STRICT, WITHOUT ROWID;

-- A user message's blocks.
CREATE TABLE prompt_blocks (
    thread_id   BLOB    NOT NULL,
    message_id  BLOB    NOT NULL,
    seq         INTEGER NOT NULL,
    id          BLOB    NOT NULL CHECK (length(id) = 16),
    author      BLOB    NOT NULL CHECK (length(author) = 16),
    text        TEXT    NOT NULL,
    PRIMARY KEY (thread_id, id),
    UNIQUE (thread_id, message_id, seq),
    FOREIGN KEY (thread_id, message_id) REFERENCES messages (thread_id, id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- The files a block attaches. Their bytes are in `attachment_data`, so
-- reading a timeline never reads them.
CREATE TABLE block_attachments (
    thread_id      BLOB    NOT NULL,
    block_id       BLOB    NOT NULL,
    seq            INTEGER NOT NULL,
    attachment_id  BLOB    NOT NULL CHECK (length(attachment_id) = 16),
    name           TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('text', 'png', 'jpeg')),
    size           INTEGER NOT NULL CHECK (size >= 0),
    creator        BLOB    NOT NULL CHECK (length(creator) = 16),
    PRIMARY KEY (thread_id, block_id, seq),
    FOREIGN KEY (thread_id, block_id) REFERENCES prompt_blocks (thread_id, id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE INDEX block_attachments_by_id ON block_attachments (thread_id, attachment_id);

-- Hangs off `threads`, not the blocks, because a save rewrites the
-- timeline rows but writes each file's bytes only once. No foreign key from
-- `block_attachments` either: bytes may be missing, and the message still
-- shows the file. Which ids have bytes (`stored_attachments`) comes from
-- this table.
CREATE TABLE attachment_data (
    thread_id  BLOB NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    id         BLOB NOT NULL CHECK (length(id) = 16),
    bytes      BLOB NOT NULL,
    PRIMARY KEY (thread_id, id)
) STRICT;  -- rowid: large blobs insert about twice as fast

-- Comments, held only by the user message that submitted them. The target
-- may be any message, or the derived id of a reply to a comment, so it has
-- no foreign key; nothing validates it, and a missing target is skipped
-- when drawn. `seq` matters: the agent names comments by position.
CREATE TABLE comments (
    thread_id          BLOB    NOT NULL,
    id                 BLOB    NOT NULL CHECK (length(id) = 16),
    message_id         BLOB    NOT NULL,
    seq                INTEGER NOT NULL,
    author             BLOB    NOT NULL CHECK (length(author) = 16),
    target_message_id  BLOB    NOT NULL CHECK (length(target_message_id) = 16),
    -- UTF-8 byte range of the target's text.
    range_start        INTEGER NOT NULL,
    range_end          INTEGER NOT NULL,
    quote              TEXT    NOT NULL,
    body               TEXT    NOT NULL,
    PRIMARY KEY (thread_id, id),
    UNIQUE (thread_id, message_id, seq),
    FOREIGN KEY (thread_id, message_id) REFERENCES messages (thread_id, id) ON DELETE CASCADE,
    CHECK (0 <= range_start AND range_start <= range_end)
) STRICT, WITHOUT ROWID;

-- An agent message: a run, ended when saved.
CREATE TABLE agent_runs (
    thread_id         BLOB    NOT NULL,
    message_id        BLOB    NOT NULL,
    comment_group_id  BLOB    CHECK (comment_group_id IS NULL OR length(comment_group_id) = 16),
    started_at        TEXT    NOT NULL CHECK (started_at LIKE '%+00:00' AND unixepoch(started_at) IS NOT NULL),
    duration_ns       INTEGER NOT NULL CHECK (duration_ns >= 0),
    -- Where the run starts in the transcript (`transcript_entries.idx`); its
    -- output runs to the next run's start.
    transcript_start  INTEGER NOT NULL CHECK (transcript_start >= 0),
    outcome           TEXT    NOT NULL CHECK (outcome IN ('completed', 'stopped', 'failed')),
    error             TEXT,
    -- The reply a stopped or failed run had sampled so far: an Envelope.
    unfinished        TEXT    CHECK (unfinished IS NULL OR json_valid(unfinished)),
    model_provider    TEXT    CHECK (model_provider IN ('ollama', 'anthropic', 'open_ai')),
    model_id          TEXT,
    PRIMARY KEY (thread_id, message_id),
    FOREIGN KEY (thread_id, message_id) REFERENCES messages (thread_id, id) ON DELETE CASCADE,
    CHECK ((outcome = 'failed') = (error IS NOT NULL)),
    CHECK ((model_provider IS NULL) = (model_id IS NULL))
) STRICT, WITHOUT ROWID;

-- The agent session's conversation, only ever appended to. `idx` is the
-- entry's TranscriptIndex.
CREATE TABLE transcript_entries (
    thread_id  BLOB    NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    idx        INTEGER NOT NULL,
    -- StoredEntry.
    record     TEXT    NOT NULL CHECK (json_valid(record)),
    PRIMARY KEY (thread_id, idx)
) STRICT, WITHOUT ROWID;

-- Accepted compactions, in order, each applied once the first `at_entries`
-- entries are (Transcript::compacted_at).
CREATE TABLE transcript_compactions (
    thread_id   BLOB    NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    at_entries  INTEGER NOT NULL,
    -- StoredCompaction.
    record      TEXT    NOT NULL CHECK (json_valid(record)),
    PRIMARY KEY (thread_id, seq)
) STRICT, WITHOUT ROWID;

-- The transcript only grows: Conversation::apply requires index == len, and
-- compaction removes nothing. These refuse a gap, which is what a save
-- queued behind a failed one would leave. A save inserts its entries
-- before its compactions.
CREATE TRIGGER transcript_entries_append
BEFORE INSERT ON transcript_entries
WHEN NEW.idx IS NOT (SELECT coalesce(max(idx) + 1, 0) FROM transcript_entries WHERE thread_id = NEW.thread_id)
BEGIN
    SELECT RAISE(ABORT, 'transcript entry out of order');
END;

CREATE TRIGGER transcript_compactions_append
BEFORE INSERT ON transcript_compactions
WHEN NEW.seq IS NOT (SELECT coalesce(max(seq) + 1, 0) FROM transcript_compactions WHERE thread_id = NEW.thread_id)
  OR NEW.at_entries > (SELECT coalesce(max(idx) + 1, 0) FROM transcript_entries WHERE thread_id = NEW.thread_id)
  OR NEW.at_entries < (SELECT coalesce(max(at_entries), 0) FROM transcript_compactions WHERE thread_id = NEW.thread_id)
BEGIN
    SELECT RAISE(ABORT, 'compaction out of order');
END;

-- The text the search palette looks in: each user block, and each agent
-- run's final reply (`ui.output.text`), as `searchable_texts` lists them.
-- `(message_seq, part)` orders a chat's texts, so the excerpt comes from the
-- first match, and a save appends a new message's texts with it.
-- A plain table, so it cascades with its chat and deletes by index; the
-- FTS5 index over it is kept in step by triggers (cascaded deletes fire
-- them). Deleting a chat's 50 rows among 100,000 takes about 1 ms; with
-- `thread_id UNINDEXED` inside the FTS table it was a scan, about 11 ms.
CREATE TABLE search_texts (
    thread_id    BLOB    NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    message_seq  INTEGER NOT NULL,
    part         INTEGER NOT NULL,
    text         TEXT    NOT NULL,
    UNIQUE (thread_id, message_seq, part)
) STRICT;

-- The trigram tokenizer makes MATCH a case- and diacritic-insensitive
-- substring search; see "Search". Join back on rowid for the chat and order.
CREATE VIRTUAL TABLE chat_search USING fts5 (
    text,
    content = 'search_texts',
    content_rowid = 'rowid',
    tokenize = 'trigram remove_diacritics 1'
);

CREATE TRIGGER search_texts_insert AFTER INSERT ON search_texts BEGIN
    INSERT INTO chat_search (rowid, text) VALUES (NEW.rowid, NEW.text);
END;

CREATE TRIGGER search_texts_delete AFTER DELETE ON search_texts BEGIN
    INSERT INTO chat_search (chat_search, rowid, text) VALUES ('delete', OLD.rowid, OLD.text);
END;

-- The user's settings; no row until one is changed. Values are brought into
-- range when read (ContextSettings::in_range).
CREATE TABLE settings (
    id                         INTEGER PRIMARY KEY CHECK (id = 1),
    -- The model new chats start with: the last one selected locally.
    model_provider             TEXT    CHECK (model_provider IN ('ollama', 'anthropic', 'open_ai')),
    model_id                   TEXT,
    context_window_tokens      INTEGER NOT NULL CHECK (context_window_tokens >= 0),
    compact_threshold_percent  INTEGER NOT NULL CHECK (compact_threshold_percent BETWEEN 0 AND 100),
    CHECK ((model_provider IS NULL) = (model_id IS NULL))
) STRICT;

-- What the profile page's statistics count of deleted chats, so they outlast
-- them; the rest they count from `threads` and `token_activity`. Deleting a
-- chat adds to these in the transaction that deletes it.
CREATE TABLE deleted_usage (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    chats            INTEGER NOT NULL CHECK (chats >= 0),
    tokens_used      INTEGER NOT NULL CHECK (tokens_used >= 0),
    longest_chat_ns  INTEGER NOT NULL CHECK (longest_chat_ns >= 0)
) STRICT;

INSERT INTO deleted_usage (id, chats, tokens_used, longest_chat_ns) VALUES (1, 0, 0, 0);

CREATE TABLE deleted_token_activity (
    seq          INTEGER PRIMARY KEY,
    at           TEXT    NOT NULL CHECK (at LIKE '%+00:00' AND unixepoch(at) IS NOT NULL),
    duration_ns  INTEGER NOT NULL CHECK (duration_ns >= 0),
    tokens       INTEGER NOT NULL CHECK (tokens >= 0)
) STRICT;

-- Scheduled tasks. See docs/schedule.md. Saved whole whenever one changes:
-- a few tasks, each with at most scheduled_task::MAX_HISTORY runs.
CREATE TABLE scheduled_tasks (
    id              BLOB    PRIMARY KEY CHECK (length(id) = 16),
    -- Creation order, which breaks ties in the run queue.
    seq             INTEGER NOT NULL UNIQUE CHECK (seq >= 0),
    title           TEXT    NOT NULL,
    prompt          TEXT    NOT NULL,
    schedule        TEXT    NOT NULL CHECK (json_valid(schedule)),
    model_provider  TEXT    NOT NULL CHECK (model_provider IN ('ollama', 'anthropic', 'open_ai')),
    model_id        TEXT    NOT NULL,
    thread_mode     TEXT    NOT NULL CHECK (thread_mode IN ('new_each_run', 'continue_last')),
    missed_runs     TEXT    NOT NULL CHECK (missed_runs IN ('run_once', 'skip')),
    project_mode    TEXT    NOT NULL CHECK (project_mode IN ('write', 'read', 'network')),
    active          INTEGER NOT NULL CHECK (active IN (0, 1)),
    -- Every occurrence up to this time has been handled.
    checked_until   TEXT    NOT NULL CHECK (unixepoch(checked_until) IS NOT NULL),
    -- The chat "continues one chat" runs submit to. No foreign key: the run
    -- saves the task before its new chat's first save.
    thread_id       BLOB    CHECK (thread_id IS NULL OR length(thread_id) = 16),
    queued_for      TEXT    CHECK (queued_for IS NULL OR unixepoch(queued_for) IS NOT NULL),
    queued_trigger  TEXT    CHECK (queued_trigger IN ('scheduled', 'catch_up', 'manual')),
    CHECK ((queued_for IS NULL) = (queued_trigger IS NULL))
) STRICT, WITHOUT ROWID;

-- The folders each run's chat has as its project, in order.
CREATE TABLE scheduled_task_folders (
    task_id  BLOB    NOT NULL REFERENCES scheduled_tasks (id) ON DELETE CASCADE,
    seq      INTEGER NOT NULL CHECK (seq >= 0),
    path     TEXT    NOT NULL,
    PRIMARY KEY (task_id, seq)
) STRICT, WITHOUT ROWID;

-- A task's runs, oldest first.
CREATE TABLE scheduled_runs (
    task_id         BLOB    NOT NULL REFERENCES scheduled_tasks (id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL CHECK (seq >= 0),
    id              BLOB    NOT NULL CHECK (length(id) = 16),
    scheduled_for   TEXT    NOT NULL CHECK (unixepoch(scheduled_for) IS NOT NULL),
    started_at      TEXT    CHECK (started_at IS NULL OR unixepoch(started_at) IS NOT NULL),
    run_trigger     TEXT    NOT NULL CHECK (run_trigger IN ('scheduled', 'catch_up', 'manual')),
    result          TEXT    NOT NULL CHECK (result IN ('running', 'completed', 'stopped', 'failed', 'interrupted', 'skipped')),
    error           TEXT,
    skipped_count   INTEGER CHECK (skipped_count >= 0),
    skip_reason     TEXT    CHECK (skip_reason IN ('missed', 'already_queued', 'paused')),
    thread_id       BLOB    CHECK (thread_id IS NULL OR length(thread_id) = 16),
    CHECK ((result = 'failed') = (error IS NOT NULL)),
    CHECK ((result = 'skipped') = (skipped_count IS NOT NULL)),
    CHECK ((skipped_count IS NULL) = (skip_reason IS NULL)),
    PRIMARY KEY (task_id, seq)
) STRICT, WITHOUT ROWID;
