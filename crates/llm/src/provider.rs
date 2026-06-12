use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::{ChatRequest, LlmError, StreamEvent};

pub type LlmStream = BoxStream<'static, Result<StreamEvent, LlmError>>;

#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream_chat(&self, request: ChatRequest) -> Result<LlmStream, LlmError>;
}
