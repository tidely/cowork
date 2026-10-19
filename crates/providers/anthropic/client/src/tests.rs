use super::*;
use futures::StreamExt as _;
use language_model::{
    Content, ContentDelta, Envelope, Message, Reasoning, StreamEvent, ToolArguments,
    collect_completion,
};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

async fn server(status: u16, body: String, sse: bool) -> (Client, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
            if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|n| n.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while request.len() < header_end + length {
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
        }
        let content_type = if sse {
            "text/event-stream"
        } else {
            "application/json"
        };
        socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        // One-byte writes exercise framing independent of HTTP and UTF-8 boundaries.
        for byte in body.as_bytes() {
            if socket.write_all(&[*byte]).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
        String::from_utf8(request).unwrap()
    });
    let client = Client::new(&format!("http://{address}/proxy/v1"))
        .unwrap()
        .with_http_client(reqwest::Client::builder().no_proxy().build().unwrap());
    (client, task)
}

fn input() -> CompletionRequest {
    CompletionRequest {
        max_output_tokens: Some(4096),
        ..CompletionRequest::new(vec![Message::user("hello")])
    }
}
fn model() -> LanguageModel {
    LanguageModel::new("chosen-model")
}
fn event(value: Value) -> String {
    format!(
        "event: {}\r\ndata: {value}\r\n\r\n",
        value["type"].as_str().unwrap()
    )
}
fn start() -> Value {
    json!({"type":"message_start","message":{"id":"msg_a","type":"message","role":"assistant","model":"chosen-model","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":20,"cache_creation_input_tokens":5}}})
}
fn completion_events() -> Vec<Value> {
    vec![
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]
}
fn text_events() -> Vec<Value> {
    let mut values = vec![
        start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello 考"}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    values.extend(completion_events());
    values
}
fn body(events: Vec<Value>) -> String {
    events.into_iter().map(event).collect()
}

#[tokio::test]
async fn streams_over_split_bytes_with_headers_and_authoritative_usage() {
    let mut values = text_events();
    values.insert(
        values.len() - 1,
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":8}}),
    );
    let (client, task) = server(200, body(values), true).await;
    let provider: Box<dyn LanguageModelProvider> =
        Box::new(client.with_api_key("test-secret").unwrap());
    let mut stream = provider.stream(&model(), input()).await.unwrap();
    assert!(
        matches!(stream.next().await.unwrap().unwrap(), StreamEvent::Delta { index: 0, delta: ContentDelta::Text(text) } if text == "Hello 考")
    );
    let StreamEvent::Finished(completion) = stream.next().await.unwrap().unwrap() else {
        panic!("missing completion")
    };
    assert!(stream.next().await.is_none());
    assert_eq!(completion.message.content, vec![Content::text("Hello 考")]);
    let usage = completion
        .message
        .generation
        .as_ref()
        .unwrap()
        .usage
        .as_ref()
        .unwrap();
    assert_eq!(usage.input_tokens, Some(35));
    assert_eq!(usage.output_tokens, Some(8));
    assert_eq!(usage.total_tokens, Some(43));
    assert_eq!(usage.cached_input_tokens, Some(20));
    let request = task.await.unwrap();
    let headers = request.to_lowercase();
    assert!(headers.starts_with("post /proxy/v1/messages "));
    assert!(headers.contains("x-api-key: test-secret\r\n"));
    assert!(headers.contains("anthropic-version: 2023-06-01\r\n"));
    assert!(!headers.contains("authorization:"));
    let value: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(value["stream"], true);
    assert_eq!(value["max_tokens"], 4096);
}

#[tokio::test]
async fn persists_signed_thinking_redactions_and_tool_identity_for_replay() {
    let mut values = vec![
        start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_a","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"foo\"}"}}),
        json!({"type":"content_block_stop","index":2}),
    ];
    values[0]["message"]["model"] = json!("resolved-model-snapshot");
    values.extend(completion_events());
    let (client, task) = server(200, body(values), true).await;
    let mut stream = client.stream(&model(), input()).await.unwrap();
    let mut preview_id = None;
    let message = loop {
        match stream.next().await.unwrap().unwrap() {
            StreamEvent::Delta {
                delta: ContentDelta::ToolCall { id, .. },
                ..
            } => {
                if let Some(previous) = &preview_id {
                    assert_eq!(previous, &id);
                }
                preview_id = Some(id);
            }
            StreamEvent::Finished(completion) => break completion.message,
            _ => {}
        }
    };
    task.await.unwrap();
    assert!(
        matches!(&message.content[0], Content::Reasoning(Reasoning::Text { text }) if text == "reason")
    );
    assert!(
        matches!(&message.content[1], Content::Reasoning(Reasoning::Redacted { data }) if data == "opaque")
    );
    let Content::ToolCall(call) = &message.content[2] else {
        panic!("missing call")
    };
    assert_eq!(Some(&call.id), preview_id.as_ref());
    assert_eq!(call.provider_id.as_deref(), Some("toolu_a"));
    assert!(matches!(&call.arguments, ToolArguments::Object { value } if value["path"] == "foo"));
    let restored = Envelope::encode_message(&message)
        .unwrap()
        .decode_message()
        .unwrap();
    let result = Message::new(
        language_model::Role::Tool,
        vec![Content::ToolResult(language_model::ToolResult {
            call: call.id.clone(),
            provider_id: None,
            name: None,
            content: vec![],
            is_error: false,
        })],
    );
    let native = anthropic_types::MessagesRequest::from_completion(
        &model(),
        CompletionRequest {
            messages: vec![restored, result],
            ..input()
        },
    )
    .unwrap();
    assert_eq!(
        json!(native)["messages"][0]["content"][0]["signature"],
        "sig"
    );
    assert_eq!(
        json!(native)["messages"][1]["content"][0]["tool_use_id"],
        "toolu_a"
    );
}

#[tokio::test]
async fn rejects_truncated_out_of_order_and_mismatched_streams() {
    let valid = text_events();
    let mut cases = Vec::new();
    // Every proper prefix lacks a final completion, even after visible text.
    for length in 0..valid.len() {
        cases.push(body(valid[..length].to_vec()));
    }
    let mut duplicate_start = vec![start(), start()];
    duplicate_start.extend(completion_events());
    cases.push(body(duplicate_start));
    for event in [
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"oops"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"oops"}}),
        json!({"type":"content_block_stop","index":0}),
    ] {
        let mut values = vec![start(), event];
        values.extend(completion_events());
        cases.push(body(values));
    }
    cases.push(body(vec![start(), json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}), json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"wrong type"}})]));
    cases.push("data: {bad json}\n\n".into());
    for body in cases {
        let (client, task) = server(200, body, true).await;
        let mut stream = client.stream(&model(), input()).await.unwrap();
        let mut failed = false;
        while let Some(event) = stream.next().await {
            assert!(!matches!(event, Ok(StreamEvent::Finished(_))));
            if event.is_err() {
                failed = true;
            }
        }
        assert!(failed);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_tool_json_remains_invalid_and_unknown_stops_survive() {
    for raw in ["[1]", "null", "{broken", ""] {
        let mut values = vec![
            start(),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_a","name":"read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":raw}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"future_stop"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ];
        // Exercise liveness events without changing assembly.
        values.insert(0, json!({"type":"ping"}));
        let (client, task) = server(200, body(values), true).await;
        let completion = collect_completion(client.stream(&model(), input()).await.unwrap())
            .await
            .unwrap();
        assert!(
            matches!(&completion.message.content[0], Content::ToolCall(call) if call.arguments == ToolArguments::Invalid { raw: raw.into() })
        );
        assert_eq!(
            completion
                .message
                .generation
                .unwrap()
                .finish_reason
                .as_deref(),
            Some("future_stop")
        );
        task.await.unwrap();
    }
}

#[tokio::test]
async fn provider_errors_redact_credentials_at_startup_and_during_streaming() {
    let error = json!({"type":"error","error":{"type":"overloaded_error","message":"do not expose secret-key"}});
    for status in [429, 200] {
        let body = if status == 200 {
            event(error.clone())
        } else {
            error.to_string()
        };
        let (client, task) = server(status, body, status == 200).await;
        let client = client.with_api_key("secret-key").unwrap();
        let error = match client.stream(&model(), input()).await {
            Err(error) => error,
            Ok(stream) => collect_completion(stream).await.unwrap_err(),
        };
        assert!(!error.to_string().contains("secret-key"));
        assert!(error.to_string().contains("[REDACTED]"));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn native_entry_point_and_discovery_preserve_metadata() {
    let (client, task) = server(200, body(text_events()), true).await;
    let native: anthropic_types::MessagesRequest = serde_json::from_value(json!({"model":"native-model","messages":[{"role":"user","content":"hi"}],"max_tokens":100,"thinking":{"type":"enabled","budget_tokens":50},"top_k":3})).unwrap();
    collect_completion(client.stream_native(native).await.unwrap())
        .await
        .unwrap();
    let request = task.await.unwrap();
    let value: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(value["top_k"], 3);
    assert_eq!(value["thinking"]["budget_tokens"], 50);
    let (client, task) = server(200, json!({"data":[{"id":"model-a","display_name":"A","max_input_tokens":200000,"capabilities":{"thinking":{"supported":true}},"created_at":"today"}],"has_more":false,"last_id":"model-a"}).to_string(), false).await;
    let models = client.list_models().await.unwrap();
    assert_eq!(models[0].context_window, Some(200000));
    assert_eq!(
        models[0].metadata["capabilities"]["thinking"]["supported"],
        true
    );
    assert!(task.await.unwrap().starts_with("GET /proxy/v1/models "));
}

#[tokio::test]
async fn follows_model_cursors_and_rejects_pagination_loops() {
    for repeated in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for page in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let n = socket.read(&mut bytes).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&bytes[..n]);
                    if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = if page == 0 {
                    json!({"data":[{"id":"first/model"}],"has_more":true,"last_id":"first/model"})
                } else {
                    json!({"data":[{"id":"second-model"}],"has_more":repeated,"last_id":"first/model"})
                }.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        let client = Client::new(&format!("http://{address}/v1"))
            .unwrap()
            .with_http_client(reqwest::Client::builder().no_proxy().build().unwrap());
        let result = client.list_models().await;
        if repeated {
            assert!(matches!(result, Err(Error::InvalidResponse(_))));
        } else {
            assert_eq!(
                result
                    .unwrap()
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["first/model", "second-model"]
            );
        }
        let requests = task.await.unwrap();
        assert!(requests[1].starts_with("GET /v1/models?after_id=first%2Fmodel "));
    }
}

#[tokio::test]
async fn dropping_the_stream_closes_unfinished_http_work() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
            if let Some(i) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse().unwrap())
            })
            .unwrap();
        while request.len() < header_end + length {
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
        }
        let prefix = body(text_events()[..3].to_vec());
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000000\r\n\r\n{prefix}").as_bytes()).await.unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    });
    let client = Client::new(&format!("http://{address}/v1"))
        .unwrap()
        .with_http_client(reqwest::Client::builder().no_proxy().build().unwrap());
    let mut stream = client.stream(&model(), input()).await.unwrap();
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::Delta { .. }))
    ));
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn retry_hints_and_client_diagnostics_keep_transport_policy_explicit() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after-ms", HeaderValue::from_static("250"));
    assert_eq!(retry_after(&headers), Some(Duration::from_millis(250)));
    headers.remove("retry-after-ms");
    headers.insert("retry-after", HeaderValue::from_static("3"));
    assert_eq!(retry_after(&headers), Some(Duration::from_secs(3)));
    let client = Client::new("https://api.anthropic.com/v1")
        .unwrap()
        .with_api_key("a-secret")
        .unwrap();
    assert!(!format!("{client:?}").contains("a-secret"));
    assert!(Client::new("https://user:password@example.com/v1").is_err());
    assert!(client.with_api_key("bad\nheader").is_err());
}
