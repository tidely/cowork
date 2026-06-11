use futures::{future::BoxFuture, stream::BoxStream};

use crate::{ChatRequest, LlmError, StreamEvent};

pub type LlmStream = BoxStream<'static, Result<StreamEvent, LlmError>>;

pub trait Provider: Send + Sync {
    fn stream_chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<LlmStream, LlmError>>;
}
