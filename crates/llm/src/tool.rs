use std::{collections::HashMap, fmt, sync::Arc};

use futures::future::BoxFuture;
use schemars::{JsonSchema, Schema, generate::SchemaSettings, schema_for};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{ToolCall, ToolDefinition, ToolSchemaFormat};

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    fn parameters_schema(&self, format: ToolSchemaFormat) -> Result<Value, ToolError>;

    fn supports_argument_streaming(&self) -> bool {
        false
    }

    fn call(&self, arguments: Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>>;

    fn definition(&self, format: ToolSchemaFormat) -> Result<ToolDefinition, ToolError> {
        Ok(ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(format)?,
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
    tools: HashMap<String, Arc<dyn Tool>>,
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
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolError::DuplicateTool(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn definitions(&self, format: ToolSchemaFormat) -> Result<Vec<ToolDefinition>, ToolError> {
        self.tools
            .values()
            .map(|tool| tool.definition(format))
            .collect()
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

pub fn schema_for<T>(format: ToolSchemaFormat) -> Result<Value, ToolError>
where
    T: JsonSchema,
{
    let schema = root_schema_for::<T>(format);
    let mut value =
        serde_json::to_value(schema).map_err(|error| ToolError::Schema(error.to_string()))?;
    preprocess_schema(&mut value, format)?;
    Ok(value)
}

fn root_schema_for<T>(format: ToolSchemaFormat) -> Schema
where
    T: JsonSchema,
{
    match format {
        ToolSchemaFormat::JsonSchema => schema_for!(T),
        ToolSchemaFormat::JsonSchemaSubset => SchemaSettings::openapi3()
            .with(|settings| {
                settings.meta_schema = None;
                settings.inline_subschemas = true;
            })
            .into_generator()
            .root_schema_for::<T>(),
    }
}

fn preprocess_schema(value: &mut Value, format: ToolSchemaFormat) -> Result<(), ToolError> {
    if let Value::Object(object) = value {
        object.remove("$schema");
        object.remove("title");
    }

    match format {
        ToolSchemaFormat::JsonSchema => ensure_object_schema_defaults(value),
        ToolSchemaFormat::JsonSchemaSubset => strip_subset_unsupported_schema_keys(value),
    }
}

fn ensure_object_schema_defaults(value: &mut Value) -> Result<(), ToolError> {
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
    Ok(())
}

fn strip_subset_unsupported_schema_keys(value: &mut Value) -> Result<(), ToolError> {
    match value {
        Value::Object(object) => {
            for key in ["if", "then", "else", "$ref"] {
                if object.contains_key(key) {
                    return Err(ToolError::Schema(format!(
                        "schema subset cannot contain {key:?}"
                    )));
                }
            }
            for key in [
                "format",
                "additionalProperties",
                "propertyNames",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "optional",
            ] {
                object.remove(key);
            }
            if let Some(one_of) = object.remove("oneOf") {
                object.insert("anyOf".to_string(), one_of);
            }
            for nested in object.values_mut() {
                strip_subset_unsupported_schema_keys(nested)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_subset_unsupported_schema_keys(item)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}
