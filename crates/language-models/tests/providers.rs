//! The facade connects both HTTP adapters to the same runtime/storage boundary.

use std::time::Duration;

use language_models::{
    CompletionRequest, Content, Envelope, FinishReason, LanguageModel, LanguageModelProvider,
    Message, Ollama, OpenAiCompatible, Role, Thinking, collect_completion, reqwest,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

async fn server(
    response: String,
    native: bool,
) -> (String, tokio::task::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request ended before its headers");
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let content_length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        while bytes.len() - header_end < content_length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request ended before its body");
            bytes.extend_from_slice(&buffer[..count]);
        }
        let body = serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
        let response = response.into_bytes();
        let content_type = if native {
            "application/x-ndjson"
        } else {
            "text/event-stream"
        };
        let response_headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        socket.write_all(response_headers.as_bytes()).await.unwrap();
        socket.write_all(&response).await.unwrap();
        (headers.lines().next().unwrap().to_owned(), body)
    });
    (address, task)
}

#[tokio::test]
async fn both_providers_return_messages_that_cross_the_storage_boundary() {
    for native in [true, false] {
        let response = if native {
            json!({
                "model":"test-model", "done":true, "done_reason":"stop",
                "message":{"role":"assistant", "content":"Hello"},
                "prompt_eval_count":10, "eval_count":2
            })
            .to_string()
        } else {
            let chunk = json!({
                "model":"test-model", "choices":[{
                    "index":0, "finish_reason":"stop",
                    "delta":{"role":"assistant", "content":"Hello"}
                }],
                "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
            });
            format!("data: {chunk}\n\ndata: [DONE]\n\n")
        };
        let (address, server) = server(response, native).await;
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let provider: Box<dyn LanguageModelProvider> = if native {
            Box::new(
                Ollama::new(&format!("{address}/proxy"))
                    .unwrap()
                    .with_http_client(http),
            )
        } else {
            Box::new(
                OpenAiCompatible::new(&format!("{address}/proxy/v1"))
                    .unwrap()
                    .with_http_client(http),
            )
        };
        let stream = {
            let model = LanguageModel::new("test-model");
            provider
                .stream(
                    &model,
                    CompletionRequest {
                        instructions: Some("Be brief.".into()),
                        thinking: Some(Thinking::Effort("medium".into())),
                        ..CompletionRequest::new(vec![Message::user("Hi")])
                    },
                )
                .await
                .unwrap()
        };
        // Startup borrows selection and connection data; body streaming owns
        // its work and does not keep either of those caller borrows alive.
        drop(provider);
        let completion = collect_completion(stream).await.unwrap();
        assert_eq!(completion.message.role, Role::Assistant);
        assert_eq!(completion.message.content, vec![Content::text("Hello")]);
        let generation = completion.message.generation.as_ref().unwrap();
        assert_eq!(generation.finish_reason, Some(FinishReason::Stop));
        let saved = Envelope::encode_message(&completion.message).unwrap();
        let loaded: Envelope =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(loaded.decode_message().unwrap(), completion.message);
        let (request_line, body) = server.await.unwrap();
        let expected_path = if native {
            "/proxy/api/chat"
        } else {
            "/proxy/v1/chat/completions"
        };
        assert_eq!(request_line, format!("POST {expected_path} HTTP/1.1"));
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        let effort_key = if native { "think" } else { "reasoning_effort" };
        assert_eq!(body[effort_key], "medium");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "Be brief.");
        assert_eq!(body["messages"][1]["content"], "Hi");
    }
}
