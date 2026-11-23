use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeRequest {
    pub url: String,
    /// When empty, use the server default (Markdown).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<Format>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_main_content: Option<bool>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    Markdown,
    Html,
    RawHtml,
    Links,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub markdown: Option<String>,
    pub html: Option<String>,
    pub raw_html: Option<String>,
    pub links: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    pub warning: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub blocked: bool,
}

#[derive(Debug, Serialize)]
pub struct MapRequest {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct MapLink {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Health {
    pub status: String,
    pub engine: String,
    pub version: String,
}
