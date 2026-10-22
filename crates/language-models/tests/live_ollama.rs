//! Explicit opt-in checks against a running Ollama server, through both APIs.

use std::time::Duration;

use futures::StreamExt as _;
use language_models::{
    CompletionRequest, Content, Envelope, FinishReason, LanguageModel, LanguageModelProvider,
    Message, Ollama, OpenAiCompatible, StreamEvent, Thinking, reqwest,
};

async fn check_stream(native: bool) {
    let base = std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let model =
        LanguageModel::new(std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gemma4:e4b".into()));
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(240))
        .build()
        .unwrap();
    let provider: Box<dyn LanguageModelProvider> = if native {
        Box::new(Ollama::new(&base).unwrap().with_http_client(http))
    } else {
        Box::new(
            OpenAiCompatible::new(&format!("{}/v1", base.trim_end_matches('/')))
                .unwrap()
                .with_http_client(http),
        )
    };
    let mut history = vec![Message::user("Reply with the single word READY.")];
    // A second request exercises replay of the actual streamed reasoning and
    // text, rather than only accepting a synthetic provider fixture.
    for turn in 0..2 {
        let mut stream = provider
            .stream(
                &model,
                CompletionRequest {
                    instructions: Some("Follow the user's instructions exactly.".into()),
                    // Compatible endpoints need a configured effort to switch
                    // thinking; they keep the model's default instead.
                    thinking: native.then_some(Thinking::Disabled),
                    max_output_tokens: Some(2048),
                    ..CompletionRequest::new(history.clone())
                },
            )
            .await
            .unwrap();
        let mut deltas = 0;
        let mut completion = None;
        while let Some(event) = stream.next().await {
            assert!(completion.is_none(), "event after final completion");
            match event.unwrap() {
                StreamEvent::Delta { .. } => deltas += 1,
                StreamEvent::Finished(value) => completion = Some(value),
            }
        }
        let reply = completion.expect("stream must finish").message;
        assert!(deltas > 0);
        assert_eq!(
            reply.generation.as_ref().unwrap().finish_reason,
            Some(FinishReason::Stop)
        );
        assert!(
            reply
                .content
                .iter()
                .any(|part| matches!(part, Content::Text { text } if !text.trim().is_empty()))
        );
        let envelope = Envelope::encode_message(&reply).unwrap();
        assert_eq!(envelope.decode_message().unwrap(), reply);
        println!(
            "native={native} turn={turn}: {deltas} deltas; completed and storage roundtrip passed"
        );
        history.push(reply);
        history.push(Message::user("Repeat the word you just said."));
    }
}

#[tokio::test]
#[ignore = "requires a local Ollama server and installed model"]
async fn native_stream_and_history_replay() {
    check_stream(true).await;
}

#[tokio::test]
#[ignore = "requires a local Ollama server and installed model"]
async fn compatible_stream_and_history_replay() {
    check_stream(false).await;
}
