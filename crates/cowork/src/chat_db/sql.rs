//! The chats' SQL: opening the database, the extent check, writing and
//! reading a chat's rows, placement updates, deletion and search. The rest
//! of the app's state is `app_state`'s. Functions take a
//! connection (a transaction, for writes) and plain rows; none decide what
//! to save. Enum columns go through `columns`.

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
        ActivityRow, AttachmentRow, BlockRow, ChatRows, CommentRow, Extent, Head, MessageRow,
        MetaRow, NewMessage, Placement, ProfileRow, RunRow,
    },
};
use crate::{models::ModelRef, transcript::TranscriptExtent};

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

/// Every chat's row, archived ones last, each list in its saved order, with
/// its token activity.
pub(super) async fn read_metas(
    pool: &SqlitePool,
) -> anyhow::Result<Vec<(MetaRow, Vec<ActivityRow>)>> {
    let mut activity = HashMap::<Uuid, Vec<ActivityRow>>::new();
    for row in sqlx::query(
        "SELECT thread_id, at, duration_ns, tokens FROM token_activity ORDER BY thread_id, seq",
    )
    .fetch_all(pool)
    .await?
    {
        activity.entry(row.get(0)).or_default().push(ActivityRow {
            at: row.get(1),
            duration_ns: row.get(2),
            tokens: row.get(3),
        });
    }
    let rows = sqlx::query(
        "SELECT id, title, archived, project_id, tokens_used, generation_ns, last_run_at
         FROM threads ORDER BY archived, position, unixepoch(last_run_at, 'subsec') DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let id: Uuid = row.get(0);
            let meta = MetaRow {
                id,
                title: row.get(1),
                archived: row.get(2),
                project: row.get(3),
                tokens_used: row.get(4),
                generation_ns: row.get(5),
                last_run_at: row.get(6),
            };
            (meta, activity.remove(&id).unwrap_or_default())
        })
        .collect())
}

/// What the database holds of a chat, as counts: three `max` lookups on
/// primary keys.
pub(super) async fn extent(conn: &mut SqliteConnection, id: Uuid) -> anyhow::Result<Extent> {
    let row = sqlx::query(
        "SELECT
             (SELECT coalesce(max(seq) + 1, 0) FROM messages WHERE thread_id = ?1),
             (SELECT coalesce(max(idx) + 1, 0) FROM transcript_entries WHERE thread_id = ?1),
             (SELECT coalesce(max(seq) + 1, 0) FROM transcript_compactions WHERE thread_id = ?1)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Extent {
        messages: columns::count(row.get(0), "messages")?,
        transcript: TranscriptExtent {
            entries: columns::count(row.get(1), "entries")?,
            compactions: columns::count(row.get(2), "compactions")?,
        },
    })
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

/// Upserts the `threads` row and replaces its small tables. Never
/// `INSERT OR REPLACE`: its delete would cascade to the whole chat. A
/// project removed since is saved as none, here and in
/// [`update_placement`]; removing it unfiled the chat's row already.
pub(super) async fn write_head(
    conn: &mut SqliteConnection,
    head: &Head,
    placement: &Placement,
) -> anyhow::Result<()> {
    let (provider, model) = model_columns(&head.model);
    sqlx::query(
        "INSERT INTO threads (id, title, participant_id, archived, position, model_provider,
             model_id, project_mode, project_id, tokens_used, context_tokens, streamed_bytes,
             generation_ns, last_run_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, (SELECT id FROM projects WHERE id = ?), ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
             title = excluded.title, participant_id = excluded.participant_id,
             archived = excluded.archived, position = excluded.position,
             model_provider = excluded.model_provider, model_id = excluded.model_id,
             project_mode = excluded.project_mode, project_id = excluded.project_id,
             tokens_used = excluded.tokens_used, context_tokens = excluded.context_tokens,
             streamed_bytes = excluded.streamed_bytes, generation_ns = excluded.generation_ns,
             last_run_at = excluded.last_run_at",
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
    .bind(head.tokens_used)
    .bind(head.context_tokens)
    .bind(head.streamed_bytes)
    .bind(head.generation_ns)
    .bind(head.last_run_at)
    .execute(&mut *conn)
    .await?;
    for delete in [
        "DELETE FROM thread_folders WHERE thread_id = ?",
        "DELETE FROM thread_profiles WHERE thread_id = ?",
        "DELETE FROM prompt_names WHERE thread_id = ?",
        "DELETE FROM token_activity WHERE thread_id = ?",
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
    for (seq, activity) in head.token_activity.iter().enumerate() {
        sqlx::query("INSERT INTO token_activity VALUES (?, ?, ?, ?, ?)")
            .bind(head.id)
            .bind(columns::index(seq, "turn")?)
            .bind(activity.at)
            .bind(activity.duration_ns)
            .bind(activity.tokens)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Inserts `message` at `seq` with everything under it and its search
/// texts.
pub(super) async fn insert_message(
    conn: &mut SqliteConnection,
    thread: Uuid,
    seq: usize,
    message: &NewMessage,
) -> anyhow::Result<()> {
    let seq = columns::index(seq, "message")?;
    let kind = match &message.row {
        MessageRow::User { .. } => "user",
        MessageRow::Agent { .. } => "agent",
    };
    sqlx::query("INSERT INTO messages (thread_id, id, seq, kind) VALUES (?, ?, ?, ?)")
        .bind(thread)
        .bind(message.row.id())
        .bind(seq)
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
                sqlx::query("INSERT INTO prompt_blocks VALUES (?, ?, ?, ?, ?, ?)")
                    .bind(thread)
                    .bind(id)
                    .bind(columns::index(block_seq, "block")?)
                    .bind(block.id)
                    .bind(block.author)
                    .bind(&block.text)
                    .execute(&mut *conn)
                    .await?;
                for (attachment_seq, attachment) in block.attachments.iter().enumerate() {
                    sqlx::query("INSERT INTO block_attachments VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                        .bind(thread)
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
                sqlx::query("INSERT INTO comments VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(thread)
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
            sqlx::query("INSERT INTO agent_runs VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(thread)
                .bind(id)
                .bind(run.comment_group_id)
                .bind(run.started_at)
                .bind(run.duration_ns)
                .bind(run.transcript_start)
                .bind(outcome)
                .bind(error)
                .bind(&run.unfinished)
                .bind(provider)
                .bind(model)
                .execute(&mut *conn)
                .await?;
        }
    }
    for (part, text) in message.search.iter().enumerate() {
        sqlx::query(
            "INSERT INTO search_texts (thread_id, message_seq, part, text) VALUES (?, ?, ?, ?)",
        )
        .bind(thread)
        .bind(seq)
        .bind(columns::index(part, "search text")?)
        .bind(text)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(super) async fn insert_entry(
    conn: &mut SqliteConnection,
    thread: Uuid,
    idx: usize,
    record: &str,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO transcript_entries VALUES (?, ?, ?)")
        .bind(thread)
        .bind(columns::index(idx, "entry")?)
        .bind(record)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(super) async fn insert_compaction(
    conn: &mut SqliteConnection,
    thread: Uuid,
    seq: usize,
    at_entries: i64,
    record: &str,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO transcript_compactions VALUES (?, ?, ?, ?)")
        .bind(thread)
        .bind(columns::index(seq, "compaction")?)
        .bind(at_entries)
        .bind(record)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Writes a file's bytes unless they are already stored: each is written
/// once.
pub(super) async fn insert_file(
    conn: &mut SqliteConnection,
    thread: Uuid,
    id: Uuid,
    bytes: &[u8],
) -> anyhow::Result<()> {
    sqlx::query("INSERT OR IGNORE INTO attachment_data VALUES (?, ?, ?)")
        .bind(thread)
        .bind(id)
        .bind(bytes)
        .execute(&mut *conn)
        .await?;
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

/// Deletes a chat: everything cascades from its `threads` row, its search
/// index included, through `search_texts`' trigger.
pub(super) async fn delete(conn: &mut SqliteConnection, id: Uuid) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM threads WHERE id = ?")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(super) async fn load(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> anyhow::Result<Option<ChatRows>> {
    let Some(row) = sqlx::query(
        "SELECT title, participant_id, model_provider, model_id, project_mode, project_id,
                tokens_used, context_tokens, streamed_bytes, generation_ns, last_run_at
         FROM threads WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let project: Option<Uuid> = row.get(5);
    let mut head = Head {
        id,
        title: row.get(0),
        participant_id: row.get(1),
        model: to_model(row.get(2), row.get(3))?,
        project_mode: columns::to_mode(row.get::<&str, _>(4))?,
        tokens_used: row.get(6),
        context_tokens: row.get(7),
        streamed_bytes: row.get(8),
        generation_ns: row.get(9),
        last_run_at: row.get::<Option<DateTime<Utc>>, _>(10),
        folders: Vec::new(),
        profiles: Vec::new(),
        prompt_names: Vec::new(),
        token_activity: Vec::new(),
    };
    head.folders =
        sqlx::query_as("SELECT name, path FROM thread_folders WHERE thread_id = ? ORDER BY seq")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
    head.profiles = sqlx::query(
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
    head.prompt_names = sqlx::query_as(
        "SELECT participant, name FROM prompt_names WHERE thread_id = ? ORDER BY participant",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    head.token_activity = sqlx::query(
        "SELECT at, duration_ns, tokens FROM token_activity WHERE thread_id = ? ORDER BY seq",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| ActivityRow {
        at: row.get(0),
        duration_ns: row.get(1),
        tokens: row.get(2),
    })
    .collect();

    let mut attachments = HashMap::<Uuid, Vec<AttachmentRow>>::new();
    for row in sqlx::query(
        "SELECT block_id, attachment_id, name, kind, size, creator FROM block_attachments
         WHERE thread_id = ? ORDER BY block_id, seq",
    )
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
    for row in sqlx::query(
        "SELECT message_id, id, author, text FROM prompt_blocks
         WHERE thread_id = ? ORDER BY message_id, seq",
    )
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
    for row in sqlx::query(
        "SELECT message_id, id, author, target_message_id, range_start, range_end, quote, body
         FROM comments WHERE thread_id = ? ORDER BY message_id, seq",
    )
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
    for row in sqlx::query(
        "SELECT message_id, comment_group_id, started_at, duration_ns, transcript_start, outcome,
                error, unfinished, model_provider, model_id
         FROM agent_runs WHERE thread_id = ?",
    )
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
                transcript_start: row.get(4),
                outcome: columns::to_outcome(row.get::<&str, _>(5), row.get(6))?,
                unfinished: row.get(7),
                model: to_model(row.get(8), row.get(9))?,
            },
        );
    }
    let mut messages = Vec::new();
    for row in sqlx::query("SELECT id, kind FROM messages WHERE thread_id = ? ORDER BY seq")
        .bind(id)
        .fetch_all(&mut *conn)
        .await?
    {
        let message: Uuid = row.get(0);
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
    let entries = sqlx::query_scalar(
        "SELECT record FROM transcript_entries WHERE thread_id = ? ORDER BY idx",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let compactions = sqlx::query_as(
        "SELECT at_entries, record FROM transcript_compactions WHERE thread_id = ? ORDER BY seq",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let files = sqlx::query_as::<_, (Uuid, Vec<u8>)>(
        "SELECT id, bytes FROM attachment_data WHERE thread_id = ?",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    Ok(Some(ChatRows {
        head,
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
/// chat's order. The trigram index needs three characters; a shorter phrase
/// finds nothing. `limit` caps the rows read.
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
        "SELECT s.thread_id, s.text FROM chat_search f
         JOIN search_texts s ON s.rowid = f.rowid
         WHERE f.chat_search MATCH ?
         ORDER BY s.thread_id, s.message_seq, s.part
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
