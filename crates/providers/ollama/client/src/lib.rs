//! Ollama's native chat API behind Cowork's owned language-model boundary.
//!
//! No task is spawned by this client. Requests/streams own their I/O, so dropping
//! them cancels local work. An optional custom HTTP client controls TLS and
//! timeouts; `reqwest` is re-exported so callers can build one.

mod convert;
mod streaming;

use std::time::{Duration, SystemTime};

use futures::future::BoxFuture;
use language_model::{
    CompletionRequest, CompletionStream, Error, LanguageModel, LanguageModelProvider,
};
pub use reqwest;
use reqwest::{
    Url,
    header::{HeaderMap, HeaderValue},
};

/// A connection to an Ollama server. Credentials are deliberately not Debug.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    authorization: Option<HeaderValue>,
}

impl Client {
    /// `base_url` is the server root, optionally including a reverse-proxy path
    /// prefix. It must not include `/api`, credentials, a query, or a fragment.
    /// Construction performs no network I/O.
    pub fn new(base_url: &str) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|error| Error::Transport(error_chain(&error)))?;
        let mut base = Url::parse(base_url)
            .map_err(|_| Error::InvalidRequest("invalid Ollama base URL".into()))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::InvalidRequest(
                "Ollama base URL must be HTTP(S) without credentials, query, or fragment".into(),
            ));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(Self {
            http,
            base,
            authorization: None,
        })
    }

    /// Replaces the default HTTP client, e.g. to configure timeouts or proxies.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Sets bearer authentication without storing it in messages or options.
    pub fn with_api_key(mut self, key: &str) -> Result<Self, Error> {
        let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| Error::InvalidRequest("invalid API key header".into()))?;
        value.set_sensitive(true);
        self.authorization = Some(value);
        Ok(self)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        // All call sites pass static relative endpoint paths; the validated
        // hierarchical base guarantees this join cannot fail.
        let url = self.base.join(path).expect("static Ollama endpoint");
        let request = self.http.request(method, url);
        match &self.authorization {
            Some(value) => request.header(reqwest::header::AUTHORIZATION, value.clone()),
            None => request,
        }
    }

    fn error_context(&self) -> ErrorContext {
        let mut secrets = Vec::new();
        if let Some(value) = self
            .authorization
            .as_ref()
            .and_then(|value| value.to_str().ok())
        {
            secrets.push(value.to_owned());
            if let Some(key) = value.strip_prefix("Bearer ").filter(|key| !key.is_empty()) {
                secrets.push(key.to_owned());
            }
        }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        ErrorContext { secrets }
    }

    /// Native discovery detail remains available without reducing model-defined
    /// capabilities or thinking controls to a hard-coded capability enum.
    pub async fn show_model(&self, model: &str) -> Result<ollama_types::ShowModelResponse, Error> {
        if model.trim().is_empty() {
            return Err(Error::InvalidRequest("model must not be empty".into()));
        }
        let response = checked(
            self.request(reqwest::Method::POST, "api/show")
                .json(&ollama_types::ShowModelRequest {
                    model: model.into(),
                    verbose: None,
                })
                .send()
                .await
                .map_err(transport)?,
            &self.error_context(),
        )
        .await?;
        response.json().await.map_err(decode)
    }

    /// Sends a native request and returns events in the shared completion schema.
    /// Selects streaming mode and checks HTTP status before returning the stream;
    /// native controls other than `stream` are preserved.
    pub async fn stream_native(
        &self,
        mut request: ollama_types::ChatRequest,
    ) -> Result<CompletionStream, Error> {
        if request.model.trim().is_empty() {
            return Err(Error::InvalidRequest("model must not be empty".into()));
        }
        request.stream = Some(true);
        let context = self.error_context();
        let response = checked(
            self.request(reqwest::Method::POST, "api/chat")
                .json(&request)
                .send()
                .await
                .map_err(transport)?,
            &context,
        )
        .await?;
        Ok(streaming::response_stream(response, context))
    }
}

impl LanguageModelProvider for Client {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, Error>> {
        Box::pin(async move {
            let response = checked(
                self.request(reqwest::Method::GET, "api/tags")
                    .send()
                    .await
                    .map_err(transport)?,
                &self.error_context(),
            )
            .await?;
            let list: ollama_types::ListModelsResponse = response.json().await.map_err(decode)?;
            list.models
                .into_iter()
                .map(|model| {
                    let serde_json::Value::Object(metadata) = serde_json::to_value(&model)
                        .map_err(|_| Error::InvalidResponse("invalid model metadata".into()))?
                    else {
                        unreachable!("native model is an object");
                    };
                    // Ollama lists capabilities by name; a missing list is unknown.
                    let has = |name: &str| {
                        model
                            .capabilities
                            .as_ref()
                            .map(|names| names.iter().any(|capability| capability == name))
                    };
                    Ok(LanguageModel {
                        tool_calls: has("tools"),
                        image_input: has("vision"),
                        id: model.model.clone().unwrap_or_else(|| model.name.clone()),
                        name: Some(model.name),
                        context_window: model.details.and_then(|details| details.context_length),
                        max_output_tokens: None,
                        thinking: Default::default(),
                        metadata,
                    })
                })
                .collect()
        })
    }

    fn stream<'a>(
        &'a self,
        model: &'a LanguageModel,
        request: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, Error>> {
        Box::pin(async move {
            self.stream_native(ollama_types::ChatRequest::from_completion(model, request)?)
                .await
        })
    }
}

fn transport(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Transport(error_chain(&error.without_url()))
    }
}

/// The error and its causes, e.g. "error sending request: client error
/// (Connect): tcp connect error: Connection refused", so a stopped server is
/// distinguishable from other failures.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// The requested delay, from `Retry-After` (delay-seconds or an HTTP-date) or
/// the more precise `retry-after-ms` some servers send, which wins.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let header = |name: &str| headers.get(name)?.to_str().ok().map(str::trim);
    if let Some(milliseconds) = header("retry-after-ms")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
    {
        return Some(Duration::from_secs_f64(milliseconds / 1000.0));
    }
    let value = header("retry-after")?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(
        date.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn decode(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else if error.is_decode() {
        Error::InvalidResponse("invalid Ollama JSON response".into())
    } else {
        transport(error)
    }
}

async fn checked(
    response: reqwest::Response,
    context: &ErrorContext,
) -> Result<reqwest::Response, Error> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        let retry_after = retry_after(response.headers());
        let bytes = response.bytes().await.map_err(transport)?;
        // Parse only the documented error field. Never display arbitrary HTTP
        // bodies (e.g. a proxy page), and redact provider diagnostics.
        let message = serde_json::from_slice::<ollama_types::ErrorResponse>(&bytes)
            .ok()
            .map(|error| context.sanitize(&error.error))
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| {
                status
                    .canonical_reason()
                    .unwrap_or("Ollama request failed")
                    .into()
            });
        Err(Error::Http {
            status: status.as_u16(),
            message,
            retry_after,
        })
    }
}

#[derive(Default)]
struct ErrorContext {
    secrets: Vec<String>,
}

impl ErrorContext {
    fn sanitize(&self, message: &str) -> String {
        let mut text = message.trim().to_owned();
        for secret in &self.secrets {
            if !secret.is_empty() {
                text = text.replace(secret, "[redacted]");
            }
        }
        text
    }

    fn provider(&self, message: &str) -> Error {
        Error::Provider(self.sanitize(message))
    }
}

#[cfg(test)]
mod tests;
