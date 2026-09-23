use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Bounds how much memory a single frame from a peer can make us buffer.
///
/// Attachments travel inline, so this must fit a user message at the
/// attachment limit, and a `Welcome` snapshot of a thread with several such
/// messages. A snapshot that grows beyond it cannot be sent.
const MAX_FRAME_LENGTH: usize = 256 * 1024 * 1024;
pub(crate) const PEER_CHANNEL_CAPACITY: usize = 128;

/// Host and collaborator must speak the same version exactly. Bump it on any
/// change to the messages below or to the model catalog.
///
/// For a mismatch to be reported rather than fail to decode, the encoding of
/// [`CollaboratorMessage::Join`] and [`HostMessage::Rejected`] must never
/// change: each keeps its variant index, and `Join` keeps the version as its
/// only field.
pub(crate) const PROTOCOL_VERSION: u32 = 1;

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
    /// The participant who submitted the message.
    pub(crate) author: uuid::Bytes,
    pub(crate) text: String,
    pub(crate) comments: Vec<UserComment>,
    pub(crate) attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attachment {
    pub(crate) name: String,
    pub(crate) content: AttachmentContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AttachmentContent {
    Text(String),
    Png(Vec<u8>),
    Jpeg(Vec<u8>),
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

    pub(crate) fn split(
        self,
    ) -> (
        async_channel::Sender<Outgoing>,
        async_channel::Receiver<Incoming>,
    ) {
        (self.outgoing, self.incoming)
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
    let (incoming_sender, incoming_receiver) = async_channel::bounded(PEER_CHANNEL_CAPACITY);

    tokio::spawn(async move {
        let mut framed = Framed::new(stream, codec());

        loop {
            tokio::select! {
                message = outgoing_receiver.recv() => {
                    let Ok(message) = message else {
                        break;
                    };
                    let bytes = postcard::to_stdvec(&message)
                        .context("Failed to encode protocol message.")?;
                    framed
                        .send(bytes.into())
                        .await
                        .context("Failed to send protocol message.")?;
                }
                frame = framed.next() => {
                    let Some(frame) = frame.transpose().context("Failed to receive protocol message.")? else {
                        break;
                    };
                    let message = postcard::from_bytes(&frame)
                        .context("Failed to decode protocol message.")?;
                    if incoming_sender.send(message).await.is_err() {
                        break;
                    }
                }
            }
        }

        Ok::<_, anyhow::Error>(())
    });

    Peer {
        outgoing: outgoing_sender,
        incoming: incoming_receiver,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    author: [11; 16],
                    text: "Question".into(),
                    attachments: vec![Attachment {
                        name: "notes.txt".into(),
                        content: AttachmentContent::Text("Details".into()),
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
