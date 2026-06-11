use std::{fmt, sync::Arc};

use futures::{StreamExt, future::BoxFuture};
use llm::{
    ChatMessage, ChatRequest, ConversationMemory, FinishReason, LlmError, Provider, StreamEvent,
    TokenUsage, ToolArgumentParseError, ToolCall, ToolError, ToolOutput, ToolRegistry,
    ToolSchemaFormat,
};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub preamble: Option<String>,
    pub max_turns: usize,
    pub tool_schema_format: ToolSchemaFormat,
}

impl AgentConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            preamble: None,
            max_turns: 100,
            tool_schema_format: ToolSchemaFormat::JsonSchema,
        }
    }
}

#[derive(Clone)]
pub struct AgentRuntime {
    provider: Arc<dyn Provider>,
    memory: Arc<dyn ConversationMemory>,
    tools: ToolRegistry,
    permission: Arc<dyn ToolPermissionPolicy>,
    config: AgentConfig,
}

impl AgentRuntime {
    pub fn new(
        provider: Arc<dyn Provider>,
        memory: Arc<dyn ConversationMemory>,
        tools: ToolRegistry,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            memory,
            tools,
            permission: Arc::new(AllowAll),
            config,
        }
    }

    /// Replace the tool-permission policy. The default allows every call; the
    /// app uses this to route writes through its permission UI.
    pub fn with_permission(mut self, permission: Arc<dyn ToolPermissionPolicy>) -> Self {
        self.permission = permission;
        self
    }

    pub async fn run(
        &self,
        conversation_id: &str,
        prompt: impl Into<String>,
        events: &mut impl EventSink,
    ) -> Result<String, AgentRunError> {
        let prompt = prompt.into();
        events.emit(AgentEvent::Started).await;

        let mut history = self.memory.load(conversation_id).await;
        let user_message = ChatMessage::user(prompt);
        self.memory
            .append(conversation_id, vec![user_message.clone()])
            .await;
        history.push(user_message);

        for turn_index in 0..self.config.max_turns {
            let turn = self.run_turn(&history, events).await?;

            let reasoning = if turn.reasoning.is_empty() {
                None
            } else {
                Some(turn.reasoning.clone())
            };
            let assistant_message = ChatMessage::assistant_with_tools(
                turn.assistant_text.clone(),
                reasoning,
                turn.tool_calls.clone(),
            );
            self.memory
                .append(conversation_id, vec![assistant_message.clone()])
                .await;
            history.push(assistant_message);

            if turn.tool_calls.is_empty() {
                events
                    .emit(AgentEvent::Finished {
                        response: turn.assistant_text.clone(),
                    })
                    .await;
                return Ok(turn.assistant_text);
            }

            let tool_messages = self.execute_tools(turn.tool_calls, events).await;
            self.memory
                .append(conversation_id, tool_messages.clone())
                .await;
            history.extend(tool_messages);

            if turn_index + 1 == self.config.max_turns {
                break;
            }
        }

        let error = AgentRunError::MaxTurns {
            max_turns: self.config.max_turns,
        };
        events
            .emit(AgentEvent::Error {
                error: error.to_string(),
            })
            .await;
        Err(error)
    }

    async fn run_turn(
        &self,
        history: &[ChatMessage],
        events: &mut impl EventSink,
    ) -> Result<TurnOutput, AgentRunError> {
        let mut messages = Vec::new();
        if let Some(preamble) = self
            .config
            .preamble
            .as_deref()
            .filter(|preamble| !preamble.trim().is_empty())
        {
            messages.push(ChatMessage::system(preamble));
        }
        messages.extend_from_slice(history);

        let mut request = ChatRequest::new(self.config.model.clone(), messages);
        request.tools = self.tools.definitions(self.config.tool_schema_format)?;

        let mut stream = self.provider.stream_chat(request).await?;
        let mut output = TurnOutput::default();

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::Queued { position } => {
                    events.emit(AgentEvent::Queued { position }).await;
                }
                StreamEvent::Started => {
                    events.emit(AgentEvent::ProviderStarted).await;
                }
                StreamEvent::TextDelta(delta) => {
                    output.assistant_text.push_str(&delta);
                    events.emit(AgentEvent::AssistantDelta { delta }).await;
                }
                StreamEvent::ReasoningDelta(delta) => {
                    output.reasoning.push_str(&delta);
                    events.emit(AgentEvent::ReasoningDelta { delta }).await;
                }
                StreamEvent::ToolCallStarted { id, name } => {
                    events.emit(AgentEvent::ToolCallStarted { id, name }).await;
                }
                StreamEvent::ToolCallArgumentDelta { id, delta } => {
                    events
                        .emit(AgentEvent::ToolCallArgumentDelta { id, delta })
                        .await;
                }
                StreamEvent::ToolCallFinished(call) => {
                    events
                        .emit(AgentEvent::ToolCallFinished(call.clone()))
                        .await;
                    output.tool_calls.push(call);
                }
                StreamEvent::ToolCallArgumentParseError(error) => {
                    events
                        .emit(AgentEvent::ToolCallArgumentParseError(error))
                        .await;
                }
                StreamEvent::Usage(usage) => {
                    events.emit(AgentEvent::Usage(usage)).await;
                }
                StreamEvent::Finished { reason } => {
                    output.finish_reason = Some(reason);
                }
            }
        }

        Ok(output)
    }

    async fn execute_tools(
        &self,
        tool_calls: Vec<ToolCall>,
        events: &mut impl EventSink,
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::with_capacity(tool_calls.len());

        for call in tool_calls {
            // A denied call is never executed; its reason goes back to the model
            // as an error tool result so it can adapt rather than silently stall.
            let output = match self.permission.decide(&call).await {
                ToolPermission::Allow => match self.tools.call(&call).await {
                    Ok(output) => output,
                    Err(error) => ToolOutput::error(error.to_string()),
                },
                ToolPermission::Deny { reason } => ToolOutput::error(reason),
            };

            events
                .emit(AgentEvent::ToolResult {
                    call: call.clone(),
                    output: output.clone(),
                })
                .await;

            messages.push(ChatMessage::tool(
                call.id,
                call.name,
                output.content,
                output.is_error,
            ));
        }

        messages
    }
}

#[derive(Debug, Clone, Default)]
struct TurnOutput {
    assistant_text: String,
    reasoning: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Started,
    Queued { position: usize },
    ProviderStarted,
    AssistantDelta { delta: String },
    ReasoningDelta { delta: String },
    ToolCallStarted { id: String, name: String },
    ToolCallArgumentDelta { id: String, delta: String },
    ToolCallFinished(ToolCall),
    ToolCallArgumentParseError(ToolArgumentParseError),
    ToolResult { call: ToolCall, output: ToolOutput },
    Usage(TokenUsage),
    Finished { response: String },
    Error { error: String },
}

pub trait EventSink {
    fn emit(&mut self, event: AgentEvent) -> BoxFuture<'_, ()>;
}

/// Decision returned by a [`ToolPermissionPolicy`] before a tool runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPermission {
    Allow,
    Deny { reason: String },
}

/// Consulted once per tool call, before execution. A simple async hook rather
/// than a policy framework: it returns allow, or deny with a reason the agent
/// loop surfaces to the model as an error tool result.
pub trait ToolPermissionPolicy: Send + Sync {
    fn decide(&self, call: &ToolCall) -> BoxFuture<'_, ToolPermission>;
}

/// Default policy: every tool call is allowed. Used when none is configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl ToolPermissionPolicy for AllowAll {
    fn decide(&self, _call: &ToolCall) -> BoxFuture<'_, ToolPermission> {
        Box::pin(async { ToolPermission::Allow })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&mut self, _event: AgentEvent) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRunError {
    Llm(LlmError),
    ToolSetup(ToolError),
    MaxTurns { max_turns: usize },
}

impl fmt::Display for AgentRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(error) => write!(f, "{error}"),
            Self::ToolSetup(error) => write!(f, "{error}"),
            Self::MaxTurns { max_turns } => {
                write!(f, "agent reached the maximum turn limit ({max_turns})")
            }
        }
    }
}

impl std::error::Error for AgentRunError {}

impl From<LlmError> for AgentRunError {
    fn from(error: LlmError) -> Self {
        Self::Llm(error)
    }
}

impl From<ToolError> for AgentRunError {
    fn from(error: ToolError) -> Self {
        Self::ToolSetup(error)
    }
}

#[cfg(test)]
mod tests {
    //! The permission gate sits between the model's tool call and execution: a
    //! denial must skip the tool and feed the reason back as an error result,
    //! while the default policy runs tools untouched.
    use super::*;
    use futures::stream;
    use llm::{ConversationStore, LlmStream, Tool};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Emits one `writer` tool call on the first turn, then a plain text finish
    /// so the loop terminates after the tool result is fed back.
    struct ScriptedProvider {
        turn: AtomicUsize,
    }

    impl Provider for ScriptedProvider {
        fn stream_chat(&self, _request: ChatRequest) -> BoxFuture<'_, Result<LlmStream, LlmError>> {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let events: Vec<Result<StreamEvent, LlmError>> = if turn == 0 {
                    vec![
                        Ok(StreamEvent::ToolCallFinished(ToolCall {
                            id: "call-1".into(),
                            name: "writer".into(),
                            raw_arguments: "{}".into(),
                            arguments: json!({}),
                        })),
                        Ok(StreamEvent::Finished {
                            reason: FinishReason::ToolCalls,
                        }),
                    ]
                } else {
                    vec![
                        Ok(StreamEvent::TextDelta("done".into())),
                        Ok(StreamEvent::Finished {
                            reason: FinishReason::Stop,
                        }),
                    ]
                };
                Ok(Box::pin(stream::iter(events)) as LlmStream)
            })
        }
    }

    /// Records whether it was actually invoked.
    struct FlagTool {
        called: Arc<AtomicBool>,
    }

    impl Tool for FlagTool {
        fn name(&self) -> &'static str {
            "writer"
        }

        fn description(&self) -> &'static str {
            "test tool"
        }

        fn parameters_schema(&self, _format: ToolSchemaFormat) -> Result<Value, ToolError> {
            Ok(json!({ "type": "object", "properties": {} }))
        }

        fn call(&self, _arguments: Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
            self.called.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(ToolOutput::text("wrote")) })
        }
    }

    struct DenyAll;

    impl ToolPermissionPolicy for DenyAll {
        fn decide(&self, call: &ToolCall) -> BoxFuture<'_, ToolPermission> {
            let name = call.name.clone();
            Box::pin(async move {
                ToolPermission::Deny {
                    reason: format!("denied: {name}"),
                }
            })
        }
    }

    fn scripted_runtime(called: Arc<AtomicBool>, memory: Arc<ConversationStore>) -> AgentRuntime {
        let mut tools = ToolRegistry::new();
        tools.insert(FlagTool { called }).expect("register tool");
        AgentRuntime::new(
            Arc::new(ScriptedProvider {
                turn: AtomicUsize::new(0),
            }),
            memory,
            tools,
            AgentConfig::new("test-model"),
        )
    }

    #[tokio::test]
    async fn denied_tool_is_not_executed_and_returns_error_result() {
        let called = Arc::new(AtomicBool::new(false));
        let memory = Arc::new(ConversationStore::new());
        let runtime =
            scripted_runtime(called.clone(), memory.clone()).with_permission(Arc::new(DenyAll));

        let response = runtime
            .run("conv", "go", &mut NoopEventSink)
            .await
            .expect("run completes");

        assert_eq!(response, "done");
        assert!(!called.load(Ordering::SeqCst), "denied tool must not run");

        let tool_message = memory
            .load("conv")
            .await
            .into_iter()
            .find_map(|message| match message {
                ChatMessage::Tool {
                    content, is_error, ..
                } => Some((content, is_error)),
                _ => None,
            })
            .expect("denied call recorded as a tool result");
        assert!(tool_message.1, "denied result is marked as an error");
        assert_eq!(tool_message.0, "denied: writer");
    }

    #[tokio::test]
    async fn default_policy_executes_tools() {
        let called = Arc::new(AtomicBool::new(false));
        let runtime = scripted_runtime(called.clone(), Arc::new(ConversationStore::new()));

        runtime
            .run("conv", "go", &mut NoopEventSink)
            .await
            .expect("run completes");

        assert!(called.load(Ordering::SeqCst), "allowed tool runs");
    }
}
