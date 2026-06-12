use std::{fmt, sync::Arc};

use async_trait::async_trait;
use futures::StreamExt;
use llm::{
    ChatMessage, ChatRequest, ConversationMemory, FinishReason, LlmError, Provider, StreamEvent,
    TokenUsage, ToolArgumentParseError, ToolCall, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub preamble: Option<String>,
    pub max_turns: usize,
}

impl AgentConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            preamble: None,
            max_turns: 100,
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
            // Malformed tool calls never produced a usable `ToolCall`, but the
            // model still emitted them, so record them on the assistant turn and
            // feed an error result back below so it can correct itself.
            let parse_error_calls: Vec<ToolCall> = turn
                .argument_parse_errors
                .iter()
                .map(argument_parse_error_call)
                .collect();
            let mut assistant_tool_calls = turn.tool_calls.clone();
            assistant_tool_calls.extend(parse_error_calls);

            let assistant_message = ChatMessage::assistant_with_tools(
                turn.assistant_text.clone(),
                reasoning,
                assistant_tool_calls,
            );
            self.memory
                .append(conversation_id, vec![assistant_message.clone()])
                .await;
            history.push(assistant_message);

            let has_tool_activity =
                !turn.tool_calls.is_empty() || !turn.argument_parse_errors.is_empty();

            // A response cut off at the output-length limit is incomplete. With
            // no tool calls to continue from, the partial text is not a real
            // answer, so surface it rather than returning it as if finished.
            if matches!(turn.finish_reason, Some(FinishReason::Length)) && !has_tool_activity {
                return Err(AgentRunError::Truncated);
            }

            if !has_tool_activity {
                return Ok(turn.assistant_text);
            }

            let mut tool_messages = self.execute_tools(turn.tool_calls, events).await;
            tool_messages.extend(
                self.report_argument_parse_errors(turn.argument_parse_errors, events)
                    .await,
            );
            self.memory
                .append(conversation_id, tool_messages.clone())
                .await;
            history.extend(tool_messages);

            if turn_index + 1 == self.config.max_turns {
                break;
            }
        }

        Err(AgentRunError::MaxTurns {
            max_turns: self.config.max_turns,
        })
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
        request.tools = self.tools.definitions()?;

        let mut stream = self.provider.stream_chat(request).await?;
        let mut output = TurnOutput::default();

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::Queued { position } => {
                    events.emit(AgentStreamEvent::Queued { position }).await;
                }
                StreamEvent::Started => {
                    events.emit(AgentStreamEvent::ProviderStarted).await;
                }
                StreamEvent::TextDelta(delta) => {
                    output.assistant_text.push_str(&delta);
                    events
                        .emit(AgentStreamEvent::AssistantDelta { delta })
                        .await;
                }
                StreamEvent::ReasoningDelta(delta) => {
                    output.reasoning.push_str(&delta);
                    events
                        .emit(AgentStreamEvent::ReasoningDelta { delta })
                        .await;
                }
                StreamEvent::ToolCallStarted { id, name } => {
                    events
                        .emit(AgentStreamEvent::ToolCallStarted { id, name })
                        .await;
                }
                StreamEvent::ToolCallArgumentDelta { id, delta } => {
                    events
                        .emit(AgentStreamEvent::ToolCallArgumentDelta { id, delta })
                        .await;
                }
                StreamEvent::ToolCallFinished(call) => {
                    events
                        .emit(AgentStreamEvent::ToolCallFinished(call.clone()))
                        .await;
                    output.tool_calls.push(call);
                }
                StreamEvent::ToolCallArgumentParseError(error) => {
                    events
                        .emit(AgentStreamEvent::ToolCallArgumentParseError(error.clone()))
                        .await;
                    output.argument_parse_errors.push(error);
                }
                StreamEvent::Usage(usage) => {
                    events.emit(AgentStreamEvent::Usage(usage)).await;
                }
                StreamEvent::Finished { reason } => {
                    output.finish_reason = Some(reason);
                }
            }
        }

        Ok(output)
    }

    /// Runs a turn's tool calls **sequentially**, one `await` at a time.
    ///
    /// Do not convert this to `join_all`/concurrent execution without an
    /// explicit design decision. The only provider is local Ollama, which
    /// serves one prompt at a time; the `subagent` tool runs a nested agent
    /// turn, so concurrent tool calls would issue overlapping Ollama requests
    /// that thrash its shared KV cache and serialize behind the global lock
    /// anyway — slower, not faster. Sequential execution is intentional here.
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
                .emit(AgentStreamEvent::ToolResult {
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

    /// Turn each malformed tool call into an error tool result so the model is
    /// told its arguments could not be parsed and can retry, instead of the call
    /// being silently dropped. Emits a matching `ToolResult` event for the UI.
    async fn report_argument_parse_errors(
        &self,
        errors: Vec<ToolArgumentParseError>,
        events: &mut impl EventSink,
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::with_capacity(errors.len());

        for error in errors {
            let call = argument_parse_error_call(&error);
            let output = ToolOutput::error(argument_parse_error_message(&error));

            events
                .emit(AgentStreamEvent::ToolResult {
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

/// Synthetic [`ToolCall`] standing in for a call whose arguments failed to
/// parse, so the assistant turn and its error result reference the same id.
fn argument_parse_error_call(error: &ToolArgumentParseError) -> ToolCall {
    ToolCall {
        id: error.id.clone(),
        name: error.name.clone(),
        raw_arguments: error.raw_arguments.clone(),
        arguments: Value::Null,
    }
}

fn argument_parse_error_message(error: &ToolArgumentParseError) -> String {
    format!(
        "invalid tool arguments: {}. The arguments could not be parsed as JSON; \
         resend the call with valid JSON arguments. Raw arguments received: {}",
        error.error, error.raw_arguments
    )
}

#[derive(Debug, Clone, Default)]
struct TurnOutput {
    assistant_text: String,
    reasoning: String,
    tool_calls: Vec<ToolCall>,
    argument_parse_errors: Vec<ToolArgumentParseError>,
    finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentStreamEvent {
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
}

#[async_trait]
pub trait EventSink {
    async fn emit(&mut self, event: AgentStreamEvent);
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
#[async_trait]
pub trait ToolPermissionPolicy: Send + Sync {
    async fn decide(&self, call: &ToolCall) -> ToolPermission;
}

/// Default policy: every tool call is allowed. Used when none is configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

#[async_trait]
impl ToolPermissionPolicy for AllowAll {
    async fn decide(&self, _call: &ToolCall) -> ToolPermission {
        ToolPermission::Allow
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopEventSink;

#[async_trait]
impl EventSink for NoopEventSink {
    async fn emit(&mut self, _event: AgentStreamEvent) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRunError {
    Llm(LlmError),
    ToolSetup(ToolError),
    MaxTurns {
        max_turns: usize,
    },
    /// The model stopped at the output-length limit with no tool calls to
    /// continue from, so the response was cut off before completion.
    Truncated,
}

impl fmt::Display for AgentRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(error) => write!(f, "{error}"),
            Self::ToolSetup(error) => write!(f, "{error}"),
            Self::MaxTurns { max_turns } => {
                write!(f, "agent reached the maximum turn limit ({max_turns})")
            }
            Self::Truncated => write!(
                f,
                "the model response was cut off at the output-length limit before completion"
            ),
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
    use async_trait::async_trait;
    use futures::stream;
    use llm::{ConversationStore, LlmStream, Tool};
    use serde_json::{Value, json};
    use std::{
        borrow::Cow,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    /// Emits one `writer` tool call on the first turn, then a plain text finish
    /// so the loop terminates after the tool result is fed back.
    struct ScriptedProvider {
        turn: AtomicUsize,
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn stream_chat(&self, _request: ChatRequest) -> Result<LlmStream, LlmError> {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
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
        }
    }

    /// Records whether it was actually invoked.
    struct FlagTool {
        called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for FlagTool {
        fn name(&self) -> Cow<'static, str> {
            "writer".into()
        }

        fn description(&self) -> Cow<'static, str> {
            "test tool".into()
        }

        fn parameters_schema(&self) -> Result<Value, ToolError> {
            Ok(json!({ "type": "object", "properties": {} }))
        }

        async fn call(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(ToolOutput::text("wrote"))
        }
    }

    struct DenyAll;

    #[async_trait]
    impl ToolPermissionPolicy for DenyAll {
        async fn decide(&self, call: &ToolCall) -> ToolPermission {
            ToolPermission::Deny {
                reason: format!("denied: {}", call.name),
            }
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

    /// Replays a fixed script of stream events per turn; falls back to a plain
    /// text finish once the script is exhausted so loops always terminate.
    struct ScriptProvider {
        turns: std::sync::Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    }

    impl ScriptProvider {
        fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                turns: std::sync::Mutex::new(turns.into()),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptProvider {
        async fn stream_chat(&self, _request: ChatRequest) -> Result<LlmStream, LlmError> {
            let turn = self.turns.lock().unwrap().pop_front().unwrap_or_else(|| {
                vec![
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::Finished {
                        reason: FinishReason::Stop,
                    },
                ]
            });
            let events: Vec<Result<StreamEvent, LlmError>> = turn.into_iter().map(Ok).collect();
            Ok(Box::pin(stream::iter(events)) as LlmStream)
        }
    }

    #[tokio::test]
    async fn malformed_tool_arguments_are_reported_to_the_model() {
        let called = Arc::new(AtomicBool::new(false));
        let memory = Arc::new(ConversationStore::new());
        let mut tools = ToolRegistry::new();
        tools
            .insert(FlagTool {
                called: called.clone(),
            })
            .expect("register tool");

        // Turn 0 emits a tool call whose arguments fail to parse; turn 1 is the
        // model recovering after it sees the error result.
        let provider = Arc::new(ScriptProvider::new(vec![
            vec![
                StreamEvent::ToolCallStarted {
                    id: "call-1".into(),
                    name: "writer".into(),
                },
                StreamEvent::ToolCallArgumentParseError(ToolArgumentParseError {
                    id: "call-1".into(),
                    name: "writer".into(),
                    raw_arguments: "{bad".into(),
                    error: "expected value".into(),
                }),
                StreamEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                StreamEvent::TextDelta("recovered".into()),
                StreamEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ]));

        let runtime = AgentRuntime::new(
            provider,
            memory.clone(),
            tools,
            AgentConfig::new("test-model"),
        );
        let response = runtime
            .run("conv", "go", &mut NoopEventSink)
            .await
            .expect("run continues past the malformed call");

        assert_eq!(response, "recovered");
        assert!(
            !called.load(Ordering::SeqCst),
            "a call with unparseable arguments must not execute the tool"
        );

        let history = memory.load("conv").await;
        let tool_result = history
            .iter()
            .find_map(|message| match message {
                ChatMessage::Tool {
                    content,
                    is_error,
                    call_id,
                    ..
                } => Some((content.clone(), *is_error, call_id.clone())),
                _ => None,
            })
            .expect("the malformed call is fed back as a tool result");
        assert!(tool_result.1, "the malformed-argument result is an error");
        assert_eq!(tool_result.2, "call-1");
        assert!(tool_result.0.contains("invalid tool arguments"));
        assert!(
            tool_result.0.contains("{bad"),
            "the raw arguments are echoed back so the model can correct them"
        );

        // The assistant turn references the malformed call so the result pairs
        // with it rather than dangling.
        assert!(
            history.iter().any(|message| matches!(
                message,
                ChatMessage::Assistant { tool_calls, .. }
                    if tool_calls.iter().any(|call| call.id == "call-1")
            )),
            "the assistant turn records the malformed call"
        );
    }

    #[tokio::test]
    async fn length_truncated_response_without_tools_surfaces_an_error() {
        let memory = Arc::new(ConversationStore::new());
        let provider = Arc::new(ScriptProvider::new(vec![vec![
            StreamEvent::TextDelta("partial".into()),
            StreamEvent::Finished {
                reason: FinishReason::Length,
            },
        ]]));
        let runtime = AgentRuntime::new(
            provider,
            memory.clone(),
            ToolRegistry::new(),
            AgentConfig::new("test-model"),
        );

        let error = runtime
            .run("conv", "go", &mut NoopEventSink)
            .await
            .expect_err("a cut-off response is not a completed answer");
        assert_eq!(error, AgentRunError::Truncated);

        // The partial text is still preserved in memory; truncation does not
        // discard what the model did produce.
        let history = memory.load("conv").await;
        assert!(
            history.iter().any(|message| matches!(
                message,
                ChatMessage::Assistant { content, .. } if content == "partial"
            )),
            "partial output is kept"
        );
    }
}
