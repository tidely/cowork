//! The tools one step offers: the session's base tools, plus dynamic tools
//! reached through `search_tools` and `call_tool`. Appending search results to
//! the conversation never changes the request's `tools`, so discovering tools
//! keeps the prompt cache with every provider.

use std::sync::Arc;

use language_model::{ToolArguments, ToolCall, ToolDefinition};
use serde::Deserialize;
use serde_json::{Value, json};
use tool_runtime::{
    RegisteredTool, Tool, ToolDescriptor, ToolDyn, ToolError, ToolErrorKind, ToolRegistry,
    TypedToolOutput,
};

use crate::AgentHost;

pub const SEARCH_TOOLS: &str = "search_tools";
pub const CALL_TOOL: &str = "call_tool";
/// Names the runtime implements itself; no registered tool may use them.
pub const BUILTIN_TOOLS: [&str; 2] = [SEARCH_TOOLS, CALL_TOOL];

/// How many matches one search returns.
const SEARCH_LIMIT: usize = 8;

/// A session's registered tools, for its host's context and access types.
pub type HostRegistry<H> = ToolRegistry<H, <H as AgentHost>::Access>;
pub type HostTool<H> = RegisteredTool<H, <H as AgentHost>::Access>;

/// Adds or removes a dynamic tool. Additions only ever append to the
/// conversation; base tools are fixed for the session's life.
pub enum DynamicToolChange<H: AgentHost> {
    Add(Arc<dyn ToolDyn<H, H::Access>>),
    Remove(String),
}

impl<H: AgentHost> DynamicToolChange<H> {
    pub fn add<T>(tool: T) -> Self
    where
        T: Tool,
        H: AsRef<T::Context>,
        H::Access: From<T::Access>,
    {
        Self::Add(Arc::new(tool))
    }
}

impl<H: AgentHost> std::fmt::Debug for DynamicToolChange<H> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add(tool) => formatter
                .debug_tuple("Add")
                .field(&tool.definition().name)
                .finish(),
            Self::Remove(name) => formatter.debug_tuple("Remove").field(name).finish(),
        }
    }
}

/// A call resolved to what will actually run. Short-lived: one per call.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Resolved<H: AgentHost> {
    Search { query: String },
    Tool { tool: HostTool<H>, args: Value },
}

/// The tools of one step, frozen when its request is assembled. Calls in the
/// step's reply run against this snapshot, even if tools change meanwhile.
pub(crate) struct ToolSnapshot<H: AgentHost> {
    /// Offered tools in request order, with the built-ins as `None`.
    offered: Vec<(String, Option<HostTool<H>>)>,
    dynamic: HostRegistry<H>,
}

impl<H: AgentHost> ToolSnapshot<H> {
    /// The built-ins first, when the session has dynamic tools, then the
    /// base tools in registration order.
    pub(crate) fn new(
        base: &HostRegistry<H>,
        dynamic_tools: bool,
        dynamic: HostRegistry<H>,
    ) -> Self {
        let builtins = BUILTIN_TOOLS
            .iter()
            .filter(|_| dynamic_tools)
            .map(|name| ((*name).to_owned(), None));
        let tools = base
            .iter()
            .map(|tool| (tool.descriptor.name().to_owned(), Some(tool.clone())));
        Self {
            offered: builtins.chain(tools).collect(),
            dynamic,
        }
    }

    fn offers(&self, name: &str) -> bool {
        self.offered.iter().any(|(offered, _)| offered == name)
    }

    /// The request's `tools`, in the order they are offered.
    pub(crate) fn definitions(&self) -> Vec<ToolDefinition> {
        self.offered
            .iter()
            .map(|(name, tool)| match tool {
                Some(tool) => tool.descriptor.definition.clone(),
                None if name == SEARCH_TOOLS => search_tools_definition(),
                None => call_tool_definition(),
            })
            .collect()
    }

    /// Base tools offered directly, without the built-ins.
    pub(crate) fn base_descriptors(&self) -> Vec<ToolDescriptor> {
        self.offered
            .iter()
            .filter_map(|(_, tool)| tool.as_ref().map(|tool| tool.descriptor.clone()))
            .collect()
    }

    pub(crate) fn dynamic_descriptors(&self) -> Vec<ToolDescriptor> {
        if self.offers(CALL_TOOL) {
            self.dynamic.descriptors().cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// What a call runs. `call_tool` resolves to the tool it names, so
    /// authorization, events, and output all see the real tool.
    pub(crate) fn resolve(&self, call: &ToolCall) -> Result<Resolved<H>, ToolError> {
        let ToolArguments::Object { value } = &call.arguments else {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "tool arguments must be a JSON object",
            ));
        };
        let args = Value::Object(value.clone());
        if !self.offers(&call.name) {
            return Err(ToolError::new(
                ToolErrorKind::NotFound,
                format!("unknown tool {}", call.name),
            ));
        }
        match call.name.as_str() {
            SEARCH_TOOLS => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Search {
                    query: String,
                }
                let search: Search = serde_json::from_value(args).map_err(invalid)?;
                Ok(Resolved::Search {
                    query: search.query,
                })
            }
            CALL_TOOL => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Call {
                    name: String,
                    #[serde(default)]
                    arguments: serde_json::Map<String, Value>,
                }
                let inner: Call = serde_json::from_value(args).map_err(invalid)?;
                if self.offers(&inner.name) {
                    return Err(ToolError::new(
                        ToolErrorKind::InvalidArguments,
                        format!("{} is offered directly; call it directly", inner.name),
                    ));
                }
                let tool = self.dynamic.get(&inner.name).cloned().ok_or_else(|| {
                    ToolError::new(
                        ToolErrorKind::NotFound,
                        format!(
                            "no tool named {}; use {SEARCH_TOOLS} to find available tools",
                            inner.name
                        ),
                    )
                })?;
                Ok(Resolved::Tool {
                    tool,
                    args: Value::Object(inner.arguments),
                })
            }
            name => Ok(Resolved::Tool {
                tool: self
                    .offered
                    .iter()
                    .find_map(|(offered, tool)| (offered == name).then(|| tool.clone()))
                    .flatten()
                    .expect("offered non-builtin tools are resolved"),
                args,
            }),
        }
    }

    /// Dynamic tools matching any word of the query in their name or
    /// description, best matches first, with their full parameter schemas.
    pub(crate) fn search(&self, query: &str) -> Result<TypedToolOutput, ToolError> {
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        let mut matches: Vec<(usize, &ToolDescriptor)> = self
            .dynamic
            .descriptors()
            .map(|descriptor| {
                let text = format!(
                    "{} {}",
                    descriptor.name().replace(['_', '-'], " "),
                    descriptor
                        .definition
                        .description
                        .as_deref()
                        .unwrap_or_default()
                )
                .to_lowercase();
                let score = words
                    .iter()
                    .filter(|word| text.contains(word.as_str()))
                    .count();
                (score, descriptor)
            })
            .filter(|(score, _)| *score > 0 || words.is_empty())
            .collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name().cmp(b.1.name())));
        let tools: Vec<_> = matches
            .into_iter()
            .take(SEARCH_LIMIT)
            .map(|(_, descriptor)| {
                json!({
                    "name": descriptor.definition.name,
                    "description": descriptor.definition.description,
                    "parameters": descriptor.definition.parameters,
                })
            })
            .collect();
        let result = if tools.is_empty() {
            json!({"tools": [], "note": "No tools match. Try other words."})
        } else {
            json!({"tools": tools, "note": format!("Call these with {CALL_TOOL}.")})
        };
        TypedToolOutput::new(SEARCH_TOOLS, result)
    }
}

fn invalid(error: serde_json::Error) -> ToolError {
    ToolError::new(ToolErrorKind::InvalidArguments, error.to_string())
}

fn schema(value: Value) -> serde_json::Map<String, Value> {
    let Value::Object(map) = value else {
        unreachable!("schemas are objects")
    };
    map
}

fn search_tools_definition() -> ToolDefinition {
    ToolDefinition {
        name: SEARCH_TOOLS.into(),
        description: Some(format!(
            "Search for tools that are not offered directly. Returns each match's name, \
             description, and parameter schema; call a match with {CALL_TOOL}."
        )),
        parameters: schema(json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words describing what the tool should do"}
            },
            "required": ["query"],
            "additionalProperties": false
        })),
    }
}

fn call_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CALL_TOOL.into(),
        description: Some(format!(
            "Call a tool found with {SEARCH_TOOLS}. Pass its exact name, and arguments \
             matching its parameter schema. Tools offered directly are called directly."
        )),
        parameters: schema(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The tool's exact name"},
                "arguments": {"type": "object", "description": "The tool's arguments"}
            },
            "required": ["name", "arguments"],
            "additionalProperties": false
        })),
    }
}
