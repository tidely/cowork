use std::{
    marker::PhantomData,
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::models::{ModelCatalog, ModelRef};
pub(crate) use crate::thread::{PeerMode, PeerPermissions, PermissionDenied};

/// Bounds how much memory a single frame from a peer can make us buffer.
///
/// Generous, as transcript prompts carry their files inline: one message's
/// attachments are up to 32 MB before base64, and a `Welcome` snapshot holds
/// every prompt of the thread. A frame beyond it cannot be sent, which ends
/// the connection it was meant for.
const MAX_FRAME_LENGTH: usize = 1024 * 1024 * 1024;
pub(crate) const PEER_CHANNEL_CAPACITY: usize = 128;
/// How many bulk messages may wait to be written. Kept small, since anything
/// queued here is written before later bulk messages but after every waiting
/// control message.
const BULK_CHANNEL_CAPACITY: usize = 2;
/// The largest piece of an attachment sent at once, so that a large file
/// holds up other messages for no longer than one chunk takes to write.
pub(crate) const ATTACHMENT_CHUNK_SIZE: usize = 64 * 1024;

/// Host and collaborator must speak the same version exactly. Bump it on any
/// change to the messages below or to model identifier semantics.
///
/// For a mismatch to be reported rather than fail to decode, the encoding of
/// [`CollaboratorMessage::Join`] and [`HostMessage::Rejected`] must never
/// change: each keeps its variant index, and `Join` keeps the version as its
/// only field.
pub(crate) const PROTOCOL_VERSION: u32 = 23;

/// A request from a collaborator to the host.
///
/// These travel on the peer's own channel. The host applies the ones that
/// change the thread and broadcasts the outcome to everyone, including the
/// requesting collaborator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CollaboratorMessage {
    /// Always the first frame a collaborator sends. The host replies with
    /// [`HostMessage::Welcome`], or [`HostMessage::Rejected`] when it cannot
    /// serve this collaborator. Must remain the first variant.
    Join { protocol_version: u32 },
    /// The collaborator's profile. Always the second frame, which the host
    /// waits for before admitting the collaborator, and sent again whenever
    /// the collaborator changes it.
    Profile(Profile),
    /// Selects the thread's model. The host ignores models the thread's
    /// catalog does not offer.
    SelectModel(ModelRef),
    /// Stops the agent run producing message `message_id`, if it is still
    /// running.
    Stop { message_id: uuid::Bytes },
    /// A Yrs update from this peer's current draft replica. Generation is
    /// assigned by the host; queued bytes from a rejected replica stay fenced
    /// out even if the peer regains Write before they reach the host.
    DraftUpdate { generation: u64, update: Vec<u8> },
    /// Submits the draft. `sequence` is the number of user messages the
    /// collaborator has seen, so a submission that raced another one is
    /// ignored instead of submitting whatever was typed in between.
    Submit { sequence: u64 },
    /// Replaces the collaborator's presence.
    Presence(Presence),
    /// The collaborator stopped uploading a file whose record is gone, so
    /// the host can discard what it received. Sent on the bulk queue, after
    /// the file's last chunk.
    AttachmentCancelled(uuid::Bytes),
    /// Part of a file the collaborator attached to the draft. Sent on the
    /// bulk queue; see [`Peer::bulk`].
    AttachmentData(AttachmentChunk),
    /// Allows or denies the tool call `call` that the run producing message
    /// `message_id` is waiting on. The host ignores a call no run is
    /// waiting on, so a decision that raced another one changes nothing.
    DecideToolCall {
        message_id: uuid::Bytes,
        call: Json<rig::message::CallId>,
        allow: bool,
    },
}

/// How a participant presents themselves. Chosen by the participant and
/// purely cosmetic: names can collide, and identity is always the participant
/// id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Profile {
    /// `None` shows the name derived from the participant id.
    pub(crate) name: Option<String>,
    /// A square JPEG. `None` shows the participant's initials.
    pub(crate) picture: Option<Vec<u8>>,
    /// The id the fallback name, the initials, and the color are derived
    /// from instead of the participant id, which the host assigns anew on
    /// every join. Lets a participant look the same in every thread. `None`
    /// uses the participant id.
    pub(crate) appearance: Option<uuid::Bytes>,
}

/// One piece of an attachment's bytes, in order. Every piece names the file,
/// so it can be put together without its draft record, which may arrive
/// later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentChunk {
    pub(crate) id: uuid::Bytes,
    pub(crate) name: String,
    pub(crate) kind: AttachmentKind,
    /// The size of the whole file.
    pub(crate) total: u64,
    /// Where in the file `bytes` start.
    pub(crate) offset: u64,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AttachmentKind {
    Text,
    Png,
    Jpeg,
}

/// Where a participant is in the draft and what they are doing there.
/// Ephemeral: it is never part of the draft document.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Presence {
    pub(crate) focus: Option<PresenceFocus>,
    /// Only while focused in an item: anchors in its body, made with
    /// `draft::Draft::anchor`.
    pub(crate) selection: Option<PresenceSelection>,
    /// Files the participant is reading into the draft.
    pub(crate) pending_reads: Vec<PendingRead>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PresenceFocus {
    DraftPosition,
    Item(uuid::Bytes),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PresenceSelection {
    pub(crate) anchor: Vec<u8>,
    /// Where the caret is.
    pub(crate) head: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingRead {
    pub(crate) id: uuid::Bytes,
    pub(crate) name: String,
    pub(crate) is_image: bool,
    /// Percent read, once known.
    pub(crate) progress: Option<u8>,
    /// The block the file will be attached to, or `None` for a new block.
    pub(crate) block: Option<uuid::Bytes>,
}

/// A change to a shared thread, authored by the host.
///
/// Most variants are thread-wide events: the host applies them locally and
/// broadcasts the identical value for collaborators to replay. `Welcome`,
/// `Rejected`, `PermissionDenied`, and `DraftReset` are peer-specific.
/// `Welcome` joins or rebases a peer; both it and `DraftReset` preserve unsent
/// draft edits only at a matching generation. A newer generation replaces the
/// draft, discarding optimistic CRDT history the host rejected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HostMessage {
    /// Replaces the collaborator's entire view of the thread. Boxed, as it
    /// is far larger than every other event.
    Welcome(Box<Welcome>),
    /// The host refused to serve this collaborator and is about to close the
    /// connection. Carries a message to show to the user. Must remain the
    /// second variant.
    Rejected(String),
    /// A participant connected. Participants are listed in join order.
    ParticipantJoined {
        participant: uuid::Bytes,
        profile: Profile,
    },
    /// A participant disconnected. Their profile is kept, since their
    /// messages still name them.
    ParticipantLeft(uuid::Bytes),
    /// A participant changed their profile.
    ProfileChanged {
        participant: uuid::Bytes,
        profile: Profile,
    },
    /// The thread's model changed. The host only selects models the thread's
    /// catalog offers.
    ModelSelected(ModelRef),
    /// Replaces the thread's catalog. A selected model the new catalog no
    /// longer offers stays selected, but cannot run until another is picked.
    ModelCatalogChanged(ModelCatalog),
    /// Replaces the names of the folders in the thread's project, in the
    /// order the host added them. Only names travel: the folders are on the
    /// host's machine, and their paths would reveal how it is laid out.
    ProjectFoldersChanged(Vec<String>),
    /// A Yrs update to the draft, made by the host or a collaborator.
    DraftUpdate(Vec<u8>),
    /// A participant's presence changed.
    Presence {
        participant: uuid::Bytes,
        presence: Presence,
    },
    /// The host holds every byte of an attachment `uploader` added, so it can
    /// be submitted. The bytes follow as `AttachmentData` to everyone else.
    AttachmentStored {
        id: uuid::Bytes,
        uploader: uuid::Bytes,
    },
    /// Part of an attachment's bytes, relayed by the host. Sent on the bulk
    /// queue; see [`Peer::bulk`].
    AttachmentData(AttachmentChunk),
    /// The thread was named, which happens on its first user message.
    ThreadTitled(String),
    /// A user message was appended to the timeline.
    UserMessage(UserMessage),
    /// The agent started responding to `prompt`, which joins the transcript;
    /// an empty message is appended and the thread is marked as generating.
    /// `comment_group_id` identifies the submitted user comments rendered at
    /// the top of this response. `started_at` is when the host started
    /// generating it.
    AgentStarted {
        id: uuid::Bytes,
        comment_group_id: Option<uuid::Bytes>,
        started_at: SystemTime,
        prompt: Json<rig::completion::Message>,
    },
    /// Something the agent did while producing message `id`, exactly as the
    /// host's agent loop reported it. Every participant, the host included,
    /// folds these into the message and the transcript the same way; see
    /// `transcript.rs`.
    AgentEvent {
        id: uuid::Bytes,
        event: Json<agent::AgentEvent>,
    },
    /// The agent finished, and how. `duration` is how long the host spent
    /// generating the message, whether or not it completed.
    AgentEnded {
        id: uuid::Bytes,
        outcome: RunOutcome,
        duration: Duration,
    },
    /// `participant` is called `name` in prompts from now on. Sent when their
    /// first item is submitted; a name never changes afterwards.
    PromptNamed {
        participant: uuid::Bytes,
        name: String,
    },
    /// A peer-specific rejection of a runtime command; the session stays open.
    PermissionDenied(PermissionDenied),
    /// Replaces a peer's draft with fresh authoritative state. A generation
    /// change discards rejected optimistic history; a repeated reset for the
    /// same generation is a normal merge, preserving fresh authorized edits.
    DraftReset {
        generation: u64,
        state: Vec<u8>,
    },
    DefaultPeerModeChanged(PeerMode),
    /// `None` resumes live inheritance from the default.
    PeerModeOverrideChanged {
        participant: uuid::Bytes,
        mode: Option<PeerMode>,
    },
    /// The run producing message `id` is waiting for someone who may approve
    /// tool calls to allow or deny `call`, one of the calls of its last
    /// reply that has not returned.
    ToolApprovalRequested {
        id: uuid::Bytes,
        call: Json<rig::message::CallId>,
    },
    /// `call` was allowed or denied, and its result follows as an
    /// `AgentEvent`: a denied call's says so.
    ToolApprovalResolved {
        id: uuid::Bytes,
        call: Json<rig::message::CallId>,
    },
}

/// A typed value encoded as a JSON string on the wire. This lets types that
/// need a self-describing format travel through postcard without changing the
/// string's wire representation. Decoding the JSON is deferred to `parse`, so
/// a malformed value from a peer can be reported by its caller.
#[derive(Serialize, Deserialize)]
#[serde(transparent, bound(serialize = "", deserialize = ""))]
pub(crate) struct Json<T> {
    raw: String,
    #[serde(skip)]
    marker: PhantomData<fn() -> T>,
}

impl<T> Json<T> {
    pub(crate) fn from_value(value: &T) -> serde_json::Result<Self>
    where
        T: Serialize,
    {
        Ok(Self {
            raw: serde_json::to_string(value)?,
            marker: PhantomData,
        })
    }

    pub(crate) fn parse(&self) -> serde_json::Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_str(&self.raw)
    }
}

impl<T> Clone for Json<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for Json<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Json").field(&self.raw).finish()
    }
}

impl<T> PartialEq for Json<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Json<T> {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Welcome {
    /// The id the host assigned to the receiving collaborator.
    pub(crate) participant_id: uuid::Bytes,
    pub(crate) thread: ThreadSnapshot,
    /// The full state of the draft as a Yrs update.
    pub(crate) draft: Vec<u8>,
    /// This receiving peer's draft epoch, not a thread-wide revision. A changed
    /// epoch requires replacement, while matching epochs preserve unsent edits.
    pub(crate) draft_generation: u64,
    /// Every connected participant's presence.
    pub(crate) presence: Vec<(uuid::Bytes, Presence)>,
    /// The attachments whose bytes the host holds. They follow the welcome
    /// as `AttachmentData`.
    pub(crate) stored_attachments: Vec<uuid::Bytes>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ThreadSnapshot {
    pub(crate) id: uuid::Bytes,
    pub(crate) title: String,
    /// Connected participants in join order, starting with the host.
    pub(crate) participants: Vec<uuid::Bytes>,
    /// The profile of everyone who has joined, including those who left.
    pub(crate) profiles: Vec<(uuid::Bytes, Profile)>,
    /// The models the thread can run: the host's.
    pub(crate) models: ModelCatalog,
    /// The selected model, if any. It may be missing from `models`; see
    /// [`HostMessage::ModelCatalogChanged`].
    pub(crate) model: Option<ModelRef>,
    /// Host-owned default and sparse overrides. The host is always Admin and
    /// alone manages policy; Admin peers cannot delegate access.
    pub(crate) peer_permissions: PeerPermissions,
    /// The context window use the provider last measured, if any.
    pub(crate) context_tokens: Option<u64>,
    /// Bytes of agent output streamed since `context_tokens` was measured.
    pub(crate) streamed_bytes: u64,
    pub(crate) messages: Vec<TimelineMessage>,
    /// Rig messages need JSON's self-describing format; postcard alone cannot decode them.
    pub(crate) transcript: Vec<Json<rig::completion::Message>>,
    /// Everyone's name in prompts, sorted by participant.
    pub(crate) prompt_names: Vec<(uuid::Bytes, String)>,
    /// The names of the project's folders; see
    /// [`HostMessage::ProjectFoldersChanged`].
    pub(crate) project_folders: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TimelineMessage {
    User(UserMessage),
    Agent(AgentMessage),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserMessage {
    pub(crate) id: uuid::Bytes,
    pub(crate) comments: Vec<UserComment>,
    /// The submitted prompt blocks in draft order.
    pub(crate) blocks: Vec<PromptBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PromptBlock {
    pub(crate) id: uuid::Bytes,
    /// The participant who created the block.
    pub(crate) author: uuid::Bytes,
    pub(crate) text: String,
    /// The block's files; their bytes travel separately as `AttachmentData`.
    pub(crate) attachments: Vec<AttachmentRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentRef {
    pub(crate) id: uuid::Bytes,
    pub(crate) name: String,
    pub(crate) kind: AttachmentKind,
    pub(crate) size: u64,
    pub(crate) creator: uuid::Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserComment {
    pub(crate) id: uuid::Bytes,
    /// The participant who wrote the comment.
    pub(crate) author: uuid::Bytes,
    pub(crate) reference: CommentReference,
    pub(crate) body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommentReference {
    pub(crate) message_id: uuid::Bytes,
    pub(crate) range: std::ops::Range<usize>,
    pub(crate) quote: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentMessage {
    pub(crate) id: uuid::Bytes,
    pub(crate) comment_group_id: Option<uuid::Bytes>,
    /// See [`HostMessage::AgentStarted`].
    pub(crate) started_at: SystemTime,
    /// Where the prompt this message answers is in the transcript. Each run
    /// adds exactly its prompt there, with `AgentStarted`, so its output is
    /// the entries after it, up to the next agent message's prompt.
    pub(crate) prompt: usize,
    /// The run's events since its output last joined the transcript, exactly
    /// as the host folded them. While generating, they are the turn in
    /// progress, which someone joining folds on from. Once a run is stopped
    /// or fails mid-turn, they are output the transcript never gets. Empty
    /// after a turn completes.
    ///
    /// What the message shows is derived from its transcript entries and
    /// these, never sent separately; see `transcript.rs`.
    pub(crate) pending_events: Vec<Json<agent::AgentEvent>>,
    pub(crate) run: AgentRun,
    /// The tool call the run is waiting for approval of; see
    /// [`HostMessage::ToolApprovalRequested`].
    pub(crate) awaiting_approval: Option<Json<rig::message::CallId>>,
}

/// Whether an agent message's run is still going.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AgentRun {
    Generating,
    /// See [`HostMessage::AgentEnded`].
    Ended {
        outcome: RunOutcome,
        duration: Duration,
    },
}

/// How an agent run ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RunOutcome {
    Completed,
    /// Someone stopped it.
    Stopped,
    /// It failed, with a message to display when the agent produced no
    /// output of its own.
    Failed(String),
}

impl AgentRun {
    pub(crate) fn is_generating(&self) -> bool {
        matches!(self, Self::Generating)
    }

    /// How the run ended. `None` while it is still going.
    pub(crate) fn outcome(&self) -> Option<&RunOutcome> {
        match self {
            Self::Generating => None,
            Self::Ended { outcome, .. } => Some(outcome),
        }
    }

    /// Why the run failed, if it did.
    pub(crate) fn failure(&self) -> Option<&str> {
        match self.outcome() {
            Some(RunOutcome::Failed(failure)) => Some(failure),
            _ => None,
        }
    }

    /// How long the host spent generating. `None` while it still is.
    pub(crate) fn duration(&self) -> Option<Duration> {
        match self {
            Self::Generating => None,
            Self::Ended { duration, .. } => Some(*duration),
        }
    }
}

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_LENGTH)
        .new_codec()
}

/// A typed, bidirectional message channel over one connection.
///
/// The type parameters name the direction, so a host holding a
/// `Peer<HostMessage, CollaboratorMessage>` cannot accidentally be handed the
/// collaborator's half.
pub(crate) struct Peer<Outgoing, Incoming> {
    pub(crate) outgoing: async_channel::Sender<Outgoing>,
    /// For attachment bytes. Whatever waits on `outgoing` is written first,
    /// so large files never hold up draft updates or presence. Messages
    /// already queued on `outgoing` are written before later bulk ones, but
    /// anything still on its way to `outgoing` can be overtaken.
    pub(crate) bulk: async_channel::Sender<Outgoing>,
    pub(crate) incoming: async_channel::Receiver<Incoming>,
}

impl<Outgoing, Incoming> Peer<Outgoing, Incoming> {
    pub(crate) async fn send(
        &self,
        message: Outgoing,
    ) -> Result<(), async_channel::SendError<Outgoing>> {
        self.outgoing.send(message).await
    }

    pub(crate) async fn receive(&self) -> Option<Incoming> {
        self.incoming.recv().await.ok()
    }

    /// The control and bulk senders, and the receiver.
    pub(crate) fn split(
        self,
    ) -> (
        async_channel::Sender<Outgoing>,
        async_channel::Sender<Outgoing>,
        async_channel::Receiver<Incoming>,
    ) {
        (self.outgoing, self.bulk, self.incoming)
    }
}

/// Drives postcard encoding and length delimited framing for one connection on
/// the tokio runtime, exposing it as a pair of channels.
///
/// The returned [`Peer`] is the only writer to the wire, so messages reach the
/// other side in the order they were sent. Callers that fan a broadcast out to
/// many peers forward through [`Peer::send`] for that reason.
pub(crate) fn spawn_peer<Outgoing, Incoming>(
    stream: impl AsyncWrite + AsyncRead + Send + Unpin + 'static,
) -> Peer<Outgoing, Incoming>
where
    Outgoing: Serialize + Send + 'static,
    Incoming: DeserializeOwned + Send + 'static,
{
    let (outgoing_sender, outgoing_receiver) = async_channel::bounded(PEER_CHANNEL_CAPACITY);
    let (bulk_sender, bulk_receiver) = async_channel::bounded::<Outgoing>(BULK_CHANNEL_CAPACITY);
    let (incoming_sender, incoming_receiver) = async_channel::bounded(PEER_CHANNEL_CAPACITY);
    let (read, write) = tokio::io::split(stream);
    // Dropped when writing ends, which ends reading too, so that dropping the
    // `Peer` closes the connection.
    let (writing, stopped_writing) = tokio::sync::oneshot::channel::<()>();

    // Writing and reading run separately: a write waiting for the other side
    // to catch up, as a large file can make it, must never stop this side
    // reading, or two sides sending at once would wait on each other forever.
    tokio::spawn(async move {
        let _writing = writing;
        let mut writer = FramedWrite::new(write, codec());
        loop {
            let message = tokio::select! {
                // Control messages first, so bulk data waits for them.
                biased;
                message = outgoing_receiver.recv() => {
                    let Ok(message) = message else {
                        break;
                    };
                    message
                }
                Ok(message) = bulk_receiver.recv() => message,
            };
            let bytes =
                postcard::to_stdvec(&message).context("Failed to encode protocol message.")?;
            writer
                .send(bytes.into())
                .await
                .context("Failed to send protocol message.")?;
        }
        Ok::<_, anyhow::Error>(())
    });
    tokio::spawn(async move {
        let mut reader = FramedRead::new(read, codec());
        let mut stopped_writing = stopped_writing;
        loop {
            let frame = tokio::select! {
                _ = &mut stopped_writing => break,
                frame = reader.next() => frame,
            };
            let Some(frame) = frame
                .transpose()
                .context("Failed to receive protocol message.")?
            else {
                break;
            };
            let message =
                postcard::from_bytes(&frame).context("Failed to decode protocol message.")?;
            if incoming_sender.send(message).await.is_err() {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    });

    Peer {
        outgoing: outgoing_sender,
        bulk: bulk_sender,
        incoming: incoming_receiver,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelInfo, ModelProvider};

    fn sample_model() -> ModelRef {
        ModelRef {
            provider: ModelProvider::Ollama,
            id: "qwen:latest".into(),
        }
    }

    fn sample_catalog() -> ModelCatalog {
        let mut catalog = ModelCatalog::default();
        catalog.set_provider(
            ModelProvider::Ollama,
            [
                (
                    "qwen:latest".into(),
                    ModelInfo {
                        name: "Qwen".into(),
                        max_tokens: 131_072,
                    },
                ),
                (
                    "llama:latest".into(),
                    ModelInfo {
                        name: "Llama".into(),
                        max_tokens: 8_192,
                    },
                ),
            ],
        );
        catalog
    }

    fn sample_profile() -> Profile {
        Profile {
            name: Some("Ada".into()),
            picture: Some(vec![0xff, 0xd8, 0xff]),
            appearance: Some([15; 16]),
        }
    }

    fn sample_presence() -> Presence {
        Presence {
            focus: Some(PresenceFocus::Item([5; 16])),
            selection: Some(PresenceSelection {
                anchor: vec![1],
                head: vec![2],
            }),
            pending_reads: vec![PendingRead {
                id: [6; 16],
                name: "notes.txt".into(),
                is_image: false,
                progress: Some(40),
                block: None,
            }],
        }
    }

    fn sample_chunk() -> AttachmentChunk {
        AttachmentChunk {
            id: [14; 16],
            name: "notes.txt".into(),
            kind: AttachmentKind::Text,
            total: 7,
            offset: 2,
            bytes: b"tail".to_vec(),
        }
    }

    /// Control messages are written before bulk ones that were waiting at
    /// the same time.
    /// Both sides sending bulk data at once still read each other's control
    /// messages.
    #[tokio::test]
    async fn bulk_transfers_both_ways_do_not_block_reading() {
        // Far smaller than what is sent, so both writers have to wait.
        let (near, far) = tokio::io::duplex(8 * 1024);
        let host: Peer<HostMessage, CollaboratorMessage> = spawn_peer(near);
        let collaborator: Peer<CollaboratorMessage, HostMessage> = spawn_peer(far);
        let chunk = AttachmentChunk {
            bytes: vec![0; ATTACHMENT_CHUNK_SIZE],
            ..sample_chunk()
        };
        let host_bulk = host.bulk.clone();
        let host_chunk = chunk.clone();
        tokio::spawn(async move {
            while host_bulk
                .send(HostMessage::AttachmentData(host_chunk.clone()))
                .await
                .is_ok()
            {}
        });
        let collaborator_bulk = collaborator.bulk.clone();
        tokio::spawn(async move {
            while collaborator_bulk
                .send(CollaboratorMessage::AttachmentData(chunk.clone()))
                .await
                .is_ok()
            {}
        });
        host.send(HostMessage::ParticipantLeft([1; 16]))
            .await
            .expect("send control");
        collaborator
            .send(CollaboratorMessage::Submit { sequence: 1 })
            .await
            .expect("send control");

        let received = async {
            let host_got = async {
                loop {
                    if let Some(CollaboratorMessage::Submit { .. }) = host.receive().await {
                        return;
                    }
                }
            };
            let collaborator_got = async {
                loop {
                    if let Some(HostMessage::ParticipantLeft(_)) = collaborator.receive().await {
                        return;
                    }
                }
            };
            tokio::join!(host_got, collaborator_got);
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), received)
            .await
            .expect("both control messages arrive");
    }

    #[tokio::test]
    async fn control_messages_overtake_waiting_bulk_messages() {
        let (near, far) = tokio::io::duplex(1 << 20);
        let sender: Peer<HostMessage, CollaboratorMessage> = spawn_peer(near);
        let receiver: Peer<CollaboratorMessage, HostMessage> = spawn_peer(far);
        // Both queued before the writer task first runs.
        sender
            .bulk
            .try_send(HostMessage::AttachmentData(sample_chunk()))
            .expect("queue bulk");
        sender
            .outgoing
            .try_send(HostMessage::ParticipantLeft([1; 16]))
            .expect("queue control");

        assert_eq!(
            receiver.receive().await,
            Some(HostMessage::ParticipantLeft([1; 16]))
        );
        assert_eq!(
            receiver.receive().await,
            Some(HostMessage::AttachmentData(sample_chunk()))
        );
    }

    fn json<T>(raw: &str) -> Json<T> {
        let encoded = postcard::to_stdvec(raw).expect("encode JSON string");
        postcard::from_bytes(&encoded).expect("decode typed JSON string")
    }

    #[test]
    fn typed_json_uses_string_wire_encoding() {
        let value = Json::from_value(&vec![1, 2, 3]).expect("encode JSON");
        assert_eq!(value.parse().expect("decode JSON"), vec![1, 2, 3]);
        assert_eq!(
            postcard::to_stdvec(&value).unwrap(),
            postcard::to_stdvec(&"[1,2,3]").unwrap()
        );
        assert!(json::<Vec<u8>>("not json").parse().is_err());
    }

    fn round_trip(message: &HostMessage) -> HostMessage {
        let encoded = postcard::to_stdvec(message).expect("encode protocol message");
        postcard::from_bytes(&encoded).expect("decode protocol message")
    }

    #[test]
    fn snapshot_round_trips_through_postcard() {
        let snapshot = ThreadSnapshot {
            id: [1; 16],
            title: "Shared thread".into(),
            participants: vec![[11; 16], [12; 16]],
            profiles: vec![([11; 16], sample_profile()), ([13; 16], Profile::default())],
            models: sample_catalog(),
            model: Some(sample_model()),
            peer_permissions: PeerPermissions::default(),
            context_tokens: Some(4_096),
            streamed_bytes: 120,
            messages: vec![
                TimelineMessage::User(UserMessage {
                    id: [2; 16],
                    blocks: vec![PromptBlock {
                        id: [13; 16],
                        author: [11; 16],
                        text: "Question".into(),
                        attachments: vec![AttachmentRef {
                            id: [14; 16],
                            name: "notes.txt".into(),
                            kind: AttachmentKind::Text,
                            size: 7,
                            creator: [11; 16],
                        }],
                    }],
                    comments: vec![UserComment {
                        id: [3; 16],
                        author: [12; 16],
                        reference: CommentReference {
                            message_id: [4; 16],
                            range: 5..10,
                            quote: "quote".into(),
                        },
                        body: "comment".into(),
                    }],
                }),
                TimelineMessage::Agent(AgentMessage {
                    id: [4; 16],
                    comment_group_id: Some([2; 16]),
                    started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                    prompt: 0,
                    pending_events: vec![json("{}")],
                    run: AgentRun::Ended {
                        outcome: RunOutcome::Failed("Failed".into()),
                        duration: Duration::from_millis(12_345),
                    },
                    awaiting_approval: Some(Json::call_id(&rig::message::CallId::from_wire(
                        "call_1",
                    ))),
                }),
            ],
            transcript: vec![json(r#"{"role":"user"}"#), json(r#"{"role":"assistant"}"#)],
            prompt_names: vec![([11; 16], "Ada".into())],
            project_folders: vec!["zed".into(), "cowork".into()],
        };
        let message = HostMessage::Welcome(Box::new(Welcome {
            participant_id: [12; 16],
            thread: snapshot,
            draft: vec![1, 2, 3],
            draft_generation: 42,
            presence: vec![([12; 16], sample_presence())],
            stored_attachments: vec![[14; 16]],
        }));

        assert_eq!(round_trip(&message), message);
    }

    /// Guards the part of the encoding every version must share; see
    /// [`PROTOCOL_VERSION`].
    #[test]
    fn version_handshake_encoding_is_stable() {
        let join = CollaboratorMessage::Join {
            protocol_version: 7,
        };
        assert_eq!(postcard::to_stdvec(&join).expect("encode join"), [0, 7]);

        let rejected = HostMessage::Rejected("no".into());
        assert_eq!(
            postcard::to_stdvec(&rejected).expect("encode rejection"),
            [1, 2, b'n', b'o']
        );
    }

    #[test]
    fn membership_and_control_messages_round_trip_through_postcard() {
        for message in [
            HostMessage::Rejected("Version mismatch".into()),
            HostMessage::ModelCatalogChanged(ModelCatalog::default()),
            HostMessage::ModelCatalogChanged(sample_catalog()),
            HostMessage::ProjectFoldersChanged(Vec::new()),
            HostMessage::ProjectFoldersChanged(vec!["zed".into(), "cowork".into()]),
            HostMessage::ParticipantJoined {
                participant: [1; 16],
                profile: sample_profile(),
            },
            HostMessage::ParticipantLeft([1; 16]),
            HostMessage::ProfileChanged {
                participant: [1; 16],
                profile: Profile::default(),
            },
            HostMessage::ModelSelected(sample_model()),
            HostMessage::DraftUpdate(vec![4, 5, 6]),
            HostMessage::DraftReset {
                generation: 7,
                state: vec![7, 8, 9],
            },
            HostMessage::PermissionDenied(PermissionDenied {
                participant: [1; 16],
                operation: crate::thread::PermissionOperation::EditDraft,
                reason: crate::thread::DenialReason::InsufficientMode,
            }),
            HostMessage::PermissionDenied(PermissionDenied {
                participant: [1; 16],
                operation: crate::thread::PermissionOperation::EditDraft,
                reason: crate::thread::DenialReason::StaleDraftGeneration,
            }),
            HostMessage::DefaultPeerModeChanged(PeerMode::ReadOnly),
            HostMessage::PeerModeOverrideChanged {
                participant: [1; 16],
                mode: Some(PeerMode::Write),
            },
            HostMessage::PeerModeOverrideChanged {
                participant: [1; 16],
                mode: None,
            },
            HostMessage::Presence {
                participant: [1; 16],
                presence: sample_presence(),
            },
            HostMessage::AttachmentStored {
                id: [14; 16],
                uploader: [1; 16],
            },
            HostMessage::AttachmentData(sample_chunk()),
        ] {
            assert_eq!(round_trip(&message), message);
        }

        for message in [
            CollaboratorMessage::Join {
                protocol_version: PROTOCOL_VERSION,
            },
            CollaboratorMessage::Profile(sample_profile()),
            CollaboratorMessage::SelectModel(sample_model()),
            CollaboratorMessage::Stop {
                message_id: [2; 16],
            },
            CollaboratorMessage::DraftUpdate {
                generation: 7,
                update: vec![7, 8],
            },
            CollaboratorMessage::Submit { sequence: 3 },
            CollaboratorMessage::Presence(sample_presence()),
            CollaboratorMessage::AttachmentData(sample_chunk()),
            CollaboratorMessage::DecideToolCall {
                message_id: [2; 16],
                call: Json::call_id(&rig::message::CallId::from_wire("call_1")),
                allow: true,
            },
            // A call id rig minted for a call the provider sent without one.
            CollaboratorMessage::DecideToolCall {
                message_id: [2; 16],
                call: Json::call_id(&rig::message::CallId::from_wire("")),
                allow: false,
            },
        ] {
            let encoded = postcard::to_stdvec(&message).expect("encode protocol message");
            let decoded: CollaboratorMessage =
                postcard::from_bytes(&encoded).expect("decode protocol message");
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn agent_stream_events_round_trip_through_postcard() {
        for message in [
            HostMessage::AgentStarted {
                id: [7; 16],
                comment_group_id: Some([8; 16]),
                started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                prompt: json(r#"{"role":"user"}"#),
            },
            HostMessage::AgentEvent {
                id: [7; 16],
                event: json(r#"{"Model":{}}"#),
            },
            HostMessage::AgentEnded {
                id: [7; 16],
                outcome: RunOutcome::Failed("Unable to generate a response".into()),
                duration: Duration::from_millis(2_500),
            },
            HostMessage::AgentEnded {
                id: [7; 16],
                outcome: RunOutcome::Stopped,
                duration: Duration::from_millis(900),
            },
            HostMessage::AgentEnded {
                id: [7; 16],
                outcome: RunOutcome::Completed,
                duration: Duration::from_secs(53),
            },
            HostMessage::PromptNamed {
                participant: [11; 16],
                name: "Ada".into(),
            },
            HostMessage::ToolApprovalRequested {
                id: [7; 16],
                call: Json::call_id(&rig::message::CallId::from_wire("call_1")),
            },
            HostMessage::ToolApprovalResolved {
                id: [7; 16],
                call: Json::call_id(&rig::message::CallId::from_wire("")),
            },
        ] {
            assert_eq!(round_trip(&message), message);
        }
    }

    /// A call id comes back as the same id, including one rig minted, which
    /// cannot be rebuilt from its text.
    #[test]
    fn call_ids_survive_the_wire() {
        for call in [
            rig::message::CallId::from_wire("call_1"),
            rig::message::CallId::from_wire(""),
        ] {
            let message = HostMessage::ToolApprovalRequested {
                id: [7; 16],
                call: Json::call_id(&call),
            };
            let HostMessage::ToolApprovalRequested { call: decoded, .. } = round_trip(&message)
            else {
                unreachable!()
            };
            assert_eq!(decoded.to_call_id().expect("a call id"), call);
        }
    }
}
