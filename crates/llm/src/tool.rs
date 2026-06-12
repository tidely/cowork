use std::{borrow::Cow, collections::BTreeMap, fmt, sync::Arc};

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{ToolCall, ToolDefinition};

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> Cow<'static, str>;

    fn description(&self) -> Cow<'static, str>;

    fn parameters_schema(&self) -> Result<Value, ToolError>;

    fn supports_argument_streaming(&self) -> bool {
        false
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput, ToolError>;

    fn definition(&self) -> Result<ToolDefinition, ToolError> {
        Ok(ToolDefinition {
            name: self.name().into_owned(),
            description: self.description().into_owned(),
            parameters: self.parameters_schema()?,
            supports_argument_streaming: self.supports_argument_streaming(),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub raw: Option<Value>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            raw: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            raw: None,
        }
    }

    pub fn with_raw(mut self, raw: Value) -> Self {
        self.raw = Some(raw);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    InvalidArguments(String),
    PermissionDenied(String),
    Execution(String),
    UnknownTool(String),
    DuplicateTool(String),
    Schema(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArguments(message) => write!(f, "invalid tool arguments: {message}"),
            Self::PermissionDenied(message) => write!(f, "tool permission denied: {message}"),
            Self::Execution(message) => write!(f, "tool execution failed: {message}"),
            Self::UnknownTool(name) => write!(f, "unknown tool: {name}"),
            Self::DuplicateTool(name) => write!(f, "duplicate tool registered: {name}"),
            Self::Schema(message) => write!(f, "tool schema error: {message}"),
        }
    }
}

impl std::error::Error for ToolError {}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    /// `BTreeMap` so `definitions` yields tools in a stable, name-sorted order;
    /// the model otherwise sees a different tool ordering on every turn.
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T>(&mut self, tool: T) -> Result<(), ToolError>
    where
        T: Tool + 'static,
    {
        self.insert_arc(Arc::new(tool))
    }

    pub fn insert_arc(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.name().into_owned();
        if self.tools.contains_key(&name) {
            return Err(ToolError::DuplicateTool(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn definitions(&self) -> Result<Vec<ToolDefinition>, ToolError> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Tool>, ToolError> {
        self.tools
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub async fn call(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        self.get(&call.name)?.call(call.arguments.clone()).await
    }
}

pub fn parse_args<T>(arguments: Value) -> Result<T, ToolError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(arguments)
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

pub fn schema_for<T>() -> Result<Value, ToolError>
where
    T: JsonSchema,
{
    let mut value = serde_json::to_value(schema_for!(T))
        .map_err(|error| ToolError::Schema(error.to_string()))?;
    if let Value::Object(object) = &mut value {
        object.remove("$schema");
        object.remove("title");
    }
    ensure_object_schema_defaults(&mut value);
    Ok(value)
}

fn ensure_object_schema_defaults(value: &mut Value) {
    if let Value::Object(object) = value
        && matches!(object.get("type"), Some(Value::String(kind)) if kind == "object")
    {
        object
            .entry("additionalProperties")
            .or_insert(Value::Bool(false));
        object
            .entry("properties")
            .or_insert(Value::Object(Default::default()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A tool whose name is whatever it was constructed with, so a registry can
    /// be populated out of alphabetical order.
    struct NamedTool {
        name: Cow<'static, str>,
        description: Cow<'static, str>,
    }

    impl NamedTool {
        fn borrowed(name: &'static str) -> Self {
            Self {
                name: Cow::Borrowed(name),
                description: Cow::Borrowed("test tool"),
            }
        }
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> Cow<'static, str> {
            self.name.clone()
        }

        fn description(&self) -> Cow<'static, str> {
            self.description.clone()
        }

        fn parameters_schema(&self) -> Result<Value, ToolError> {
            Ok(json!({ "type": "object", "properties": {} }))
        }

        async fn call(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    #[test]
    fn definitions_are_returned_in_stable_name_order() {
        let mut registry = ToolRegistry::new();
        // Insert out of order; the registry must still emit them name-sorted so
        // the model sees a deterministic tool list every turn.
        for name in ["write_file", "edit_file", "read_file", "list_directory"] {
            registry
                .insert(NamedTool::borrowed(name))
                .expect("unique tool");
        }

        let names: Vec<String> = registry
            .definitions()
            .expect("definitions build")
            .into_iter()
            .map(|definition| definition.name)
            .collect();

        assert_eq!(
            names,
            ["edit_file", "list_directory", "read_file", "write_file"]
        );
    }

    #[test]
    fn definitions_accept_owned_tool_metadata() {
        let name = String::from("dynamic_tool");
        let description = format!("generated description for {name}");
        let mut registry = ToolRegistry::new();

        registry
            .insert(NamedTool {
                name: Cow::Owned(name),
                description: Cow::Owned(description.clone()),
            })
            .expect("unique tool");

        let definitions = registry.definitions().expect("definitions build");

        assert_eq!(definitions[0].name, "dynamic_tool");
        assert_eq!(definitions[0].description, description);
    }
}
