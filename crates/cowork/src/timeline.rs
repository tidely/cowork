//! A thread's submitted messages and the agent's replies, and their wire
//! form.

use std::{ops::Range, time::SystemTime};

use agent::AgentEvent;
use draft::AttachmentRecord;
use gpui::{AppContext, Entity, SharedString};
use gpui_base::{TextViewState, input::TextareaState};
use rig::{
    completion::message::ToolResultContent,
    message::{CallId, ToolCall},
};
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
    /// The tool call the run is waiting for someone to allow or deny. Kept
    /// beside `output` rather than in it, since the output is refolded on
    /// every event. One at most, as the agent runs calls one at a time.
    pub(crate) awaiting_approval: Option<CallId>,
    /// See [`protocol::AgentMessage::steering`].
    pub(crate) steering: Vec<Uuid>,

    // Local view state.
    /// Whether the agent's work (everything but its response) is shown. Open
    /// while the run is going, and closed when it ends.
    pub(crate) work_expanded: bool,
}

/// What an agent message shows: a function of its run's output alone, the
/// Rig messages it added to the transcript followed by the one it is
/// folding, as far as it has come. Every copy of the thread derives the same
/// from the same messages; see `AgentOutput::of`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AgentOutput {
    /// Everything the agent produced, in order.
    pub(crate) steps: Vec<AgentStep>,
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
    /// Where a steering prompt joined the run. The `n`th shows the user
    /// message [`AgentMessage::steering`]`[n]`; the prompt itself is in the
    /// transcript.
    Steering,
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
    Text { view: Entity<TextViewState> },
    /// Whether the call's input and output are shown.
    ToolCall { expanded: bool },
    /// Shown by the user message it stands for.
    Steering,
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

/// A tool call the agent made, and what the tool returned, as Rig has them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentToolCall {
    pub(crate) call: ToolCall,
    /// `None` until the tool returns. A run that ends first leaves it so.
    pub(crate) result: Option<Vec<ToolResultContent>>,
}

impl AgentToolCall {
    pub(crate) fn arguments_text(&self) -> String {
        pretty_json(&self.call.function.arguments_value())
    }

    /// A quiet description for the collapsed row; raw arguments stay in the details.
    pub(crate) fn description(&self) -> String {
        match self.call.function.name.as_str() {
            "calculate" => {
                let arguments = self.call.function.arguments_value();
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
        if self.call.function.name.as_str() != "run_command" {
            return None;
        }
        let arguments = self.call.function.arguments_value();
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
                    ToolResultContent::Text(text) => text.text.clone(),
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
    use rig::message::{ToolCall, ToolFunction, ToolName};
    use serde_json::{Value, json};

    fn tool_call(name: &str, arguments: Value) -> AgentToolCall {
        AgentToolCall {
            call: ToolCall::from_wire(
                "test",
                ToolFunction::new(ToolName::new(name).unwrap(), arguments),
            ),
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
            assert_eq!(
                call.description(),
                expected
                    .map(|command| format!("Ran {command}"))
                    .unwrap_or_else(|| "Ran command".into()),
            );
            assert_eq!(call.call.function.arguments_value()["command"], command);
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
    fn known_tools_have_readable_descriptions() {
        for (name, expected) in [
            ("run_command", "Ran command"),
            ("respond_to_comment", "Replied to comment"),
            ("read_file", "Read file"),
            ("write_file", "Wrote to file"),
            ("list_directory", "Listed directory"),
            ("request_permissions", "Requested permissions"),
            ("other_tool", "Ran other_tool"),
        ] {
            assert_eq!(description(name, json!({})), expected);
        }
    }

    #[test]
    fn calculations_show_the_expression_instead_of_json() {
        for (operation, expected) in [
            ("add", "Calculated 3+4"),
            ("subtract", "Calculated 3-4"),
            ("multiply", "Calculated 3*4"),
            ("divide", "Calculated 3/4"),
        ] {
            assert_eq!(
                description("calculate", json!({"a": 3, "b": 4, "operation": operation})),
                expected,
            );
        }
        assert_eq!(
            description("calculate", json!({"a": -1.5, "b": 2, "operation": "add"})),
            "Calculated -1.5+2",
        );
    }

    #[test]
    fn malformed_calculations_keep_a_readable_fallback() {
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
    pub(crate) fn tool_calls(&self) -> impl DoubleEndedIterator<Item = &AgentToolCall> {
        self.steps.iter().filter_map(|step| match step {
            AgentStep::ToolCall(call) => Some(call),
            AgentStep::Thinking(_) | AgentStep::Text(_) | AgentStep::Steering => None,
        })
    }

    #[cfg(test)]
    pub(crate) fn thinking(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|step| match step {
            AgentStep::Thinking(text) => Some(text.as_str()),
            AgentStep::Text(_) | AgentStep::ToolCall(_) | AgentStep::Steering => None,
        })
    }

    /// Where the steps that can hold the response start: after the last
    /// tool call or steering prompt. Text before a steering prompt answered
    /// something the user has since added to, so it is work too.
    pub(crate) fn response_start(&self) -> usize {
        self.steps
            .iter()
            .rposition(|step| matches!(step, AgentStep::ToolCall(_) | AgentStep::Steering))
            .map_or(0, |index| index + 1)
    }

    /// The positions of the steps that are the agent's work rather than its
    /// response: all but the text after the last tool call or steering.
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
                AgentStep::Thinking(_) | AgentStep::ToolCall(_) | AgentStep::Steering => None,
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
            AgentStep::Steering => Self::Steering,
        }
    }

    /// Whether the step is opened up. Text and steering have nothing to open.
    pub(crate) fn expanded(&self) -> bool {
        match self {
            Self::Thinking { expanded, .. } | Self::ToolCall { expanded } => *expanded,
            Self::Text { .. } | Self::Steering => false,
        }
    }

    pub(crate) fn set_expanded(&mut self, value: bool) {
        match self {
            Self::Thinking { expanded, .. } | Self::ToolCall { expanded } => *expanded = value,
            Self::Text { .. } | Self::Steering => {}
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
            work_expanded: run.is_generating(),
            run,
            // What an empty output shows: the model is not reasoning yet.
            output: AgentOutput {
                thinking_complete: true,
                ..AgentOutput::default()
            },
            committed: OutputMark::default(),
            comment_calls_checked: 0,
            awaiting_approval: None,
            steering: Vec::new(),
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

    /// Where the call awaiting approval is among `output`'s steps. Rig may
    /// mint the same id for id-less calls of different replies, and only the
    /// last reply's calls can be waiting, so it is the latest with that id.
    pub(crate) fn awaiting_approval_step(&self) -> Option<usize> {
        let awaiting = self.awaiting_approval.as_ref()?;
        self.output.steps.iter().rposition(
            |step| matches!(step, AgentStep::ToolCall(call) if call.call.id == *awaiting),
        )
    }

    pub(crate) fn to_protocol(&self) -> protocol::AgentMessage {
        protocol::AgentMessage {
            id: self.id.into_bytes(),
            comment_group_id: self.comment_group_id.map(Uuid::into_bytes),
            started_at: self.started_at,
            prompt: self.prompt,
            pending_events: self.pending_events.clone(),
            run: self.run.clone(),
            awaiting_approval: self.awaiting_approval.as_ref().map(protocol::Json::call_id),
            steering: self.steering.iter().map(|id| id.into_bytes()).collect(),
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
            self.prompt,
            self.run,
            self.pending_events,
            cx,
        );
        message.awaiting_approval = self
            .awaiting_approval
            .map(|call| call.to_call_id().expect("validated with the snapshot"));
        message.steering = self.steering.into_iter().map(Uuid::from_bytes).collect();
        message
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
