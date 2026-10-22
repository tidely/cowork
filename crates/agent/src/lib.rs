//! The agent stack in one crate: the session runtime at the root, the
//! provider-neutral model contract in [`model`], typed tools in [`tool`], and
//! the supported provider connections.
//!
//! [`model::LanguageModel`] describes a model; a provider such as [`Ollama`]
//! owns the connection that lists models and streams requests for one. An
//! [`AgentDefinition`] says what the prompt is made of, and an
//! [`AgentSession`] (or the managed [`Agent`]) runs one conversation on it.
//! [`reqwest`] is re-exported for configuring a custom HTTP client.
//!
//! ```no_run
//! use agent::{Agent, AgentDefinition, Ollama, model::LanguageModel};
//!
//! # async fn example(model: String) -> Result<(), Box<dyn std::error::Error>> {
//! let provider = Ollama::new("http://localhost:11434")?;
//! let definition = AgentDefinition::new(LanguageModel::new(model)).instructions("Be brief.");
//! let agent = Agent::builder(provider, definition).build().await?;
//! println!("{}", agent.prompt("Hello").await?.text());
//! agent.close().await?;
//! # Ok(())
//! # }
//! ```

pub use agent_runtime::*;

/// Owned conversation data, versioned message encoding, and the provider
/// contract (`language-model`).
pub mod model {
    pub use language_model::*;
}

/// Typed tools, their contexts and per-call access, and registries
/// (`tool-runtime`).
pub mod tool {
    pub use tool_runtime::*;
}

/// A native Ollama server root, optionally under a proxy prefix.
pub use ollama::Client as Ollama;
/// A chat-completions API root, including `/v1` where required.
pub use openai_compatible::Client as OpenAiCompatible;
pub use reqwest;

/// An Anthropic Messages API root, including `/v1`.
pub use anthropic::Client as Anthropic;
