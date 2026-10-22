use language_model::{JsonObject, LanguageModel, Thinking};

use crate::tools::{CALL_TOOL, SEARCH_TOOLS};

/// The part of an agent whose change invalidates the prompt cache: the model,
/// the instructions, the tools offered, thinking, and provider options. Plain
/// data, shareable by many conversations; it holds no connection or tool
/// implementation (tool names resolve against the session's registry).
///
/// A definition is never patched: `ReplaceDefinition` swaps it whole, which
/// starts a new [`DefinitionRevision`]. Everything that doesn't change the
/// prompt is [`Configuration`](crate::Configuration).
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDefinition {
    pub model: LanguageModel,
    pub instructions: Option<String>,
    /// Tools offered in every request, in this order. Includes
    /// [`SEARCH_TOOLS`] and [`CALL_TOOL`] by default, which reach the
    /// session's dynamic tools; without `call_tool` there are none.
    pub tools: Vec<String>,
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
            tools: vec![SEARCH_TOOLS.into(), CALL_TOOL.into()],
            thinking: None,
            options: JsonObject::new(),
        }
    }

    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = Some(text.into());
        self
    }

    /// Adds a base tool, offered directly in every request.
    pub fn tool(mut self, name: impl Into<String>) -> Self {
        self.tools.push(name.into());
        self
    }

    pub fn thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Whether the session can hold dynamic tools, reached through `call_tool`.
    pub fn has_dynamic_tools(&self) -> bool {
        self.tools.iter().any(|name| name == CALL_TOOL)
    }
}

/// Counts definitions within one conversation; each step records the one it
/// used. Starts at 0 for the definition the session was built with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionRevision(pub u32);
