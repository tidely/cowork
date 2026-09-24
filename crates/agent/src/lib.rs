//! A small agent loop built directly on Rig's low-level completion stream.
//!
//! Unlike Rig's multi-turn agent stream, this crate surfaces a completed tool
//! call as soon as the provider closes that call's block. Tool execution still
//! waits for the complete model turn, keeping the assistant message and its tool
//! results in a valid history order.

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use rig::{
    completion::{AssistantContent, CompletionModel, CompletionResponse, Message, Usage},
    message::{ToolCall, UserContent},
    streaming::{StreamEvent, StreamingCompletionResponse},
    tool::{ToolContext, ToolResult, ToolSet},
};

/// An observation produced while an agent run is in progress.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// A normalized provider event. This includes text and reasoning deltas,
    /// block boundaries, and the terminal record.
    Model(StreamEvent),
    /// A complete tool call, emitted while the model turn is still streaming.
    ToolCall(ToolCall),
    /// The result of executing a tool after its model turn completed.
    ToolResult { call: ToolCall, result: ToolResult },
    /// A message was added to the history, exactly as later requests send
    /// it: each completed model turn, and each tool-result prompt before its
    /// request streams, so an interrupted run's history still ends with what
    /// the model was last asked. The initial prompt is not repeated here.
    HistoryAppended(Message),
    /// The tokens a completed model request used, as the provider reported
    /// them. A run makes one request per model turn, so summing these gives
    /// the run's usage even when a later request fails or is cancelled.
    Usage(Usage),
}

/// The completed response and the history assembled by the loop.
#[derive(Clone, Debug)]
pub struct AgentOutput {
    pub response: CompletionResponse,
    pub history: Vec<Message>,
}

/// A provider-independent, streaming tool loop.
pub struct Agent<M> {
    model: M,
    tools: ToolSet,
    preamble: Option<String>,
    additional_params: Option<serde_json::Value>,
}

impl<M> Agent<M> {
    pub fn new(model: M, tools: ToolSet) -> Self {
        Self {
            model,
            tools,
            preamble: None,
            additional_params: None,
        }
    }

    pub fn preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = Some(preamble.into());
        self
    }

    pub fn additional_params(mut self, params: serde_json::Value) -> Self {
        self.additional_params = Some(params);
        self
    }
}

impl<M> Agent<M>
where
    M: CompletionModel + Clone,
{
    /// Runs until the model produces a turn with no tool calls.
    ///
    /// `emit` is called synchronously as stream events arrive. It should do
    /// little work itself; forwarding events to a channel is a good default.
    ///
    /// Every message this adds to `history` after `prompt` is also emitted
    /// as [`AgentEvent::HistoryAppended`]. On failure, `history` ends with
    /// the prompt of the request that failed.
    pub async fn run(
        &self,
        mut prompt: Message,
        history: &mut Vec<Message>,
        mut emit: impl FnMut(AgentEvent),
    ) -> Result<CompletionResponse> {
        let mut initial = true;
        loop {
            let mut request = self
                .model
                .completion_request(prompt.clone())
                .messages(history.clone())
                .tools(self.tools.tool_definitions());

            if let Some(preamble) = &self.preamble {
                request = request.preamble(preamble.clone());
            }
            if let Some(params) = &self.additional_params {
                request = request.additional_params(params.clone());
            }
            history.push(prompt.clone());
            if !std::mem::take(&mut initial) {
                emit(AgentEvent::HistoryAppended(prompt));
            }

            let mut stream = request
                .stream()
                .await
                .context("failed to start model stream")?;
            let tool_calls = consume_turn(&mut stream, &mut emit).await?;
            let response = stream.finish();
            emit(AgentEvent::Usage(response.usage));

            let reply = Message::Assistant {
                id: response.message_id.clone(),
                content: response.choice.clone(),
            };
            history.push(reply.clone());
            emit(AgentEvent::HistoryAppended(reply));

            if tool_calls.is_empty() {
                return Ok(response);
            }

            let mut results = Vec::with_capacity(tool_calls.len());
            for call in tool_calls {
                let arguments = serde_json::to_string(&call.function.arguments)
                    .context("failed to serialize tool arguments")?;
                let result = self
                    .tools
                    .execute(&call.function.name, arguments, &mut ToolContext::new())
                    .await;

                results.push(UserContent::tool_result_for(
                    call.id.clone(),
                    call.provider.clone(),
                    call.function.name.clone(),
                    result.output().clone().into_content(),
                ));
                emit(AgentEvent::ToolResult { call, result });
            }

            prompt = Message::User { content: results };
        }
    }
}

async fn consume_turn(
    stream: &mut StreamingCompletionResponse,
    emit: &mut impl FnMut(AgentEvent),
) -> Result<Vec<ToolCall>> {
    let mut tool_calls = Vec::new();

    while let Some(event) = stream.next().await {
        let event = event.context("model stream failed")?;
        let completed_call = match &event {
            StreamEvent::BlockEnd {
                block: Some(AssistantContent::ToolCall(call)),
                ..
            } => Some(call.clone()),
            _ => None,
        };

        emit(AgentEvent::Model(event));
        if let Some(call) = completed_call {
            tool_calls.push(call.clone());
            emit(AgentEvent::ToolCall(call));
        }
    }

    Ok(tool_calls)
}
