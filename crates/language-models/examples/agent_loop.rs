//! OLLAMA_MODEL=<installed-model> cargo run -p language-models --example agent_loop
//! The loop works with any LanguageModelProvider and ToolDispatch implementation.
use language_models::{
    CompletionRequest, Content, LanguageModel, LanguageModelProvider, Message, Ollama, Role,
    ToolArguments, ToolResult, ToolResultContent, collect_completion,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tool_runtime::{
    Tool, ToolCallContext, ToolCapabilities, ToolDispatch, ToolError, ToolOutput, ToolRegistry,
    collect_output,
};

type Error = Box<dyn std::error::Error>;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    a: i32,
    b: i32,
}

#[derive(Serialize)]
struct Sum {
    value: i64,
}

impl ToolOutput for Sum {}

struct Add;
impl Tool for Add {
    type Args = AddArgs;
    type Output = Sum;

    fn name(&self) -> &str {
        "add"
    }

    fn description(&self) -> String {
        "Add two integers".into()
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }

    async fn run(&self, _: ToolCallContext, args: AddArgs) -> Result<Sum, ToolError> {
        Ok(Sum {
            value: i64::from(args.a) + i64::from(args.b),
        })
    }
}

async fn agent_loop(
    provider: &dyn LanguageModelProvider,
    model: &LanguageModel,
    tools: &dyn ToolDispatch,
    mut history: Vec<Message>,
) -> Result<Message, Error> {
    loop {
        let request = CompletionRequest {
            messages: history.clone(),
            tools: tools.definitions(),
            max_output_tokens: Some(4096),
            ..Default::default()
        };
        // Deltas are previews. Dispatch only after a complete, accepted reply.
        let reply = collect_completion(provider.stream(model, request).await?)
            .await?
            .message;
        let reason = reply
            .generation
            .as_ref()
            .and_then(|meta| meta.finish_reason.as_deref());
        // These are the ordinary completion/tool-call reasons of our providers.
        // Stop before side effects on truncated, refused, or unknown outcomes.
        if !matches!(
            reason,
            Some("stop" | "tool_calls" | "end_turn" | "tool_use")
        ) {
            return Err(format!("model stopped with an unusable finish reason: {reason:?}").into());
        }
        let calls = reply
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .map(|call| match &call.arguments {
                ToolArguments::Object { value } => Ok((call.clone(), Value::Object(value.clone()))),
                // Some protocols cannot replay malformed tool input. Reject the
                // entire batch before executing any call instead of inventing args.
                ToolArguments::Invalid { .. } => {
                    Err(format!("invalid arguments for {}", call.name))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if calls.is_empty() {
            return Ok(reply);
        }

        // Preserve the complete assistant message, including native replay data.
        history.push(reply);
        for (call, args) in calls {
            let context = ToolCallContext::new(call.id.clone());
            // A permission/control wrapper can intercept this dispatch boundary.
            let result = collect_output(tools.call(&call.name, args, context).await).await;
            let (content, is_error) = match result {
                // output.output is also available here for application/UI logic.
                Ok(output) => (output.model_output, false),
                Err(error) => (
                    vec![ToolResultContent::Json {
                        value: serde_json::to_value(error)?,
                    }],
                    true,
                ),
            };
            history.push(Message::new(
                Role::Tool,
                vec![Content::ToolResult(ToolResult {
                    call: call.id,
                    provider_id: call.provider_id,
                    name: Some(call.name),
                    content,
                    is_error,
                })],
            ));
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let url = std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let provider = Ollama::new(&url)?;
    let model = LanguageModel::new(std::env::var("OLLAMA_MODEL")?);
    let mut tools = ToolRegistry::default();
    tools.register(Add)?;
    let reply = agent_loop(
        &provider,
        &model,
        &tools,
        vec![
            Message::system("Use the available tools for arithmetic."),
            Message::user("Use add to calculate 1234567 + 7654321, then tell me the result."),
        ],
    )
    .await?;

    for content in reply.content {
        if let Content::Text { text } = content {
            println!("{text}");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "support/agent_loop_tests.rs"]
mod tests;
