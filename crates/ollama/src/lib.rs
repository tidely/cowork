mod types;

use async_trait::async_trait;
use futures::StreamExt;
use llm::{ChatRequest, LlmError, LlmStream, Provider, StreamEvent};

pub use types::{
    OllamaChatMessage, OllamaChatOptions, OllamaChatRequest, OllamaChatResponse,
    OllamaFunctionCall, OllamaFunctionTool, OllamaTool, OllamaToolCall,
};

#[derive(Clone)]
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn from_env() -> Self {
        let base_url =
            std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
        Self::new(base_url)
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.base_url)
    }
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::from_env()
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn stream_chat(&self, request: ChatRequest) -> Result<LlmStream, LlmError> {
        let response = self
            .client
            .post(self.chat_url())
            .json(&OllamaChatRequest::from_chat_request(request))
            .send()
            .await
            .map_err(|error| LlmError::Transport(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read error body: {error}"));
            return Err(LlmError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        Ok(Box::pin(stream_response(response)) as LlmStream)
    }
}

fn stream_response(
    response: reqwest::Response,
) -> impl futures::Stream<Item = Result<StreamEvent, LlmError>> + Send + 'static {
    async_stream::try_stream! {
        yield StreamEvent::Started;

        let mut chunks = response.bytes_stream();
        let mut buffer = String::new();
        let mut next_tool_index = 0_u64;

        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| LlmError::Transport(error.to_string()))?;
            let chunk = std::str::from_utf8(&chunk)
                .map_err(|error| LlmError::Decode(error.to_string()))?;
            buffer.push_str(chunk);

            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer.drain(..=newline);
                if line.is_empty() {
                    continue;
                }

                for event in response_events(&line, &mut next_tool_index)? {
                    yield event;
                }
            }
        }

        let line = buffer.trim();
        if !line.is_empty() {
            for event in response_events(line, &mut next_tool_index)? {
                yield event;
            }
        }
    }
}

fn response_events(line: &str, next_tool_index: &mut u64) -> Result<Vec<StreamEvent>, LlmError> {
    let response: OllamaChatResponse =
        serde_json::from_str(line).map_err(|error| LlmError::Decode(error.to_string()))?;
    response.into_stream_events(next_tool_index)
}
