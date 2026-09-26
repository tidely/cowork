//! A thread's submitted messages and the agent's replies, and their wire
//! form.

use std::{ops::Range, time::SystemTime};

use agent::AgentEvent;
use draft::AttachmentRecord;
use gpui::{AppContext, Entity, SharedString};
use gpui_base::{TextViewState, input::TextareaState};
use rig::{completion::message::ToolResultContent, message::ToolCall};
use uuid::Uuid;

use crate::{
    attachments::{record_from_protocol, record_to_protocol},
    participant::ParticipantId,
    protocol::{self, AgentRun},
    thread_draft::ItemPresence,
};

#[derive(Clone, Copy)]
pub(crate) enum MessageAuthor {
    User(ParticipantId),
    Agent,
}

// A thread holds a handful of these, so boxing the larger variant would
// only add an indirection.
#[allow(clippy::large_enum_variant)]
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
    /// See [`protocol::AgentMessage::prompt`].
    pub(crate) prompt: usize,
    /// See [`protocol::AgentMessage::pending_events`].
    pub(crate) pending_events: Vec<protocol::Json<AgentEvent>>,
    pub(crate) run: AgentRun,

    /// What the message shows, derived from its run's output as Rig has it;
    /// see `transcript.rs`.
    pub(crate) output: AgentOutput,
    /// How much of `output` comes from the run's transcript entries, which
    /// never change; the rest comes from the message the run is folding.
    pub(crate) committed: OutputMark,
    /// How each of `output`'s steps is shown, position for position, and the
    /// view of its text. Kept in step with `output`, but not part of it: the
    /// output is refolded on every event, and this state must outlive that.
    pub(crate) step_views: Vec<StepView>,
    pub(crate) text_view: Entity<TextViewState>,
    /// The replies to comments among `output`'s tool calls. Kept rather than
    /// derived each time, since comments on a reply refer to its view.
    pub(crate) comment_responses: Vec<AgentCommentResponse>,
    /// How many of `output`'s tool calls have been checked for replies to
    /// comments.
    pub(crate) comment_calls_checked: usize,
}

/// What an agent message shows: a function of its run's output alone, the
/// Rig messages it added to the transcript followed by the one it is
/// folding, as far as it has come. Every copy of the thread derives the same
/// from the same messages; see `AgentOutput::of`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AgentOutput {
    /// The agent's reasoning and tool calls, in the order it produced them.
    pub(crate) steps: Vec<AgentStep>,
    /// False only while the model is still reasoning: the run is generating
    /// and reasoning is the last part of the reply streaming. The last step
    /// is then the thinking in progress.
    pub(crate) thinking_complete: bool,
    /// The reply, all of it in one: comments on it are anchored by offsets
    /// into this text.
    pub(crate) text: String,
}

/// One step of an agent's work, as [`AgentOutput::steps`] orders them.
// Most steps are tool calls, so boxing the larger variant would only add
// an indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AgentStep {
    /// A stretch of reasoning. Reasoning continues the previous step if that
    /// is reasoning too, so consecutive steps are never both thinking.
    Thinking(String),
    ToolCall(AgentToolCall),
}

/// How an [`AgentStep`] is shown: local state, never sent.
#[derive(Clone)]
pub(crate) enum StepView {
    /// Whether completed thinking is shown; thinking in progress always is.
    Thinking {
        view: Entity<TextViewState>,
        expanded: bool,
    },
    /// Whether the call's input and output are shown.
    ToolCall { expanded: bool },
}

/// Where an [`AgentOutput`]'s committed part ends: how many steps it has and,
/// if the last is thinking, its length, the length of its text, and how many
/// of its tool calls have their results. Committed calls are answered in
/// order, a reply's results all at once, so the answered ones come first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputMark {
    pub(crate) steps: usize,
    pub(crate) thinking_tail: usize,
    pub(crate) text: usize,
    pub(crate) answered: usize,
}

#[derive(Clone)]
pub(crate) struct AgentCommentResponse {
    pub(crate) id: Uuid,
    pub(crate) comment_id: Uuid,
    pub(crate) response: String,
    pub(crate) response_view: Entity<TextViewState>,
}

/// A tool call the agent made, and what the tool returned, as Rig has them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentToolCall {
    pub(crate) call: ToolCall,
    /// `None` until the tool returns. A run that ends first leaves it so.
    pub(crate) result: Option<Vec<ToolResultContent>>,
}

impl AgentToolCall {
    pub(crate) fn arguments_text(&self) -> String {
        pretty_json(&self.call.function.arguments)
    }

    /// The arguments on one line, for the call's collapsed row.
    pub(crate) fn arguments_summary(&self) -> String {
        self.call.function.arguments.to_string()
    }

    /// The result's content as text, one item per line.
    pub(crate) fn result_text(&self) -> Option<String> {
        let content = self.result.as_ref()?;
        Some(
            content
                .iter()
                .map(|item| match item {
                    ToolResultContent::Text(text) => text.text.clone(),
                    ToolResultContent::Json { value } => pretty_json(value),
                    ToolResultContent::Image(_) => "[image]".to_owned(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

impl AgentOutput {
    pub(crate) fn tool_calls(&self) -> impl DoubleEndedIterator<Item = &AgentToolCall> {
        self.steps.iter().filter_map(|step| match step {
            AgentStep::ToolCall(call) => Some(call),
            AgentStep::Thinking(_) => None,
        })
    }

    #[cfg(test)]
    pub(crate) fn thinking(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|step| match step {
            AgentStep::Thinking(text) => Some(text.as_str()),
            AgentStep::ToolCall(_) => None,
        })
    }

    /// Whether step `index` is the thinking the model is still writing.
    pub(crate) fn thinking_in_progress(&self, index: usize) -> bool {
        !self.thinking_complete
            && index + 1 == self.steps.len()
            && matches!(self.steps[index], AgentStep::Thinking(_))
    }
}

impl StepView {
    pub(crate) fn new(step: &AgentStep, cx: &mut impl AppContext) -> Self {
        match step {
            AgentStep::Thinking(text) => Self::Thinking {
                view: cx.new(|cx| TextViewState::markdown(text, cx)),
                expanded: false,
            },
            AgentStep::ToolCall(_) => Self::ToolCall { expanded: false },
        }
    }

    pub(crate) fn expanded(&self) -> bool {
        match self {
            Self::Thinking { expanded, .. } | Self::ToolCall { expanded } => *expanded,
        }
    }

    pub(crate) fn set_expanded(&mut self, value: bool) {
        match self {
            Self::Thinking { expanded, .. } | Self::ToolCall { expanded } => *expanded = value,
        }
    }
}

fn pretty_json(value: &serde_json::Value) -> String {
    // A `Value` always encodes.
    serde_json::to_string_pretty(value).expect("a JSON value encodes")
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
    /// A message answering the prompt at `prompt` in the transcript. It
    /// shows nothing until its output is shown.
    pub(crate) fn new(
        id: Uuid,
        comment_group_id: Option<Uuid>,
        started_at: SystemTime,
        prompt: usize,
        run: AgentRun,
        pending_events: Vec<protocol::Json<AgentEvent>>,
        cx: &mut impl AppContext,
    ) -> Self {
        Self {
            id,
            comment_group_id,
            started_at,
            prompt,
            pending_events,
            run,
            // What an empty output shows: the model is not reasoning yet.
            output: AgentOutput {
                thinking_complete: true,
                ..AgentOutput::default()
            },
            committed: OutputMark::default(),
            comment_calls_checked: 0,
            step_views: Vec::new(),
            text_view: cx.new(|cx| TextViewState::markdown("", cx)),
            comment_responses: Vec::new(),
        }
    }

    /// Shows `output` afresh, with new views and everything collapsed.
    #[cfg(test)]
    pub(crate) fn show_output(&mut self, output: AgentOutput, cx: &mut impl AppContext) {
        self.output = output;
        self.step_views = self
            .output
            .steps
            .iter()
            .map(|step| StepView::new(step, cx))
            .collect();
        self.text_view = cx.new(|cx| TextViewState::markdown(&self.output.text, cx));
    }

    pub(crate) fn is_generating(&self) -> bool {
        self.run.is_generating()
    }

    pub(crate) fn to_protocol(&self) -> protocol::AgentMessage {
        protocol::AgentMessage {
            id: self.id.into_bytes(),
            comment_group_id: self.comment_group_id.map(Uuid::into_bytes),
            started_at: self.started_at,
            prompt: self.prompt,
            pending_events: self.pending_events.clone(),
            run: self.run.clone(),
        }
    }
}

impl protocol::AgentMessage {
    /// The message, showing nothing until `Thread::restore_agent_output`
    /// shows its output.
    pub(crate) fn into_native(self, cx: &mut impl AppContext) -> AgentMessage {
        AgentMessage::new(
            Uuid::from_bytes(self.id),
            self.comment_group_id.map(Uuid::from_bytes),
            self.started_at,
            self.prompt,
            self.run,
            self.pending_events,
            cx,
        )
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
