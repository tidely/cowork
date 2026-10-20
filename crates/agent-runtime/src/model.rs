use std::sync::Arc;

use futures::{
    StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use language_model::{
    CompletionRequest, CompletionStream, Error, LanguageModel, LanguageModelProvider, Message,
    Role, StreamEvent,
};

use crate::session::WorkEvent;

struct Sampling {
    startup: Option<BoxFuture<'static, Result<CompletionStream, Error>>>,
    stream: Option<CompletionStream>,
    finished: Option<Message>,
    done: bool,
}

/// Relay deltas as previews, but withhold the accepted message until EOF has
/// proved there is exactly one final and no trailing events/errors. This applies
/// the same contract as collect_completion to arbitrary provider implementations.
pub(crate) fn sample(
    provider: Arc<dyn LanguageModelProvider>,
    model: LanguageModel,
    request: CompletionRequest,
) -> BoxStream<'static, WorkEvent> {
    let startup = Box::pin(async move { provider.stream(&model, request).await });
    let state = Sampling {
        startup: Some(startup),
        stream: None,
        finished: None,
        done: false,
    };
    stream::unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        if let Some(startup) = state.startup.take() {
            match startup.await {
                Ok(stream) => state.stream = Some(stream),
                Err(error) => {
                    state.done = true;
                    return Some((WorkEvent::Model(Err(error)), state));
                }
            }
        }
        loop {
            match state
                .stream
                .as_mut()
                .expect("startup succeeded")
                .next()
                .await
            {
                Some(Err(error)) => {
                    state.done = true;
                    return Some((WorkEvent::Model(Err(error)), state));
                }
                Some(_) if state.finished.is_some() => {
                    state.done = true;
                    return Some((
                        WorkEvent::Model(Err(Error::InvalidResponse(
                            "event after final completion".into(),
                        ))),
                        state,
                    ));
                }
                Some(Ok(StreamEvent::Delta { index, delta })) => {
                    return Some((WorkEvent::Preview { index, delta }, state));
                }
                Some(Ok(StreamEvent::Finished(completion))) => {
                    let message = completion.message;
                    if message.role != Role::Assistant {
                        state.done = true;
                        return Some((
                            WorkEvent::Model(Err(Error::InvalidResponse(
                                "completion must have assistant role".into(),
                            ))),
                            state,
                        ));
                    }
                    if let Err(error) = message.validate() {
                        state.done = true;
                        return Some((
                            WorkEvent::Model(Err(Error::InvalidResponse(error.to_string()))),
                            state,
                        ));
                    }
                    state.finished = Some(message);
                }
                None => {
                    state.done = true;
                    return Some((
                        WorkEvent::Model(state.finished.take().ok_or(Error::IncompleteStream)),
                        state,
                    ));
                }
            }
        }
    })
    .boxed()
}
