//! A thread's submitted messages and the agent's replies, and their wire
//! form.

use std::{ops::Range, time::SystemTime};

use agent::model::{
    Content, Message, Reasoning, Role, ToolArguments, ToolCall, ToolCallId, ToolResultContent,
};
use draft::AttachmentRecord;
use gpui::{AppContext, Entity, SharedString};
use gpui_base::{TextViewState, input::TextareaState};
use uuid::Uuid;

use crate::{
    attachments::{record_from_protocol, record_to_protocol},
    participant::ParticipantId,
    protocol::{self, AgentRun},
    thread_draft::ItemPresence,
    transcript::Transcript,
};

#[derive(Clone, Copy)]
pub(crate) enum MessageAuthor {
    User(ParticipantId),
    Agent,
}

/// Orthogonal activity is owned by the thread's one active execution. Ended
/// run records retain replay tails, but cannot carry a live activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunActivity {
    Working,
    AwaitingApproval,
    Compacting,
}

impl AgentMessageUi {
    pub(crate) fn step_view(&self, index: usize) -> Option<&StepView> {
        self.step_views.get(index)
    }

    pub(crate) fn step_view_mut(&mut self, index: usize) -> Option<&mut StepView> {
        self.step_views.get_mut(index)
    }

    #[cfg(test)]
    pub(crate) fn views(&self) -> impl Iterator<Item = &StepView> {
        self.step_views.iter()
    }
}

// A thread holds a handful of these, so boxing the larger variant would
// only add an indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub(crate) enum TimelineMessage {
    User(UserMessage),
    Agent(AgentMessage),
}

/// Shared run identity and replay metadata; no derived output or GPUI views.
#[derive(Clone)]
pub(crate) struct AgentRunRecord {
    pub(crate) id: Uuid,
    pub(crate) comment_group_id: Option<Uuid>,
    /// When the host started generating this message.
    pub(crate) started_at: SystemTime,
    /// See [`protocol::AgentMessage::start`].
    pub(crate) start: crate::transcript::TranscriptIndex,
    /// See [`protocol::AgentMessage::pending_events`].
    pub(crate) pending_events: Vec<protocol::Preview>,
    pub(crate) run: AgentRun,

    /// The tool call the run is waiting for someone to allow or deny. Kept
    /// beside `output` rather than in it, since the output is refolded on
    /// every event. One at most, as the agent runs calls one at a time.
    pub(crate) awaiting_approval: Option<ToolCallId>,
}

/// Local presentation and interaction state, owned beside the shared run record.
#[derive(Clone)]
pub(crate) struct AgentMessageUi {
    /// What the message shows, derived from its run's output; see
    /// `transcript.rs`.
    pub(crate) output: AgentOutput,
    /// How much of `output` comes from the run's transcript entries, which
    /// never change; the rest comes from the reply being previewed.
    pub(crate) committed: OutputMark,
    /// One local view per folded step, in the same order. Compaction is a
    /// step too, so rendering and collapse never change these indices.
    pub(crate) step_views: Vec<StepView>,
    pub(crate) text_view: Entity<TextViewState>,
    /// The replies to comments among `output`'s tool calls. Kept rather than
    /// derived each time, since comments on a reply refer to its view.
    pub(crate) comment_responses: Vec<AgentCommentResponse>,
    /// How many of `output`'s tool calls have been checked for replies to
    /// comments.
    pub(crate) comment_calls_checked: usize,
    // Local view state.
    /// Whether the agent's work (everything but its response) is shown. Open
    /// while the run is going, and closed when it ends.
    pub(crate) work_expanded: bool,
}

/// A resident timeline entry. Shared metadata and local presentation have
/// distinct fields, but share their lifetime while all history is in memory.
#[derive(Clone)]
pub(crate) struct AgentMessage {
    pub(crate) record: AgentRunRecord,
    pub(crate) ui: AgentMessageUi,
}

#[derive(Clone)]
pub(crate) struct UserMessage {
    pub(crate) record: SubmittedGroup,
    pub(crate) ui: UserMessageUi,
}

#[derive(Clone)]
pub(crate) struct UserMessageUi {
    pub(crate) comments_folded: bool,
}

impl From<UserMessageGroup> for UserMessage {
    fn from(group: UserMessageGroup) -> Self {
        Self {
            record: SubmittedGroup {
                id: group.id,
                blocks: group.blocks,
                comments: group
                    .comments
                    .into_iter()
                    .map(|comment| {
                        let UserCommentBody::Submitted(body) = comment.body else {
                            unreachable!("timeline comments have been submitted");
                        };
                        SubmittedComment {
                            id: comment.id,
                            author: comment.author,
                            reference: comment.reference,
                            body,
                        }
                    })
                    .collect(),
            },
            ui: UserMessageUi {
                comments_folded: group.comments_folded,
            },
        }
    }
}

impl TimelineMessage {
    /// Only shared records cross the wire; entities and local folding never do.
    pub(crate) fn to_protocol(&self) -> protocol::TimelineMessage {
        match self {
            Self::User(message) => protocol::TimelineMessage::User(message.record.to_protocol()),
            Self::Agent(message) => protocol::TimelineMessage::Agent(message.record.to_protocol()),
        }
    }
}

/// What an agent message shows: a function of its run's output alone, the
/// transcript entries it added followed by the reply being previewed, as far
/// as it has come. Every copy of the thread derives the same
/// from the same messages; see `AgentOutput::from_transcript`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AgentOutput {
    /// Everything the agent produced, in order.
    steps: Vec<AgentStep>,
    /// False only while the model is still reasoning: the run is generating
    /// and reasoning is the last part of the reply streaming. The last step
    /// is then the thinking in progress.
    pub(crate) thinking_complete: bool,
    /// The response: the text steps after the last tool call, joined. Only
    /// it can be commented on, by offsets into this text; text before a tool
    /// call is part of the agent's work.
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
    /// A stretch of text, continued the same way.
    Text(String),
    ToolCall(AgentToolCall),
    /// An accepted summary at this point in the folded output.
    Compaction,
}

/// How an [`AgentStep`] is shown: local state, never sent.
#[derive(Clone)]
pub(crate) enum StepView {
    /// Whether completed thinking is shown; thinking in progress always is.
    Thinking {
        view: Entity<TextViewState>,
        expanded: bool,
    },
    /// Text in the agent's work. The response is shown by
    /// [`AgentMessage::text_view`] instead.
    Text {
        view: Entity<TextViewState>,
    },
    /// Whether the call's input and output are shown.
    ToolCall {
        expanded: bool,
    },
    Compaction,
}

/// Where an [`AgentOutput`]'s committed part ends: how many steps it has,
/// the length of the last if it is thinking or text, and how many of its
/// tool calls have their results. Committed calls are answered in order, a
/// reply's results all at once, so the answered ones come first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputMark {
    pub(crate) steps: usize,
    pub(crate) tail: usize,
    pub(crate) answered: usize,
}

#[derive(Clone)]
pub(crate) struct AgentCommentResponse {
    pub(crate) id: Uuid,
    pub(crate) comment_id: Uuid,
    pub(crate) response: String,
    pub(crate) response_view: Entity<TextViewState>,
}

/// A tool call the agent made, and what the tool returned.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentToolCall {
    pub(crate) call: ToolCall,
    /// `None` until the tool returns. A run that ends first leaves it so.
    pub(crate) result: Option<Vec<ToolResultContent>>,
}

/// A call's arguments as JSON: the object, or the text of arguments that
/// were not one.
pub(crate) fn arguments_value(call: &ToolCall) -> serde_json::Value {
    match &call.arguments {
        ToolArguments::Object { value } => serde_json::Value::Object(value.clone()),
        ToolArguments::Invalid { raw } => serde_json::Value::String(raw.clone()),
    }
}

/// A call of `name` with `arguments`, under `id`, for previews and tests.
pub(crate) fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: ToolCallId(id.into()),
        provider_id: None,
        name: name.into(),
        arguments: match arguments {
            serde_json::Value::Object(value) => ToolArguments::Object { value },
            other => ToolArguments::Invalid {
                raw: other.to_string(),
            },
        },
    }
}

impl AgentToolCall {
    pub(crate) fn arguments_text(&self) -> String {
        pretty_json(&arguments_value(&self.call))
    }

    /// A quiet description for the collapsed row; raw arguments stay in the details.
    pub(crate) fn description(&self) -> String {
        match self.call.name.as_str() {
            "calculate" => {
                let arguments = arguments_value(&self.call);
                let calculation = (|| {
                    let a = arguments.get("a")?.as_f64()?;
                    let b = arguments.get("b")?.as_f64()?;
                    let operator = match arguments.get("operation")?.as_str()? {
                        "add" => "+",
                        "subtract" => "-",
                        "multiply" => "*",
                        "divide" => "/",
                        _ => return None,
                    };
                    Some(format!("Calculated {a}{operator}{b}"))
                })();
                calculation.unwrap_or_else(|| "Calculated".into())
            }
            "run_command" => self
                .command_preview()
                .map(|command| format!("Ran {command}"))
                .unwrap_or_else(|| "Ran command".into()),
            "respond_to_comment" => "Replied to comment".into(),
            "read_file" => "Read file".into(),
            "write_file" => "Wrote to file".into(),
            "list_directory" => "Listed directory".into(),
            "request_permissions" => "Requested permissions".into(),
            name => format!("Ran {}", name),
        }
    }

    /// Preview only the first nonempty shell line, never flattening a script
    /// into something that looks like a different command.
    pub(crate) fn command_preview(&self) -> Option<String> {
        if self.call.name != "run_command" {
            return None;
        }
        let arguments = arguments_value(&self.call);
        let command = arguments.get("command")?.as_str()?;
        let mut lines = command
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        let mut preview = lines.next()?.to_owned();
        if lines.next().is_some() {
            preview.push_str(" …");
        }
        Some(preview)
    }

    /// The result's content as text, one item per line.
    pub(crate) fn result_text(&self) -> Option<String> {
        let content = self.result.as_ref()?;
        Some(
            content
                .iter()
                .map(|item| match item {
                    ToolResultContent::Text { text } => text.clone(),
                    ToolResultContent::Json { value } => pretty_json(value),
                    ToolResultContent::Image(_) => "[image]".to_owned(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

#[cfg(test)]
mod tool_call_tests {
    use super::AgentToolCall;
    use serde_json::{Value, json};

    fn tool_call(name: &str, arguments: Value) -> AgentToolCall {
        AgentToolCall {
            call: super::tool_call("test", name, arguments),
            result: Some(Vec::new()),
        }
    }

    fn description(name: &str, arguments: Value) -> String {
        tool_call(name, arguments).description()
    }

    #[test]
    fn command_previews_keep_shell_lines_separate() {
        for (command, expected) in [
            ("cargo test -p cowork", Some("cargo test -p cowork")),
            ("\n  echo café\n\n cargo test\n", Some("echo café …")),
            ("echo done\n", Some("echo done")),
            (" \n\t", None),
        ] {
            let call = tool_call("run_command", json!({"command": command}));
            assert_eq!(call.command_preview().as_deref(), expected);
        }
        for arguments in [json!({}), json!({"command": 42})] {
            assert!(
                tool_call("run_command", arguments)
                    .command_preview()
                    .is_none()
            );
        }
        assert!(
            tool_call("other_tool", json!({"command": "cargo test"}))
                .command_preview()
                .is_none()
        );
    }

    #[test]
    fn calculations_show_the_expression_or_a_fallback() {
        assert_eq!(
            description("calculate", json!({"a": 3, "b": 4, "operation": "divide"})),
            "Calculated 3/4",
        );
        assert_eq!(
            description("calculate", json!({"a": -1.5, "b": 2, "operation": "add"})),
            "Calculated -1.5+2",
        );
        for arguments in [
            json!({}),
            json!({"a": "three", "b": 4, "operation": "multiply"}),
            json!({"a": 3, "b": 4, "operation": "unknown"}),
        ] {
            assert_eq!(description("calculate", arguments), "Calculated");
        }
    }
}

impl AgentOutput {
    pub(crate) fn steps(&self) -> &[AgentStep] {
        &self.steps
    }

    #[cfg(test)]
    pub(crate) fn for_test(steps: Vec<AgentStep>, text: String, thinking_complete: bool) -> Self {
        Self {
            steps,
            text,
            thinking_complete,
        }
    }
    pub(crate) fn tool_calls(&self) -> impl DoubleEndedIterator<Item = &AgentToolCall> {
        self.steps.iter().filter_map(|step| match step {
            AgentStep::ToolCall(call) => Some(call),
            AgentStep::Thinking(_) | AgentStep::Text(_) | AgentStep::Compaction => None,
        })
    }

    #[cfg(test)]
    pub(crate) fn thinking(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|step| match step {
            AgentStep::Thinking(text) => Some(text.as_str()),
            AgentStep::Text(_) | AgentStep::ToolCall(_) | AgentStep::Compaction => None,
        })
    }

    /// Where the steps that can hold the response start: after the last
    /// tool call.
    pub(crate) fn response_start(&self) -> usize {
        self.steps
            .iter()
            .rposition(|step| matches!(step, AgentStep::ToolCall(_)))
            .map_or(0, |index| index + 1)
    }

    /// The positions of the steps that are the agent's work rather than its
    /// response: all but the text after the last tool call.
    pub(crate) fn work(&self) -> impl Iterator<Item = usize> + '_ {
        let response_start = self.response_start();
        self.steps
            .iter()
            .enumerate()
            .filter(move |(index, step)| {
                !matches!(step, AgentStep::Text(_)) || *index < response_start
            })
            .map(|(index, _)| index)
    }

    /// The response, from the steps.
    pub(crate) fn response(&self) -> String {
        self.steps[self.response_start()..]
            .iter()
            .filter_map(|step| match step {
                AgentStep::Text(text) => Some(text.as_str()),
                AgentStep::Thinking(_) | AgentStep::ToolCall(_) | AgentStep::Compaction => None,
            })
            .collect()
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
            AgentStep::Text(text) => Self::Text {
                view: cx.new(|cx| TextViewState::markdown(text, cx)),
            },
            AgentStep::ToolCall(_) => Self::ToolCall { expanded: false },
            AgentStep::Compaction => Self::Compaction,
        }
    }

    /// Whether the step is opened up. Text has nothing to open.
    pub(crate) fn expanded(&self) -> bool {
        match self {
            Self::Thinking { expanded, .. } | Self::ToolCall { expanded } => *expanded,
            Self::Text { .. } | Self::Compaction => false,
        }
    }

    pub(crate) fn set_expanded(&mut self, value: bool) {
        match self {
            Self::Thinking { expanded, .. } | Self::ToolCall { expanded } => *expanded = value,
            Self::Text { .. } | Self::Compaction => {}
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

/// Shared submission content, without draft editor entities or local folding.
#[derive(Clone)]
pub(crate) struct SubmittedGroup {
    pub(crate) id: Uuid,
    pub(crate) comments: Vec<SubmittedComment>,
    pub(crate) blocks: Vec<PromptBlock>,
}

#[derive(Clone)]
pub(crate) struct SubmittedComment {
    pub(crate) id: Uuid,
    pub(crate) author: ParticipantId,
    pub(crate) reference: CommentReference,
    pub(crate) body: SharedString,
}

impl SubmittedComment {
    /// Reuse the draft comment renderer only at the leaf that needs its shape.
    pub(crate) fn to_view(&self) -> UserComment {
        UserComment {
            id: self.id,
            author: self.author,
            reference: self.reference.clone(),
            presence: ItemPresence::default(),
            body: UserCommentBody::Submitted(self.body.clone()),
        }
    }
}

impl SubmittedGroup {
    pub(crate) fn to_protocol(&self) -> protocol::UserMessage {
        protocol::UserMessage {
            id: self.id.into_bytes(),
            blocks: self.blocks.iter().map(PromptBlock::to_protocol).collect(),
            comments: self
                .comments
                .iter()
                .map(|comment| protocol::UserComment {
                    id: comment.id.into_bytes(),
                    author: comment.author.into_bytes(),
                    reference: protocol::CommentReference {
                        message_id: comment.reference.message_id.into_bytes(),
                        range: comment.reference.range.clone(),
                        quote: comment.reference.quote.clone(),
                    },
                    body: comment.body.to_string(),
                })
                .collect(),
        }
    }
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
    /// A response whose run begins at `start`, before any pre-run summaries
    /// and its prompt. It shows nothing until its output is folded.
    pub(crate) fn new(
        id: Uuid,
        comment_group_id: Option<Uuid>,
        started_at: SystemTime,
        start: crate::transcript::TranscriptIndex,
        run: AgentRun,
        pending_events: Vec<protocol::Preview>,
        cx: &mut impl AppContext,
    ) -> Self {
        let work_expanded = run.is_generating();
        Self {
            record: AgentRunRecord {
                id,
                comment_group_id,
                started_at,
                start,
                pending_events,
                run,
                awaiting_approval: None,
            },
            ui: AgentMessageUi {
                work_expanded,
                output: AgentOutput {
                    thinking_complete: true,
                    ..AgentOutput::default()
                },
                committed: OutputMark::default(),
                comment_calls_checked: 0,
                step_views: Vec::new(),
                text_view: cx.new(|cx| TextViewState::markdown("", cx)),
                comment_responses: Vec::new(),
            },
        }
    }

    /// Shows `output` afresh, with new views and everything collapsed.
    #[cfg(test)]
    pub(crate) fn show_output(&mut self, output: AgentOutput, cx: &mut impl AppContext) {
        self.ui.output = output;
        self.ui.step_views = self
            .ui
            .output
            .steps
            .iter()
            .map(|step| StepView::new(step, cx))
            .collect();
        self.ui.text_view = cx.new(|cx| TextViewState::markdown(&self.ui.output.text, cx));
    }

    pub(crate) fn is_generating(&self) -> bool {
        self.record.run.is_generating()
    }

    /// Where the call awaiting approval is among `output`'s steps; only the
    /// last reply's calls can be waiting, so it is the latest with that id.
    pub(crate) fn awaiting_approval_step(&self) -> Option<usize> {
        let awaiting = self.record.awaiting_approval.as_ref()?;
        self.ui.output.steps.iter().rposition(
            |step| matches!(step, AgentStep::ToolCall(call) if call.call.id == *awaiting),
        )
    }
}

impl AgentRunRecord {
    pub(crate) fn to_protocol(&self) -> protocol::AgentMessage {
        protocol::AgentMessage {
            id: self.id.into_bytes(),
            comment_group_id: self.comment_group_id.map(Uuid::into_bytes),
            started_at: self.started_at,
            start: self.start,
            pending_events: self.pending_events.clone(),
            run: self.run.clone(),
            awaiting_approval: self.awaiting_approval.clone(),
        }
    }
}

impl protocol::AgentMessage {
    /// The message, showing nothing until `Thread::restore_agent_output`
    /// shows its output.
    /// `awaiting_approval` must have passed `validate_agent_runs`.
    pub(crate) fn into_native(self, cx: &mut impl AppContext) -> AgentMessage {
        let mut message = AgentMessage::new(
            Uuid::from_bytes(self.id),
            self.comment_group_id.map(Uuid::from_bytes),
            self.started_at,
            self.start,
            self.run,
            self.pending_events,
            cx,
        );
        message.record.awaiting_approval = self.awaiting_approval;
        message
    }
}

impl protocol::TimelineMessage {
    pub(crate) fn into_native(self, cx: &mut impl AppContext) -> TimelineMessage {
        match self {
            Self::User(message) => TimelineMessage::User(message.into_native().into()),
            Self::Agent(message) => TimelineMessage::Agent(message.into_native(cx)),
        }
    }
}

impl AgentOutput {
    pub(crate) fn push_compaction(&mut self) {
        self.steps.push(AgentStep::Compaction);
    }

    /// Adds what `message`, the next message of a run's output, shows.
    /// Context notes and inputs show nothing; the run's user group does.
    pub(crate) fn push(&mut self, message: &Message) {
        match message.role {
            Role::Assistant => {
                for part in &message.content {
                    match part {
                        Content::Reasoning(Reasoning::Text { text }) => self.push_thinking(text),
                        Content::Text { text } => self.push_text(text),
                        Content::ToolCall(call) => {
                            self.steps.push(AgentStep::ToolCall(AgentToolCall {
                                call: call.clone(),
                                result: None,
                            }));
                        }
                        Content::Reasoning(Reasoning::Redacted { .. })
                        | Content::Image(_)
                        | Content::ToolResult(_) => {}
                    }
                }
            }
            Role::Tool => {
                for part in &message.content {
                    if let Content::ToolResult(result) = part {
                        self.record_tool_result(&result.call, &result.content);
                    }
                }
            }
            Role::User | Role::System | Role::Developer => {}
        }
    }

    /// Continues the thinking the output ends with, or starts new thinking
    /// if it ends with something else.
    fn push_thinking(&mut self, text: &str) {
        match self.steps.last_mut() {
            Some(AgentStep::Thinking(thinking)) => thinking.push_str(text),
            _ if text.is_empty() => {}
            _ => {
                self.steps.push(AgentStep::Thinking(text.to_owned()));
            }
        }
    }

    /// Continues the text the output ends with, or starts new text.
    fn push_text(&mut self, text: &str) {
        match self.steps.last_mut() {
            Some(AgentStep::Text(existing)) => existing.push_str(text),
            _ if text.is_empty() => {}
            _ => {
                self.steps.push(AgentStep::Text(text.to_owned()));
            }
        }
    }

    fn extend(&mut self, message: Option<&Message>) {
        if let Some(message) = message {
            self.push(message);
        }
    }

    fn tool_calls_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut AgentToolCall> {
        self.steps.iter_mut().filter_map(|step| match step {
            AgentStep::ToolCall(call) => Some(call),
            AgentStep::Thinking(_) | AgentStep::Text(_) | AgentStep::Compaction => None,
        })
    }

    /// Records what the tool returned for `call`.
    fn record_tool_result(&mut self, call: &ToolCallId, content: &[ToolResultContent]) {
        if let Some(tool_call) = self
            .tool_calls_mut()
            .rfind(|tool_call| tool_call.call.id == *call)
        {
            tool_call.result = Some(content.to_vec());
        };
    }

    /// Where the output ends now, as its committed part.
    fn mark(&self) -> OutputMark {
        let answered = self
            .tool_calls()
            .take_while(|call| call.result.is_some())
            .count();
        debug_assert!(
            self.tool_calls()
                .skip(answered)
                .all(|call| call.result.is_none()),
            "committed calls are answered in order"
        );
        OutputMark {
            steps: self.steps.len(),
            tail: match self.steps.last() {
                Some(AgentStep::Thinking(text) | AgentStep::Text(text)) => text.len(),
                _ => 0,
            },
            answered,
        }
    }

    /// Drops everything after `mark`, the committed part: what the message a
    /// run is folding added, including results for committed calls.
    fn rewind(&mut self, mark: OutputMark) {
        self.steps.truncate(mark.steps);
        if let Some(AgentStep::Thinking(text) | AgentStep::Text(text)) = self.steps.last_mut() {
            text.truncate(mark.tail);
        }
        for call in self.tool_calls_mut().skip(mark.answered) {
            call.result = None;
        }
    }

    /// Whether `partial`, the message a run is folding, shows the model
    /// still reasoning: it is the reply streaming, ending in reasoning that
    /// has some text, which is then the output's last step.
    fn still_reasoning(partial: Option<&Message>) -> bool {
        partial.is_some_and(|reply| {
            matches!(
                reply.content.last(),
                Some(Content::Reasoning(Reasoning::Text { text })) if !text.is_empty()
            )
        })
    }
}

impl AgentMessageUi {
    /// Summary entries commit at a safe boundary, just like completed messages.
    pub(crate) fn compact(&mut self, cx: &mut impl AppContext) {
        let first = self.output.steps.len();
        self.output.push_compaction();
        self.committed = self.output.mark();
        self.show_steps(first, &[], cx);
    }

    /// Shows a run's output after its fold moved on: `completed`, a message
    /// it has just added to the transcript, if any, then `partial`, the
    /// message it is folding. Only the part after the committed mark is
    /// redone, and the text views only get what that part added.
    pub(crate) fn advance(
        &mut self,
        completed: Option<&Message>,
        partial: Option<&Message>,
        generating: bool,
        cx: &mut impl AppContext,
    ) {
        let from = self.committed;
        // Only the last committed step, if it is thinking or text, and the
        // steps after it can change what they show.
        let first_changed = from.steps.saturating_sub(1);
        let old_texts = self.output.steps[first_changed..]
            .iter()
            .map(|step| match step {
                AgentStep::Thinking(text) | AgentStep::Text(text) => Some(text.clone()),
                AgentStep::ToolCall(_) | AgentStep::Compaction => None,
            })
            .collect::<Vec<_>>();
        self.output.rewind(from);
        if let Some(completed) = completed {
            self.output.push(completed);
            self.committed = self.output.mark();
        }
        self.output.extend(partial);
        self.set_thinking_complete(partial, generating);
        self.show_steps(first_changed, &old_texts, cx);
        let response = self.output.response();
        let old_response = std::mem::replace(&mut self.output.text, response);
        show_tail(&self.text_view, &self.output.text, 0, &old_response, cx);
    }

    /// Brings the step views, which showed steps `..first` as they are now
    /// followed by steps whose thinking or text was `old`, to showing
    /// `output.steps`. A step that stays what it was keeps its view state.
    fn show_steps(&mut self, first: usize, old: &[Option<String>], cx: &mut impl AppContext) {
        self.step_views.truncate(self.output.steps.len());
        for (index, step) in self.output.steps.iter().enumerate().skip(first) {
            match (self.step_views.get_mut(index), step) {
                (Some(StepView::Thinking { view, .. }), AgentStep::Thinking(text))
                | (Some(StepView::Text { view }), AgentStep::Text(text)) => {
                    update_step_text(view, text, old.get(index - first), cx);
                }
                (Some(StepView::ToolCall { .. }), AgentStep::ToolCall(_))
                | (Some(StepView::Compaction), AgentStep::Compaction) => {}
                (Some(view), step) => *view = StepView::new(step, cx),
                (None, step) => self.step_views.push(StepView::new(step, cx)),
            }
        }
    }

    /// Shows a run's `output` entries of `transcript`, followed by
    /// `partial`, the reply being previewed, in a message fresh from a
    /// snapshot. The text views are shown whole, as history rather than as a
    /// stream.
    pub(crate) fn restore(
        &mut self,
        transcript: &Transcript,
        output: std::ops::Range<usize>,
        partial: Option<&Message>,
        generating: bool,
        cx: &mut impl AppContext,
    ) {
        self.output = AgentOutput::from_transcript(transcript, output);
        self.committed = self.output.mark();
        self.output.extend(partial);
        self.set_thinking_complete(partial, generating);
        self.output.text = self.output.response();
        self.step_views = self
            .output
            .steps
            .iter()
            .map(|step| StepView::new(step, cx))
            .collect();
        self.text_view = cx.new(|cx| TextViewState::markdown(&self.output.text, cx));
    }

    /// Thinking is complete unless the model is still reasoning. Thinking in
    /// progress cannot be expanded by hand, so it completes collapsed.
    fn set_thinking_complete(&mut self, partial: Option<&Message>, generating: bool) {
        self.output.thinking_complete = !(generating && AgentOutput::still_reasoning(partial));
    }
}

/// Brings `view` from showing `new[..from]` followed by `old_tail` to showing
/// `new`, appending when `new` extends that, as it does while a run streams.
fn show_tail(
    view: &gpui::Entity<TextViewState>,
    new: &str,
    from: usize,
    old_tail: &str,
    cx: &mut impl AppContext,
) {
    match new[from..].strip_prefix(old_tail) {
        Some("") => {}
        Some(added) => view.update(cx, |view, cx| view.push_str(added, cx)),
        None => view.update(cx, |view, cx| view.set_text(new, cx)),
    }
}

fn update_step_text(
    view: &Entity<TextViewState>,
    text: &str,
    old: Option<&Option<String>>,
    cx: &mut impl AppContext,
) {
    match old {
        Some(Some(old)) => show_tail(view, text, 0, old, cx),
        _ => view.update(cx, |view, cx| view.set_text(text, cx)),
    }
}
