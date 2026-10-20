//! cargo run -p language-models --example agent_session
use agent_runtime::{Agent, AgentDefinition, ToolPermissions};
use language_models::{LanguageModel, Ollama, Thinking};
use schemars::JsonSchema;
use serde::Deserialize;
use tool_runtime::{Tool, ToolCallContext, ToolCapabilities, ToolError};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    a: i32,
    b: i32,
}

struct Add;
impl Tool for Add {
    type Args = AddArgs;
    type Output = serde_json::Value;

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
    async fn run(&self, _: ToolCallContext, args: AddArgs) -> Result<Self::Output, ToolError> {
        Ok(serde_json::json!({"value": i64::from(args.a) + i64::from(args.b)}))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let definition = AgentDefinition {
        name: "arithmetic".into(),
        description: "Answers arithmetic questions using add".into(),
        system_prompt: "Use tools for arithmetic.".into(),
        tools: Some(vec!["add".into()]),
        permissions: ToolPermissions::AllowListed(vec!["add".into()]),
        thinking: Some(Thinking::Disabled),
        ..Default::default()
    };
    let agent = Agent::builder(
        Ollama::new("http://localhost:11434")?,
        LanguageModel::new("qwen3.8:27b"),
    )
    .definition(&definition)
    .tool(Add)?
    .open_or_create("/tmp/cowork-agent-session")
    .build()
    .await?;

    println!(
        "{}",
        agent
            .prompt("Use add to calculate 1234567 + 7654321, then tell me the result.")
            .await?
            .text()
    );
    agent.compact().await?;
    println!(
        "{}",
        agent
            .prompt("What was the result we calculated? Use the earlier context.")
            .await?
            .text()
    );
    agent.close().await?;
    Ok(())
}
