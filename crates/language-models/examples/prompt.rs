//! The simplest use of the agent runtime: define an agent, prompt it, and
//! read its replies. Needs a local Ollama server with `gemma4:e4b`.
//!
//! cargo run -p language-models --example prompt

use agent_runtime::{Agent, AgentDefinition};
use language_models::{LanguageModel, Ollama, Thinking};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The definition is what the prompt cache depends on: the model, the
    // instructions, the tools offered, and thinking.
    let definition = AgentDefinition::new(LanguageModel::new("gemma4:e4b"))
        .instructions("You are a concise assistant. Answer in one or two sentences.")
        .thinking(Thinking::Disabled);
    let agent = Agent::builder(Ollama::new("http://localhost:11434")?, definition)
        .build()
        .await?;

    for question in [
        "What is the capital of Denmark?",
        "Roughly how many people live there?",
    ] {
        println!("> {question}");
        println!("{}\n", agent.prompt(question).await?.text());
    }

    agent.close().await?;
    Ok(())
}
