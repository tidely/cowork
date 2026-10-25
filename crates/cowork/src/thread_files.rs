//! Saving the user's own threads, one folder per thread in
//! `~/.cowork/threads`, and reading them back when Cowork starts:
//!
//! ```text
//! <thread id>/thread.json       the thread: its title, settings, and messages
//! <thread id>/messages.jsonl    its transcript, one record per line, only appended to
//! <thread id>/attachments/<id>  the files its messages attach
//! <thread id>/scratch/          its commands' scratch folder; see `sandbox::settings`
//! ```
//!
//! Only the host saves a thread; a joined one is its host's. A thread is
//! saved at turn boundaries: when a run ends, when it is archived or
//! restored, and when Cowork quits, never during a run, so every tool call
//! in its transcript has its result. Its draft, pending submissions, and
//! sharing are not saved: it comes back unshared, with an empty draft.
//!
//! The files have types of their own rather than the wire's. Transcript
//! records are the agent's versioned ones ([`TranscriptLine`]); the rest is
//! Cowork's, converted only in memory to the snapshot a joining collaborator
//! gets, so a saved thread is checked as a host's snapshot is. Files of
//! another format version are neither read nor overwritten.
//!
//! Files are written on a thread of their own, in the order they were
//! saved. `thread.json` is replaced whole once everything it refers to is
//! written, and records how much of the message log it holds, so a save
//! cut short leaves the thread as it was saved before.
//!
//! One Cowork instance keeps the threads, holding `.lock`. Another normal
//! launch fails rather than opening unsaved threads. An explicitly ephemeral
//! instance skips the lock and keeps its threads' folders in a temporary
//! folder it removes when it quits.

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Seek as _, SeekFrom, Write as _},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, SystemTime},
};

use agent::model::Envelope;
use anyhow::Context as _;
use base64::Engine as _;
use draft::AttachmentId;
use futures::channel::oneshot;
use gpui::{App, AppContext, Context, Entity};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Cowork,
    attachments::{FileAttachment, kind_from_protocol},
    models::{ModelCatalog, ModelRef},
    project_folders::ProjectFolder,
    project_mode::ProjectMode,
    projects::ProjectId,
    protocol,
    thread::{Thread, ThreadOwnership},
    transcript::{ReplyPreview, Transcript, TranscriptExtent, TranscriptLine, TranscriptRecord},
    usage::TokenActivity,
};

/// The format of `thread.json` and `messages.jsonl`. 2 records each run's
/// model.
const FILE_VERSION: u32 = 2;
const THREAD_FILE: &str = "thread.json";
const MESSAGES_FILE: &str = "messages.jsonl";
const ATTACHMENTS_DIR: &str = "attachments";
const LOCK_FILE: &str = ".lock";

/// Where threads are saved: next to Cowork's other files.
pub(crate) fn threads_dir() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".cowork").join("threads"))
}

/// `thread.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThreadFile {
    version: u32,
    id: Uuid,
    title: String,
    /// Who the user is in the thread, as its messages name them.
    participant_id: Uuid,
    archived: bool,
    /// Where it was in the sidebar's recents, or in the archive, when saved.
    position: usize,
    model: Option<ModelRef>,
    project_folders: Vec<SavedFolder>,
    project_mode: ProjectMode,
    /// The sidebar project the thread is filed under; see `projects.rs`.
    /// Missing in files saved before projects, which read as none, so it
    /// needs no new format.
    #[serde(default)]
    project: Option<ProjectId>,
    /// Everyone who joined while it was shared, so their messages keep
    /// their names.
    profiles: Vec<SavedProfile>,
    prompt_names: Vec<(Uuid, String)>,
    tokens_used: u64,
    token_activity: Vec<TokenActivity>,
    context_tokens: Option<u64>,
    streamed_bytes: u64,
    /// How many bytes of `messages.jsonl` this save holds. Any after them
    /// are of a save that never finished.
    messages_bytes: u64,
    messages: Vec<SavedMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedFolder {
    name: String,
    path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedProfile {
    participant: Uuid,
    name: Option<String>,
    /// A square JPEG, in base64.
    picture: Option<String>,
    appearance: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SavedMessage {
    User(SavedUserMessage),
    Agent(SavedRun),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedUserMessage {
    id: Uuid,
    blocks: Vec<SavedBlock>,
    comments: Vec<SavedComment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedBlock {
    id: Uuid,
    author: Uuid,
    text: String,
    attachments: Vec<SavedAttachment>,
}

/// A file a block attaches, whose bytes are in `attachments/<id>`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedAttachment {
    id: Uuid,
    name: String,
    kind: SavedKind,
    size: u64,
    creator: Uuid,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SavedKind {
    Text,
    Png,
    Jpeg,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedComment {
    id: Uuid,
    author: Uuid,
    /// The message it comments on, and the UTF-8 byte range of its text.
    message_id: Uuid,
    range: Range<usize>,
    quote: String,
    body: String,
}

/// An agent run, which had ended when it was saved.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedRun {
    id: Uuid,
    comment_group_id: Option<Uuid>,
    started_at: SystemTime,
    duration: Duration,
    /// Where the run starts in the transcript.
    start: u64,
    outcome: SavedOutcome,
    /// The reply a stopped or failed run had sampled so far, which the
    /// transcript never got.
    unfinished: Option<Envelope>,
    /// The model that wrote the reply; `None` when no model did.
    model: Option<ModelRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SavedOutcome {
    Completed,
    Stopped,
    Failed(String),
}

impl From<protocol::AttachmentKind> for SavedKind {
    fn from(kind: protocol::AttachmentKind) -> Self {
        match kind {
            protocol::AttachmentKind::Text => Self::Text,
            protocol::AttachmentKind::Png => Self::Png,
            protocol::AttachmentKind::Jpeg => Self::Jpeg,
        }
    }
}

impl From<SavedKind> for protocol::AttachmentKind {
    fn from(kind: SavedKind) -> Self {
        match kind {
            SavedKind::Text => Self::Text,
            SavedKind::Png => Self::Png,
            SavedKind::Jpeg => Self::Jpeg,
        }
    }
}

impl SavedMessage {
    fn from_protocol(message: protocol::TimelineMessage) -> Self {
        match message {
            protocol::TimelineMessage::User(message) => Self::User(SavedUserMessage {
                id: Uuid::from_bytes(message.id),
                blocks: message
                    .blocks
                    .into_iter()
                    .map(|block| SavedBlock {
                        id: Uuid::from_bytes(block.id),
                        author: Uuid::from_bytes(block.author),
                        text: block.text,
                        attachments: block
                            .attachments
                            .into_iter()
                            .map(|attachment| SavedAttachment {
                                id: Uuid::from_bytes(attachment.id),
                                name: attachment.name,
                                kind: attachment.kind.into(),
                                size: attachment.size,
                                creator: Uuid::from_bytes(attachment.creator),
                            })
                            .collect(),
                    })
                    .collect(),
                comments: message
                    .comments
                    .into_iter()
                    .map(|comment| SavedComment {
                        id: Uuid::from_bytes(comment.id),
                        author: Uuid::from_bytes(comment.author),
                        message_id: Uuid::from_bytes(comment.reference.message_id),
                        range: comment.reference.range,
                        quote: comment.reference.quote,
                        body: comment.body,
                    })
                    .collect(),
            }),
            protocol::TimelineMessage::Agent(run) => {
                let (outcome, duration) = match run.run {
                    protocol::AgentRun::Ended { outcome, duration } => (
                        match outcome {
                            protocol::RunOutcome::Completed => SavedOutcome::Completed,
                            protocol::RunOutcome::Stopped => SavedOutcome::Stopped,
                            protocol::RunOutcome::Failed(error) => SavedOutcome::Failed(error),
                        },
                        duration,
                    ),
                    // Threads are saved between runs; one still going here
                    // would not go on after a restart.
                    protocol::AgentRun::Generating => (SavedOutcome::Stopped, Duration::ZERO),
                };
                Self::Agent(SavedRun {
                    id: Uuid::from_bytes(run.id),
                    comment_group_id: run.comment_group_id.map(Uuid::from_bytes),
                    started_at: run.started_at,
                    duration,
                    start: run.start.0,
                    outcome,
                    unfinished: ReplyPreview::of(&run.pending_events)
                        .partial()
                        .and_then(|reply| Envelope::encode_message(&reply).ok()),
                    model: run.model,
                })
            }
        }
    }

    fn into_protocol(self) -> anyhow::Result<protocol::TimelineMessage> {
        Ok(match self {
            Self::User(message) => protocol::TimelineMessage::User(protocol::UserMessage {
                id: message.id.into_bytes(),
                blocks: message
                    .blocks
                    .into_iter()
                    .map(|block| protocol::PromptBlock {
                        id: block.id.into_bytes(),
                        author: block.author.into_bytes(),
                        text: block.text,
                        attachments: block
                            .attachments
                            .into_iter()
                            .map(|attachment| protocol::AttachmentRef {
                                id: attachment.id.into_bytes(),
                                name: attachment.name,
                                kind: attachment.kind.into(),
                                size: attachment.size,
                                creator: attachment.creator.into_bytes(),
                            })
                            .collect(),
                    })
                    .collect(),
                comments: message
                    .comments
                    .into_iter()
                    .map(|comment| protocol::UserComment {
                        id: comment.id.into_bytes(),
                        author: comment.author.into_bytes(),
                        reference: protocol::CommentReference {
                            message_id: comment.message_id.into_bytes(),
                            range: comment.range,
                            quote: comment.quote,
                        },
                        body: comment.body,
                    })
                    .collect(),
            }),
            Self::Agent(run) => {
                let pending_events = match &run.unfinished {
                    Some(reply) => ReplyPreview::previews_of(
                        &reply.decode_message().context("invalid unfinished reply")?,
                    ),
                    None => Vec::new(),
                };
                protocol::TimelineMessage::Agent(protocol::AgentMessage {
                    id: run.id.into_bytes(),
                    comment_group_id: run.comment_group_id.map(Uuid::into_bytes),
                    started_at: run.started_at,
                    start: crate::transcript::TranscriptIndex(run.start),
                    pending_events,
                    run: protocol::AgentRun::Ended {
                        outcome: match run.outcome {
                            SavedOutcome::Completed => protocol::RunOutcome::Completed,
                            SavedOutcome::Stopped => protocol::RunOutcome::Stopped,
                            SavedOutcome::Failed(error) => protocol::RunOutcome::Failed(error),
                        },
                        duration: run.duration,
                    },
                    model: run.model,
                    awaiting_approval: None,
                })
            }
        })
    }

    fn attachments(&self) -> impl Iterator<Item = &SavedAttachment> {
        let blocks = match self {
            Self::User(message) => message.blocks.as_slice(),
            Self::Agent(_) => &[],
        };
        blocks.iter().flat_map(|block| &block.attachments)
    }
}

/// `thread` as `thread.json` keeps it, with `messages_bytes` still to be
/// filled in.
fn thread_file(thread: &Thread, archived: bool, position: usize) -> ThreadFile {
    let mut profiles = thread
        .profiles
        .iter()
        .map(|(participant, profile)| {
            let profile = profile.to_protocol();
            SavedProfile {
                participant: participant.as_uuid(),
                name: profile.name,
                picture: profile
                    .picture
                    .map(|picture| base64::engine::general_purpose::STANDARD.encode(picture)),
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
    ThreadFile {
        version: FILE_VERSION,
        id: thread.instance_id,
        title: thread.summary.title.clone(),
        participant_id: thread.participant_id().as_uuid(),
        archived,
        position,
        model: thread.model().cloned(),
        project_folders: thread
            .project_folders()
            .iter()
            .filter_map(|folder| {
                Some(SavedFolder {
                    name: folder.name.to_string(),
                    path: folder.path.clone()?,
                })
            })
            .collect(),
        project_mode: thread.project_mode(),
        project: thread.project(),
        profiles,
        prompt_names,
        tokens_used: thread.tokens_used,
        token_activity: thread.token_activity.clone(),
        context_tokens: thread.context_tokens,
        streamed_bytes: thread.streamed_bytes,
        messages_bytes: 0,
        messages: thread
            .timeline
            .iter()
            .map(|message| SavedMessage::from_protocol(message.to_protocol()))
            .collect(),
    }
}

/// A thread read from its folder, not yet made into a [`Thread`].
pub(crate) struct SavedThread {
    file: ThreadFile,
    transcript: Transcript,
    files: HashMap<AttachmentId, FileAttachment>,
}

impl SavedThread {
    pub(crate) fn id(&self) -> Uuid {
        self.file.id
    }

    pub(crate) fn archived(&self) -> bool {
        self.file.archived
    }

    pub(crate) fn position(&self) -> usize {
        self.file.position
    }

    /// Reads the thread in `dir`. `Ok(None)` when it has no `thread.json`,
    /// as a thread whose first run never ended, or of another format.
    fn read(dir: &Path) -> anyhow::Result<Option<Self>> {
        let bytes = match std::fs::read(dir.join(THREAD_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("could not read thread.json"),
        };
        let version = serde_json::from_slice::<VersionOnly>(&bytes)
            .context("invalid thread.json")?
            .version;
        if version != FILE_VERSION {
            eprintln!(
                "the thread in {} is format {version}, not {FILE_VERSION}; left as it is",
                dir.display()
            );
            return Ok(None);
        }
        let file: ThreadFile = serde_json::from_slice(&bytes).context("invalid thread.json")?;

        let log = std::fs::read(dir.join(MESSAGES_FILE)).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound && file.messages_bytes == 0 {
                Ok(Vec::new())
            } else {
                Err(error).context("could not read messages.jsonl")
            }
        })?;
        let log = usize::try_from(file.messages_bytes)
            .ok()
            .and_then(|end| log.get(..end))
            .context("messages.jsonl is shorter than thread.json says")?;
        let mut transcript = Transcript::default();
        for line in log
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let line: TranscriptLine =
                serde_json::from_slice(line).context("invalid line in messages.jsonl")?;
            transcript.apply_line(&line)?;
        }

        let mut files = HashMap::new();
        for attachment in file.messages.iter().flat_map(SavedMessage::attachments) {
            let path = dir.join(ATTACHMENTS_DIR).join(attachment.id.to_string());
            let loaded = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    FileAttachment::from_bytes(
                        attachment.name.clone(),
                        kind_from_protocol(attachment.kind.into()),
                        bytes,
                    )
                });
            match loaded {
                Ok(loaded) => {
                    files.insert(AttachmentId::from_uuid(attachment.id), loaded);
                }
                // The message still shows the file, without its contents.
                Err(error) => eprintln!("could not read {}: {error:#}", path.display()),
            }
        }
        Ok(Some(Self {
            file,
            transcript,
            files,
        }))
    }

    /// The thread, checked as a host's snapshot is, with `models` to run.
    pub(crate) fn into_thread(
        self,
        models: ModelCatalog,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<Thread> {
        let Self {
            file,
            transcript,
            files,
        } = self;
        let messages = file
            .messages
            .into_iter()
            .map(SavedMessage::into_protocol)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let welcome = protocol::Welcome {
            participant_id: file.participant_id.into_bytes(),
            thread: protocol::ThreadSnapshot {
                submission_sequence: messages
                    .iter()
                    .filter(|message| matches!(message, protocol::TimelineMessage::User(_)))
                    .count() as u64,
                pending: Vec::new(),
                id: file.id.into_bytes(),
                title: file.title,
                participants: Vec::new(),
                profiles: file
                    .profiles
                    .into_iter()
                    .map(|profile| {
                        let picture = profile
                            .picture
                            .map(|picture| {
                                base64::engine::general_purpose::STANDARD.decode(picture)
                            })
                            .transpose()
                            .context("invalid profile picture")?;
                        anyhow::Ok((
                            profile.participant.into_bytes(),
                            protocol::Profile {
                                name: profile.name,
                                picture,
                                appearance: profile.appearance.map(Uuid::into_bytes),
                            },
                        ))
                    })
                    .collect::<anyhow::Result<_>>()?,
                models,
                model: file.model,
                peer_permissions: Default::default(),
                context_tokens: file.context_tokens,
                compacting: false,
                streamed_bytes: file.streamed_bytes,
                messages,
                transcript: protocol::Json::from_value(&TranscriptRecord::default())?,
                prompt_names: file
                    .prompt_names
                    .into_iter()
                    .map(|(participant, name)| (participant.into_bytes(), name))
                    .collect(),
                project_folders: file
                    .project_folders
                    .iter()
                    .map(|folder| folder.name.clone())
                    .collect(),
                project_mode: file.project_mode,
            },
            draft: ::draft::Draft::new().encode_state(),
            draft_generation: 0,
            presence: Vec::new(),
            stored_attachments: files.keys().map(|id| id.as_uuid().into_bytes()).collect(),
        };
        let prepared = Thread::prepare_snapshot(welcome, transcript)?;
        let folders = file
            .project_folders
            .into_iter()
            .map(|folder| ProjectFolder {
                name: folder.name.into(),
                path: Some(folder.path),
            })
            .collect();
        let mut thread = Thread::from_saved(prepared, folders, files, cx);
        thread.set_project(file.project);
        thread.tokens_used = file.tokens_used;
        thread.token_activity = file.token_activity;
        Ok(thread)
    }
}

/// Read first, so a file of another format is recognized as one.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// Reads every thread saved in `root`, with the length of each one's
/// message log as saved. Threads that can't be read are left as they are.
fn read_threads(root: &Path) -> (Vec<SavedThread>, HashMap<Uuid, u64>) {
    let mut threads = Vec::new();
    let mut lengths = HashMap::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (threads, lengths);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        match SavedThread::read(&dir) {
            Ok(Some(thread)) => {
                if dir.file_name() != Some(thread.file.id.to_string().as_ref()) {
                    eprintln!(
                        "the thread in {} is not in its own folder; left as it is",
                        dir.display()
                    );
                    continue;
                }
                lengths.insert(thread.file.id, thread.file.messages_bytes);
                threads.push(thread);
            }
            Ok(None) => {}
            Err(error) => eprintln!(
                "could not read the thread in {}: {error:#}; left as it is",
                dir.display()
            ),
        }
    }
    (threads, lengths)
}

/// The saved threads' folders, and the writer saving them.
pub(crate) struct ThreadFiles {
    root: PathBuf,
    /// Whether threads are saved; false for an ephemeral instance.
    persistent: bool,
    jobs: mpsc::Sender<Job>,
    /// What of each thread is saved, so the next save adds only what is new.
    saved: HashMap<Uuid, Saved>,
    /// Threads whose last save failed, which the next save writes whole.
    failed: Arc<Mutex<HashSet<Uuid>>>,
    /// Held while this instance keeps the threads.
    _lock: Option<File>,
}

#[derive(Default)]
struct Saved {
    transcript: TranscriptExtent,
    attachments: HashSet<AttachmentId>,
}

enum Job {
    Save(Box<SaveJob>),
    Delete(Uuid),
    /// Answered once everything before it is written, and the folders
    /// removed if they were temporary.
    Quit(oneshot::Sender<()>),
}

struct SaveJob {
    id: Uuid,
    file: ThreadFile,
    /// The transcript records after those already saved, or all of them.
    lines: Vec<TranscriptLine>,
    /// Whether `lines` replace the whole log.
    rewrite: bool,
    attachments: Vec<(AttachmentId, Vec<u8>)>,
}

impl ThreadFiles {
    /// Keeps threads in `root`, or fails if it cannot exclusively own them.
    /// `None` explicitly selects an ephemeral instance: no lock or saved
    /// threads, and temporary folders for this run only.
    pub(crate) fn open(root: Option<PathBuf>) -> anyhow::Result<(Self, Vec<SavedThread>)> {
        let (root, lock, persistent) = match root {
            Some(root) => {
                let file = lock(&root)
                    .with_context(|| format!("could not lock threads in {}", root.display()))?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Cowork is already running. Use --ephemeral to open a separate instance."
                        )
                    })?;
                (root, Some(file), true)
            }
            None => (
                std::env::temp_dir().join(format!("cowork-threads-{}", Uuid::new_v4().simple())),
                None,
                false,
            ),
        };
        let (threads, lengths) = if persistent {
            read_threads(&root)
        } else {
            Default::default()
        };
        let saved = threads
            .iter()
            .map(|thread| {
                let saved = Saved {
                    transcript: thread.transcript.extent(),
                    attachments: thread.files.keys().copied().collect(),
                };
                (thread.file.id, saved)
            })
            .collect();
        let failed = Arc::default();
        let (jobs, received) = mpsc::channel();
        let writer = Writer {
            root: root.clone(),
            persistent,
            lengths,
            failed: Arc::clone(&failed),
        };
        std::thread::Builder::new()
            .name("cowork-thread-files".into())
            .spawn(move || writer.run(received))
            .expect("the thread file writer starts");
        let files = Self {
            root,
            persistent,
            jobs,
            saved,
            failed,
            _lock: lock,
        };
        Ok((files, threads))
    }

    /// The threads' folders, each with its commands' scratch folder.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Saves what changed in `thread` since it was last saved. `thread` must
    /// not be running.
    fn save(&mut self, thread: &Thread, archived: bool, position: usize) {
        if !self.persistent {
            return;
        }
        let id = thread.instance_id;
        let failed = self.failed.lock().remove(&id);
        let rewrite = failed || !self.saved.contains_key(&id);
        let saved = self.saved.entry(id).or_default();
        if rewrite {
            *saved = Saved::default();
        }
        let file = thread_file(thread, archived, position);
        let lines = thread.transcript.lines_after(saved.transcript);
        saved.transcript = thread.transcript.extent();
        let files = &thread.draft().files;
        let attachments = file
            .messages
            .iter()
            .flat_map(SavedMessage::attachments)
            .map(|attachment| AttachmentId::from_uuid(attachment.id))
            .filter(|id| saved.attachments.insert(*id))
            .filter_map(|id| Some((id, files.get(&id)?.bytes().to_vec())))
            .collect();
        _ = self.jobs.send(Job::Save(Box::new(SaveJob {
            id,
            file,
            lines,
            rewrite,
            attachments,
        })));
    }

    /// Removes `thread`'s folder, after any save of it still being written.
    fn delete(&mut self, thread: Uuid) {
        self.saved.remove(&thread);
        _ = self.jobs.send(Job::Delete(thread));
    }

    /// Finishes writing, and removes temporary folders.
    pub(crate) fn quit(&self) -> oneshot::Receiver<()> {
        let (done, finished) = oneshot::channel();
        _ = self.jobs.send(Job::Quit(done));
        finished
    }
}

/// Takes `root`'s lock, or `None` when another instance holds it.
fn lock(root: &Path) -> anyhow::Result<Option<File>> {
    std::fs::create_dir_all(root)?;
    let file = File::create(root.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Writes saves on a thread of its own, in order.
struct Writer {
    root: PathBuf,
    persistent: bool,
    /// How long each thread's message log is as last saved.
    lengths: HashMap<Uuid, u64>,
    failed: Arc<Mutex<HashSet<Uuid>>>,
}

impl Writer {
    fn run(mut self, jobs: mpsc::Receiver<Job>) {
        for job in jobs {
            match job {
                Job::Save(job) => {
                    let id = job.id;
                    if let Err(error) = self.write(*job) {
                        eprintln!("could not save thread {id}: {error:#}");
                        self.lengths.remove(&id);
                        self.failed.lock().insert(id);
                    }
                }
                Job::Delete(id) => {
                    self.lengths.remove(&id);
                    remove_dir(&self.root.join(id.to_string()));
                }
                Job::Quit(done) => {
                    if !self.persistent {
                        remove_dir(&self.root);
                    }
                    _ = done.send(());
                }
            }
        }
    }

    fn write(&mut self, job: SaveJob) -> anyhow::Result<()> {
        let SaveJob {
            id,
            mut file,
            lines,
            rewrite,
            attachments,
        } = job;
        let dir = self.root.join(id.to_string());
        let attachments_dir = dir.join(ATTACHMENTS_DIR);
        std::fs::create_dir_all(&attachments_dir)?;
        for (attachment, bytes) in attachments {
            write_whole(
                &attachments_dir.join(attachment.as_uuid().to_string()),
                &bytes,
            )?;
        }

        let start = if rewrite {
            0
        } else {
            *self
                .lengths
                .get(&id)
                .context("an earlier save of the thread failed")?
        };
        let mut buffer = Vec::new();
        for line in &lines {
            serde_json::to_writer(&mut buffer, line)?;
            buffer.push(b'\n');
        }
        let mut log = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(MESSAGES_FILE))?;
        // Drops what a save cut short left after the saved log.
        log.set_len(start)?;
        log.seek(SeekFrom::Start(start))?;
        log.write_all(&buffer)?;
        log.sync_data()?;
        let end = start + buffer.len() as u64;

        file.messages_bytes = end;
        write_whole(&dir.join(THREAD_FILE), &serde_json::to_vec_pretty(&file)?)?;
        self.lengths.insert(id, end);
        Ok(())
    }
}

/// Replaces `path` with `bytes` only once they are written, so a crash
/// never leaves half a file.
fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    std::fs::rename(&temporary, path)
}

fn remove_dir(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("could not remove {}: {error}", path.display());
    }
}

impl Cowork {
    /// Keeps threads with `files` from now on, starting with `saved`, the
    /// threads read back from them, in the sidebar and archive where they
    /// were. Cowork saves its threads again when it quits.
    pub(crate) fn start_thread_files(
        &mut self,
        files: ThreadFiles,
        mut saved: Vec<SavedThread>,
        cx: &mut Context<Self>,
    ) {
        saved.sort_by_key(SavedThread::position);
        self.profile_statistics = None;
        for thread in saved {
            let id = thread.id();
            let archived = thread.archived();
            let thread = match thread.into_thread((*self.models).clone(), cx) {
                Ok(thread) => cx.new(|_| thread),
                Err(error) => {
                    eprintln!("could not restore thread {id}: {error:#}; left as it is");
                    continue;
                }
            };
            self.thread_store.update(cx, |store, _| {
                if archived {
                    store.archived.push_back(thread.clone());
                } else {
                    store.threads.push_back(thread.clone());
                }
            });
            self.sync_sandbox_project(&thread, cx);
        }
        self.thread_files = Some(files);
        cx.on_app_quit(|this, cx| {
            this.save_all_threads(cx);
            let finished = this.thread_files.as_ref().map(ThreadFiles::quit);
            async move {
                if let Some(finished) = finished {
                    _ = finished.await;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Saves `thread` as it is now, if it is one of the user's own and not
    /// running; see the module documentation for when.
    pub(crate) fn save_thread(&mut self, thread: &Entity<Thread>, cx: &App) {
        let Some(files) = &mut self.thread_files else {
            return;
        };
        let state = thread.read(cx);
        if state.ownership() != ThreadOwnership::Local || state.is_generating() {
            return;
        }
        let store = self.thread_store.read(cx);
        let place = |list: &std::collections::VecDeque<Entity<Thread>>| {
            list.iter().position(|other| other == thread)
        };
        let (archived, position) = match (place(&store.threads), place(&store.archived)) {
            (Some(position), _) => (false, position),
            (None, Some(position)) => (true, position),
            // Deleted.
            (None, None) => return,
        };
        files.save(state, archived, position);
    }

    /// Saves every thread of the user's own not running, as when Cowork
    /// quits, which also keeps their order.
    pub(crate) fn save_all_threads(&mut self, cx: &App) {
        let store = self.thread_store.read(cx);
        let threads = store
            .threads
            .iter()
            .chain(&store.archived)
            .cloned()
            .collect::<Vec<_>>();
        for thread in &threads {
            self.save_thread(thread, cx);
        }
    }

    /// Removes a deleted thread's folder, its commands' scratch folder
    /// included.
    pub(crate) fn delete_thread_files(&mut self, thread_id: Uuid) {
        if let Some(files) = &mut self.thread_files {
            files.delete(thread_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use draft::AttachmentRecord;
    use gpui::SharedString;

    use super::*;
    use crate::{
        participant::ParticipantId,
        profile::Profile,
        test_support::{
            ScriptedHost, attached, streamed_text, streamed_tool_call, text_attachment,
        },
        thread_draft::ThreadDraft,
        timeline::{
            CommentReference, PromptBlock, TimelineMessage, UserComment, UserCommentBody,
            UserMessageGroup,
        },
    };

    /// A fresh folder for threads, removed when dropped.
    struct Root(PathBuf);

    impl Root {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cowork-threads-{}", Uuid::new_v4()));
            Self(path)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn persistent_instances_require_exclusive_ownership() {
        let root = Root::new();
        let (files, _) = ThreadFiles::open(Some(root.0.clone())).unwrap();
        let error = ThreadFiles::open(Some(root.0.clone())).err().unwrap();
        assert!(error.to_string().contains("already running"));
        futures::executor::block_on(files.quit()).unwrap();
        drop(files);
        let (files, _) = ThreadFiles::open(Some(root.0.clone())).unwrap();
        futures::executor::block_on(files.quit()).unwrap();

        let invalid = Root::new();
        std::fs::write(&invalid.0, b"not a directory").unwrap();
        assert!(ThreadFiles::open(Some(invalid.0.clone())).is_err());
        std::fs::remove_file(&invalid.0).unwrap();
    }

    #[gpui::test]
    fn ephemeral_instances_are_independent_and_do_not_save(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let root = Root::new();
            let (persistent, _) = ThreadFiles::open(Some(root.0.clone())).unwrap();
            let (mut first, saved) = ThreadFiles::open(None).unwrap();
            let (second, other_saved) = ThreadFiles::open(None).unwrap();
            assert!(saved.is_empty() && other_saved.is_empty());
            assert_ne!(first.root(), second.root());
            assert_ne!(first.root(), persistent.root());
            let mut host = ScriptedHost::default();
            let (thread, _) = thread_with_history(&mut host, &std::env::temp_dir(), cx);
            let temporary = first.root().to_owned();
            std::fs::create_dir_all(&temporary).unwrap();
            std::fs::write(temporary.join("scratch-marker"), b"scratch").unwrap();
            first.save(&thread, false, 0);
            assert!(first.saved.is_empty(), "ephemeral saves are ignored");
            // Quit drains the writer queue and removes temporary scratch,
            // but no saved thread or lock should ever have been created.
            assert!(!temporary.join(LOCK_FILE).exists());
            assert!(!temporary.join(thread.instance_id.to_string()).exists());
            futures::executor::block_on(first.quit()).unwrap();
            assert!(!temporary.exists());
            futures::executor::block_on(second.quit()).unwrap();
            futures::executor::block_on(persistent.quit()).unwrap();
        });
    }

    /// What a thread shows and runs on: its snapshot, with each unfinished
    /// reply as the reply it shows rather than the previews that made it.
    fn shown(thread: &Thread) -> protocol::ThreadSnapshot {
        let mut snapshot = thread.to_protocol();
        for message in &mut snapshot.messages {
            if let protocol::TimelineMessage::Agent(run) = message {
                run.pending_events = ReplyPreview::of(&run.pending_events)
                    .partial()
                    .map(|reply| ReplyPreview::previews_of(&reply))
                    .unwrap_or_default();
            }
        }
        snapshot
    }

    /// A thread with a user message attaching a file and commenting, a
    /// collaborator's profile, a project folder, and a run calling a tool.
    fn thread_with_history(
        host: &mut ScriptedHost,
        folder: &Path,
        cx: &mut gpui::App,
    ) -> (Thread, AttachmentRecord) {
        let author = ParticipantId::new();
        let (records, files) = attached([text_attachment("notes.md", "# Notes")]);
        let group = UserMessageGroup {
            id: Uuid::new_v4(),
            comments: vec![UserComment {
                id: Uuid::new_v4(),
                author,
                presence: Default::default(),
                reference: CommentReference {
                    message_id: Uuid::new_v4(),
                    range: 0..4,
                    quote: "text".into(),
                },
                body: UserCommentBody::Submitted("Look here".into()),
            }],
            blocks: vec![PromptBlock {
                id: Uuid::new_v4(),
                author,
                text: "Read the notes".into(),
                attachments: records.clone(),
            }],
            comments_folded: false,
        };
        let mut draft = ThreadDraft::new(author);
        draft.files = files;
        let mut thread = Thread::new_local(
            "Notes".into(),
            vec![TimelineMessage::User(group.into())],
            draft,
            author,
            Arc::default(),
            None,
        );
        thread.profiles.insert(
            ParticipantId::new(),
            Profile {
                name: Some(SharedString::from("Ada")),
                ..Profile::default()
            },
        );
        thread.add_project_folders(vec![folder.to_owned()]);
        thread.set_project(Some(ProjectId::new()));
        thread.tokens_used = 120;
        thread.token_activity.push(TokenActivity {
            at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_secs(2),
            tokens: 120,
        });

        let run = Uuid::new_v4();
        let mut events = host.start(run, "Read the notes");
        events.extend(host.turn(
            run,
            [streamed_tool_call(
                "call",
                "read_file",
                serde_json::json!({ "path": "notes.md" }),
            )],
            50,
        ));
        events.extend(host.results(run, "# Notes"));
        events.extend(host.compaction(run, 3, "Read."));
        events.extend(host.turn(run, [streamed_text("answer", "Done.", false)], 70));
        events.push(host.ended(run, protocol::RunOutcome::Completed));
        for event in events {
            thread.apply_for_test(event, cx);
        }
        for message in &mut thread.timeline {
            if let TimelineMessage::Agent(message) = message {
                message.record.model = Some(ModelRef {
                    provider: crate::models::ModelProvider::Anthropic,
                    id: "claude".into(),
                });
            }
        }
        (thread, records.into_iter().next().unwrap())
    }

    /// Saves `thread` with `files` and waits until it is written.
    fn save(files: &mut ThreadFiles, thread: &Thread) {
        files.save(thread, false, 0);
        let (done, finished) = oneshot::channel();
        _ = files.jobs.send(Job::Quit(done));
        futures::executor::block_on(finished).unwrap();
    }

    /// The threads in `root`, as Cowork reads them when it starts.
    fn reopen(root: &Root, cx: &mut gpui::App) -> (ThreadFiles, Vec<Thread>) {
        let (files, saved) = ThreadFiles::open(Some(root.0.clone())).unwrap();
        let threads = saved
            .into_iter()
            .map(|thread| thread.into_thread(ModelCatalog::default(), cx).unwrap())
            .collect();
        (files, threads)
    }

    #[gpui::test]
    fn a_saved_thread_comes_back_as_it_was(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let root = Root::new();
            let folder = std::env::temp_dir();
            let mut host = ScriptedHost::default();
            let (mut thread, attachment) = thread_with_history(&mut host, &folder, cx);
            {
                let (mut files, saved) = ThreadFiles::open(Some(root.0.clone())).unwrap();
                assert!(saved.is_empty());
                save(&mut files, &thread);
            }
            let (mut files, threads) = reopen(&root, cx);
            let [restored] = threads.try_into().ok().expect("one saved thread");
            assert_eq!(shown(&restored), shown(&thread));
            assert_eq!(restored.instance_id, thread.instance_id);
            assert_eq!(restored.participant_id(), thread.participant_id());
            assert_eq!(restored.project_folders(), thread.project_folders());
            assert_eq!(restored.project(), thread.project());
            assert_eq!(restored.tokens_used, 120);
            assert_eq!(restored.token_activity, thread.token_activity);
            assert_eq!(
                restored.draft().files[&attachment.id].bytes(),
                b"# Notes",
                "its attachment is read back"
            );

            // A save cut short leaves part of a record; the next save,
            // adding a stopped run with an unfinished reply, replaces it.
            let log = root
                .0
                .join(thread.instance_id.to_string())
                .join(MESSAGES_FILE);
            let mut cut_short = OpenOptions::new().append(true).open(&log).unwrap();
            cut_short.write_all(b"{\"entry\":").unwrap();
            let run = Uuid::new_v4();
            let mut events = host.start(run, "Again");
            events.extend(
                host.turn(run, [streamed_text("answer", "Half an ans", false)], 90)
                    .into_iter()
                    .take_while(|event| {
                        !matches!(
                            event,
                            protocol::HostMessage::AgentEvent {
                                event: protocol::RunEvent::Usage { .. },
                                ..
                            }
                        )
                    }),
            );
            events.push(host.ended(run, protocol::RunOutcome::Stopped));
            for event in events {
                thread.apply_for_test(event, cx);
            }
            save(&mut files, &thread);
            drop(files);

            let (files, threads) = reopen(&root, cx);
            let [restored] = threads.try_into().ok().expect("one saved thread");
            assert_eq!(shown(&restored), shown(&thread));
            drop(files);

            // Saved before projects, a thread is in none.
            let path = root
                .0
                .join(thread.instance_id.to_string())
                .join(THREAD_FILE);
            let mut saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            saved.as_object_mut().unwrap().remove("project").unwrap();
            std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
            let (_files, threads) = reopen(&root, cx);
            let [restored] = threads.try_into().ok().expect("one saved thread");
            assert_eq!(restored.project(), None);
        });
    }
}
