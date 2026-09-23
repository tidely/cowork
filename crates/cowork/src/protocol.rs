use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

/// Bounds how much memory a single frame from a peer can make us buffer.
///
/// Attachment bytes travel in chunks, so frames only need to fit text: a
/// `Welcome` snapshot of a long thread. A snapshot that grows beyond it cannot
/// be sent.
const MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;
pub(crate) const PEER_CHANNEL_CAPACITY: usize = 128;
/// How many bulk messages may wait to be written. Kept small, since anything
/// queued here is written before later bulk messages but after every waiting
/// control message.
const BULK_CHANNEL_CAPACITY: usize = 2;
/// The largest piece of an attachment sent at once, so that a large file
/// holds up other messages for no longer than one chunk takes to write.
pub(crate) const ATTACHMENT_CHUNK_SIZE: usize = 64 * 1024;

/// Host and collaborator must speak the same version exactly. Bump it on any
/// change to the messages below or to the model catalog.
///
/// For a mismatch to be reported rather than fail to decode, the encoding of
/// [`CollaboratorMessage::Join`] and [`HostMessage::Rejected`] must never
/// change: each keeps its variant index, and `Join` keeps the version as its
/// only field.
pub(crate) const PROTOCOL_VERSION: u32 = 5;

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
    /// Selects the thread's model by its catalog id. Unknown ids are ignored.
    SelectModel { catalog_id: String },
    /// Stops the agent run producing message `message_id`, if it is still
    /// running.
    Stop { message_id: uuid::Bytes },
    /// A Yrs update of the collaborator's own changes to the draft.
    DraftUpdate(Vec<u8>),
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
/// Every variant except [`HostMessage::Welcome`] and [`HostMessage::Rejected`]
/// is a thread wide event: the host applies it to its own thread and
/// broadcasts the identical value to all connected collaborators, who replay
/// it onto their mirror of the timeline. `Welcome` is peer specific and
/// re-bases a single collaborator onto a full snapshot, which is how a peer
/// both joins and recovers from falling behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HostMessage {
    /// Replaces the collaborator's entire view of the thread.
    Welcome(Welcome),
    /// The host refused to serve this collaborator and is about to close the
    /// connection. Carries a message to show to the user. Must remain the
    /// second variant.
    Rejected(String),
    /// A participant connected. Participants are listed in join order.
    ParticipantJoined(uuid::Bytes),
    /// A participant disconnected.
    ParticipantLeft(uuid::Bytes),
    /// The thread's model changed.
    ModelSelected { catalog_id: String },
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
    /// The agent started responding; an empty message is appended and the
    /// thread is marked as generating. `comment_group_id` identifies the
    /// submitted user comments rendered at the top of this response.
    AgentStarted {
        id: uuid::Bytes,
        comment_group_id: Option<uuid::Bytes>,
    },
    /// A chunk of streamed agent output to append to an in-flight message.
    AgentTextAppended {
        id: uuid::Bytes,
        target: AgentText,
        text: String,
    },
    /// The agent stopped reasoning and is about to answer.
    AgentThinkingEnded { id: uuid::Bytes },
    /// The agent responded to one submitted comment.
    AgentCommentResponded {
        id: uuid::Bytes,
        response_id: uuid::Bytes,
        comment_id: uuid::Bytes,
        response: String,
    },
    /// The agent finished. `failure` carries a message to display when the
    /// agent produced no output of its own.
    AgentEnded {
        id: uuid::Bytes,
        failure: Option<String>,
    },
}

/// Which half of an agent message a [`HostMessage::AgentTextAppended`] extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AgentText {
    Thinking,
    Response,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Welcome {
    /// The id the host assigned to the receiving collaborator.
    pub(crate) participant_id: uuid::Bytes,
    pub(crate) thread: ThreadSnapshot,
    /// The full state of the draft as a Yrs update.
    pub(crate) draft: Vec<u8>,
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
    /// Catalog id of the thread's model.
    pub(crate) model: String,
    pub(crate) messages: Vec<TimelineMessage>,
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
    pub(crate) comment_responses: Vec<AgentCommentResponse>,
    pub(crate) thinking: String,
    pub(crate) thinking_complete: bool,
    pub(crate) text: String,
    pub(crate) complete: bool,
    pub(crate) failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentCommentResponse {
    pub(crate) id: uuid::Bytes,
    pub(crate) comment_id: uuid::Bytes,
    pub(crate) response: String,
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
        host.send(HostMessage::ParticipantJoined([1; 16]))
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
                    if let Some(HostMessage::ParticipantJoined(_)) = collaborator.receive().await {
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
            .try_send(HostMessage::ParticipantJoined([1; 16]))
            .expect("queue control");

        assert_eq!(
            receiver.receive().await,
            Some(HostMessage::ParticipantJoined([1; 16]))
        );
        assert_eq!(
            receiver.receive().await,
            Some(HostMessage::AttachmentData(sample_chunk()))
        );
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
            model: "catalog-model".into(),
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
                    comment_responses: vec![AgentCommentResponse {
                        id: [5; 16],
                        comment_id: [3; 16],
                        response: "Reply".into(),
                    }],
                    thinking: "Reasoning".into(),
                    thinking_complete: true,
                    text: "Answer".into(),
                    complete: true,
                    failed: false,
                }),
            ],
        };
        let message = HostMessage::Welcome(Welcome {
            participant_id: [12; 16],
            thread: snapshot,
            draft: vec![1, 2, 3],
            presence: vec![([12; 16], sample_presence())],
            stored_attachments: vec![[14; 16]],
        });

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
            HostMessage::ParticipantJoined([1; 16]),
            HostMessage::ParticipantLeft([1; 16]),
            HostMessage::ModelSelected {
                catalog_id: "catalog-model".into(),
            },
            HostMessage::DraftUpdate(vec![4, 5, 6]),
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
            CollaboratorMessage::SelectModel {
                catalog_id: "catalog-model".into(),
            },
            CollaboratorMessage::Stop {
                message_id: [2; 16],
            },
            CollaboratorMessage::DraftUpdate(vec![7, 8]),
            CollaboratorMessage::Submit { sequence: 3 },
            CollaboratorMessage::Presence(sample_presence()),
            CollaboratorMessage::AttachmentData(sample_chunk()),
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
            },
            HostMessage::AgentTextAppended {
                id: [7; 16],
                target: AgentText::Thinking,
                text: "Let me think".into(),
            },
            HostMessage::AgentThinkingEnded { id: [7; 16] },
            HostMessage::AgentCommentResponded {
                id: [7; 16],
                response_id: [10; 16],
                comment_id: [9; 16],
                response: "Reply".into(),
            },
            HostMessage::AgentTextAppended {
                id: [7; 16],
                target: AgentText::Response,
                text: "Here you go".into(),
            },
            HostMessage::AgentEnded {
                id: [7; 16],
                failure: Some("Unable to generate a response".into()),
            },
        ] {
            assert_eq!(round_trip(&message), message);
        }
    }
}
