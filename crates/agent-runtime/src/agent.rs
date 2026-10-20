use std::{
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use agent_input::{Delivery, Fallback, Input, InputId, Lifetime, Policy, QueueAccess};
use language_model::{
    CompletionRequest, LanguageModel, LanguageModelProvider, Message, Role, Thinking,
};
use serde::Serialize;
use tool_runtime::{Tool, ToolDispatch, ToolError, ToolErrorKind, ToolRegistry};

use crate::persistence::record::StoredEvent;
use crate::{
    AgentDefinition, AgentEvent, AgentSession, AllowAll, AllowTools, Compaction, CompactionError,
    CompactionPolicy, PromptError, Reply, RunError, SessionHandle, SessionResult, ToolExecutor,
    ToolPermissions, ToolPolicy,
    persistence::{JsonlLog, LogError},
};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("invalid session configuration: {0}")]
    Configuration(String),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Compaction(#[from] CompactionError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("session driver failed: {0}")]
    Driver(String),
}

/// A short conversation or an unhelpful summary is an unchanged view, not a
/// failed operation. Other errors (transport, invalid summary, cancellation)
/// remain errors. The low-level handle exposes the original strict Result.
#[derive(Debug)]
pub enum CompactionOutcome {
    Applied(Box<Compaction>),
    Unchanged(CompactionError),
}

enum StorageMode {
    Create,
    Resume,
    OpenOrCreate,
}
struct Storage {
    directory: PathBuf,
    mode: StorageMode,
}
type Observer<S> = Box<dyn FnMut(AgentEvent<S>) + Send>;
type Encoder<S> = fn(&AgentEvent<S>) -> Result<StoredEvent, LogError>;

/// Configuration for a managed session. Defaults deny tools, execute sequentially,
/// limit runs to eight model turns and 4096 output tokens per request, and permit
/// manual compaction only. Storage and observers are opt-in.
pub struct AgentBuilder<S = String> {
    definition: AgentDefinition,
    tools: Option<Vec<String>>,
    provider: Arc<dyn LanguageModelProvider>,
    model: LanguageModel,
    source: S,
    registry: ToolRegistry,
    dispatch: Option<Arc<dyn ToolDispatch>>,
    policy: Arc<dyn ToolPolicy>,
    request: CompletionRequest,
    max_turns: usize,
    input_policy: Policy,
    compaction: Option<CompactionPolicy>,
    storage: Option<Storage>,
    saved_instructions: bool,
    observer: Option<Observer<S>>,
    encoder: Option<Encoder<S>>,
}

impl<S> AgentBuilder<S> {
    /// Shared-provider/typed-source entry point. Common callers can use
    /// Agent::builder with a concrete provider and the default local source.
    pub fn new(provider: Arc<dyn LanguageModelProvider>, model: LanguageModel, source: S) -> Self {
        let definition = AgentDefinition::default();
        Self {
            provider,
            model,
            source,
            registry: ToolRegistry::default(),
            dispatch: None,
            policy: Arc::new(AllowTools::default()),
            request: CompletionRequest {
                max_output_tokens: Some(definition.max_output_tokens),
                ..Default::default()
            },
            max_turns: definition.max_turns,
            definition,
            tools: None,
            input_policy: Policy {
                access: QueueAccess::Protected,
                fallback: Fallback::Queue,
                lifetime: Lifetime::Retain,
            },
            compaction: Some(CompactionPolicy::default()),
            storage: None,
            saved_instructions: false,
            observer: None,
            encoder: None,
        }
    }

    /// Applies a reusable definition. Later builder calls override its settings;
    /// runtime services, storage, observers, and input policy are left in place.
    /// The definition is cloned so it can seed independent agents.
    pub fn definition(mut self, definition: &AgentDefinition) -> Self {
        self.request
            .messages
            .retain(|message| message.role != Role::System);
        if !definition.system_prompt.is_empty() {
            self.request
                .messages
                .insert(0, Message::system(&definition.system_prompt));
        }
        self.request.max_output_tokens = Some(definition.max_output_tokens);
        self.request.thinking = definition.thinking.clone();
        self.max_turns = definition.max_turns;
        self.tools = definition.tools.clone();
        self.policy = match &definition.permissions {
            ToolPermissions::DenyAll => Arc::new(AllowTools::default()),
            ToolPermissions::AllowListed(names) => Arc::new(AllowTools::new(names.clone())),
            ToolPermissions::AllowAll => Arc::new(AllowAll),
        };
        self.definition = definition.clone();
        self
    }

    /// Exposes only these registered tool names, independently of permission.
    pub fn tools(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = Some(names.into_iter().map(Into::into).collect());
        self
    }

    pub fn source(mut self, source: S) -> Self {
        self.source = source;
        self
    }

    pub fn system_prompt(mut self, text: impl Into<String>) -> Self {
        let message = Message::system(text);
        if self
            .request
            .messages
            .first()
            .is_some_and(|message| message.role == Role::System)
        {
            self.request.messages[0] = message;
        } else {
            self.request.messages.insert(0, message);
        }
        self
    }

    pub fn tool<T: Tool + 'static>(mut self, tool: T) -> Result<Self, ToolError> {
        if self.dispatch.is_some() {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "choose registered tools or a custom dispatcher",
            ));
        }
        self.registry.register(tool)?;
        Ok(self)
    }

    pub fn tool_dispatch(mut self, dispatch: Arc<dyn ToolDispatch>) -> Self {
        self.dispatch = Some(dispatch);
        self
    }

    /// Replaces the current policy with an explicit allowlist. Read-only metadata
    /// alone never grants permission. All other tool names remain denied.
    pub fn allow_tools(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.policy = Arc::new(AllowTools::new(names));
        self
    }

    pub fn allow_all_tools(mut self) -> Self {
        self.policy = Arc::new(AllowAll);
        self
    }

    pub fn tool_policy(mut self, policy: impl ToolPolicy + 'static) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    pub fn max_turns(mut self, limit: usize) -> Self {
        self.max_turns = limit;
        self
    }
    pub fn max_output_tokens(mut self, limit: u64) -> Self {
        self.request.max_output_tokens = Some(limit);
        self
    }
    pub fn thinking(mut self, thinking: Thinking) -> Self {
        self.request.thinking = Some(thinking);
        self
    }

    /// Replaces all initial history and generation controls. Tool definitions
    /// must still match the executor. Use this for provider-specific options.
    pub fn request(mut self, request: CompletionRequest) -> Self {
        self.request = request;
        self
    }
    pub fn input_policy(mut self, policy: Policy) -> Self {
        self.input_policy = policy;
        self
    }
    pub fn compaction(mut self, policy: CompactionPolicy) -> Self {
        self.compaction = Some(policy);
        self
    }
    pub fn disable_compaction(mut self) -> Self {
        self.compaction = None;
        self
    }

    /// Refuses an existing session log. No files are written unless configured.
    pub fn create_at(mut self, directory: impl Into<PathBuf>) -> Self
    where
        S: Serialize,
    {
        self.encoder = Some(StoredEvent::encode::<S>);
        self.storage = Some(Storage {
            directory: directory.into(),
            mode: StorageMode::Create,
        });
        self
    }
    pub fn resume_from(mut self, directory: impl Into<PathBuf>) -> Self
    where
        S: Serialize,
    {
        self.encoder = Some(StoredEvent::encode::<S>);
        self.storage = Some(Storage {
            directory: directory.into(),
            mode: StorageMode::Resume,
        });
        self
    }
    /// Explicitly opt into saved-session reuse, rather than silently replacing
    /// configured initial history. Saved instructions must match by default.
    pub fn open_or_create(mut self, directory: impl Into<PathBuf>) -> Self
    where
        S: Serialize,
    {
        self.encoder = Some(StoredEvent::encode::<S>);
        self.storage = Some(Storage {
            directory: directory.into(),
            mode: StorageMode::OpenOrCreate,
        });
        self
    }
    pub fn use_saved_instructions(mut self) -> Self {
        self.saved_instructions = true;
        self
    }
    pub fn on_event(mut self, observer: impl FnMut(AgentEvent<S>) + Send + 'static) -> Self {
        self.observer = Some(Box::new(observer));
        self
    }
}

impl<S: Clone + PartialEq + Send + 'static> AgentBuilder<S> {
    /// Starts the owner on the current Tokio runtime. Validation and optional
    /// log setup complete before the driver can execute any model/tool request.
    pub async fn build(self) -> Result<Agent<S>, AgentError> {
        let limit = NonZeroUsize::new(self.max_turns)
            .ok_or_else(|| AgentError::Configuration("max_turns must be positive".into()))?;
        if self.dispatch.is_some() && !self.registry.descriptors().is_empty() {
            return Err(AgentError::Configuration(
                "choose registered tools or a custom dispatcher".into(),
            ));
        }
        let dispatch = self.dispatch.unwrap_or_else(|| Arc::new(self.registry));
        let mut executor = ToolExecutor::new(dispatch, self.policy)?;
        if let Some(names) = &self.tools {
            executor = executor.select_tools(names)?;
        }
        let configured_history = self.request.messages.clone();
        let (mut owner, handle) =
            AgentSession::new(self.provider, self.model, executor, self.request)?;
        owner = owner.with_max_model_turns(limit);
        if let Some(policy) = self.compaction {
            owner = owner.with_compaction(policy);
        }
        let log = if let Some(storage) = self.storage {
            let resume = match storage.mode {
                StorageMode::Create => false,
                StorageMode::Resume => true,
                StorageMode::OpenOrCreate => {
                    tokio::fs::try_exists(storage.directory.join("updates.jsonl"))
                        .await
                        .map_err(LogError::Io)?
                }
            };
            if resume {
                let (log, loaded) = JsonlLog::open(&storage.directory).await?;
                if !self.saved_instructions
                    && !configured_history.is_empty()
                    && !matching_instructions(&configured_history, loaded.conversation.messages())
                {
                    log.finish().await?;
                    return Err(AgentError::Configuration("saved instructions differ from configured history; use_saved_instructions explicitly selects saved history".into()));
                }
                owner = match owner.with_conversation(loaded.conversation, loaded.next_sequence) {
                    Ok(owner) => owner,
                    Err(error) => {
                        log.finish().await?;
                        return Err(error.into());
                    }
                };
                Some(log)
            } else {
                Some(JsonlLog::create(&storage.directory, owner.conversation(), 0).await?)
            }
        } else {
            None
        };
        let recorder = log.as_ref().map(JsonlLog::recorder);
        let encoder = self.encoder;
        let log_error = Arc::new(Mutex::new(None));
        let write_error = log_error.clone();
        let mut observer = self.observer;
        let driver = tokio::spawn(owner.run(move |event| {
            if let Some(recorder) = &recorder
                && let Err(error) = encoder.expect("storage has an encoder")(&event)
                    .and_then(|event| recorder.record_encoded(event))
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
            definition: self.definition,
            handle,
            driver: Some(driver),
            log,
            log_error,
            source: self.source,
            input_policy: self.input_policy,
        })
    }
}

fn matching_instructions(configured: &[Message], saved: &[Message]) -> bool {
    let instructions = |message: &&Message| matches!(message.role, Role::System | Role::Developer);
    let prefix = saved.iter().take_while(instructions).collect::<Vec<_>>();
    configured.len() == prefix.len()
        && configured
            .iter()
            .zip(prefix)
            .all(|(a, b)| a.role == b.role && a.content == b.content)
}

/// Managed owner for sequential prompt call sites. The low-level handle remains
/// available for multiple sources, queue policy, steering, and scoped stops.
/// close joins both the owner and log writer. Drop only requests shutdown.
pub struct Agent<S: Clone = String> {
    definition: AgentDefinition,
    handle: SessionHandle<S>,
    driver: Option<tokio::task::JoinHandle<SessionResult<S>>>,
    log: Option<JsonlLog>,
    log_error: Arc<Mutex<Option<LogError>>>,
    source: S,
    input_policy: Policy,
}

impl Agent<String> {
    pub fn builder(
        provider: impl LanguageModelProvider + 'static,
        model: LanguageModel,
    ) -> AgentBuilder<String> {
        AgentBuilder::new(Arc::new(provider), model, "local".into())
    }
}

impl<S: Clone> Agent<S> {
    /// The authored definition used to build this agent. Per-instance builder
    /// overrides and live state are separate from this reusable blueprint.
    pub fn definition(&self) -> &AgentDefinition {
        &self.definition
    }

    pub fn handle(&self) -> SessionHandle<S> {
        self.handle.clone()
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<Reply, AgentError> {
        Ok(self
            .handle
            .run_input(
                Input {
                    id: InputId::new(),
                    source: self.source.clone(),
                    payload: Message::user(text),
                    policy: self.input_policy,
                },
                Delivery::Queue,
            )
            .await?)
    }

    pub async fn compact(&self) -> Result<CompactionOutcome, AgentError> {
        match self.handle.compact().await {
            Ok(compaction) => Ok(CompactionOutcome::Applied(Box::new(compaction))),
            Err(error @ (CompactionError::NothingToCompact | CompactionError::NotSmaller)) => {
                Ok(CompactionOutcome::Unchanged(error))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Stop accepting work, settle the active run, and flush/join logging even
    /// if the driver failed. Background write errors are always returned here.
    pub async fn close(mut self) -> Result<SessionResult<S>, AgentError> {
        let _ = self.handle.shutdown();
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

impl<S: Clone> Drop for Agent<S> {
    fn drop(&mut self) {
        if self.driver.is_some() {
            let _ = self.handle.shutdown();
        }
    }
}
