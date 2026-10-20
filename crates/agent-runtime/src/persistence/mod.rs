//! Optional asynchronous JSONL persistence. updates.jsonl is the full transcript
//! and replay source; chat_history.jsonl is a replaceable model-context projection.
//! Recording queues writes and never gates agent operations on disk durability.

use std::{
    io,
    path::{Path, PathBuf},
};

use language_model::Envelope;
use serde::Serialize;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
    sync::{mpsc, oneshot},
};

use crate::{AgentEvent, CompactionError, Conversation, ConversationError};

pub mod record;
use record::{RECORD_TYPE, Record, StoredEvent, StoredEventKind};

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Schema(#[from] language_model::SchemaError),
    #[error(transparent)]
    Conversation(#[from] ConversationError),
    #[error(transparent)]
    Compaction(#[from] CompactionError),
    #[error("unsupported log record: {0}")]
    Unsupported(String),
    #[error("invalid log: {0}")]
    Invalid(String),
    #[error("session log writer has closed")]
    Closed,
    #[error("session log writer task failed: {0}")]
    Task(String),
}

pub struct LoadedSession {
    /// Seed for replaying updates in order (including provisional UI elements).
    pub initial_conversation: Conversation,
    pub conversation: Conversation,
    /// Includes previews, tool progress and structured outputs, not just messages.
    pub updates: Vec<StoredEvent>,
    pub next_sequence: u64,
    pub ignored_torn_tail: bool,
}

enum WriteCommand {
    Event(Box<StoredEvent>),
    Flush(oneshot::Sender<()>),
}

/// Single writer per directory; the caller owns exclusion from other writers.
/// File appends/flushes are buffered OS writes, without per-operation fsync or a
/// transactional gate. finish surfaces any background write failure.
pub struct JsonlLog {
    sender: mpsc::UnboundedSender<WriteCommand>,
    task: tokio::task::JoinHandle<Result<(), LogError>>,
}

/// Cloneable event sink. The log owner still joins the worker with finish after
/// the driver has dropped its sink, so callers need not coordinate sender counts.
#[derive(Clone)]
pub struct LogRecorder {
    sender: mpsc::UnboundedSender<WriteCommand>,
}

impl LogRecorder {
    pub fn record<S: Serialize>(&self, event: &AgentEvent<S>) -> Result<(), LogError> {
        let event = StoredEvent::encode(event)?;
        self.record_encoded(event)
    }

    pub fn record_encoded(&self, event: StoredEvent) -> Result<(), LogError> {
        self.sender
            .send(WriteCommand::Event(Box::new(event)))
            .map_err(|_| LogError::Closed)
    }
}

impl JsonlLog {
    pub fn recorder(&self) -> LogRecorder {
        LogRecorder {
            sender: self.sender.clone(),
        }
    }
    pub async fn create(
        directory: impl AsRef<Path>,
        conversation: &Conversation,
        next_sequence: u64,
    ) -> Result<Self, LogError> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory).await?;
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join("updates.jsonl"))
            .await?;
        let mut writer = BufWriter::new(file);
        write_record(&mut writer, &record::initial(conversation, next_sequence)?).await?;
        writer.flush().await?;
        replace_context(&directory, conversation).await?;
        Ok(Self::spawn(
            directory,
            writer,
            conversation.clone(),
            next_sequence,
        ))
    }

    pub async fn open(directory: impl AsRef<Path>) -> Result<(Self, LoadedSession), LogError> {
        let directory = directory.as_ref().to_path_buf();
        let loaded = load(&directory).await?;
        let bytes = fs::read(directory.join("updates.jsonl")).await?;
        let file = OpenOptions::new()
            .append(true)
            .open(directory.join("updates.jsonl"))
            .await?;
        let mut writer = BufWriter::new(file);
        if loaded.ignored_torn_tail {
            // Remove only the incomplete final record; prior complete records
            // and unsupported versions are never silently discarded.
            let length = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1);
            writer.get_mut().set_len(length as u64).await?;
        } else if bytes.last().is_some_and(|byte| *byte != b'\n') {
            writer.write_all(b"\n").await?;
        }
        // Rebuild the derived file if a crash left it behind the transcript.
        replace_context(&directory, &loaded.conversation).await?;
        let log = Self::spawn(
            directory,
            writer,
            loaded.conversation.clone(),
            loaded.next_sequence,
        );
        Ok((log, loaded))
    }

    fn spawn(
        directory: PathBuf,
        mut writer: BufWriter<File>,
        mut conversation: Conversation,
        mut next_sequence: u64,
    ) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    WriteCommand::Event(event) => {
                        if event.sequence != next_sequence {
                            return Err(LogError::Invalid(
                                "event sequence gap or duplicate".into(),
                            ));
                        }
                        event.apply(&mut conversation)?;
                        next_sequence = next_sequence
                            .checked_add(1)
                            .ok_or_else(|| LogError::Invalid("event sequence overflow".into()))?;
                        let appended = match &event.event {
                            StoredEventKind::MessageCommitted { message, .. } => {
                                Some(message.clone())
                            }
                            _ => None,
                        };
                        let compacted = matches!(event.event, StoredEventKind::Compacted { .. });
                        write_record(&mut writer, &Record::Event { event: *event }).await?;
                        // Flush on batch drain rather than each token. A crash
                        // may lose this tail; accepted context remains distinct
                        // from any previews already written.
                        if let Some(message) = appended {
                            let mut bytes = serde_json::to_vec(&message)?;
                            bytes.push(b'\n');
                            OpenOptions::new()
                                .append(true)
                                .open(directory.join("chat_history.jsonl"))
                                .await?
                                .write_all(&bytes)
                                .await?;
                        } else if compacted {
                            replace_context(&directory, &conversation).await?;
                        }
                    }
                    WriteCommand::Flush(response) => {
                        writer.flush().await?;
                        let _ = response.send(());
                    }
                }
                if receiver.is_empty() {
                    writer.flush().await?;
                }
            }
            writer.flush().await?;
            Ok(())
        });
        Self { sender, task }
    }

    pub fn record<S: Serialize>(&self, event: &AgentEvent<S>) -> Result<(), LogError> {
        let event = StoredEvent::encode(event)?;
        self.sender
            .send(WriteCommand::Event(Box::new(event)))
            .map_err(|_| LogError::Closed)
    }

    /// An ordered buffered flush, not an fsync guarantee. No operation in the
    /// agent waits on this; hosts can use it at their chosen session boundaries.
    pub async fn flush(&self) -> Result<(), LogError> {
        let (response, reply) = oneshot::channel();
        self.sender
            .send(WriteCommand::Flush(response))
            .map_err(|_| LogError::Closed)?;
        reply.await.map_err(|_| LogError::Closed)
    }

    pub async fn finish(self) -> Result<(), LogError> {
        drop(self.sender);
        self.task
            .await
            .map_err(|error| LogError::Task(error.to_string()))?
    }
}

/// Load the complete transcript and rebuild its independent model-context view.
/// An invalid unterminated last JSON line is ignored as a torn append. Unknown
/// schemas and corruption in complete lines are errors, never destructive repair.
pub async fn load(directory: impl AsRef<Path>) -> Result<LoadedSession, LogError> {
    let bytes = fs::read(directory.as_ref().join("updates.jsonl")).await?;
    let mut conversation = None;
    let mut initial_conversation = None;
    let mut updates = Vec::new();
    let mut next_sequence = 0;
    let mut ignored_torn_tail = false;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let envelope: Envelope = match serde_json::from_slice(line) {
            Ok(envelope) => envelope,
            Err(_)
                if !line.ends_with(b"\n")
                    && serde_json::from_slice::<serde_json::Value>(line).is_err() =>
            {
                ignored_torn_tail = true;
                break;
            }
            Err(error) => return Err(error.into()),
        };
        if envelope.type_id != RECORD_TYPE {
            return Err(LogError::Unsupported(envelope.type_id));
        }
        match serde_json::from_value::<Record>(envelope.payload)? {
            Record::Initial {
                messages,
                compactions,
                next_sequence: sequence,
            } => {
                if conversation.is_some() {
                    return Err(LogError::Invalid("duplicate initial snapshot".into()));
                }
                let messages = messages
                    .iter()
                    .map(Envelope::decode_message)
                    .collect::<Result<_, _>>()?;
                let mut initial = Conversation::new(messages)?;
                for compaction in compactions {
                    initial.apply_compaction(compaction.decode()?)?;
                }
                initial_conversation = Some(initial.clone());
                conversation = Some(initial);
                next_sequence = sequence;
            }
            Record::Event { event } => {
                let conversation = conversation
                    .as_mut()
                    .ok_or_else(|| LogError::Invalid("missing initial snapshot".into()))?;
                if event.sequence != next_sequence {
                    return Err(LogError::Invalid("event sequence gap or duplicate".into()));
                }
                event.apply(conversation)?;
                next_sequence = next_sequence
                    .checked_add(1)
                    .ok_or_else(|| LogError::Invalid("event sequence overflow".into()))?;
                updates.push(event);
            }
        }
    }
    Ok(LoadedSession {
        initial_conversation: initial_conversation
            .ok_or_else(|| LogError::Invalid("missing initial snapshot".into()))?,
        conversation: conversation
            .ok_or_else(|| LogError::Invalid("missing initial snapshot".into()))?,
        updates,
        next_sequence,
        ignored_torn_tail,
    })
}

async fn write_record(writer: &mut BufWriter<File>, record: &Record) -> Result<(), LogError> {
    let envelope = Envelope {
        type_id: RECORD_TYPE.into(),
        payload: serde_json::to_value(record)?,
    };
    let mut bytes = serde_json::to_vec(&envelope)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

async fn replace_context(directory: &Path, conversation: &Conversation) -> Result<(), LogError> {
    let temporary = directory.join("chat_history.jsonl.tmp");
    let mut file = BufWriter::new(File::create(&temporary).await?);
    for message in conversation.model_messages() {
        let mut bytes = serde_json::to_vec(&Envelope::encode_message(&message)?)?;
        bytes.push(b'\n');
        file.write_all(&bytes).await?;
    }
    file.flush().await?;
    drop(file);
    fs::rename(temporary, directory.join("chat_history.jsonl")).await?;
    Ok(())
}
