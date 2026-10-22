//! Anthropic Messages adapter with indexed streaming and signed thinking replay.
//! Transport policy belongs to the optional custom reqwest client (re-exported
//! here); no retries or tasks are spawned. Dropping a response stream drops its
//! HTTP body.

mod stream;

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
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::Value;

/// One Messages connection. The base URL includes the API prefix (`/v1`).
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
        let mut value = HeaderValue::from_str(key)
            .map_err(|_| Error::InvalidRequest("invalid API key header value".into()))?;
        value.set_sensitive(true);
        self.authorization = Some(value);
        Ok(self)
    }

    /// Sends native Messages input and returns the shared completion stream.
    /// Request conversion is bypassed; HTTP status is checked before returning.
    pub async fn stream_native(
        &self,
        mut request: anthropic_types::MessagesRequest,
    ) -> Result<CompletionStream, Error> {
        if request.model.trim().is_empty() || request.max_tokens == 0 {
            return Err(Error::InvalidRequest(
                "native request needs a model and positive max_tokens".into(),
            ));
        }
        request.stream = Some(true);
        let response = self
            .request(reqwest::Method::POST, "messages")
            .json(&request)
            .send()
            .await
            .map_err(transport)?;
        let response = self.checked(response).await?;
        Ok(stream::completion_stream(
            self.clone(),
            response,
            request.model,
        ))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = self.base.join(path).expect("constant relative endpoint");
        self.request_url(method, url)
    }

    fn request_url(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        let request = self
            .http
            .request(method, url)
            .header("anthropic-version", "2023-06-01");
        match &self.authorization {
            Some(value) => request.header("x-api-key", value.clone()),
            None => request,
        }
    }

    fn redact(&self, text: &str) -> String {
        match self.authorization.as_ref().and_then(|v| v.to_str().ok()) {
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
        let message = match response.json::<ApiResponse<IgnoredAny>>().await {
            Ok(ApiResponse::Error { error }) => self.error_message(&error),
            Ok(ApiResponse::Success(_)) => None,
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
        let value: ApiResponse<T> = response.json().await.map_err(|error| {
            if error.is_decode() {
                Error::InvalidResponse("invalid JSON response".into())
            } else {
                transport(error)
            }
        })?;
        match value {
            ApiResponse::Success(value) => Ok(value),
            ApiResponse::Error { error } => Err(self.provider_error(&error)),
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

/// Check errors before attempting the success shape; a JSON error must not
/// deserialize into a permissive success record.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ApiResponse<T> {
    Error { error: Value },
    Success(T),
}

impl LanguageModelProvider for Client {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, Error>> {
        Box::pin(async move {
            let mut models = Vec::new();
            let mut cursor: Option<String> = None;
            let mut cursors = std::collections::HashSet::new();
            let mut ids = std::collections::HashSet::new();
            loop {
                let mut url = self
                    .base
                    .join("models")
                    .expect("constant relative endpoint");
                if let Some(cursor) = &cursor {
                    url.query_pairs_mut().append_pair("after_id", cursor);
                }
                let request = self.request_url(reqwest::Method::GET, url);
                let response = request.send().await.map_err(transport)?;
                let page: anthropic_types::ModelsResponse = self.json(response).await?;
                for info in page.data {
                    if info.id.trim().is_empty() || !ids.insert(info.id.clone()) {
                        return Err(Error::InvalidResponse("empty or duplicate model ID".into()));
                    }
                    let Value::Object(metadata) = serde_json::to_value(&info)
                        .map_err(|_| Error::InvalidResponse("invalid model metadata".into()))?
                    else {
                        unreachable!()
                    };
                    models.push(LanguageModel {
                        id: info.id,
                        name: info.display_name,
                        context_window: info.max_input_tokens,
                        max_output_tokens: info.max_tokens,
                        metadata,
                        ..LanguageModel::new("")
                    });
                }
                if !page.has_more {
                    break;
                }
                let next = page.last_id.filter(|id| !id.is_empty()).ok_or_else(|| {
                    Error::InvalidResponse("model page lacks pagination cursor".into())
                })?;
                if !cursors.insert(next.clone()) {
                    return Err(Error::InvalidResponse(
                        "repeated model pagination cursor".into(),
                    ));
                }
                cursor = Some(next);
            }
            Ok(models)
        })
    }

    fn stream<'a>(
        &'a self,
        model: &'a LanguageModel,
        request: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, Error>> {
        Box::pin(async move {
            self.stream_native(anthropic_types::MessagesRequest::from_completion(
                model, request,
            )?)
            .await
        })
    }
}

#[cfg(test)]
mod tests;
