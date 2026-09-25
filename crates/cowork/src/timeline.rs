//! A thread's submitted messages and the agent's replies, and their wire
//! form.

use std::{
    ops::Range,
    time::{Duration, SystemTime},
};

use draft::AttachmentRecord;
use gpui::{AppContext, Entity, SharedString};
use gpui_base::{TextViewState, input::TextareaState};
use uuid::Uuid;

use crate::{
    attachments::{record_from_protocol, record_to_protocol},
    participant::ParticipantId,
    protocol,
    thread_draft::ItemPresence,
};

#[derive(Clone, Copy)]
pub(crate) enum MessageAuthor {
    User(ParticipantId),
    Agent,
}

#[derive(Clone)]
pub(crate) enum TimelineMessage {
    User(UserMessageGroup),
    Agent(AgentMessage),
}

#[derive(Clone)]
pub(crate) struct AgentMessage {
    pub(crate) id: Uuid,
    pub(crate) comment_group_id: Option<Uuid>,
    /// When the host started generating this message.
    pub(crate) started_at: SystemTime,
    pub(crate) comment_responses: Vec<AgentCommentResponse>,
    pub(crate) thinking: String,
    pub(crate) thinking_view: Entity<TextViewState>,
    pub(crate) thinking_complete: bool,
    pub(crate) thinking_expanded: bool,
    pub(crate) text: String,
    pub(crate) text_view: Entity<TextViewState>,
    pub(crate) complete: bool,
    pub(crate) failed: bool,
    /// How long the host spent generating this message, stopped and failed
    /// runs included. `None` while generating.
    pub(crate) duration: Option<Duration>,
}

#[derive(Clone)]
pub(crate) struct AgentCommentResponse {
    pub(crate) id: Uuid,
    pub(crate) comment_id: Uuid,
    pub(crate) response: String,
    pub(crate) response_view: Entity<TextViewState>,
}

/// A submitted user message: the comments and prompt blocks of one
/// submission.
#[derive(Clone)]
pub(crate) struct UserMessageGroup {
    pub(crate) id: Uuid,
    pub(crate) comments: Vec<UserComment>,
    pub(crate) blocks: Vec<PromptBlock>,
    pub(crate) comments_folded: bool,
}

#[derive(Clone)]
pub(crate) struct PromptBlock {
    pub(crate) id: Uuid,
    pub(crate) author: ParticipantId,
    pub(crate) text: String,
    /// The block's files; their bytes are in the thread's
    /// [`ThreadDraft::files`].
    pub(crate) attachments: Vec<AttachmentRecord>,
}

#[derive(Clone)]
pub(crate) struct UserComment {
    pub(crate) id: Uuid,
    pub(crate) author: ParticipantId,
    /// Who is in a draft comment right now; empty once submitted.
    pub(crate) presence: ItemPresence,
    pub(crate) reference: CommentReference,
    pub(crate) body: UserCommentBody,
}

#[derive(Clone)]
pub(crate) struct CommentReference {
    pub(crate) message_id: Uuid,
    pub(crate) range: Range<usize>,
    pub(crate) quote: String,
}

#[derive(Clone)]
pub(crate) enum UserCommentBody {
    Editing {
        inline: Entity<TextareaState>,
        composer: Entity<TextareaState>,
    },
    Submitted(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ThreadMessageId {
    pub(crate) thread_id: Uuid,
    pub(crate) message_id: Uuid,
}

impl UserComment {
    pub(crate) fn to_protocol(&self) -> Option<protocol::UserComment> {
        let UserCommentBody::Submitted(body) = &self.body else {
            return None;
        };
        Some(protocol::UserComment {
            id: self.id.into_bytes(),
            author: self.author.into_bytes(),
            reference: protocol::CommentReference {
                message_id: self.reference.message_id.into_bytes(),
                range: self.reference.range.clone(),
                quote: self.reference.quote.clone(),
            },
            body: body.to_string(),
        })
    }
}

impl protocol::UserComment {
    pub(crate) fn into_native(self) -> UserComment {
        UserComment {
            id: Uuid::from_bytes(self.id),
            author: ParticipantId::from_bytes(self.author),
            presence: ItemPresence::default(),
            reference: CommentReference {
                message_id: Uuid::from_bytes(self.reference.message_id),
                range: self.reference.range,
                quote: self.reference.quote,
            },
            body: UserCommentBody::Submitted(self.body.into()),
        }
    }
}

impl UserMessageGroup {
    pub(crate) fn to_protocol(&self) -> protocol::UserMessage {
        protocol::UserMessage {
            id: self.id.into_bytes(),
            comments: self
                .comments
                .iter()
                .filter_map(UserComment::to_protocol)
                .collect(),
            blocks: self.blocks.iter().map(PromptBlock::to_protocol).collect(),
        }
    }

    /// The text the thread is titled after.
    pub(crate) fn title_text(&self) -> &str {
        self.blocks
            .iter()
            .map(|block| block.text.as_str())
            .chain(
                self.comments
                    .iter()
                    .filter_map(|comment| match &comment.body {
                        UserCommentBody::Submitted(body) => Some(body.as_ref()),
                        UserCommentBody::Editing { .. } => None,
                    }),
            )
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default()
    }
}

impl PromptBlock {
    pub(crate) fn to_protocol(&self) -> protocol::PromptBlock {
        protocol::PromptBlock {
            id: self.id.into_bytes(),
            author: self.author.into_bytes(),
            text: self.text.clone(),
            attachments: self.attachments.iter().map(record_to_protocol).collect(),
        }
    }
}

impl protocol::UserMessage {
    pub(crate) fn into_native(self) -> UserMessageGroup {
        UserMessageGroup {
            id: Uuid::from_bytes(self.id),
            comments: self
                .comments
                .into_iter()
                .map(protocol::UserComment::into_native)
                .collect(),
            blocks: self
                .blocks
                .into_iter()
                .map(|block| PromptBlock {
                    id: Uuid::from_bytes(block.id),
                    author: ParticipantId::from_bytes(block.author),
                    text: block.text,
                    attachments: block
                        .attachments
                        .into_iter()
                        .map(record_from_protocol)
                        .collect(),
                })
                .collect(),
            comments_folded: false,
        }
    }
}

impl AgentMessage {
    /// An empty message for an agent that has just started responding.
    pub(crate) fn new(
        id: Uuid,
        comment_group_id: Option<Uuid>,
        started_at: SystemTime,
        cx: &mut impl AppContext,
    ) -> Self {
        Self {
            id,
            comment_group_id,
            started_at,
            comment_responses: Vec::new(),
            thinking: String::new(),
            thinking_view: cx.new(|cx| TextViewState::markdown("", cx)),
            thinking_complete: false,
            thinking_expanded: true,
            text: String::new(),
            text_view: cx.new(|cx| TextViewState::markdown("", cx)),
            complete: false,
            failed: false,
            duration: None,
        }
    }

    pub(crate) fn to_protocol(&self) -> protocol::AgentMessage {
        protocol::AgentMessage {
            id: self.id.into_bytes(),
            comment_group_id: self.comment_group_id.map(Uuid::into_bytes),
            started_at: self.started_at,
            comment_responses: self
                .comment_responses
                .iter()
                .map(|response| protocol::AgentCommentResponse {
                    id: response.id.into_bytes(),
                    comment_id: response.comment_id.into_bytes(),
                    response: response.response.clone(),
                })
                .collect(),
            thinking: self.thinking.clone(),
            thinking_complete: self.thinking_complete,
            text: self.text.clone(),
            complete: self.complete,
            failed: self.failed,
            duration: self.duration,
        }
    }
}

impl protocol::AgentMessage {
    pub(crate) fn into_native(self, cx: &mut impl AppContext) -> AgentMessage {
        let thinking_view = cx.new(|cx| TextViewState::markdown(&self.thinking, cx));
        let text_view = cx.new(|cx| TextViewState::markdown(&self.text, cx));
        AgentMessage {
            id: Uuid::from_bytes(self.id),
            comment_group_id: self.comment_group_id.map(Uuid::from_bytes),
            started_at: self.started_at,
            comment_responses: self
                .comment_responses
                .into_iter()
                .map(|response| AgentCommentResponse {
                    id: Uuid::from_bytes(response.id),
                    comment_id: Uuid::from_bytes(response.comment_id),
                    response_view: cx.new(|cx| TextViewState::markdown(&response.response, cx)),
                    response: response.response,
                })
                .collect(),
            thinking: self.thinking,
            thinking_view,
            thinking_complete: self.thinking_complete,
            thinking_expanded: !self.thinking_complete,
            text: self.text,
            text_view,
            complete: self.complete,
            failed: self.failed,
            duration: self.duration,
        }
    }
}

impl TimelineMessage {
    pub(crate) fn to_protocol(&self) -> protocol::TimelineMessage {
        match self {
            Self::User(message) => protocol::TimelineMessage::User(message.to_protocol()),
            Self::Agent(message) => protocol::TimelineMessage::Agent(message.to_protocol()),
        }
    }
}

impl protocol::TimelineMessage {
    pub(crate) fn into_native(self, cx: &mut impl AppContext) -> TimelineMessage {
        match self {
            Self::User(message) => TimelineMessage::User(message.into_native()),
            Self::Agent(message) => TimelineMessage::Agent(message.into_native(cx)),
        }
    }
}
