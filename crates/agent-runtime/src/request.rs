//! Everything that changes a session arrives as a [`Request`] with a source.

use std::sync::Arc;

use agent_input::{Admitted, Delivery, Input, InputId, Policy, RunId, StateError, Withdrawn};
use futures::channel::{mpsc, oneshot};
use language_model::{Content, LanguageModelProvider, Message, ToolCallId};

use crate::{
    AgentDefinition, AgentHost, Compaction, CompactionError, ConfigurationChange,
    DefinitionRevision, DynamicToolChange, Phase, RunError,
};

pub struct Request<H: AgentHost> {
    /// Who asked. The host's `admit` decides whether this source may.
    pub source: H::Source,
    pub kind: RequestKind<H>,
}

impl<H: AgentHost> Request<H> {
    pub fn new(source: H::Source, kind: RequestKind<H>) -> Self {
        Self { source, kind }
    }
}

// Requests are built and sent one at a time (and boxed in transit), so the
// size of `ReplaceDefinition` costs nothing worth an awkward boxed field.
#[allow(clippy::large_enum_variant)]
pub enum RequestKind<H: AgentHost> {
    // Model-visible work, through the inbox's delivery lanes.
    Input {
        id: InputId,
        payload: H::Input,
        delivery: Delivery,
        policy: Policy,
    },
    EditInput {
        id: InputId,
        payload: H::Input,
    },
    RemoveInput {
        id: InputId,
    },
    /// Change a queued input's delivery, for example turning it into a steer.
    RedirectInput {
        id: InputId,
        delivery: Delivery,
    },
    /// Reorder the ordinary queue; `None` moves the input to the end.
    MoveInput {
        id: InputId,
        before: Option<InputId>,
    },
    /// The request's source has closed: its producer-bound and ephemeral
    /// pending inputs are withdrawn. Committed entries are never retracted.
    SourceClosed,

    // Run lifecycle.
    /// Stop names a run, so a late stop can't cancel its successor. Pending
    /// work stays queued and the queue pauses until `Resume`.
    Stop {
        run: RunId,
    },
    Resume,
    /// Compact at the next safe boundary, including while idle.
    Compact,
    /// Stop the active run and accept no more requests.
    Shutdown,

    // State. Changes the model should know about reach it through context
    // sections at the next step.
    /// Replaces the definition whole. `provider` is needed only when the new
    /// model is served by a different connection.
    ReplaceDefinition {
        definition: AgentDefinition,
        provider: Option<Arc<dyn LanguageModelProvider>>,
    },
    Configure(ConfigurationChange<H::Permissions>),
    DynamicTools(DynamicToolChange<H>),

    // Answers to questions the session asked.
    ResolveApproval {
        call: ToolCallId,
        decision: ApprovalDecision,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    /// Allow this call, and this tool with the same access for the rest of
    /// the run (the host's `authorize` sees it in `grants`).
    AllowForRun,
    Deny(String),
}

// One response per request, moved once; see `RequestKind`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Response<H: AgentHost> {
    Done,
    Admitted(Admitted<H::Source, H::Input>),
    Removed(Input<H::Source, H::Input>),
    Withdrawn(Withdrawn<H::Source, H::Input>),
    Compacted(Compaction),
    DefinitionReplaced(DefinitionRevision),
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RequestError {
    #[error("request rejected: {0}")]
    Rejected(String),
    #[error("session owner has closed")]
    Closed,
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Compaction(#[from] CompactionError),
    #[error("no call {} is waiting for approval", .0.0)]
    NoApproval(ToolCallId),
}

/// Completion of a particular run, rather than an admission acknowledgement.
#[derive(Clone, Debug)]
pub struct Reply {
    pub run: RunId,
    pub message: Message,
}

impl Reply {
    pub fn text(&self) -> String {
        self.message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
    }
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PromptError {
    #[error(transparent)]
    Request(#[from] RequestError),
    #[error("input was withdrawn before delivery")]
    Discarded,
    #[error("run {0} was stopped")]
    Stopped(RunId),
    #[error(transparent)]
    Failed(#[from] RunError),
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub run: Option<RunId>,
    pub phase: Phase,
    pub paused: bool,
    pub pending_inputs: usize,
    pub committed_entries: usize,
    pub revision: DefinitionRevision,
}

pub(crate) type Responder<H> = oneshot::Sender<Result<Response<H>, RequestError>>;
pub(crate) type CompletionResponder = oneshot::Sender<Result<Reply, PromptError>>;

pub(crate) enum Command<H: AgentHost> {
    Request {
        request: Box<Request<H>>,
        response: Responder<H>,
        /// For inputs: also report the result of the run consuming it.
        completion: Option<CompletionResponder>,
    },
    Snapshot(oneshot::Sender<Snapshot>),
}

/// A command endpoint, not a shared mutable session. All requests route
/// through the one owner, which serializes admission with run completion.
pub struct SessionHandle<H: AgentHost> {
    pub(crate) commands: mpsc::UnboundedSender<Command<H>>,
}

impl<H: AgentHost> Clone for SessionHandle<H> {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
        }
    }
}

impl<H: AgentHost> SessionHandle<H> {
    /// Sends a request and waits for the session's answer. `Compact` answers
    /// once the summary is applied. A dropped future does not withdraw a
    /// request already sent.
    pub async fn send(&self, request: Request<H>) -> Result<Response<H>, RequestError> {
        let (response, reply) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Request {
                request: Box::new(request),
                response,
                completion: None,
            })
            .map_err(|_| RequestError::Closed)?;
        reply.await.map_err(|_| RequestError::Closed)?
    }

    /// Sends without waiting for the answer, for use where awaiting isn't
    /// possible (event callbacks, `Drop`).
    pub fn post(&self, request: Request<H>) -> Result<(), RequestError> {
        let (response, _) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Request {
                request: Box::new(request),
                response,
                completion: None,
            })
            .map_err(|_| RequestError::Closed)
    }

    /// Queues an input and waits for the run that consumes it. The owner
    /// binds the receipt before starting any work, so a fast run can't race it.
    pub async fn run(
        &self,
        source: H::Source,
        payload: H::Input,
        delivery: Delivery,
        policy: Policy,
    ) -> Result<Reply, PromptError> {
        let (response, admitted) = oneshot::channel();
        let (completion, reply) = oneshot::channel();
        let request = Request::new(
            source,
            RequestKind::Input {
                id: InputId::new(),
                payload,
                delivery,
                policy,
            },
        );
        self.commands
            .unbounded_send(Command::Request {
                request: Box::new(request),
                response,
                completion: Some(completion),
            })
            .map_err(|_| RequestError::Closed)?;
        admitted.await.map_err(|_| RequestError::Closed)??;
        reply.await.map_err(|_| RequestError::Closed)?
    }

    pub async fn snapshot(&self) -> Result<Snapshot, RequestError> {
        let (response, reply) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Snapshot(response))
            .map_err(|_| RequestError::Closed)?;
        reply.await.map_err(|_| RequestError::Closed)
    }
}
