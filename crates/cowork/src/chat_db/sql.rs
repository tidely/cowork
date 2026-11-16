//! The chats' SQL: opening the database, reading a chat's stored head,
//! writing and reading a chat's rows, placement updates, deletion and
//! search. The rest of the app's state is `app_state`'s. Functions take a
//! connection (a transaction, for writes) and plain rows; none decide what
//! to save. Enum columns go through `columns`.
//!
//! Messages form a tree (see `migrations/0002_chat_tree.sql`); a chat's
//! rows are those of the messages on the path from its head to the root.

use std::{collections::HashMap, path::Path};

use chrono::{DateTime, Utc};
use sqlx::{
    Row, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use uuid::Uuid;

use super::{
    columns,
    rows::{
        ActivityRow, AttachmentRow, BlockRow, ChatRows, CommentRow, MessageRow, MetaRow,
        NewMessage, PathPoint, Placement, ProfileRow, RunRow, ThreadRow,
    },
};
use crate::models::ModelRef;

/// `$query` with `path (id, parent_id, depth)` bound to the messages on the
/// path of chat `?1`, from its head up to the root.
macro_rules! on_path {
    ($query:literal) => {
        concat!(
            "WITH RECURSIVE path (id, parent_id, depth) AS (
                 SELECT m.id, m.parent_id, m.depth FROM threads t
                 JOIN messages m ON m.id = t.head_id WHERE t.id = ?1
                 UNION ALL
                 SELECT m.id, m.parent_id, m.depth FROM path p
                 JOIN messages m ON m.id = p.parent_id
             ) ",
            $query
        )
    };
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Opens (or creates) the database at `path` and brings its schema up to
/// date. A database a newer Cowork migrated fails here.
pub(super) async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// Every chat's row, archived ones last, each list in its saved order.
pub(super) async fn read_metas(pool: &SqlitePool) -> anyhow::Result<Vec<MetaRow>> {
    Ok(sqlx::query(
        "SELECT id, title, archived, project_id, generation_ns, last_run_at
         FROM threads ORDER BY archived, position, unixepoch(last_run_at, 'subsec') DESC",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| MetaRow {
        id: row.get(0),
        title: row.get(1),
        archived: row.get(2),
        project: row.get(3),
        generation_ns: row.get(4),
        last_run_at: row.get(5),
    })
    .collect())
}

/// Every saved run's usage that used any tokens, for statistics. Each run is
/// stored once, however many chats share it.
pub(super) async fn read_run_activity(pool: &SqlitePool) -> anyhow::Result<Vec<ActivityRow>> {
    Ok(sqlx::query(
        "SELECT started_at, duration_ns, tokens FROM agent_runs
         WHERE tokens > 0 ORDER BY unixepoch(started_at, 'subsec')",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| ActivityRow {
        at: row.get(0),
        duration_ns: row.get(1),
        tokens: row.get(2),
    })
    .collect())
}

/// A chat's stored head and its root, if it has a row with messages.
pub(super) async fn stored_head(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> anyhow::Result<Option<(PathPoint, Uuid)>> {
    Ok(sqlx::query(
        "SELECT t.head_id, m.depth, t.root_id FROM threads t
         JOIN messages m ON m.id = t.head_id WHERE t.id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .map(|row| {
        (
            PathPoint {
                id: row.get(0),
                depth: row.get(1),
            },
            row.get(2),
        )
    }))
}

fn model_columns(model: &Option<ModelRef>) -> (Option<&'static str>, Option<&str>) {
    match model {
        Some(model) => (Some(columns::provider(model.provider)), Some(&model.id)),
        None => (None, None),
    }
}

fn to_model(provider: Option<String>, id: Option<String>) -> anyhow::Result<Option<ModelRef>> {
    Ok(match (provider, id) {
        (Some(provider), Some(id)) => Some(ModelRef {
            provider: columns::to_provider(&provider)?,
            id,
        }),
        _ => None,
    })
}

/// Upserts the `threads` row, pointing it at `head` (with the root of its
/// tree), and replaces its small tables. Never `INSERT OR REPLACE`: its
/// delete would cascade to the chat's small tables. A project removed since
/// is saved as none, here and in [`update_placement`]; removing it unfiled
/// the chat's row already.
pub(super) async fn write_thread(
    conn: &mut SqliteConnection,
    head: &ThreadRow,
    placement: &Placement,
    tip: Option<(Uuid, Uuid)>,
) -> anyhow::Result<()> {
    let (provider, model) = model_columns(&head.model);
    let (head_id, root_id) = tip.unzip();
    sqlx::query(
        "INSERT INTO threads (id, title, participant_id, archived, position, model_provider,
             model_id, project_mode, project_id, context_tokens, streamed_bytes,
             generation_ns, last_run_at, head_id, root_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, (SELECT id FROM projects WHERE id = ?), ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
             title = excluded.title, participant_id = excluded.participant_id,
             archived = excluded.archived, position = excluded.position,
             model_provider = excluded.model_provider, model_id = excluded.model_id,
             project_mode = excluded.project_mode, project_id = excluded.project_id,
             context_tokens = excluded.context_tokens,
             streamed_bytes = excluded.streamed_bytes, generation_ns = excluded.generation_ns,
             last_run_at = excluded.last_run_at, head_id = excluded.head_id,
             root_id = excluded.root_id",
    )
    .bind(head.id)
    .bind(&head.title)
    .bind(head.participant_id)
    .bind(placement.archived)
    .bind(placement.position)
    .bind(provider)
    .bind(model)
    .bind(columns::mode(head.project_mode))
    .bind(placement.project)
    .bind(head.context_tokens)
    .bind(head.streamed_bytes)
    .bind(head.generation_ns)
    .bind(head.last_run_at)
    .bind(head_id)
    .bind(root_id)
    .execute(&mut *conn)
    .await?;
    for delete in [
        "DELETE FROM thread_folders WHERE thread_id = ?",
        "DELETE FROM thread_profiles WHERE thread_id = ?",
        "DELETE FROM prompt_names WHERE thread_id = ?",
    ] {
        sqlx::query(delete)
            .bind(head.id)
            .execute(&mut *conn)
            .await?;
    }
    for (seq, (name, path)) in head.folders.iter().enumerate() {
        sqlx::query("INSERT INTO thread_folders VALUES (?, ?, ?, ?)")
            .bind(head.id)
            .bind(columns::index(seq, "folder")?)
            .bind(name)
            .bind(path)
            .execute(&mut *conn)
            .await?;
    }
    for profile in &head.profiles {
        sqlx::query("INSERT INTO thread_profiles VALUES (?, ?, ?, ?, ?)")
            .bind(head.id)
            .bind(profile.participant)
            .bind(&profile.name)
            .bind(&profile.picture)
            .bind(profile.appearance)
            .execute(&mut *conn)
            .await?;
    }
    for (participant, name) in &head.prompt_names {
        sqlx::query("INSERT INTO prompt_names VALUES (?, ?, ?)")
            .bind(head.id)
            .bind(participant)
            .bind(name)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Inserts `message` below `parent` in the tree rooted at `root`, with
/// everything it holds: its blocks or run, its run's entries and
/// compactions, its files' bytes and its search texts.
pub(super) async fn insert_message(
    conn: &mut SqliteConnection,
    parent: Option<Uuid>,
    root: Uuid,
    message: &NewMessage,
) -> anyhow::Result<()> {
    let kind = match &message.row {
        MessageRow::User { .. } => "user",
        MessageRow::Agent { .. } => "agent",
    };
    sqlx::query(
        "INSERT INTO messages (id, parent_id, depth, root_id, kind) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(message.row.id())
    .bind(parent)
    .bind(message.depth)
    .bind(root)
    .bind(kind)
    .execute(&mut *conn)
    .await?;
    match &message.row {
        MessageRow::User {
            id,
            blocks,
            comments,
        } => {
            for (block_seq, block) in blocks.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO prompt_blocks (id, message_id, seq, author, text)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(block.id)
                .bind(id)
                .bind(columns::index(block_seq, "block")?)
                .bind(block.author)
                .bind(&block.text)
                .execute(&mut *conn)
                .await?;
                for (attachment_seq, attachment) in block.attachments.iter().enumerate() {
                    sqlx::query("INSERT INTO block_attachments VALUES (?, ?, ?, ?, ?, ?, ?)")
                        .bind(block.id)
                        .bind(columns::index(attachment_seq, "attachment")?)
                        .bind(attachment.id)
                        .bind(&attachment.name)
                        .bind(columns::attachment_kind(attachment.kind))
                        .bind(attachment.size)
                        .bind(attachment.creator)
                        .execute(&mut *conn)
                        .await?;
                }
            }
            for (comment_seq, comment) in comments.iter().enumerate() {
                sqlx::query("INSERT INTO comments VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(comment.id)
                    .bind(id)
                    .bind(columns::index(comment_seq, "comment")?)
                    .bind(comment.author)
                    .bind(comment.target)
                    .bind(comment.range_start)
                    .bind(comment.range_end)
                    .bind(&comment.quote)
                    .bind(&comment.body)
                    .execute(&mut *conn)
                    .await?;
            }
        }
        MessageRow::Agent { id, run } => {
            let (outcome, error) = columns::outcome(&run.outcome);
            let (provider, model) = model_columns(&run.model);
            sqlx::query(
                "INSERT INTO agent_runs (message_id, comment_group_id, started_at, duration_ns,
                     tokens, transcript_start, outcome, error, unfinished, model_provider,
                     model_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(run.comment_group_id)
            .bind(run.started_at)
            .bind(run.duration_ns)
            .bind(run.tokens)
            .bind(run.transcript_start)
            .bind(outcome)
            .bind(error)
            .bind(&run.unfinished)
            .bind(provider)
            .bind(model)
            .execute(&mut *conn)
            .await?;
            // Entries before compactions: the trigger checks a compaction
            // against the run's entries.
            for (idx, record) in &message.entries {
                sqlx::query(
                    "INSERT INTO transcript_entries (run_id, idx, record) VALUES (?, ?, ?)",
                )
                .bind(id)
                .bind(idx)
                .bind(record)
                .execute(&mut *conn)
                .await?;
            }
            for (seq, (at_entries, record)) in message.compactions.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO transcript_compactions (run_id, seq, at_entries, record)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(id)
                .bind(columns::index(seq, "compaction")?)
                .bind(at_entries)
                .bind(record)
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    // Each file is written once, with the message attaching it.
    for (file, bytes) in &message.files {
        sqlx::query("INSERT INTO attachment_data (id, message_id, bytes) VALUES (?, ?, ?)")
            .bind(file)
            .bind(message.row.id())
            .bind(bytes)
            .execute(&mut *conn)
            .await?;
    }
    for (part, text) in message.search.iter().enumerate() {
        sqlx::query("INSERT INTO search_texts (message_id, part, text) VALUES (?, ?, ?)")
            .bind(message.row.id())
            .bind(columns::index(part, "search text")?)
            .bind(text)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Updates a saved chat's placement. Never inserts: a chat whose first save
/// failed has no row, and must not get an empty one.
pub(super) async fn update_placement(
    conn: &mut SqliteConnection,
    id: Uuid,
    placement: &Placement,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE threads SET archived = ?, position = ?,
             project_id = (SELECT id FROM projects WHERE id = ?)
         WHERE id = ?",
    )
    .bind(placement.archived)
    .bind(placement.position)
    .bind(placement.project)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Deletes a chat's row, then its messages from its head up, stopping at a
/// message another chat still needs: one with another child, or another
/// chat's head (a chat can end where another branches off). Each message
/// takes what it holds with it by cascade, its search index included
/// through `search_texts`' trigger. Returns the usage of the runs removed,
/// so shared runs stay counted with the chats keeping them.
pub(super) async fn delete(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> anyhow::Result<Vec<ActivityRow>> {
    let head: Option<Uuid> = sqlx::query_scalar("SELECT head_id FROM threads WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    sqlx::query("DELETE FROM threads WHERE id = ?")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    let mut removed = Vec::new();
    let mut next = head;
    while let Some(message) = next {
        let needed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE parent_id = ?1)
                 OR EXISTS (SELECT 1 FROM threads WHERE head_id = ?1)",
        )
        .bind(message)
        .fetch_one(&mut *conn)
        .await?;
        if needed {
            break;
        }
        if let Some(row) = sqlx::query(
            "SELECT started_at, duration_ns, tokens FROM agent_runs
             WHERE message_id = ? AND tokens > 0",
        )
        .bind(message)
        .fetch_optional(&mut *conn)
        .await?
        {
            removed.push(ActivityRow {
                at: row.get(0),
                duration_ns: row.get(1),
                tokens: row.get(2),
            });
        }
        next = sqlx::query_scalar("DELETE FROM messages WHERE id = ? RETURNING parent_id")
            .bind(message)
            .fetch_one(&mut *conn)
            .await?;
    }
    Ok(removed)
}

/// A chat's row and everything on its path, or `None` if it has no row.
/// Fails unless the path runs from the head down to the chat's root, with
/// every depth once, and its entries are the whole transcript in order.
pub(super) async fn load(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> anyhow::Result<Option<ChatRows>> {
    let Some(row) = sqlx::query(
        "SELECT title, participant_id, model_provider, model_id, project_mode, project_id,
                context_tokens, streamed_bytes, generation_ns, last_run_at, root_id
         FROM threads WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let project: Option<Uuid> = row.get(5);
    let root: Option<Uuid> = row.get(10);
    let mut thread = ThreadRow {
        id,
        title: row.get(0),
        participant_id: row.get(1),
        model: to_model(row.get(2), row.get(3))?,
        project_mode: columns::to_mode(row.get::<&str, _>(4))?,
        context_tokens: row.get(6),
        streamed_bytes: row.get(7),
        generation_ns: row.get(8),
        last_run_at: row.get::<Option<DateTime<Utc>>, _>(9),
        folders: Vec::new(),
        profiles: Vec::new(),
        prompt_names: Vec::new(),
    };
    thread.folders =
        sqlx::query_as("SELECT name, path FROM thread_folders WHERE thread_id = ? ORDER BY seq")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
    thread.profiles = sqlx::query(
        "SELECT participant, name, picture, appearance FROM thread_profiles
         WHERE thread_id = ? ORDER BY participant",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| ProfileRow {
        participant: row.get(0),
        name: row.get(1),
        picture: row.get(2),
        appearance: row.get(3),
    })
    .collect();
    thread.prompt_names = sqlx::query_as(
        "SELECT participant, name FROM prompt_names WHERE thread_id = ? ORDER BY participant",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;

    let mut attachments = HashMap::<Uuid, Vec<AttachmentRow>>::new();
    for row in sqlx::query(on_path!(
        "SELECT a.block_id, a.attachment_id, a.name, a.kind, a.size, a.creator FROM path p
         JOIN prompt_blocks b ON b.message_id = p.id
         JOIN block_attachments a ON a.block_id = b.id
         ORDER BY a.block_id, a.seq"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    {
        attachments
            .entry(row.get(0))
            .or_default()
            .push(AttachmentRow {
                id: row.get(1),
                name: row.get(2),
                kind: columns::to_attachment_kind(row.get::<&str, _>(3))?,
                size: row.get(4),
                creator: row.get(5),
            });
    }
    let mut blocks = HashMap::<Uuid, Vec<BlockRow>>::new();
    for row in sqlx::query(on_path!(
        "SELECT b.message_id, b.id, b.author, b.text FROM path p
         JOIN prompt_blocks b ON b.message_id = p.id
         ORDER BY b.message_id, b.seq"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    {
        let block_id: Uuid = row.get(1);
        blocks.entry(row.get(0)).or_default().push(BlockRow {
            id: block_id,
            author: row.get(2),
            text: row.get(3),
            attachments: attachments.remove(&block_id).unwrap_or_default(),
        });
    }
    let mut comments = HashMap::<Uuid, Vec<CommentRow>>::new();
    for row in sqlx::query(on_path!(
        "SELECT c.message_id, c.id, c.author, c.target_message_id, c.range_start, c.range_end,
                c.quote, c.body
         FROM path p JOIN comments c ON c.message_id = p.id
         ORDER BY c.message_id, c.seq"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    {
        comments.entry(row.get(0)).or_default().push(CommentRow {
            id: row.get(1),
            author: row.get(2),
            target: row.get(3),
            range_start: row.get(4),
            range_end: row.get(5),
            quote: row.get(6),
            body: row.get(7),
        });
    }
    let mut runs = HashMap::<Uuid, RunRow>::new();
    for row in sqlx::query(on_path!(
        "SELECT r.message_id, r.comment_group_id, r.started_at, r.duration_ns, r.tokens,
                r.transcript_start, r.outcome, r.error, r.unfinished, r.model_provider,
                r.model_id
         FROM path p JOIN agent_runs r ON r.message_id = p.id"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    {
        runs.insert(
            row.get(0),
            RunRow {
                comment_group_id: row.get(1),
                started_at: row.get(2),
                duration_ns: row.get(3),
                tokens: row.get(4),
                transcript_start: row.get(5),
                outcome: columns::to_outcome(row.get::<&str, _>(6), row.get(7))?,
                unfinished: row.get(8),
                model: to_model(row.get(9), row.get(10))?,
            },
        );
    }
    let mut messages = Vec::new();
    let mut path_root = None;
    for row in sqlx::query(on_path!(
        "SELECT m.id, m.kind, m.depth, m.parent_id FROM path p
         JOIN messages m ON m.id = p.id ORDER BY p.depth"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    {
        let message: Uuid = row.get(0);
        let depth: i64 = row.get(2);
        let parent: Option<Uuid> = row.get(3);
        anyhow::ensure!(
            depth == columns::index(messages.len(), "depth")?
                && parent.is_none() == messages.is_empty(),
            "message {message} is out of place on the chat's path"
        );
        path_root.get_or_insert(message);
        messages.push(match row.get::<&str, _>(1) {
            "user" => MessageRow::User {
                id: message,
                blocks: blocks.remove(&message).unwrap_or_default(),
                comments: comments.remove(&message).unwrap_or_default(),
            },
            "agent" => MessageRow::Agent {
                id: message,
                run: runs
                    .remove(&message)
                    .ok_or_else(|| anyhow::anyhow!("agent message {message} has no run"))?,
            },
            kind => anyhow::bail!("unknown message kind {kind:?}"),
        });
    }
    anyhow::ensure!(path_root == root, "the chat's path doesn't reach its root");
    let mut entries = Vec::new();
    for (idx, record) in sqlx::query_as::<_, (i64, String)>(on_path!(
        "SELECT e.idx, e.record FROM path p
         JOIN transcript_entries e ON e.run_id = p.id ORDER BY e.idx"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    {
        anyhow::ensure!(
            idx == columns::index(entries.len(), "entry")?,
            "the chat's transcript misses entry {}",
            entries.len()
        );
        entries.push(record);
    }
    let compactions = sqlx::query_as(on_path!(
        "SELECT c.at_entries, c.record FROM path p
         JOIN transcript_compactions c ON c.run_id = p.id ORDER BY p.depth, c.seq"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let files = sqlx::query_as::<_, (Uuid, Vec<u8>)>(on_path!(
        "SELECT f.id, f.bytes FROM path p JOIN attachment_data f ON f.message_id = p.id"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    Ok(Some(ChatRows {
        thread,
        project,
        messages,
        entries,
        compactions,
        files,
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SearchHit {
    pub(crate) thread_id: Uuid,
    pub(crate) text: String,
}

/// The texts containing `phrase`, ignoring case and diacritics, in each
/// chat's order. A match is a message, so it is a hit for every chat in the
/// message's tree. The trigram index needs three characters; a shorter
/// phrase finds nothing. `limit` caps the rows read.
pub(super) async fn search(
    pool: &SqlitePool,
    phrase: &str,
    limit: i64,
) -> anyhow::Result<Vec<SearchHit>> {
    if phrase.chars().count() < 3 {
        return Ok(Vec::new());
    }
    let quoted = format!("\"{}\"", phrase.replace('"', "\"\""));
    Ok(sqlx::query(
        "SELECT t.id, s.text FROM chat_search f
         JOIN search_texts s ON s.rowid = f.rowid
         JOIN messages m ON m.id = s.message_id
         JOIN threads t ON t.root_id = m.root_id
         WHERE f.chat_search MATCH ?
         ORDER BY t.id, m.depth, s.part
         LIMIT ?",
    )
    .bind(quoted)
    .bind(limit)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| SearchHit {
        thread_id: row.get(0),
        text: row.get(1),
    })
    .collect())
}

/// Makes chat `fork` end at `at`, a message of `source`'s path, with
/// `source`'s per-chat rows: what forking will do, for the storage tests
/// until it exists.
#[cfg(test)]
pub(super) async fn fork(
    conn: &mut SqliteConnection,
    source: Uuid,
    at: Uuid,
    fork: Uuid,
) -> anyhow::Result<()> {
    for statement in [
        "INSERT INTO threads (id, title, participant_id, archived, position, model_provider,
             model_id, project_mode, project_id, context_tokens, streamed_bytes, generation_ns,
             last_run_at, head_id, root_id)
         SELECT ?3, title, participant_id, archived, position, model_provider, model_id,
             project_mode, project_id, context_tokens, streamed_bytes, generation_ns,
             last_run_at, ?2, root_id
         FROM threads WHERE id = ?1",
        "INSERT INTO thread_folders SELECT ?3, seq, name, path FROM thread_folders
         WHERE thread_id = ?1",
        "INSERT INTO thread_profiles SELECT ?3, participant, name, picture, appearance
         FROM thread_profiles WHERE thread_id = ?1",
        "INSERT INTO prompt_names SELECT ?3, participant, name FROM prompt_names
         WHERE thread_id = ?1",
    ] {
        sqlx::query(statement)
            .bind(source)
            .bind(at)
            .bind(fork)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
