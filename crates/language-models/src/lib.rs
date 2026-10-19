//! Supported provider connections through one owned language model contract.
//!
//! [`LanguageModel`] describes a model. [`LanguageModelProvider`] owns the
//! connection that lists models and streams requests for a selected model.
//! [`reqwest`] is re-exported for configuring a custom HTTP client.
//!
//! ```no_run
//! use language_models::{CompletionRequest, Envelope, LanguageModel, LanguageModelProvider, Message, Ollama, collect_completion};
//!
//! # async fn example(model: String) -> Result<(), Box<dyn std::error::Error>> {
//! let provider = Ollama::new("http://localhost:11434")?;
//! let model = LanguageModel::new(model);
//! let request = CompletionRequest::new(vec![Message::user("Hello")]);
//! let stream = provider.stream(&model, request).await?;
//! let completion = collect_completion(stream).await?;
//! let stored = Envelope::encode_message(&completion.message)?;
//! # let _ = stored;
//! # Ok(())
//! # }
//! ```

pub use language_model::*;
/// A native Ollama server root, optionally under a proxy prefix.
pub use ollama::Client as Ollama;
/// A chat-completions API root, including `/v1` where required.
pub use openai_compatible::Client as OpenAiCompatible;
pub use reqwest;
