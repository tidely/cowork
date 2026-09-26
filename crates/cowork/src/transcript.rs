//! The transcript and the agent's events: on the wire, and folded into a
//! thread.
//!
//! The transcript is everything the agent has been sent and has replied,
//! exactly as sent, as Rig's own messages. Prompts carry their files' content
//! like any other request does.
//!
//! The host forwards what its agent loop reports ([`AgentEvent`]s) as it
//! happens, and every participant, the host included, folds them with the
//! agent crate's [`TurnFold`]: into the agent message the timeline shows,
//! live, and into the transcript, which ends up exactly as the loop recorded
//! it. So every copy of a thread holds the same conversation.

use agent::{AgentEvent, TurnFold};
use anyhow::Context as _;
use gpui::AppContext;
use gpui_base::TextViewState;
use rig::{
    completion::{AssistantContent, Message as RigMessage},
    message::ToolCall,
    streaming::{BlockClose, Delta, StreamEvent},
    tool::Tool as _,
};
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

use crate::{
    protocol::{AgentEventMessage, TranscriptMessage},
    thread::Thread,
    timeline::{AgentCommentResponse, TimelineMessage},
    usage::usage_tokens,
};

impl TranscriptMessage {
    pub(crate) fn from_rig(message: &RigMessage) -> Self {
        // Rig's messages are plain data, which always encodes.
        Self(serde_json::to_string(message).expect("a Rig message encodes as JSON"))
    }

    pub(crate) fn to_rig(&self) -> anyhow::Result<RigMessage> {
        serde_json::from_str(&self.0).context("failed to decode a transcript message")
    }
}

impl AgentEventMessage {
    /// The event as it is folded, or `None` when folding ignores it: the
    /// stream's terminal record, whose usage and message id `TurnEnded`
    /// carries, and provider payloads Rig does not model. A block's end
    /// loses the block it completed, which folding rebuilds from what
    /// streamed before, rather than sending its content again.
    pub(crate) fn shared(event: AgentEvent) -> Option<Self> {
        let event = match event {
            AgentEvent::Model(StreamEvent::Final(_) | StreamEvent::Unknown(_)) => return None,
            AgentEvent::Model(StreamEvent::BlockEnd { id, end, .. }) => {
                AgentEvent::Model(StreamEvent::BlockEnd {
                    id,
                    end,
                    block: None,
                })
            }
            event => event,
        };
        // Rig's events are plain data, which always encodes.
        Some(Self(
            serde_json::to_string(&event).expect("an agent event encodes as JSON"),
        ))
    }

    pub(crate) fn to_agent(&self) -> anyhow::Result<AgentEvent> {
        serde_json::from_str(&self.0).context("failed to decode an agent event")
    }
}

impl Thread {
    pub(crate) fn push_transcript(&mut self, message: &TranscriptMessage) -> anyhow::Result<()> {
        self.transcript.push(message.to_rig()?);
        Ok(())
    }

    /// Folds an event of the agent producing message `message_id` into it
    /// and into the transcript.
    pub(crate) fn apply_agent_event(
        &mut self,
        message_id: Uuid,
        event: AgentEventMessage,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        let decoded = event.to_agent()?;
        self.agent_events.push(event);
        let block = self.fold_agent_event(&decoded);
        self.show_agent_event(message_id, &decoded, block, cx);
        Ok(())
    }

    /// Picks up the running agent's turn from a snapshot: its transcript,
    /// and the events since the transcript last grew.
    pub(crate) fn resume_agent_turn(
        &mut self,
        events: Vec<AgentEventMessage>,
    ) -> anyhow::Result<()> {
        self.agent_turn = self
            .transcript
            .last()
            .map(TurnFold::after)
            .unwrap_or_default();
        self.agent_events.clear();
        for event in events {
            let decoded = event.to_agent()?;
            self.agent_events.push(event);
            // The snapshot's timeline already shows them.
            self.fold_agent_event(&decoded);
        }
        Ok(())
    }

    /// Folds an event into the transcript. Returns the block of the reply it
    /// completed, if any.
    fn fold_agent_event(&mut self, event: &AgentEvent) -> Option<AssistantContent> {
        let folded = self.agent_turn.apply(event);
        if let Some(message) = folded.message {
            self.transcript.push(message);
            self.agent_events.clear();
        }
        folded.block
    }

    /// Shows an event in agent message `message_id`: its streamed text and
    /// thinking, its replies to comments, and the context measured.
    fn show_agent_event(
        &mut self,
        message_id: Uuid,
        event: &AgentEvent,
        block: Option<AssistantContent>,
        cx: &mut impl AppContext,
    ) {
        match event {
            AgentEvent::Model(StreamEvent::BlockDelta { delta, .. }) => {
                let (text, thinking) = match delta {
                    Delta::Text { text } => (text, false),
                    Delta::Reasoning { text } => (text, true),
                    _ => return,
                };
                self.streamed_bytes += text.len() as u64;
                let Some(message) = self.agent_message_mut(message_id.into_bytes()) else {
                    return;
                };
                let view = if thinking {
                    message.thinking.push_str(text);
                    message.thinking_view.clone()
                } else {
                    // Some models never close the reasoning block, so the
                    // first answer token ends it instead.
                    if !message.thinking.is_empty() && !message.thinking_complete {
                        message.thinking_complete = true;
                        message.thinking_expanded = false;
                    }
                    message.text.push_str(text);
                    message.text_view.clone()
                };
                view.update(cx, |view, cx| view.push_str(text, cx));
            }
            AgentEvent::Model(StreamEvent::BlockEnd {
                end: BlockClose::Reasoning { .. },
                ..
            }) => {
                if let Some(message) = self.agent_message_mut(message_id.into_bytes()) {
                    message.thinking_complete = true;
                    message.thinking_expanded = false;
                }
            }
            // Each request sends the whole transcript, so its usage is how
            // full the context is.
            AgentEvent::TurnEnded { usage, .. } if usage.is_reported() => {
                self.context_tokens = Some(usage_tokens(*usage));
                self.streamed_bytes = 0;
            }
            _ => {}
        }
        if let Some(AssistantContent::ToolCall(call)) = block {
            self.show_comment_response(message_id, &call, cx);
        }
    }

    /// Shows the reply to a comment a `respond_to_comment` call makes, once
    /// per comment.
    fn show_comment_response(
        &mut self,
        message_id: Uuid,
        call: &ToolCall,
        cx: &mut impl AppContext,
    ) {
        if call.function.name != RespondToComment::NAME {
            return;
        }
        let Ok(args) =
            serde_json::from_value::<RespondToCommentArgs>(call.function.arguments.clone())
        else {
            return;
        };
        if args.response.trim().is_empty() {
            return;
        }
        // The prompt named the submission's comments by their position.
        let comment_group = self.timeline.iter().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if message.id == message_id => message.comment_group_id,
            _ => None,
        });
        let comments = self
            .timeline
            .iter()
            .find_map(|entry| match entry {
                TimelineMessage::User(group) if Some(group.id) == comment_group => {
                    Some(group.comments.iter().map(|comment| comment.id).collect())
                }
                _ => None,
            })
            .unwrap_or_else(Vec::new);
        let Some(comment_id) = TurnComments::new(comments.len())
            .comment_ids()
            .iter()
            .position(|alias| alias.as_str() == args.comment_id)
            .and_then(|index| comments.get(index).copied())
        else {
            return;
        };
        let Some(message) = self.agent_message_mut(message_id.into_bytes()) else {
            return;
        };
        if message
            .comment_responses
            .iter()
            .any(|response| response.comment_id == comment_id)
        {
            return;
        }
        message.comment_responses.push(AgentCommentResponse {
            // Derived rather than random, so every participant names the
            // reply alike and comments on it reach the same one.
            id: Uuid::new_v5(&message_id, call.id.to_string().as_bytes()),
            comment_id,
            response_view: cx.new(|cx| TextViewState::markdown(&args.response, cx)),
            response: args.response,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rig::message::{
        ImageMediaType, Reasoning, ToolCall, ToolCallId, ToolFunction, UserContent,
    };
    use serde_json::json;

    /// A prompt with an image, a reply with reasoning and a tool call, and
    /// the tool's result come back from the wire exactly as they were
    /// recorded.
    #[test]
    fn messages_round_trip_exactly() {
        let call_id = ToolCallId::new("call_1").expect("a valid id");
        let messages = [
            RigMessage::User {
                content: vec![
                    UserContent::text("Ada:\nWhat is this?"),
                    UserContent::image_base64("AQID", Some(ImageMediaType::PNG), None),
                ],
            },
            RigMessage::Assistant {
                id: Some("reply-1".into()),
                content: vec![
                    AssistantContent::Reasoning(Reasoning::new("Look it up first.")),
                    AssistantContent::text("Checking."),
                    AssistantContent::ToolCall(ToolCall::new(
                        call_id,
                        ToolFunction::new(
                            "respond_to_comment".into(),
                            json!({"comment_id": "comment_1", "response": "Yes", "n": 1.5}),
                        ),
                    )),
                ],
            },
            RigMessage::tool_result("call_1", "respond_to_comment", "Recorded"),
        ];
        for message in messages {
            let wire = postcard::to_stdvec(&TranscriptMessage::from_rig(&message)).expect("encode");
            let received: TranscriptMessage = postcard::from_bytes(&wire).expect("decode");
            assert_eq!(received.to_rig().expect("a message"), message);
        }
    }
}
