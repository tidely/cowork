//! Owned conversation data and the boundary implemented by provider adapters.
//!
//! [`latest`] is an in-memory API, not a storage schema. Persist messages through
//! [`Envelope::encode_message`]; [`v1`] is the first frozen wire schema. Neither
//! the runtime, a provider, nor an HTTP client defines that schema.

mod conversion;
pub mod latest;
pub mod v1;

use std::time::Duration;

use futures::{StreamExt as _, future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use latest::{
    Content, FinishReason, GenerationMetadata, Image, ImageSource, Message, Reasoning, ReplayData,
    Role, ToolArguments, ToolCall, ToolResult, ToolResultContent, Usage,
};

pub type JsonObject = Map<String, Value>;

/// A schema's permanent identifier includes its explicit version. Runtime
/// `latest` types deliberately do not implement this trait or Serde.
pub trait VersionedType {
    const TYPE_ID: &'static str;
}

macro_rules! local_id {
    ($name:ident) => {
        /// An opaque Cowork identity, independent of any provider-issued ID.
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

local_id!(MessageId);
local_id!(ToolCallId);

/// The envelope can be retained even when this executable cannot interpret its
/// payload. Decode borrows it, so unsupported records can still be exported.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub type_id: String,
    pub payload: Value,
}

impl Envelope {
    pub fn encode_message(message: &Message) -> Result<Self, SchemaError> {
        message.validate().map_err(SchemaError::InvalidMessage)?;
        Ok(Self {
            type_id: v1::Message::TYPE_ID.into(),
            payload: serde_json::to_value(v1::Message::from(message.clone()))?,
        })
    }

    /// Dispatch explicitly by stored identifier. A future version adds its own
    /// decoder and `From<old::Message> for latest::Message`; it never changes v1.
    pub fn decode_message(&self) -> Result<Message, SchemaError> {
        let message = match self.type_id.as_str() {
            v1::Message::TYPE_ID => Message::from(v1::Message::deserialize(&self.payload)?),
            _ => return Err(SchemaError::UnsupportedType(self.type_id.clone())),
        };
        message.validate().map_err(SchemaError::InvalidMessage)?;
        Ok(message)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("unsupported persisted type: {0}")]
    UnsupportedType(String),
    #[error("invalid persisted payload: {0}")]
    InvalidPayload(#[from] serde_json::Error),
    #[error("invalid persisted message: {0}")]
    InvalidMessage(Error),
}

/// The contents of one generation request, independent of model selection.
/// Owned input allows providers to finish I/O without borrowing UI entities.
/// A request is assembled once and consumed by one provider call; it is flat
/// plain data, not state anyone keeps editing.
///
/// Typed controls are the ones every supported provider adopts; `None` (or an
/// empty `stop`) preserves provider defaults. `options` contains
/// adapter-specific request controls, never credentials. Supplying a typed
/// control and its raw wire option together is an error.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompletionRequest {
    /// The system prompt. Its own field, rather than a leading `System`
    /// message, so instructions in the middle of a conversation cannot be
    /// expressed; `messages` never contain `System` messages.
    pub instructions: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Where the stable prompt prefix ends, in request order. Adapters with
    /// explicit cache markers place them here (keeping as many as they allow,
    /// latest first); providers that cache automatically ignore them.
    pub cache: Vec<CachePoint>,
    /// Explicit controls must be mapped or rejected, never silently replaced
    /// with a different effort.
    pub thinking: Option<Thinking>,
    /// Upper bound on generated tokens, reasoning included where the provider
    /// counts it. Not the context window.
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Sequences that end generation; they are not part of the reply.
    pub stop: Vec<String>,
    /// Best-effort reproducibility; providers don't guarantee determinism.
    pub seed: Option<i64>,
    pub options: JsonObject,
}

impl CompletionRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self
            .instructions
            .as_ref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err(Error::InvalidRequest(
                "instructions must not be empty; omit them instead".into(),
            ));
        }
        if self
            .messages
            .iter()
            .any(|message| message.role == Role::System)
        {
            return Err(Error::InvalidRequest(
                "system messages are not allowed; use instructions".into(),
            ));
        }
        for point in &self.cache {
            if let CachePoint::AfterMessage(id) = point
                && !self.messages.iter().any(|message| &message.id == id)
            {
                return Err(Error::InvalidRequest(
                    "cache point names a message that is not in the request".into(),
                ));
            }
        }
        if let Some(Thinking::Effort(effort)) = &self.thinking {
            validate_effort_name(effort)?;
        }
        if self.max_output_tokens == Some(0) {
            return Err(Error::InvalidRequest(
                "max_output_tokens must be positive".into(),
            ));
        }
        if self
            .temperature
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err(Error::InvalidRequest(
                "temperature must be finite and non-negative".into(),
            ));
        }
        if self
            .top_p
            .is_some_and(|value| !(0.0..=1.0).contains(&value))
        {
            return Err(Error::InvalidRequest(
                "top_p must be between 0 and 1".into(),
            ));
        }
        if self.stop.iter().any(String::is_empty) {
            return Err(Error::InvalidRequest(
                "stop sequences must not be empty".into(),
            ));
        }
        for message in &self.messages {
            message.validate()?;
        }
        let mut names = std::collections::HashSet::with_capacity(self.tools.len());
        for tool in &self.tools {
            if tool.name.trim().is_empty() || !names.insert(&tool.name) {
                return Err(Error::InvalidRequest(
                    "tool definitions need unique nonempty names".into(),
                ));
            }
        }
        Ok(())
    }
}

/// The end of a stable request prefix, as the runtime knows it. Each adapter
/// decides how (and whether) its provider can use the hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CachePoint {
    AfterTools,
    AfterInstructions,
    AfterMessage(MessageId),
}

/// Shared thinking intent; effort values are opaque model-defined strings,
/// rather than a universal low/medium/high scale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Thinking {
    Enabled,
    Disabled,
    Effort(String),
}

/// One selectable effort supplied by model configuration or discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingLevel {
    pub value: String,
    pub label: String,
}

/// Optional thinking metadata. Unknown values stay absent; an explicitly empty
/// effort list means there are no named levels (binary controls may still work).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThinkingCapabilities {
    pub effort_levels: Option<Vec<ThinkingLevel>>,
    /// Effort to use when enabling thinking on an endpoint that requires a level.
    pub default_effort: Option<String>,
    /// Explicitly known effort value that disables thinking on this model's
    /// endpoint. It need not belong to the selectable enabled-level list.
    /// Absence does not authorize assuming a value such as "none".
    pub disabled_effort: Option<String>,
}

impl ThinkingCapabilities {
    /// Validate against advertised levels when available. Unknown capabilities
    /// allow an explicit level through for the provider/server to validate.
    pub fn validate_effort(&self, effort: &str) -> Result<(), Error> {
        validate_effort_name(effort)?;
        if self
            .effort_levels
            .as_ref()
            .is_some_and(|levels| !levels.iter().any(|level| level.value == effort))
        {
            return Err(Error::Unsupported(format!(
                "thinking effort {effort:?} is not supported by the selected model"
            )));
        }
        Ok(())
    }
}

fn validate_effort_name(effort: &str) -> Result<(), Error> {
    if effort.trim().is_empty() {
        return Err(Error::InvalidRequest(
            "thinking effort must not be empty".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    /// Full JSON Schema, not a reduced map of primitive parameter types.
    pub parameters: JsonObject,
}

/// The single authoritative result of one model request. A length stop is a
/// completed request too; agent policy decides whether its calls may execute.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    pub message: Message,
}

/// Observations are provisional. Only `Finished` supplies accepted content;
/// callers must not construct it by treating the last delta as a final reply.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Delta { index: usize, delta: ContentDelta },
    // One allocation per request keeps every frequent delta compact even when
    // workspace feature unification changes the size of metadata JSON maps.
    Finished(Box<Completion>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContentDelta {
    Text(String),
    Reasoning(String),
    /// Argument fragments may not yet be valid JSON. `id` remains the same for
    /// every fragment of this call and its authoritative finalized call.
    ToolCall {
        id: ToolCallId,
        name: Option<String>,
        arguments: String,
    },
}

pub type CompletionStream = BoxStream<'static, Result<StreamEvent, Error>>;

/// Information about a model offered by a provider. The ID is scoped to that
/// provider's connection; this value contains no transport or credentials.
/// The same weights served elsewhere can report different values.
///
/// Typed fields are the facts code above the provider acts on. Each is
/// `None` when unknown, which is not the same as unsupported: unknown values
/// leave the decision to the provider. How a provider's protocol expresses
/// things (roles, cache markers, finish strings) is not model metadata; it
/// stays inside the adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct LanguageModel {
    pub id: String,
    pub name: Option<String>,
    /// Input tokens the model can attend to; drives compaction triggers.
    pub context_window: Option<u64>,
    /// The largest output the model can generate in one reply.
    pub max_output_tokens: Option<u64>,
    /// Whether the model accepts tool definitions and emits tool calls.
    pub tool_calls: Option<bool>,
    /// Whether the model accepts images as input.
    pub image_input: Option<bool>,
    pub thinking: ThinkingCapabilities,
    /// Everything else the provider reported, kept verbatim.
    pub metadata: JsonObject,
}

impl LanguageModel {
    /// Select a known model ID without requiring a discovery request.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            context_window: None,
            max_output_tokens: None,
            tool_calls: None,
            image_input: None,
            thinking: ThinkingCapabilities::default(),
            metadata: JsonObject::new(),
        }
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.id.trim().is_empty() {
            return Err(Error::InvalidRequest("model ID must not be empty".into()));
        }
        Ok(())
    }
}

/// One provider connection, independent of an agent loop or storage backend.
///
/// Implementations validate requests and fail unsupported conversions rather
/// than drop content. A successful stream emits exactly one `Finished`, last.
/// A missing completion signal must produce `IncompleteStream`, including
/// when some text was already observed. Dropping the returned future/stream
/// cancels local I/O; this does not promise server-side cancellation.
/// `stream().await` performs request startup and rejects initial transport or
/// provider status errors. Body parsing and model output errors arrive through
/// the returned stream.
pub trait LanguageModelProvider: Send + Sync {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, Error>>;

    /// The selected model is borrowed only during request startup. The returned
    /// stream owns its work and can outlive both this borrow and the model data.
    fn stream<'a>(
        &'a self,
        model: &'a LanguageModel,
        request: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, Error>>;
}

/// Drains an existing stream when previews are not needed; no separate request
/// is made. Also validates completion event ordering for scripted or third-party
/// adapters. This helper cannot make a raw stream's EOF turn into an error for other consumers;
/// implementations must enforce that contract in their stream itself.
pub async fn collect_completion(mut stream: CompletionStream) -> Result<Completion, Error> {
    let mut completion = None;
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::Finished(value) if completion.is_none() => {
                if value.message.role != Role::Assistant {
                    return Err(Error::InvalidResponse(
                        "completion must have assistant role".into(),
                    ));
                }
                value
                    .message
                    .validate()
                    .map_err(|error| Error::InvalidResponse(error.to_string()))?;
                completion = Some(*value);
            }
            StreamEvent::Delta { .. } if completion.is_none() => {}
            _ => {
                return Err(Error::InvalidResponse(
                    "event after final completion".into(),
                ));
            }
        }
    }
    completion.ok_or(Error::IncompleteStream)
}

/// Errors carry no provider SDK or HTTP-client error types across the boundary.
/// Diagnostics keep the provider's full detail, with credentials redacted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("unsupported conversion or capability: {0}")]
    Unsupported(String),
    #[error("transport failed: {0}")]
    Transport(String),
    /// A non-success HTTP status, as its numeric code.
    #[error("HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        /// How long the server asked the client to wait (`Retry-After`), as
        /// a delay from when the response arrived; typically on 429 and 503.
        retry_after: Option<Duration>,
    },
    #[error("provider failed: {0}")]
    Provider(String),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("stream ended without a completion signal")]
    IncompleteStream,
    #[error("request timed out")]
    Timeout,
    #[error("request was cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests;
