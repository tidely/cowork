//! Opt-in checks of the agent runtime against a running Ollama server: a base
//! tool called directly, and a dynamic tool found with `search_tools` and run
//! through `call_tool`. Instructions are explicit because small local models
//! need them to be.
//!
//! cargo test -p agent --test live_agent -- --ignored --nocapture

use std::sync::Arc;

use agent::tool::{Tool, ToolCall, ToolError};
use agent::{Agent, AgentDefinition, DynamicToolChange, EventKind, RequestKind, ToolPermissions};
use agent::{
    Ollama,
    model::{LanguageModel, Thinking},
};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Numbers {
    a: i64,
    b: i64,
}

struct Add;

impl Tool for Add {
    type Args = Numbers;
    type Output = serde_json::Value;
    type Context = ();
    type Access = ();

    fn name(&self) -> &str {
        "add"
    }
    fn description(&self) -> String {
        "Add two integers a and b".into()
    }
    async fn run(&self, _: ToolCall<()>, _: &(), args: Numbers) -> Result<Self::Output, ToolError> {
        Ok(serde_json::json!({"sum": args.a + args.b}))
    }
}

struct Multiply;

impl Tool for Multiply {
    type Args = Numbers;
    type Output = serde_json::Value;
    type Context = ();
    type Access = ();

    fn name(&self) -> &str {
        "multiply"
    }
    fn description(&self) -> String {
        "Multiply two integers a and b".into()
    }
    async fn run(&self, _: ToolCall<()>, _: &(), args: Numbers) -> Result<Self::Output, ToolError> {
        Ok(serde_json::json!({"product": args.a * args.b}))
    }
}

#[tokio::test]
#[ignore = "requires a local Ollama server and installed model"]
async fn base_and_dynamic_tools_run_against_ollama() {
    let base = std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gemma4:e4b".into());
    let definition = AgentDefinition::new(LanguageModel::new(model))
        .instructions(
            "You do arithmetic only with tools, never in your head. \
             When asked for a number, reply with just that number.",
        )
        .thinking(Thinking::Disabled);
    let started = Arc::new(Mutex::new(Vec::new()));
    let observed = started.clone();
    let agent = Agent::builder(Ollama::new(&base).unwrap(), definition)
        .tool(Add)
        .unwrap()
        .permissions(ToolPermissions::allow_all())
        .on_event(move |event| {
            if let EventKind::ToolStarted { tool, .. } = event.kind {
                observed.lock().push(tool);
            }
        })
        .build()
        .await
        .unwrap();

    let reply = agent
        .prompt("Call the add tool with a=1234 and b=4321. Reply with only the sum.")
        .await
        .unwrap();
    println!("add: {}", reply.text());
    assert!(reply.text().contains("5555"), "{}", reply.text());

    agent
        .send(RequestKind::DynamicTools(DynamicToolChange::add(Multiply)))
        .await
        .unwrap();
    let reply = agent
        .prompt(
            "First call search_tools with the query \"multiply\". Then call call_tool with \
             name \"multiply\" and arguments {\"a\": 12, \"b\": 34}. Reply with only the product.",
        )
        .await
        .unwrap();
    println!("multiply: {}", reply.text());
    assert!(reply.text().contains("408"), "{}", reply.text());

    let started = started.lock().clone();
    println!("tools started: {started:?}");
    assert!(started.iter().any(|tool| tool == "add"));
    assert!(started.iter().any(|tool| tool == "multiply"));
    agent.close().await.unwrap();
}
