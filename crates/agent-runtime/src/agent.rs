use std::{
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use agent_input::{Delivery, Fallback, Lifetime, Policy, QueueAccess};
use std::collections::HashSet;

use futures::future::BoxFuture;
use language_model::{LanguageModelProvider, ToolCallId};
use tool_runtime::{Provides, Tool, ToolError};

use crate::{
    AgentDefinition, AgentEvent, AgentHost, AgentSession, BasicHost, Compaction, CompactionError,
    CompactionPolicy, Configuration, ContextSection, Conversation, PromptError, Reply, Request,
    RequestError, RequestKind, Response, Sampling, SessionError, SessionHandle, SessionResult,
    persistence::{JsonlLog, LogError, StoredSource, record::StoredEvent},
    tools::HostRegistry,
};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error(transparent)]
    Request(#[from] RequestError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("session driver failed: {0}")]
    Driver(String),
}

/// A short conversation or an unhelpful summary is an unchanged view, not a
/// failed operation. Other errors (transport, cancellation) remain errors.
#[derive(Debug)]
pub enum CompactionOutcome {
    Applied(Box<Compaction>),
    Unchanged(CompactionError),
}

type Observer<H> = Box<dyn FnMut(AgentEvent<H>) + Send>;
/// Adds a context section once the session exists.
type SectionInstaller<H> = Box<dyn FnOnce(AgentSession<H>) -> AgentSession<H> + Send>;
type Encoder<H> = fn(&AgentEvent<H>) -> Result<StoredEvent, LogError>;
type Restored<S> = Option<(Conversation<S>, u64, HashSet<ToolCallId>)>;
/// Opens the log for a fresh (empty) or resumed session.
type Opener<S> =
    Box<dyn FnOnce() -> BoxFuture<'static, Result<(JsonlLog, Restored<S>), LogError>> + Send>;

/// Builds a managed [`Agent`]: a session, its driver task, and optional
/// storage. Defaults: tools are refused unless permitted, 16 steps per run,
/// 4096 output tokens per request, manual compaction only.
pub struct AgentBuilder<H: AgentHost> {
    host: Arc<H>,
    provider: Arc<dyn LanguageModelProvider>,
    definition: AgentDefinition,
    configuration: Configuration<H::Permissions>,
    registry: HostRegistry<H>,
    sections: Vec<SectionInstaller<H>>,
    source: H::Source,
    input_policy: Policy,
    storage: Option<(Opener<H::Source>, Encoder<H>)>,
    observer: Option<Observer<H>>,
}

impl Agent<BasicHost> {
    /// A single-user agent: string prompts from source `"user"`.
    pub fn builder(
        provider: impl LanguageModelProvider + 'static,
        definition: AgentDefinition,
    ) -> AgentBuilder<BasicHost> {
        AgentBuilder::new(
            Arc::new(BasicHost),
            Arc::new(provider),
            definition,
            "user".into(),
        )
    }
}

impl<H: AgentHost> AgentBuilder<H> {
    /// `source` is the source of this agent's own prompts and requests.
    pub fn new(
        host: Arc<H>,
        provider: Arc<dyn LanguageModelProvider>,
        definition: AgentDefinition,
        source: H::Source,
    ) -> Self
    where
        H::Permissions: Default,
    {
        Self {
            host,
            provider,
            definition,
            configuration: Configuration::default(),
            registry: HostRegistry::<H>::default(),
            sections: Vec::new(),
            source,
            input_policy: Policy {
                access: QueueAccess::Protected,
                fallback: Fallback::Queue,
                lifetime: Lifetime::Retain,
            },
            storage: None,
            observer: None,
        }
    }

    /// Makes a tool available. The definition decides whether it is offered
    /// directly; otherwise it can still be added as a dynamic tool by name.
    pub fn tool<T>(mut self, tool: T) -> Result<Self, ToolError>
    where
        T: Tool,
        H: Provides<T::Context>,
        H::Access: From<T::Access>,
    {
        self.registry.register(tool)?;
        Ok(self)
    }

    pub fn configuration(mut self, configuration: Configuration<H::Permissions>) -> Self {
        self.configuration = configuration;
        self
    }

    pub fn permissions(mut self, permissions: H::Permissions) -> Self {
        self.configuration.permissions = permissions;
        self
    }

    pub fn sampling(mut self, sampling: Sampling) -> Self {
        self.configuration.sampling = sampling;
        self
    }

    pub fn max_steps(mut self, limit: NonZeroUsize) -> Self {
        self.configuration.max_steps = limit;
        self
    }

    pub fn compaction(mut self, policy: CompactionPolicy) -> Self {
        self.configuration.compaction = policy;
        self
    }

    /// The policy of inputs sent by [`Agent::prompt`].
    pub fn input_policy(mut self, policy: Policy) -> Self {
        self.input_policy = policy;
        self
    }

    pub fn section(mut self, section: impl ContextSection<H>) -> Self {
        self.sections
            .push(Box::new(move |session| session.with_section(section)));
        self
    }

    /// Starts a new log; refuses a directory that already has one.
    pub fn create_at(mut self, directory: impl Into<PathBuf>) -> Self
    where
        H::Source: StoredSource,
    {
        let directory = directory.into();
        self.storage = Some((
            Box::new(move || Box::pin(create::<H::Source>(directory))),
            StoredEvent::encode::<H>,
        ));
        self
    }

    /// Continues a saved session; the log must exist.
    pub fn resume_from(mut self, directory: impl Into<PathBuf>) -> Self
    where
        H::Source: StoredSource,
    {
        let directory = directory.into();
        self.storage = Some((
            Box::new(move || Box::pin(resume::<H::Source>(directory))),
            StoredEvent::encode::<H>,
        ));
        self
    }

    /// Continues a saved session if there is one, otherwise starts one.
    pub fn open_or_create(mut self, directory: impl Into<PathBuf>) -> Self
    where
        H::Source: StoredSource,
    {
        let directory = directory.into();
        self.storage = Some((
            Box::new(move || {
                Box::pin(async move {
                    if tokio::fs::try_exists(directory.join("updates.jsonl")).await? {
                        resume::<H::Source>(directory).await
                    } else {
                        create::<H::Source>(directory).await
                    }
                })
            }),
            StoredEvent::encode::<H>,
        ));
        self
    }

    /// Observes every event, after storage. Runs on the driver; keep it cheap.
    pub fn on_event(mut self, observer: impl FnMut(AgentEvent<H>) + Send + 'static) -> Self {
        self.observer = Some(Box::new(observer));
        self
    }

    /// Starts the session on the current Tokio runtime. Validation and log
    /// setup complete before any model or tool request can run.
    pub async fn build(self) -> Result<Agent<H>, AgentError> {
        let (mut session, handle) = AgentSession::new(
            self.host,
            self.provider,
            self.definition,
            self.configuration,
            self.registry,
        )?;
        for add in self.sections {
            session = add(session);
        }
        let (log, encoder) = match self.storage {
            Some((open, encoder)) => {
                let (log, restored) = open().await?;
                if let Some((conversation, next_sequence, started)) = restored {
                    let resumed = session.with_interrupted_conversation(
                        conversation,
                        next_sequence,
                        |call| started.contains(call),
                    );
                    session = match resumed {
                        Ok(session) => session,
                        Err(error) => {
                            log.finish().await?;
                            return Err(error.into());
                        }
                    };
                }
                (Some(log), Some(encoder))
            }
            None => (None, None),
        };
        let recorder = log.as_ref().map(JsonlLog::recorder);
        let log_error = Arc::new(Mutex::new(None));
        let write_error = log_error.clone();
        let mut observer = self.observer;
        let driver = tokio::spawn(session.run(move |event| {
            if let (Some(recorder), Some(encoder)) = (&recorder, encoder)
                && let Err(error) = encoder(&event).and_then(|event| recorder.record_encoded(event))
            {
                write_error
                    .lock()
                    .expect("log error lock")
                    .get_or_insert(error);
            }
            if let Some(observer) = &mut observer {
                observer(event);
            }
        }));
        Ok(Agent {
            handle,
            driver: Some(driver),
            log,
            log_error,
            source: self.source,
            input_policy: self.input_policy,
        })
    }
}

async fn create<S: StoredSource>(directory: PathBuf) -> Result<(JsonlLog, Restored<S>), LogError> {
    Ok((
        JsonlLog::create(&directory, &Conversation::<S>::new(), 0).await?,
        None,
    ))
}

async fn resume<S: StoredSource>(directory: PathBuf) -> Result<(JsonlLog, Restored<S>), LogError> {
    let (log, loaded) = JsonlLog::open::<S>(&directory).await?;
    let started = loaded.started_calls();
    Ok((
        log,
        Some((loaded.conversation, loaded.next_sequence, started)),
    ))
}

/// A managed session for sequential prompt call sites. The handle remains
/// available for other sources, steering, approvals, and state changes.
/// `close` joins the driver and the log writer; dropping only requests shutdown.
pub struct Agent<H: AgentHost = BasicHost> {
    handle: SessionHandle<H>,
    driver: Option<tokio::task::JoinHandle<SessionResult<H>>>,
    log: Option<JsonlLog>,
    log_error: Arc<Mutex<Option<LogError>>>,
    source: H::Source,
    input_policy: Policy,
}

impl<H: AgentHost> Agent<H> {
    pub fn handle(&self) -> SessionHandle<H> {
        self.handle.clone()
    }

    /// Queues a prompt and waits for the reply of the run that consumes it.
    pub async fn prompt(&self, payload: impl Into<H::Input>) -> Result<Reply, AgentError> {
        Ok(self
            .handle
            .run(
                self.source.clone(),
                payload.into(),
                Delivery::Queue,
                self.input_policy,
            )
            .await?)
    }

    /// Sends a request from this agent's own source.
    pub async fn send(&self, kind: RequestKind<H>) -> Result<Response<H>, AgentError> {
        Ok(self
            .handle
            .send(Request::new(self.source.clone(), kind))
            .await?)
    }

    pub async fn compact(&self) -> Result<CompactionOutcome, AgentError> {
        match self.send(RequestKind::Compact).await {
            Ok(Response::Compacted(compaction)) => {
                Ok(CompactionOutcome::Applied(Box::new(compaction)))
            }
            Ok(_) => unreachable!("compaction answers with its result"),
            Err(AgentError::Request(RequestError::Compaction(
                error @ (CompactionError::NothingToCompact | CompactionError::NotSmaller),
            ))) => Ok(CompactionOutcome::Unchanged(error)),
            Err(error) => Err(error),
        }
    }

    /// Stops accepting work, settles the active run, and flushes and joins
    /// logging even if the driver failed. Background write errors surface here.
    pub async fn close(mut self) -> Result<SessionResult<H>, AgentError> {
        let _ = self
            .handle
            .post(Request::new(self.source.clone(), RequestKind::Shutdown));
        let result = self
            .driver
            .take()
            .expect("owned driver")
            .await
            .map_err(|error| AgentError::Driver(error.to_string()));
        let written = if let Some(log) = self.log.take() {
            log.finish().await
        } else {
            Ok(())
        };
        let result = result?;
        written?;
        if let Some(error) = self.log_error.lock().expect("log error lock").take() {
            return Err(error.into());
        }
        Ok(result)
    }
}

impl<H: AgentHost> Drop for Agent<H> {
    fn drop(&mut self) {
        if self.driver.is_some() {
            let _ = self
                .handle
                .post(Request::new(self.source.clone(), RequestKind::Shutdown));
        }
    }
}
