use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

const MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;
pub(crate) const PEER_CHANNEL_CAPACITY: usize = 128;

/// A request from a collaborator to the host.
///
/// These are peer specific: the host answers each collaborator individually, so
/// they travel on the peer's own channel rather than the thread wide broadcast.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CollaboratorMessage {
    /// Always the first frame a collaborator sends. The host replies with
    /// [`HostMessage::Welcome`].
    Join,
}

/// A change to a shared thread, authored by the host.
///
/// Every variant except [`HostMessage::Welcome`] is a thread wide event: the
/// host applies it to its own thread and broadcasts the identical value to all
/// connected collaborators, who replay it onto their mirror of the timeline.
/// `Welcome` is peer specific and re-bases a single collaborator onto a full
/// snapshot, which is how a peer both joins and recovers from falling behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HostMessage {
    /// Replaces the collaborator's entire view of the thread.
    Welcome(ThreadSnapshot),
    /// The thread was named, which happens on its first user message.
    ThreadTitled(String),
    /// A user message was appended to the timeline.
    UserMessage(UserMessage),
    /// The agent started responding; an empty message is appended and the
    /// thread is marked as generating.
    AgentStarted { id: uuid::Bytes },
    /// A chunk of streamed agent output to append to an in-flight message.
    AgentTextAppended {
        id: uuid::Bytes,
        target: AgentText,
        text: String,
    },
    /// The agent stopped reasoning and is about to answer.
    AgentThinkingEnded { id: uuid::Bytes },
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
pub(crate) struct ThreadSnapshot {
    pub(crate) id: uuid::Bytes,
    pub(crate) title: String,
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
    pub(crate) text: String,
    pub(crate) comments: Vec<UserComment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserComment {
    pub(crate) id: uuid::Bytes,
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
    pub(crate) thinking: String,
    pub(crate) thinking_complete: bool,
    pub(crate) text: String,
    pub(crate) complete: bool,
    pub(crate) failed: bool,
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
            messages: vec![
                TimelineMessage::User(UserMessage {
                    id: [2; 16],
                    text: "Question".into(),
                    comments: vec![UserComment {
                        id: [3; 16],
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
                    thinking: "Reasoning".into(),
                    thinking_complete: true,
                    text: "Answer".into(),
                    complete: true,
                    failed: false,
                }),
            ],
        };
        let message = HostMessage::Welcome(snapshot);

        assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn agent_stream_events_round_trip_through_postcard() {
        for message in [
            HostMessage::AgentStarted { id: [7; 16] },
            HostMessage::AgentTextAppended {
                id: [7; 16],
                target: AgentText::Thinking,
                text: "Let me think".into(),
            },
            HostMessage::AgentThinkingEnded { id: [7; 16] },
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
