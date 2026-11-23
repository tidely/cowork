//! Async client for the synchronous endpoints of Firecrawl Local. Pass the
//! server root (e.g. `http://127.0.0.1:3002`), without a `/v2` suffix.

mod types;

pub use types::*;

use reqwest::{RequestBuilder, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};

#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("Firecrawl ({status}): {message}")]
    Api {
        status: StatusCode,
        code: Option<String>,
        message: String,
    },
}

#[derive(Deserialize)]
struct ApiFailure {
    error: String,
    code: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Reply<T> {
    Failure(ApiFailure),
    Ok(T),
}

impl Client {
    /// Reuses the caller's HTTP pool, timeouts, and optional default auth headers.
    pub fn new(http: reqwest::Client, base_url: &str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    pub async fn health(&self) -> Result<Health, Error> {
        send(self.http.get(format!("{}/health", self.base_url))).await
    }

    pub async fn scrape(&self, request: &ScrapeRequest) -> Result<Document, Error> {
        #[derive(Deserialize)]
        struct Scrape {
            data: Document,
        }
        let reply: Scrape = send(
            self.http
                .post(format!("{}/v2/scrape", self.base_url))
                .json(request),
        )
        .await?;
        Ok(reply.data)
    }

    pub async fn map(&self, request: &MapRequest) -> Result<Vec<MapLink>, Error> {
        #[derive(Deserialize)]
        struct SiteMap {
            links: Vec<MapLink>,
        }
        let reply: SiteMap = send(
            self.http
                .post(format!("{}/v2/map", self.base_url))
                .json(request),
        )
        .await?;
        Ok(reply.links)
    }
}

async fn send<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, Error> {
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let failure = response
            .json::<ApiFailure>()
            .await
            .unwrap_or_else(|_| ApiFailure {
                error: "Server returned no Firecrawl error details".into(),
                code: None,
            });
        return Err(Error::Api {
            status,
            message: failure.error,
            code: failure.code,
        });
    }
    match response.json::<Reply<T>>().await? {
        Reply::Ok(value) => Ok(value),
        Reply::Failure(failure) => Err(Error::Api {
            status,
            message: failure.error,
            code: failure.code,
        }),
    }
}
