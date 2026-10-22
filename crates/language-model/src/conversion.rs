//! Explicit conversions keep schema evolution out of Serde and provider code.

use crate::{latest, v1};

macro_rules! field {
    ($value:expr) => {
        $value
    };
    ($value:expr, into) => {
        $value.into()
    };
    ($value:expr, optional) => {
        $value.map(Into::into)
    };
    ($value:expr, many) => {
        $value.into_iter().map(Into::into).collect()
    };
}

// Matching fields still require an explicit entry here. Adding a runtime field
// cannot accidentally alter the frozen serialized shape.
macro_rules! matching_struct {
    ($name:ident { $($field:ident $(=> $conversion:ident)?),* $(,)? }) => {
        impl From<v1::$name> for latest::$name {
            fn from(value: v1::$name) -> Self {
                Self { $($field: field!(value.$field $(, $conversion)?)),* }
            }
        }

        impl From<latest::$name> for v1::$name {
            fn from(value: latest::$name) -> Self {
                Self { $($field: field!(value.$field $(, $conversion)?)),* }
            }
        }
    };
}

matching_struct!(Message {
    id,
    role => into,
    content => many,
    generation => optional,
    replay => many,
});
matching_struct!(Image { media_type, source => into });
matching_struct!(ToolCall { id, provider_id, name, arguments => into });
matching_struct!(ToolResult { call, provider_id, name, content => many, is_error });
matching_struct!(Usage {
    input_tokens,
    output_tokens,
    total_tokens,
    cached_input_tokens,
    reasoning_tokens
});
matching_struct!(GenerationMetadata { provider, model, finish_reason => optional, usage => optional, extra });

impl From<v1::FinishReason> for latest::FinishReason {
    fn from(value: v1::FinishReason) -> Self {
        match value {
            v1::FinishReason::Stop => Self::Stop,
            v1::FinishReason::ToolCalls => Self::ToolCalls,
            v1::FinishReason::Length => Self::Length,
            v1::FinishReason::ContentFilter => Self::ContentFilter,
            v1::FinishReason::Refusal => Self::Refusal,
            v1::FinishReason::Other(reason) => Self::Other(reason),
        }
    }
}

impl From<latest::FinishReason> for v1::FinishReason {
    fn from(value: latest::FinishReason) -> Self {
        match value {
            latest::FinishReason::Stop => Self::Stop,
            latest::FinishReason::ToolCalls => Self::ToolCalls,
            latest::FinishReason::Length => Self::Length,
            latest::FinishReason::ContentFilter => Self::ContentFilter,
            latest::FinishReason::Refusal => Self::Refusal,
            latest::FinishReason::Other(reason) => Self::Other(reason),
        }
    }
}
matching_struct!(ReplayData {
    provider,
    model,
    format,
    version,
    payload
});

impl From<v1::Role> for latest::Role {
    fn from(value: v1::Role) -> Self {
        match value {
            v1::Role::System => Self::System,
            v1::Role::Developer => Self::Developer,
            v1::Role::User => Self::User,
            v1::Role::Assistant => Self::Assistant,
            v1::Role::Tool => Self::Tool,
        }
    }
}

impl From<latest::Role> for v1::Role {
    fn from(value: latest::Role) -> Self {
        match value {
            latest::Role::System => Self::System,
            latest::Role::Developer => Self::Developer,
            latest::Role::User => Self::User,
            latest::Role::Assistant => Self::Assistant,
            latest::Role::Tool => Self::Tool,
        }
    }
}

impl From<v1::Content> for latest::Content {
    fn from(value: v1::Content) -> Self {
        match value {
            v1::Content::Text { text } => Self::Text { text },
            v1::Content::Reasoning { reasoning } => Self::Reasoning(reasoning.into()),
            v1::Content::Image { image } => Self::Image(image.into()),
            v1::Content::ToolCall { call } => Self::ToolCall(call.into()),
            v1::Content::ToolResult { result } => Self::ToolResult(result.into()),
        }
    }
}

impl From<latest::Content> for v1::Content {
    fn from(value: latest::Content) -> Self {
        match value {
            latest::Content::Text { text } => Self::Text { text },
            latest::Content::Reasoning(reasoning) => Self::Reasoning {
                reasoning: reasoning.into(),
            },
            latest::Content::Image(image) => Self::Image {
                image: image.into(),
            },
            latest::Content::ToolCall(call) => Self::ToolCall { call: call.into() },
            latest::Content::ToolResult(result) => Self::ToolResult {
                result: result.into(),
            },
        }
    }
}

impl From<v1::Reasoning> for latest::Reasoning {
    fn from(value: v1::Reasoning) -> Self {
        match value {
            v1::Reasoning::Text { text } => Self::Text { text },
            v1::Reasoning::Redacted { data } => Self::Redacted { data },
        }
    }
}

impl From<latest::Reasoning> for v1::Reasoning {
    fn from(value: latest::Reasoning) -> Self {
        match value {
            latest::Reasoning::Text { text } => Self::Text { text },
            latest::Reasoning::Redacted { data } => Self::Redacted { data },
        }
    }
}

impl From<v1::ImageSource> for latest::ImageSource {
    fn from(value: v1::ImageSource) -> Self {
        match value {
            v1::ImageSource::Base64 { data } => Self::Base64 { data },
            v1::ImageSource::Url { url } => Self::Url { url },
        }
    }
}

impl From<latest::ImageSource> for v1::ImageSource {
    fn from(value: latest::ImageSource) -> Self {
        match value {
            latest::ImageSource::Base64 { data } => Self::Base64 { data },
            latest::ImageSource::Url { url } => Self::Url { url },
        }
    }
}

impl From<v1::ToolArguments> for latest::ToolArguments {
    fn from(value: v1::ToolArguments) -> Self {
        match value {
            v1::ToolArguments::Object { value } => Self::Object { value },
            v1::ToolArguments::Invalid { raw } => Self::Invalid { raw },
        }
    }
}

impl From<latest::ToolArguments> for v1::ToolArguments {
    fn from(value: latest::ToolArguments) -> Self {
        match value {
            latest::ToolArguments::Object { value } => Self::Object { value },
            latest::ToolArguments::Invalid { raw } => Self::Invalid { raw },
        }
    }
}

impl From<v1::ToolResultContent> for latest::ToolResultContent {
    fn from(value: v1::ToolResultContent) -> Self {
        match value {
            v1::ToolResultContent::Text { text } => Self::Text { text },
            v1::ToolResultContent::Json { value } => Self::Json { value },
            v1::ToolResultContent::Image { image } => Self::Image(image.into()),
        }
    }
}

impl From<latest::ToolResultContent> for v1::ToolResultContent {
    fn from(value: latest::ToolResultContent) -> Self {
        match value {
            latest::ToolResultContent::Text { text } => Self::Text { text },
            latest::ToolResultContent::Json { value } => Self::Json { value },
            latest::ToolResultContent::Image(image) => Self::Image {
                image: image.into(),
            },
        }
    }
}
