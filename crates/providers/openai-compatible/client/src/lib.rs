//! A single-choice chat-completions adapter for configurable compatible servers.
//! Transport policy belongs to the optional custom reqwest client (re-exported
//! here); no retries or tasks are spawned. Dropping a response stream drops its
//! HTTP body.

mod response;
mod stream;

use std::time::{Duration, SystemTime};

use futures::future::BoxFuture;
use language_model::{
    CompletionRequest, CompletionStream, Error, LanguageModel, LanguageModelProvider,
};
pub use reqwest;
use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::Value;

/// One server connection. The base URL includes the API prefix (usually /v1).
/// Authentication is deliberately excluded from Debug and stored messages.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    authorization: Option<HeaderValue>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("authenticated", &self.authorization.is_some())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// `base_url` is the API root, including `/v1` where the server needs it.
    /// Construction performs no network I/O.
    pub fn new(base_url: &str) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|error| Error::Transport(error_chain(&error)))?;
        let mut base = Url::parse(base_url)
            .map_err(|_| Error::InvalidRequest("invalid API base URL".into()))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::InvalidRequest(
                "API base must be HTTP(S), without credentials, query or fragment".into(),
            ));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
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

    pub fn with_api_key(mut self, key: &str) -> Result<Self, Error> {
        if key.is_empty() {
            return Err(Error::InvalidRequest("API key must not be empty".into()));
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| Error::InvalidRequest("invalid API key header value".into()))?;
        value.set_sensitive(true);
        self.authorization = Some(value);
        Ok(self)
    }

    /// Sends a native request and returns shared completion events.
    /// The client selects streaming mode and asks for usage unless the caller
    /// set `include_usage`, retaining other caller-supplied controls.
    /// HTTP status is checked before returning; dropping the stream cancels
    /// local response I/O. Native input bypasses generic history conversion.
    pub async fn stream_native(
        &self,
        mut request: openai_compatible_types::ChatCompletionRequest,
    ) -> Result<CompletionStream, Error> {
        if request.model.trim().is_empty() {
            return Err(Error::InvalidRequest("model must not be empty".into()));
        }
        request.stream = Some(true);
        // Streams carry usage only when asked; every supported server accepts
        // this (vLLM, Ollama's /v1).
        request
            .stream_options
            .get_or_insert_default()
            .include_usage
            .get_or_insert(true);
        let response = self
            .request(reqwest::Method::POST, "chat/completions")
            .json(&request)
            .send()
            .await
            .map_err(transport)?;
        let response = self.checked(response).await?;
        Ok(stream::completion_stream(self.clone(), response))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = self.base.join(path).expect("constant relative endpoint");
        let request = self.http.request(method, url);
        match &self.authorization {
            Some(value) => request.header(AUTHORIZATION, value.clone()),
            None => request,
        }
    }

    fn redact(&self, text: &str) -> String {
        match self
            .authorization
            .as_ref()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            Some(secret) => text.replace(secret, "[REDACTED]"),
            None => text.into(),
        }
    }

    async fn checked(&self, response: reqwest::Response) -> Result<reqwest::Response, Error> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let retry_after = retry_after(response.headers());
        let message = match response
            .json::<openai_compatible_types::ApiResponse<IgnoredAny>>()
            .await
        {
            Ok(openai_compatible_types::ApiResponse::Error { error }) => self.error_message(&error),
            Ok(openai_compatible_types::ApiResponse::Success(_)) => None,
            Err(error) if error.is_decode() => None,
            Err(error) => return Err(transport(error)),
        };
        Err(Error::Http {
            status: status.as_u16(),
            message: message
                .unwrap_or_else(|| "server returned an HTTP error (unrecognized body)".into()),
            retry_after,
        })
    }

    async fn json<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T, Error> {
        let response = self.checked(response).await?;
        let value: openai_compatible_types::ApiResponse<T> =
            response.json().await.map_err(|error| {
                if error.is_decode() {
                    Error::InvalidResponse("invalid JSON response".into())
                } else {
                    transport(error)
                }
            })?;
        match value {
            openai_compatible_types::ApiResponse::Success(value) => Ok(value),
            openai_compatible_types::ApiResponse::Error { error } => {
                Err(self.provider_error(&error))
            }
        }
    }

    fn error_message(&self, error: &Value) -> Option<String> {
        error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .map(|message| self.redact(message))
    }

    fn provider_error(&self, error: &Value) -> Error {
        Error::Provider(
            self.error_message(error)
                .unwrap_or_else(|| "unspecified provider error".into()),
        )
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

impl LanguageModelProvider for Client {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, Error>> {
        Box::pin(async move {
            let response = self
                .request(reqwest::Method::GET, "models")
                .send()
                .await
                .map_err(transport)?;
            let list: openai_compatible_types::ModelList = self.json(response).await?;
            Ok(list
                .data
                .into_iter()
                .map(|model| {
                    let mut metadata = model.extra;
                    if let Some(value) = model.created {
                        metadata.insert("created".into(), value.into());
                    }
                    if let Some(value) = model.owned_by {
                        metadata.insert("owned_by".into(), value.into());
                    }
                    LanguageModel {
                        // vLLM reports its configured context length here.
                        context_window: metadata
                            .get("max_model_len")
                            .and_then(serde_json::Value::as_u64),
                        metadata,
                        ..LanguageModel::new(model.id)
                    }
                })
                .collect())
        })
    }

    fn stream<'a>(
        &'a self,
        model: &'a LanguageModel,
        request: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, Error>> {
        Box::pin(async move {
            self.stream_native(
                openai_compatible_types::ChatCompletionRequest::from_completion(model, request)?,
            )
            .await
        })
    }
}

#[cfg(test)]
mod tests;
