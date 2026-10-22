use language_model::ToolResultContent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ToolError, ToolErrorKind};

/// The application's built-in result vocabulary. Named variants will let the UI
/// and other consumers match known results without interpreting arbitrary JSON
/// or scraping model-facing prose. No tool has a named variant yet, so only
/// the dynamic case exists; it also remains the escape hatch for custom and
/// runtime-discovered tools as named built-in variants are added.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum BuiltinToolOutput {
    Dynamic(Value),
}

/// A concrete tool result's structured and model-facing representations.
/// Implementations can customize either projection; the default retains the
/// serialized value and presents that same JSON to the model. Serialization is
/// ordinary runtime data and makes no versioned storage or wire-format promise.
pub trait ToolOutput: Serialize {
    /// `None` uses the structured result's default JSON projection. `Some`
    /// supplies explicit content and must contain at least one block.
    fn model_output(&self) -> Option<Vec<ToolResultContent>> {
        None
    }

    /// Built-ins can override this to retain a named enum variant. Custom
    /// outputs only need `impl ToolOutput for MyOutput {}`.
    fn into_output(self) -> Result<BuiltinToolOutput, serde_json::Error>
    where
        Self: Sized,
    {
        serde_json::to_value(self).map(BuiltinToolOutput::Dynamic)
    }
}

impl ToolOutput for Value {}
/// Text reaches the model as text, not as a JSON string.
impl ToolOutput for String {
    fn model_output(&self) -> Option<Vec<ToolResultContent>> {
        Some(vec![ToolResultContent::Text { text: self.clone() }])
    }
}

impl ToolOutput for BuiltinToolOutput {
    fn into_output(self) -> Result<BuiltinToolOutput, serde_json::Error> {
        Ok(self)
    }
}

/// The result retained after erasing a tool's concrete Rust type. Application
/// logic uses `output`; providers receive only `model_output`. Formatting or
/// adding host guidance to the latter must not overwrite structured results.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedToolOutput {
    pub tool_name: String,
    pub output: BuiltinToolOutput,
    pub model_output: Vec<ToolResultContent>,
}

impl TypedToolOutput {
    pub fn new<T: ToolOutput>(tool_name: impl Into<String>, output: T) -> Result<Self, ToolError> {
        // Extract custom content while the concrete type is still available.
        let model_output = output.model_output();
        let output = output.into_output().map_err(|error| {
            ToolError::new(
                ToolErrorKind::InvalidOutput,
                format!("could not serialize tool output: {error}"),
            )
        })?;
        let model_output = model_output.unwrap_or_else(|| match &output {
            BuiltinToolOutput::Dynamic(value) => {
                vec![ToolResultContent::Json {
                    value: value.clone(),
                }]
            }
        });
        if model_output.is_empty() {
            return Err(ToolError::new(
                ToolErrorKind::InvalidOutput,
                "tool output must contain model-facing content",
            ));
        }
        Ok(Self {
            tool_name: tool_name.into(),
            output,
            model_output,
        })
    }
}
