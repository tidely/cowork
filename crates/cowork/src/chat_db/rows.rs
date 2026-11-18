//! A chat as plain rows, both ways, with no IO: what a save writes, built
//! from a loaded [`Thread`] on the GPUI side, and what a load reads, made
//! back into one. Jobs hold only these, so only plain data crosses into
//! Tokio, and every number in them already fits its column.

use std::collections::HashMap;

use agent::{
    model::Envelope,
    persistence::record::{StoredCompaction, StoredEntry},
};
use anyhow::Context as _;
use chrono::{DateTime, Utc};
use draft::AttachmentId;
use gpui::AppContext;
use uuid::Uuid;

use super::columns;
use crate::{
    attachments::{FileAttachment, kind_from_protocol},
    chat_meta::ChatMeta,
    models::{ModelCatalog, ModelRef},
    project_folders::ProjectFolder,
    project_mode::ProjectMode,
    projects::ProjectId,
    protocol,
    search_palette::message_search_texts,
    thread::Thread,
    transcript::{ReplyPreview, Transcript, TranscriptRecord},
    usage::TokenActivity,
};

/// A chat's last message, if it has any: what a save moves the stored head
/// to.
pub(crate) fn head_of(thread: &Thread) -> Option<Uuid> {
    thread.timeline.last().map(timeline_id)
}

fn timeline_id(message: &crate::timeline::TimelineMessage) -> Uuid {
    match message {
        crate::timeline::TimelineMessage::User(message) => message.record.id,
        crate::timeline::TimelineMessage::Agent(message) => message.record.id,
    }
}

/// A message on a chat's path, by its depth there: its timeline index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PathPoint {
    pub(crate) id: Uuid,
    pub(crate) depth: i64,
}

impl PathPoint {
    /// Where `id` is in `thread`'s timeline, searched from the end, where
    /// the stored head usually is.
    pub(crate) fn find(thread: &Thread, id: Uuid) -> Option<Self> {
        let index = thread
            .timeline
            .iter()
            .rposition(|message| timeline_id(message) == id)?;
        Some(Self {
            id,
            depth: columns::index(index, "depth").ok()?,
        })
    }
}

/// Where a chat is listed: all a metadata save writes. It has no timeline,
/// so such a save can't change one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) archived: bool,
    pub(crate) position: i64,
    pub(crate) project: Option<Uuid>,
}

impl Placement {
    pub(crate) fn new(
        archived: bool,
        position: usize,
        project: Option<ProjectId>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            archived,
            position: columns::index(position, "position")?,
            project: project.map(ProjectId::as_uuid),
        })
    }
}

/// The `threads` row, apart from its placement and head, and the small
/// tables replaced with it on every save.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThreadRow {
    pub(crate) id: Uuid,
    pub(crate) title: String,
    pub(crate) participant_id: Uuid,
    pub(crate) model: Option<ModelRef>,
    pub(crate) project_mode: ProjectMode,
    pub(crate) context_tokens: Option<i64>,
    pub(crate) streamed_bytes: i64,
    pub(crate) generation_ns: i64,
    pub(crate) last_run_at: Option<DateTime<Utc>>,
    pub(crate) folders: Vec<(String, String)>,
    /// Sorted by participant.
    pub(crate) profiles: Vec<ProfileRow>,
    /// Sorted by participant.
    pub(crate) prompt_names: Vec<(Uuid, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProfileRow {
    pub(crate) participant: Uuid,
    pub(crate) name: Option<String>,
    pub(crate) picture: Option<Vec<u8>>,
    pub(crate) appearance: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActivityRow {
    pub(crate) at: DateTime<Utc>,
    pub(crate) duration_ns: i64,
    pub(crate) tokens: i64,
}

impl ActivityRow {
    pub(crate) fn of(activity: &TokenActivity) -> anyhow::Result<Self> {
        Ok(Self {
            at: columns::time(activity.at),
            duration_ns: columns::nanos(activity.duration, "turn duration")?,
            tokens: columns::int(activity.tokens, "turn tokens")?,
        })
    }

    pub(crate) fn into_activity(self) -> anyhow::Result<TokenActivity> {
        Ok(TokenActivity {
            at: columns::system_time(self.at),
            duration: columns::duration(self.duration_ns, "turn duration")?,
            tokens: columns::uint(self.tokens, "turn tokens")?,
        })
    }
}

impl ThreadRow {
    pub(crate) fn of(thread: &Thread) -> anyhow::Result<Self> {
        let mut profiles = thread
            .profiles
            .iter()
            .map(|(participant, profile)| {
                let profile = profile.to_protocol();
                ProfileRow {
                    participant: participant.as_uuid(),
                    name: profile.name,
                    picture: profile.picture,
                    appearance: profile.appearance.map(Uuid::from_bytes),
                }
            })
            .collect::<Vec<_>>();
        profiles.sort_by_key(|profile| profile.participant);
        let mut prompt_names = thread
            .prompt_names
            .iter()
            .map(|(participant, name)| (participant.as_uuid(), name.to_string()))
            .collect::<Vec<_>>();
        prompt_names.sort();
        Ok(Self {
            id: thread.instance_id,
            title: thread.summary.title.clone(),
            participant_id: thread.participant_id().as_uuid(),
            model: thread.model().cloned(),
            project_mode: thread.project_mode(),
            context_tokens: thread
                .context_tokens
                .map(|tokens| columns::int(tokens, "context tokens"))
                .transpose()?,
            streamed_bytes: columns::int(thread.streamed_bytes, "streamed bytes")?,
            generation_ns: columns::nanos(thread.generation_time(), "generation time")?,
            last_run_at: thread.last_activity().map(columns::time),
            // A folder whose path isn't UTF-8 can't be saved; the JSON files
            // couldn't hold one either.
            folders: thread
                .project_folders()
                .iter()
                .filter_map(|folder| {
                    let path = folder.path.as_ref()?.to_str()?;
                    Some((folder.name.to_string(), path.to_owned()))
                })
                .collect(),
            profiles,
            prompt_names,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MessageRow {
    User {
        id: Uuid,
        blocks: Vec<BlockRow>,
        /// In the order the agent numbers them.
        comments: Vec<CommentRow>,
    },
    Agent {
        id: Uuid,
        run: RunRow,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BlockRow {
    pub(crate) id: Uuid,
    pub(crate) author: Uuid,
    pub(crate) text: String,
    pub(crate) attachments: Vec<AttachmentRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttachmentRow {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) kind: protocol::AttachmentKind,
    pub(crate) size: i64,
    pub(crate) creator: Uuid,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommentRow {
    pub(crate) id: Uuid,
    pub(crate) author: Uuid,
    pub(crate) target: Uuid,
    pub(crate) range_start: i64,
    pub(crate) range_end: i64,
    pub(crate) quote: String,
    pub(crate) body: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunRow {
    pub(crate) comment_group_id: Option<Uuid>,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) first_content_at: Option<DateTime<Utc>>,
    pub(crate) duration_ns: i64,
    pub(crate) tokens: i64,
    pub(crate) transcript_start: i64,
    pub(crate) outcome: protocol::RunOutcome,
    /// The reply a stopped or failed run had sampled so far, which the
    /// transcript never got: an [`Envelope`], as JSON.
    pub(crate) unfinished: Option<String>,
    pub(crate) model: Option<ModelRef>,
}

impl MessageRow {
    pub(crate) fn id(&self) -> Uuid {
        match self {
            Self::User { id, .. } | Self::Agent { id, .. } => *id,
        }
    }

    fn attachments(&self) -> impl Iterator<Item = &AttachmentRow> {
        let blocks = match self {
            Self::User { blocks, .. } => blocks.as_slice(),
            Self::Agent { .. } => &[],
        };
        blocks.iter().flat_map(|block| &block.attachments)
    }

    fn from_protocol(message: protocol::TimelineMessage) -> anyhow::Result<Self> {
        Ok(match message {
            protocol::TimelineMessage::User(message) => Self::User {
                id: Uuid::from_bytes(message.id),
                blocks: message
                    .blocks
                    .into_iter()
                    .map(|block| {
                        Ok(BlockRow {
                            id: Uuid::from_bytes(block.id),
                            author: Uuid::from_bytes(block.author),
                            text: block.text,
                            attachments: block
                                .attachments
                                .into_iter()
                                .map(|attachment| {
                                    Ok(AttachmentRow {
                                        id: Uuid::from_bytes(attachment.id),
                                        name: attachment.name,
                                        kind: attachment.kind,
                                        size: columns::int(attachment.size, "attachment size")?,
                                        creator: Uuid::from_bytes(attachment.creator),
                                    })
                                })
                                .collect::<anyhow::Result<_>>()?,
                        })
                    })
                    .collect::<anyhow::Result<_>>()?,
                comments: message
                    .comments
                    .into_iter()
                    .map(|comment| {
                        Ok(CommentRow {
                            id: Uuid::from_bytes(comment.id),
                            author: Uuid::from_bytes(comment.author),
                            target: Uuid::from_bytes(comment.reference.message_id),
                            range_start: columns::index(comment.reference.range.start, "range")?,
                            range_end: columns::index(comment.reference.range.end, "range")?,
                            quote: comment.reference.quote,
                            body: comment.body,
                        })
                    })
                    .collect::<anyhow::Result<_>>()?,
            },
            protocol::TimelineMessage::Agent(run) => {
                let (outcome, duration) = match run.run {
                    protocol::AgentRun::Ended { outcome, duration } => (outcome, duration),
                    // Chats are saved between runs; one still going here
                    // would not go on after a restart.
                    protocol::AgentRun::Generating => {
                        (protocol::RunOutcome::Stopped, std::time::Duration::ZERO)
                    }
                };
                let unfinished = ReplyPreview::of(&run.pending_events)
                    .partial()
                    .and_then(|reply| Envelope::encode_message(&reply).ok())
                    .map(|envelope| serde_json::to_string(&envelope))
                    .transpose()?;
                Self::Agent {
                    id: Uuid::from_bytes(run.id),
                    run: RunRow {
                        comment_group_id: run.comment_group_id.map(Uuid::from_bytes),
                        started_at: columns::time(run.started_at),
                        first_content_at: run.first_content_at.map(columns::time),
                        duration_ns: columns::nanos(duration, "run duration")?,
                        tokens: columns::int(run.tokens, "run tokens")?,
                        transcript_start: columns::int(run.start.0, "run start")?,
                        outcome,
                        unfinished,
                        model: run.model,
                    },
                }
            }
        })
    }

    fn into_protocol(self) -> anyhow::Result<protocol::TimelineMessage> {
        Ok(match self {
            Self::User {
                id,
                blocks,
                comments,
            } => protocol::TimelineMessage::User(protocol::UserMessage {
                id: id.into_bytes(),
                blocks: blocks
                    .into_iter()
                    .map(|block| {
                        Ok(protocol::PromptBlock {
                            id: block.id.into_bytes(),
                            author: block.author.into_bytes(),
                            text: block.text,
                            attachments: block
                                .attachments
                                .into_iter()
                                .map(|attachment| {
                                    Ok(protocol::AttachmentRef {
                                        id: attachment.id.into_bytes(),
                                        name: attachment.name,
                                        kind: attachment.kind,
                                        size: columns::uint(attachment.size, "attachment size")?,
                                        creator: attachment.creator.into_bytes(),
                                    })
                                })
                                .collect::<anyhow::Result<_>>()?,
                        })
                    })
                    .collect::<anyhow::Result<_>>()?,
                comments: comments
                    .into_iter()
                    .map(|comment| {
                        Ok(protocol::UserComment {
                            id: comment.id.into_bytes(),
                            author: comment.author.into_bytes(),
                            reference: protocol::CommentReference {
                                message_id: comment.target.into_bytes(),
                                range: columns::count(comment.range_start, "range")?
                                    ..columns::count(comment.range_end, "range")?,
                                quote: comment.quote,
                            },
                            body: comment.body,
                        })
                    })
                    .collect::<anyhow::Result<_>>()?,
            }),
            Self::Agent { id, run } => {
                let pending_events = match &run.unfinished {
                    Some(reply) => ReplyPreview::previews_of(
                        &serde_json::from_str::<Envelope>(reply)
                            .context("invalid unfinished reply")?
                            .decode_message()
                            .context("invalid unfinished reply")?,
                    ),
                    None => Vec::new(),
                };
                protocol::TimelineMessage::Agent(protocol::AgentMessage {
                    id: id.into_bytes(),
                    comment_group_id: run.comment_group_id.map(Uuid::into_bytes),
                    started_at: columns::system_time(run.started_at),
                    first_content_at: run.first_content_at.map(columns::system_time),
                    start: crate::transcript::TranscriptIndex(columns::uint(
                        run.transcript_start,
                        "run start",
                    )?),
                    pending_events,
                    run: protocol::AgentRun::Ended {
                        outcome: run.outcome,
                        duration: columns::duration(run.duration_ns, "run duration")?,
                    },
                    tokens: columns::uint(run.tokens, "run tokens")?,
                    model: run.model,
                    awaiting_approval: None,
                })
            }
        })
    }
}

/// A message to save with everything it holds: the texts the search palette
/// looks in, the bytes of the files it attaches and, for an agent message,
/// the transcript entries and compactions its run committed.
#[derive(Debug)]
pub(crate) struct NewMessage {
    /// Its timeline index.
    pub(crate) depth: i64,
    pub(crate) row: MessageRow,
    pub(crate) search: Vec<String>,
    /// `StoredEntry` JSON, with each entry's index.
    pub(crate) entries: Vec<(i64, String)>,
    /// The number of entries each was accepted at, and `StoredCompaction`
    /// JSON, in order.
    pub(crate) compactions: Vec<(i64, String)>,
    pub(crate) files: Vec<(Uuid, Vec<u8>)>,
}

impl NewMessage {
    /// `thread`'s messages from timeline index `start` on. Agent message k
    /// owns the entries from its start to the next run's (the transcript's
    /// end for the last), and the compactions accepted after more than its
    /// start and at most that many entries.
    pub(crate) fn after(thread: &Thread, start: usize) -> anyhow::Result<Vec<Self>> {
        use crate::timeline::TimelineMessage;

        let starts = thread
            .timeline
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::Agent(message) => Some(message.record.start.0 as usize),
                TimelineMessage::User(_) => None,
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            starts
                .first()
                .map_or(thread.transcript.len() == 0, |first| *first == 0),
            "transcript entries before the first run"
        );
        let files = &thread.draft().files;
        let mut run = 0;
        let mut messages = Vec::new();
        for (index, message) in thread.timeline.iter().enumerate() {
            let owned = match message {
                TimelineMessage::Agent(_) => {
                    run += 1;
                    let start = starts[run - 1];
                    let end = starts.get(run).copied().unwrap_or(thread.transcript.len());
                    Some(start..end.max(start))
                }
                TimelineMessage::User(_) => None,
            };
            if index < start {
                continue;
            }
            let row = MessageRow::from_protocol(message.to_protocol())?;
            let (entries, compactions) = match owned {
                Some(owned) => {
                    let first = owned.start;
                    let (entries, compactions) = thread.transcript.run_records(owned);
                    (
                        entries
                            .iter()
                            .enumerate()
                            .map(|(offset, entry)| {
                                Ok((
                                    columns::index(first + offset, "entry")?,
                                    serde_json::to_string(entry)?,
                                ))
                            })
                            .collect::<anyhow::Result<_>>()?,
                        compactions
                            .iter()
                            .map(|(at, compaction)| {
                                Ok((
                                    columns::int(*at, "compaction")?,
                                    serde_json::to_string(compaction)?,
                                ))
                            })
                            .collect::<anyhow::Result<_>>()?,
                    )
                }
                None => (Vec::new(), Vec::new()),
            };
            let files = row
                .attachments()
                .filter_map(|attachment| {
                    let file = files.get(&AttachmentId::from_uuid(attachment.id))?;
                    Some((attachment.id, file.bytes().to_vec()))
                })
                .collect();
            messages.push(Self {
                depth: columns::index(index, "depth")?,
                search: message_search_texts(message)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                row,
                entries,
                compactions,
                files,
            });
        }
        Ok(messages)
    }
}

/// Everything saved of a chat, as a load reads it. Only a load makes one,
/// so no code can build a `Thread` with a saved chat's id out of anything
/// less: saving such a placeholder would be refused, but it would show an
/// empty chat.
pub(crate) struct ChatRows {
    pub(crate) thread: ThreadRow,
    pub(crate) project: Option<Uuid>,
    /// The path from the root to the head, in order.
    pub(crate) messages: Vec<MessageRow>,
    pub(crate) entries: Vec<String>,
    pub(crate) compactions: Vec<(i64, String)>,
    pub(crate) files: HashMap<Uuid, Vec<u8>>,
}

impl ChatRows {
    pub(crate) fn head(&self) -> Option<Uuid> {
        self.messages.last().map(MessageRow::id)
    }

    #[cfg(test)]
    pub(crate) fn of_meta(meta: &ChatMeta) -> Self {
        Self {
            thread: ThreadRow {
                id: meta.id,
                title: meta.title.clone(),
                participant_id: Uuid::new_v4(),
                model: None,
                project_mode: ProjectMode::default(),
                context_tokens: None,
                streamed_bytes: 0,
                generation_ns: 0,
                last_run_at: None,
                folders: Vec::new(),
                profiles: Vec::new(),
                prompt_names: Vec::new(),
            },
            project: meta.project.map(ProjectId::as_uuid),
            messages: Vec::new(),
            entries: Vec::new(),
            compactions: Vec::new(),
            files: HashMap::new(),
        }
    }

    /// The chat, checked as a host's snapshot is, with `models` to run. It
    /// comes back with an empty draft, unshared.
    pub(crate) fn into_thread(
        self,
        models: ModelCatalog,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<Thread> {
        let Self {
            thread: head,
            project,
            messages,
            entries,
            compactions,
            files: mut bytes,
        } = self;
        let mut files = HashMap::new();
        for attachment in messages.iter().flat_map(MessageRow::attachments) {
            let Some(bytes) = bytes.remove(&attachment.id) else {
                continue;
            };
            match FileAttachment::from_bytes(
                attachment.name.clone(),
                kind_from_protocol(attachment.kind),
                bytes,
            ) {
                Ok(file) => {
                    files.insert(AttachmentId::from_uuid(attachment.id), file);
                }
                // The message still shows the file, without its contents.
                Err(error) => eprintln!("could not read attachment {}: {error:#}", attachment.id),
            }
        }
        let transcript = Transcript::from_records(
            entries
                .iter()
                .map(|entry| serde_json::from_str::<StoredEntry>(entry))
                .collect::<Result<Vec<_>, _>>()
                .context("invalid transcript entry")?,
            compactions
                .iter()
                .map(|(at, compaction)| {
                    Ok((
                        columns::uint(*at, "compaction")?,
                        serde_json::from_str::<StoredCompaction>(compaction)
                            .context("invalid compaction")?,
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        )?;
        let messages = messages
            .into_iter()
            .map(MessageRow::into_protocol)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let welcome = protocol::Welcome {
            participant_id: head.participant_id.into_bytes(),
            thread: protocol::ThreadSnapshot {
                submission_sequence: messages
                    .iter()
                    .filter(|message| matches!(message, protocol::TimelineMessage::User(_)))
                    .count() as u64,
                pending: Vec::new(),
                id: head.id.into_bytes(),
                title: head.title,
                participants: Vec::new(),
                profiles: head
                    .profiles
                    .into_iter()
                    .map(|profile| {
                        (
                            profile.participant.into_bytes(),
                            protocol::Profile {
                                name: profile.name,
                                picture: profile.picture,
                                appearance: profile.appearance.map(Uuid::into_bytes),
                            },
                        )
                    })
                    .collect(),
                models,
                model: head.model,
                peer_permissions: Default::default(),
                context_tokens: head
                    .context_tokens
                    .map(|tokens| columns::uint(tokens, "context tokens"))
                    .transpose()?,
                compacting: false,
                streamed_bytes: columns::uint(head.streamed_bytes, "streamed bytes")?,
                messages,
                transcript: protocol::Json::from_value(&TranscriptRecord::default())?,
                prompt_names: head
                    .prompt_names
                    .into_iter()
                    .map(|(participant, name)| (participant.into_bytes(), name))
                    .collect(),
                project_folders: head.folders.iter().map(|(name, _)| name.clone()).collect(),
                project_mode: head.project_mode,
            },
            draft: ::draft::Draft::new().encode_state(),
            draft_generation: 0,
            presence: Vec::new(),
            stored_attachments: files.keys().map(|id| id.as_uuid().into_bytes()).collect(),
        };
        let prepared = Thread::prepare_snapshot(welcome, transcript)?;
        let folders = head
            .folders
            .into_iter()
            .map(|(name, path)| ProjectFolder {
                name: name.into(),
                path: Some(path.into()),
            })
            .collect();
        let mut thread = Thread::from_saved(prepared, folders, files, cx);
        thread.set_project(project.map(ProjectId::from_uuid));
        Ok(thread)
    }
}

pub(crate) struct MetaRow {
    pub(crate) id: Uuid,
    pub(crate) title: String,
    pub(crate) archived: bool,
    pub(crate) project: Option<Uuid>,
    pub(crate) generation_ns: i64,
    pub(crate) last_run_at: Option<DateTime<Utc>>,
}

impl MetaRow {
    pub(crate) fn into_meta(self) -> anyhow::Result<ChatMeta> {
        Ok(ChatMeta {
            id: self.id,
            title: self.title,
            project: self.project.map(ProjectId::from_uuid),
            generation_time: columns::duration(self.generation_ns, "generation time")?,
            last_run_at: self.last_run_at.map(columns::system_time),
        })
    }
}
