use language_model::{Error, Usage};
use openai_compatible_types as wire;
use serde_json::{Map, Value};

pub(crate) fn reasoning(a: Option<String>, b: Option<String>) -> Result<Option<String>, Error> {
    match (a, b) {
        (Some(a), Some(b)) if a != b => Err(Error::Unsupported(
            "conflicting reasoning and reasoning_content fields".into(),
        )),
        (Some(a), _) | (_, Some(a)) => Ok(Some(a)),
        _ => Ok(None),
    }
}

pub(crate) fn usage(value: wire::Usage) -> Usage {
    Usage {
        input_tokens: value.prompt_tokens,
        output_tokens: value.completion_tokens,
        total_tokens: value.total_tokens,
        cached_input_tokens: value
            .prompt_tokens_details
            .as_ref()
            .and_then(|v| v.get("cached_tokens"))
            .and_then(Value::as_u64),
        reasoning_tokens: value
            .completion_tokens_details
            .as_ref()
            .and_then(|v| v.get("reasoning_tokens"))
            .and_then(Value::as_u64),
    }
}

pub(crate) fn reject_unsupported(extra: &Map<String, Value>) -> Result<(), Error> {
    if ["audio", "function_call", "images", "video"]
        .iter()
        .any(|key| extra.get(*key).is_some_and(|v| !v.is_null()))
    {
        return Err(Error::Unsupported(
            "response contains unsupported content".into(),
        ));
    }
    Ok(())
}
