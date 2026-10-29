use language_model::{JsonObject, LanguageModel, Thinking};

/// The part of an agent that can change within a session, at the cost of the
/// prompt cache: the model, the instructions, thinking, and provider options.
/// Plain data, shareable by many conversations; it holds no connection.
///
/// Tools are not part of it. A session's tools are the ones registered when
/// it is built, offered in registration order for its whole life, ahead of
/// the instructions in every request; see [`AgentSession::new`]. A definition
/// shared between programs (say, a saved agent profile) would carry a
/// selection of tools resolved against the receiver's own implementations,
/// which supply the schemas. Definitions have no compatibility guarantees yet.
///
/// A definition is never patched: `ReplaceDefinition` swaps it whole, which
/// starts a new [`DefinitionRevision`]. Everything that doesn't change the
/// prompt is [`Configuration`](crate::Configuration).
///
/// [`AgentSession::new`]: crate::AgentSession::new
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDefinition {
    pub model: LanguageModel,
    pub instructions: Option<String>,
    pub thinking: Option<Thinking>,
    /// Provider-specific request options; these can change the prompt or
    /// reload the model (Ollama's `num_ctx`).
    pub options: JsonObject,
}

impl AgentDefinition {
    pub fn new(model: LanguageModel) -> Self {
        Self {
            model,
            instructions: None,
            thinking: None,
            options: JsonObject::new(),
        }
    }

    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = Some(text.into());
        self
    }

    pub fn thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = Some(thinking);
        self
    }
}

/// Counts definitions within one conversation; each step records the one it
/// used. Starts at 0 for the definition the session was built with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionRevision(pub u32);
