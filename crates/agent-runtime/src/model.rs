use std::sync::Arc;

use futures::{
    StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use language_model::{
    CompletionRequest, CompletionStream, Content, Error, LanguageModel, LanguageModelProvider,
    Message, Role, StreamEvent, ToolResultContent,
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
pub(crate) fn sample<A: Send + 'static>(
    provider: Arc<dyn LanguageModelProvider>,
    model: LanguageModel,
    request: CompletionRequest,
) -> BoxStream<'static, WorkEvent<A>> {
    let request = for_model(&model, request);
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

/// What a model that doesn't accept images is sent in place of each image.
const IMAGE_OMITTED: &str = "[Image omitted: the current model does not accept images.]";

/// `request` as `model` can take it. A model known not to accept images gets
/// a note in place of each one, so a conversation that holds images (an
/// attachment, a tool's result, or what an earlier model saw) still runs,
/// and the model learns something was there. The conversation keeps the
/// images, for a later model that accepts them. Unknown support sends them
/// as they are, for the provider to judge.
fn for_model(model: &LanguageModel, mut request: CompletionRequest) -> CompletionRequest {
    if model.image_input != Some(false) {
        return request;
    }
    let note = || IMAGE_OMITTED.to_owned();
    for message in &mut request.messages {
        for content in &mut message.content {
            match content {
                Content::Image(_) => *content = Content::Text { text: note() },
                Content::ToolResult(result) => {
                    for part in &mut result.content {
                        if matches!(part, ToolResultContent::Image(_)) {
                            *part = ToolResultContent::Text { text: note() };
                        }
                    }
                }
                _ => {}
            }
        }
    }
    request
}

#[cfg(test)]
mod tests {
    use language_model::{Image, ImageSource, ToolCallId, ToolResult};

    use super::*;

    #[test]
    fn images_become_notes_only_for_models_without_image_input() {
        let image = || Image {
            media_type: "image/png".into(),
            source: ImageSource::Base64 {
                data: "AA==".into(),
            },
        };
        let request = CompletionRequest::new(vec![
            Message::new(
                Role::User,
                vec![Content::text("look"), Content::Image(image())],
            ),
            Message::new(
                Role::Tool,
                vec![Content::ToolResult(ToolResult {
                    call: ToolCallId::new(),
                    provider_id: None,
                    name: Some("view_image".into()),
                    content: vec![
                        ToolResultContent::Text {
                            text: "plot.png".into(),
                        },
                        ToolResultContent::Image(image()),
                    ],
                    is_error: false,
                })],
            ),
        ]);
        let mut model = LanguageModel::new("model");
        for image_input in [None, Some(true)] {
            model.image_input = image_input;
            assert_eq!(for_model(&model, request.clone()), request);
        }

        model.image_input = Some(false);
        let sent = for_model(&model, request.clone());
        let note = || IMAGE_OMITTED.to_owned();
        assert_eq!(
            sent.messages[0].content,
            [Content::text("look"), Content::Text { text: note() }]
        );
        let Content::ToolResult(result) = &sent.messages[1].content[0] else {
            panic!("expected the tool result");
        };
        assert_eq!(
            result.content,
            [
                ToolResultContent::Text {
                    text: "plot.png".into()
                },
                ToolResultContent::Text { text: note() },
            ]
        );
        sent.validate().unwrap();
    }
}
