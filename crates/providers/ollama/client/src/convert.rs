use language_model::{Error, ToolArguments};
use ollama_types as native;
use serde_json::json;

pub(crate) fn response_error(message: &str) -> Error {
    Error::InvalidResponse(message.into())
}

pub(crate) fn arguments(value: Option<native::ToolArguments>) -> ToolArguments {
    match value {
        Some(native::ToolArguments::Object(value)) => ToolArguments::Object { value },
        Some(native::ToolArguments::Invalid(value)) => ToolArguments::Invalid {
            raw: value.to_string(),
        },
        None => ToolArguments::Invalid { raw: String::new() },
    }
}

pub(crate) fn argument_text(arguments: &ToolArguments) -> String {
    match arguments {
        ToolArguments::Object { value } => json!(value).to_string(),
        ToolArguments::Invalid { raw } => raw.clone(),
    }
}
