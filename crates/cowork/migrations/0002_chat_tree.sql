-- Chats as a tree of timeline messages, so several chats can share one stored
-- history up to a fork point. A message points at its parent; a chat
-- (`threads`) points at its last message, its head, and its history is the
-- path from the root to the head. Everything a message holds (blocks,
-- comments, its run, the run's transcript entries and compactions, attached
-- files, search texts) cascades from it. See docs/sqlite-storage.md.
--
-- Saved chats are not kept: every chat table is dropped and made anew.
-- Projects, settings, scheduled tasks and the deleted chats' statistics stay.
-- Encodings are as in 0001.

DROP TABLE chat_search;
DROP TABLE search_texts;
DROP TABLE transcript_compactions;
DROP TABLE transcript_entries;
DROP TABLE agent_runs;
DROP TABLE comments;
DROP TABLE attachment_data;
DROP TABLE block_attachments;
DROP TABLE prompt_blocks;
DROP TABLE messages;
DROP TABLE token_activity;
DROP TABLE prompt_names;
DROP TABLE thread_profiles;
DROP TABLE thread_folders;
DROP TABLE threads;

-- Lifetime tokens are the sum of the runs' usage, deleted chats' included
-- (`deleted_token_activity`), so they are not kept twice.
ALTER TABLE deleted_usage DROP COLUMN tokens_used;

-- One row per chat: everything the sidebar, archive, schedules and statistics
-- need, so startup reads only this table and the runs' usage.
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
    context_tokens  INTEGER CHECK (context_tokens >= 0),
    streamed_bytes  INTEGER NOT NULL CHECK (streamed_bytes >= 0),
    -- Derived from the runs on the chat's path on each save, so an unloaded
    -- chat has them: Thread::generation_time (statistics, deleting an
    -- archived chat) and Thread::last_activity (ordering chats within a
    -- project). They can't go stale: a path's runs never change.
    generation_ns   INTEGER NOT NULL CHECK (generation_ns >= 0),
    last_run_at     TEXT    CHECK (last_run_at IS NULL OR (last_run_at LIKE '%+00:00' AND unixepoch(last_run_at) IS NOT NULL)),
    -- The chat's last message, and the root of its tree; both NULL for an
    -- empty chat. A save moves the head only to a descendant, so the root
    -- never changes once set. No ON DELETE: deleting a chat's head fails.
    head_id         BLOB    REFERENCES messages (id),
    root_id         BLOB,
    CHECK ((model_provider IS NULL) = (model_id IS NULL)),
    CHECK ((head_id IS NULL) = (root_id IS NULL))
) STRICT;

-- Not unique: saving one chat leaves the others' positions stale.
CREATE INDEX threads_by_position ON threads (archived, position);

-- Removing a project unfiles its chats through it.
CREATE INDEX threads_by_project ON threads (project_id) WHERE project_id IS NOT NULL;

-- Deleting a message checks no chat ends there.
CREATE INDEX threads_by_head ON threads (head_id);

-- Search finds a match's chats by its tree.
CREATE INDEX threads_by_root ON threads (root_id);

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

-- The timeline messages of every chat, as a tree. `depth` is the message's
-- index in the timeline of every chat whose path holds it. `parent_id`,
-- `depth` and `root_id` are fixed when the row is inserted. No ON DELETE on
-- `parent_id`: deleting a message that still has a child fails.
CREATE TABLE messages (
    id         BLOB    PRIMARY KEY CHECK (length(id) = 16),
    parent_id  BLOB    REFERENCES messages (id),
    depth      INTEGER NOT NULL,
    root_id    BLOB    NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('user', 'agent'))
) STRICT, WITHOUT ROWID;

-- Deleting walks up from a head while a message has no other child.
CREATE INDEX messages_by_parent ON messages (parent_id);

CREATE TRIGGER messages_in_tree
BEFORE INSERT ON messages
WHEN CASE
    WHEN NEW.parent_id IS NULL THEN NEW.depth IS NOT 0 OR NEW.root_id IS NOT NEW.id
    ELSE NOT EXISTS (
        SELECT 1 FROM messages parent
        WHERE parent.id = NEW.parent_id
          AND parent.depth + 1 = NEW.depth
          AND parent.root_id = NEW.root_id
    )
END
BEGIN
    SELECT RAISE(ABORT, 'message out of place in its tree');
END;

CREATE TRIGGER messages_fixed
BEFORE UPDATE OF id, parent_id, depth, root_id ON messages
BEGIN
    SELECT RAISE(ABORT, 'a message''s place in its tree never changes');
END;

CREATE TRIGGER threads_head_insert
BEFORE INSERT ON threads
WHEN NEW.head_id IS NOT NULL
  AND (SELECT root_id FROM messages WHERE id = NEW.head_id) IS NOT NEW.root_id
BEGIN
    SELECT RAISE(ABORT, 'a chat''s head is outside its tree');
END;

CREATE TRIGGER threads_head_update
BEFORE UPDATE OF head_id, root_id ON threads
WHEN (OLD.root_id IS NOT NULL AND NEW.root_id IS NOT OLD.root_id)
  OR (NEW.head_id IS NOT NULL
      AND (SELECT root_id FROM messages WHERE id = NEW.head_id) IS NOT NEW.root_id)
BEGIN
    SELECT RAISE(ABORT, 'a chat''s head is outside its tree');
END;

-- A user message's blocks. Ids are global; a reused one fails the save.
CREATE TABLE prompt_blocks (
    id          BLOB    PRIMARY KEY CHECK (length(id) = 16),
    message_id  BLOB    NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    author      BLOB    NOT NULL CHECK (length(author) = 16),
    text        TEXT    NOT NULL,
    UNIQUE (message_id, seq)
) STRICT, WITHOUT ROWID;

-- The files a block attaches. Their bytes are in `attachment_data`, so
-- reading a timeline never reads them.
CREATE TABLE block_attachments (
    block_id       BLOB    NOT NULL REFERENCES prompt_blocks (id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    attachment_id  BLOB    NOT NULL CHECK (length(attachment_id) = 16),
    name           TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('text', 'png', 'jpeg')),
    size           INTEGER NOT NULL CHECK (size >= 0),
    creator        BLOB    NOT NULL CHECK (length(creator) = 16),
    PRIMARY KEY (block_id, seq)
) STRICT, WITHOUT ROWID;

-- A file's bytes, held by the message whose block attaches it: an
-- attachment id belongs to one block, and a fork reaches the bytes through
-- that message on its path. No foreign key from `block_attachments`: bytes
-- may be missing, and the message still shows the file. Which ids have
-- bytes (`stored_attachments`) comes from this table.
CREATE TABLE attachment_data (
    id          BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    message_id  BLOB NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    bytes       BLOB NOT NULL
) STRICT;  -- rowid: large blobs insert about twice as fast

CREATE INDEX attachment_data_by_message ON attachment_data (message_id);

-- Comments, held only by the user message that submitted them. The target
-- may be any message, or the derived id of a reply to a comment, so it has
-- no foreign key; nothing validates it, and a missing target is skipped
-- when drawn. `seq` matters: the agent names comments by position.
CREATE TABLE comments (
    id                 BLOB    PRIMARY KEY CHECK (length(id) = 16),
    message_id         BLOB    NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    seq                INTEGER NOT NULL,
    author             BLOB    NOT NULL CHECK (length(author) = 16),
    target_message_id  BLOB    NOT NULL CHECK (length(target_message_id) = 16),
    -- UTF-8 byte range of the target's text.
    range_start        INTEGER NOT NULL,
    range_end          INTEGER NOT NULL,
    quote              TEXT    NOT NULL,
    body               TEXT    NOT NULL,
    UNIQUE (message_id, seq),
    CHECK (0 <= range_start AND range_start <= range_end)
) STRICT, WITHOUT ROWID;

-- An agent message: a run, ended when saved. Read whole at startup for
-- statistics: each run is stored once, however many chats share it.
CREATE TABLE agent_runs (
    message_id        BLOB    PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE,
    comment_group_id  BLOB    CHECK (comment_group_id IS NULL OR length(comment_group_id) = 16),
    started_at        TEXT    NOT NULL CHECK (started_at LIKE '%+00:00' AND unixepoch(started_at) IS NOT NULL),
    duration_ns       INTEGER NOT NULL CHECK (duration_ns >= 0),
    -- What the run's requests used, summaries included.
    tokens            INTEGER NOT NULL CHECK (tokens >= 0),
    -- Where the run starts in the transcript (`transcript_entries.idx`); it
    -- owns the entries up to the next run's start.
    transcript_start  INTEGER NOT NULL CHECK (transcript_start >= 0),
    outcome           TEXT    NOT NULL CHECK (outcome IN ('completed', 'stopped', 'failed')),
    error             TEXT,
    -- The reply a stopped or failed run had sampled so far: an Envelope.
    unfinished        TEXT    CHECK (unfinished IS NULL OR json_valid(unfinished)),
    model_provider    TEXT    CHECK (model_provider IN ('ollama', 'anthropic', 'open_ai')),
    model_id          TEXT,
    CHECK ((outcome = 'failed') = (error IS NOT NULL)),
    CHECK ((model_provider IS NULL) = (model_id IS NULL))
) STRICT, WITHOUT ROWID;

-- The agent session's conversation, by the run that committed each entry.
-- `idx` is the entry's TranscriptIndex along the chat's path, so a load
-- orders the rows without walking the tree.
CREATE TABLE transcript_entries (
    run_id  BLOB    NOT NULL REFERENCES agent_runs (message_id) ON DELETE CASCADE,
    idx     INTEGER NOT NULL,
    -- StoredEntry.
    record  TEXT    NOT NULL CHECK (json_valid(record)),
    PRIMARY KEY (run_id, idx)
) STRICT, WITHOUT ROWID;

-- Accepted compactions, by the run that accepted each, in order; each
-- applies once the first `at_entries` entries are (Transcript::compacted_at).
-- A fork inherits the compactions on its path, as its context had them.
CREATE TABLE transcript_compactions (
    run_id      BLOB    NOT NULL REFERENCES agent_runs (message_id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    at_entries  INTEGER NOT NULL,
    -- StoredCompaction.
    record      TEXT    NOT NULL CHECK (json_valid(record)),
    PRIMARY KEY (run_id, seq)
) STRICT, WITHOUT ROWID;

-- A run's entries are contiguous from its start, and its compactions in
-- order within its entries: Conversation::apply requires index == len, and
-- compaction removes nothing. A save inserts a run's entries before its
-- compactions.
CREATE TRIGGER transcript_entries_append
BEFORE INSERT ON transcript_entries
WHEN NEW.idx IS NOT coalesce(
    (SELECT max(idx) + 1 FROM transcript_entries WHERE run_id = NEW.run_id),
    (SELECT transcript_start FROM agent_runs WHERE message_id = NEW.run_id)
)
BEGIN
    SELECT RAISE(ABORT, 'transcript entry out of order');
END;

CREATE TRIGGER transcript_compactions_append
BEFORE INSERT ON transcript_compactions
WHEN NEW.seq IS NOT (SELECT coalesce(max(seq) + 1, 0) FROM transcript_compactions WHERE run_id = NEW.run_id)
  OR NEW.at_entries < (SELECT transcript_start FROM agent_runs WHERE message_id = NEW.run_id)
  OR NEW.at_entries > (SELECT transcript_start FROM agent_runs WHERE message_id = NEW.run_id)
                      + (SELECT count(*) FROM transcript_entries WHERE run_id = NEW.run_id)
  OR NEW.at_entries < (SELECT coalesce(max(at_entries), 0) FROM transcript_compactions WHERE run_id = NEW.run_id)
BEGIN
    SELECT RAISE(ABORT, 'compaction out of order');
END;

-- The text the search palette looks in: each user block, and each agent
-- run's final reply (`ui.output.text`), as `searchable_texts` lists them.
-- `part` orders a message's texts, and `messages.depth` the messages, so the
-- excerpt comes from a chat's first match. A plain table, so it cascades
-- with its message and deletes by index; the FTS5 index over it is kept in
-- step by triggers (cascaded deletes fire them).
CREATE TABLE search_texts (
    message_id  BLOB    NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    part        INTEGER NOT NULL,
    text        TEXT    NOT NULL,
    UNIQUE (message_id, part)
) STRICT;

-- The trigram tokenizer makes MATCH a case- and diacritic-insensitive
-- substring search; see "Search". Join back on rowid for the message.
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
