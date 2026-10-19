use std::time::Duration;

use futures::StreamExt as _;
use language_model::{
    CompletionRequest, Content, ContentDelta, Error, Image, ImageSource, LanguageModel,
    LanguageModelProvider, Message, Reasoning, ReplayData, Role, StreamEvent, Thinking,
    ToolArguments, ToolCall, ToolCallId, ToolDefinition, ToolResult, ToolResultContent,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use crate::Client;

struct Observed {
    headers: String,
    body: Value,
}

async fn read_request(socket: &mut TcpStream) -> Observed {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let end = loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "request did not finish headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or_default();
    while bytes.len() < end + length {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    Observed {
        headers,
        body: if length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&bytes[end..end + length]).unwrap()
        },
    }
}

async fn server(status: u16, chunks: Vec<Vec<u8>>) -> (String, JoinHandle<Observed>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/proxy/", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let observed = read_request(&mut socket).await;
        socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        for chunk in chunks {
            socket
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(&chunk).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            socket.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        observed
    });
    (base, handle)
}

fn client(base: &str) -> Client {
    Client::new(base)
        .unwrap()
        .with_http_client(reqwest::Client::builder().no_proxy().build().unwrap())
}

fn request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::new(Role::User, vec![Content::text("Hello")])],
        ..Default::default()
    }
}

fn image() -> Image {
    Image {
        media_type: "image/png".into(),
        source: ImageSource::Base64 {
            data: "YWJj".into(),
        },
    }
}

#[tokio::test]
async fn discovery_preserves_prefix_and_optional_native_metadata() {
    let (base, observed) = server(200, vec![json!({"models":[
        {"name":"Display", "model":"exact-id", "details":{"context_length":8192}, "capabilities":["tools","future"], "remote_host":"host"},
        {"name":"unknown-capabilities"}
    ]}).to_string().into_bytes()]).await;
    let models = client(base.trim_end_matches('/'))
        .list_models()
        .await
        .unwrap();
    assert_eq!(models[0].id, "exact-id");
    assert_eq!(models[0].context_window, Some(8192));
    assert_eq!(models[0].metadata["remote_host"], "host");
    assert_eq!(models[0].capabilities.as_ref().unwrap()[1], "future");
    assert!(models[1].capabilities.is_none());
    assert!(
        observed
            .await
            .unwrap()
            .headers
            .starts_with("GET /proxy/api/tags ")
    );

    let (base, observed) = server(
        200,
        vec![
            json!({"thinking":{"values":[false,"custom"]},"model_info":{"a":42}})
                .to_string()
                .into_bytes(),
        ],
    )
    .await;
    let show = client(&base).show_model("model").await.unwrap();
    assert_eq!(show.thinking.unwrap()["values"][1], "custom");
    assert_eq!(observed.await.unwrap().body["model"], "model");
}

#[tokio::test]
async fn native_stream_projects_history_tools_controls_and_auth_once() {
    let (base, observed) = server(200, vec![json!({"model":"resolved", "message":{"role":"assistant","content":"answer"},"done":true,"done_reason":"stop"}).to_string().into_bytes()]).await;
    let call = ToolCall {
        id: ToolCallId::new(),
        provider_id: Some("native-id".into()),
        name: "calculate".into(),
        arguments: ToolArguments::Object {
            value: json!({"a":2}).as_object().unwrap().clone(),
        },
    };
    let mut input = request();
    input.messages[0].content.push(Content::Image(image()));
    input.messages.push(Message::new(
        Role::Assistant,
        vec![
            Content::Reasoning(Reasoning::Text {
                text: "thought".into(),
            }),
            Content::ToolCall(call.clone()),
        ],
    ));
    input.messages.push(Message::new(
        Role::Tool,
        vec![Content::ToolResult(ToolResult {
            call: call.id,
            provider_id: Some("native-id".into()),
            name: Some("calculate".into()),
            content: vec![
                ToolResultContent::Text {
                    text: "literal".into(),
                },
                ToolResultContent::Json {
                    value: json!({"a":2}),
                },
                ToolResultContent::Image(image()),
            ],
            is_error: true,
        })],
    ));
    input.tools.push(ToolDefinition {
        name: "calculate".into(),
        description: Some("math".into()),
        parameters: json!({"type":"object","$defs":{"input":{"type":"integer"}}})
            .as_object()
            .unwrap()
            .clone(),
    });
    input.thinking = Some(Thinking::Effort("custom-level".into()));
    input.options =
        json!({"options":{"num_ctx":1024,"future":true}, "keep_alive":-1, "truncate":false})
            .as_object()
            .unwrap()
            .clone();
    let mut input =
        ollama_types::ChatRequest::from_completion(&LanguageModel::new("test-model"), input)
            .unwrap();
    input.stream = Some(false);
    let stream = client(&base)
        .with_api_key("secret-key")
        .unwrap()
        .stream_native(input)
        .await
        .unwrap();
    let completion = language_model::collect_completion(stream).await.unwrap();
    assert_eq!(completion.message.content, vec![Content::text("answer")]);
    let observed = observed.await.unwrap();
    assert!(observed.headers.starts_with("POST /proxy/api/chat "));
    assert!(
        observed
            .headers
            .to_lowercase()
            .contains("authorization: bearer secret-key")
    );
    assert_eq!(observed.body["stream"], true);
    assert_eq!(observed.body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(observed.body["messages"][0]["images"], json!(["YWJj"]));
    assert_eq!(observed.body["messages"][1]["thinking"], "thought");
    assert_eq!(
        observed.body["messages"][1]["tool_calls"][0]["function"]["arguments"],
        json!({"a":2})
    );
    assert_eq!(
        observed.body["messages"][2]["content"],
        "Error: literal\n{\"a\":2}"
    );
    assert_eq!(observed.body["messages"][2]["tool_name"], "calculate");
    assert_eq!(observed.body["messages"][2]["tool_call_id"], "native-id");
    assert_eq!(observed.body["messages"][2]["images"], json!(["YWJj"]));
    assert_eq!(
        observed.body["tools"][0]["function"]["parameters"]["$defs"]["input"]["type"],
        "integer"
    );
    assert_eq!(observed.body["think"], "custom-level");
    assert_eq!(observed.body["options"]["future"], true);
    assert_eq!(observed.body["truncate"], false);
}

#[tokio::test]
async fn stream_handles_byte_boundaries_tools_thinking_and_final_usage() {
    let ndjson = concat!(
        " \t\r\n\n",
        "{\"model\":\"resolved\",\"message\":{\"role\":\"assistant\",\"thinking\":\"考え\"},\"done\":false}\r\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"index\":0,\"name\":\"work\",\"arguments\":{\"a\":1}}}]},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"provider-call\",\"function\":{\"index\":1,\"name\":\"broken\",\"arguments\":null}}]},\"done\":false}\n",
        "{\"done\":true,\"done_reason\":\"custom-stop\",\"prompt_eval_count\":10,\"prompt_eval_cached_count\":4,\"eval_count\":2,\"total_tokens\":12,\"total_duration\":100,\"future\":42}"
    );
    let (base, observed) = server(
        200,
        ndjson.as_bytes().chunks(1).map(<[u8]>::to_vec).collect(),
    )
    .await;
    let mut input =
        ollama_types::ChatRequest::from_completion(&LanguageModel::new("test-model"), request())
            .unwrap();
    input.stream = Some(false);
    let events = client(&base)
        .stream_native(input)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let events: Vec<_> = events.into_iter().collect::<Result<_, _>>().unwrap();
    assert!(
        matches!(&events[0], StreamEvent::Delta { index:0, delta: ContentDelta::Reasoning(text) } if text == "考え")
    );
    let StreamEvent::Finished(completion) = events.last().unwrap() else {
        panic!("missing final completion")
    };
    let message = &completion.message;
    let Content::ToolCall(call) = &message.content[2] else {
        panic!("expected tool")
    };
    assert!(call.provider_id.is_none());
    assert!(
        matches!(&events[2], StreamEvent::Delta { index:2, delta:ContentDelta::ToolCall { id, .. } } if id == &call.id)
    );
    let Content::ToolCall(invalid) = &message.content[3] else {
        panic!("expected tool")
    };
    assert_eq!(invalid.provider_id.as_deref(), Some("provider-call"));
    assert!(matches!(&invalid.arguments, ToolArguments::Invalid { raw } if raw == "null"));
    let metadata = message.generation.as_ref().unwrap();
    assert_eq!(metadata.model.as_deref(), Some("resolved"));
    assert_eq!(metadata.finish_reason.as_deref(), Some("custom-stop"));
    let usage = metadata.usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.cached_input_tokens, Some(4));
    assert_eq!(usage.output_tokens, Some(2));
    assert_eq!(usage.total_tokens, Some(12));
    assert_eq!(metadata.extra["future"], 42);
    assert_eq!(metadata.extra["total_duration"], 100);
    assert_eq!(
        metadata.extra["native_tool_calls"][1]["function"]["index"],
        1
    );
    assert_eq!(observed.await.unwrap().body["stream"], true);
}

#[tokio::test]
async fn idless_native_occurrences_are_distinct_even_with_repeated_indexes() {
    let chunk = json!({"message":{"role":"assistant","tool_calls":[{"function":{"index":0,"name":"same","arguments":{}}}]},"done":false}).to_string() + "\n";
    let end = json!({"done":true}).to_string();
    let (base, observed) = server(
        200,
        vec![
            chunk.clone().into_bytes(),
            chunk.into_bytes(),
            end.into_bytes(),
        ],
    )
    .await;
    let completion = language_model::collect_completion(
        client(&base)
            .stream(&LanguageModel::new("test-model"), request())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let calls: Vec<_> = completion
        .message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].id, calls[1].id);
    observed.await.unwrap();
}

#[tokio::test]
async fn truncation_and_provider_errors_never_finish_successfully() {
    let cases = [
        (
            200,
            "{\"message\":{\"role\":\"assistant\",\"content\":\"partial\"},\"done\":false}\n",
            Error::IncompleteStream,
        ),
        (
            200,
            "{\"error\":\"unsupported thinking value\"}\n",
            Error::Provider("unsupported thinking value".into()),
        ),
        (
            400,
            "{\"error\":\"unsupported thinking value\"}",
            Error::Http {
                status: 400,
                message: "unsupported thinking value".into(),
                retry_after: None,
            },
        ),
    ];
    for (status, body, expected) in cases {
        let (base, observed) = server(status, vec![body.as_bytes().to_vec()]).await;
        let stream = client(&base)
            .stream(&LanguageModel::new("test-model"), request())
            .await;
        if status != 200 {
            assert_eq!(stream.err().unwrap(), expected);
        } else {
            let events = stream.unwrap().collect::<Vec<_>>().await;
            assert_eq!(events.last().unwrap().as_ref().unwrap_err(), &expected);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamEvent::Finished(_))))
            );
        }
        observed.await.unwrap();
    }
}

#[tokio::test]
async fn diagnostics_are_complete_redacted_and_not_arbitrary_http_bodies() {
    let (base, observed) = server(
        401,
        vec![
            json!({"error":format!("invalid secret-key {}", "x".repeat(1000))})
                .to_string()
                .into_bytes(),
        ],
    )
    .await;
    let error = client(&base)
        .with_api_key("secret-key")
        .unwrap()
        .stream(&LanguageModel::new("test-model"), request())
        .await
        .err()
        .unwrap();
    let Error::Http {
        status, message, ..
    } = error
    else {
        panic!("expected HTTP error")
    };
    assert_eq!(status, 401);
    assert_eq!(message, format!("invalid [redacted] {}", "x".repeat(1000)));
    observed.await.unwrap();
    let (base, observed) = server(
        502,
        vec![b"<html>arbitrary secret proxy page</html>".to_vec()],
    )
    .await;
    let error = client(&base).list_models().await.unwrap_err();
    assert!(!error.to_string().contains("arbitrary secret"));
    observed.await.unwrap();
}

#[tokio::test]
async fn stream_checks_headers_before_returning_and_drop_closes_local_io() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (headers_sent, headers_received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n").await.unwrap();
        headers_sent.send(()).unwrap();
        let mut byte = [0];
        tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
            .await
            .unwrap()
            .unwrap()
    });
    assert!(matches!(
        client(&base)
            .stream_native(ollama_types::ChatRequest {
                model: " \t".into(),
                ..Default::default()
            })
            .await,
        Err(Error::InvalidRequest(_))
    ));
    let stream = tokio::time::timeout(
        Duration::from_secs(2),
        client(&base).stream(&LanguageModel::new("test-model"), request()),
    )
    .await
    .unwrap()
    .unwrap();
    headers_received.await.unwrap();
    drop(stream);
    assert_eq!(server.await.unwrap(), 0);
}

#[tokio::test]
async fn streaming_body_timeout_retains_its_classification() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut byte = [0];
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await;
    });
    let http = reqwest::Client::builder()
        .no_proxy()
        .read_timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let mut stream = Client::new(&base)
        .unwrap()
        .with_http_client(http)
        .stream(&LanguageModel::new("test-model"), request())
        .await
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap_err(), Error::Timeout);
    assert!(stream.next().await.is_none());
    drop(stream);
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_or_oversized_ndjson_ends_with_a_response_error() {
    for body in [vec![0xff, b'\n'], vec![b'x'; 8 * 1024 * 1024 + 1]] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            // Oversized input may be rejected before the server finishes sending.
            let _ = socket.write_all(&body).await;
        });
        let mut stream = client(&base)
            .stream(&LanguageModel::new("test-model"), request())
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await.unwrap(),
            Err(Error::InvalidResponse(_))
        ));
        assert!(stream.next().await.is_none());
        server.await.unwrap();
    }
}

#[test]
fn unsupported_content_and_ambiguous_history_are_explicit_errors() {
    for content in [
        Content::Image(Image {
            media_type: "image/png".into(),
            source: ImageSource::Url {
                url: "https://example.invalid/a.png".into(),
            },
        }),
        Content::Reasoning(Reasoning::Redacted {
            data: "opaque".into(),
        }),
    ] {
        let mut input = request();
        let role = if matches!(&content, Content::Reasoning(_)) {
            Role::Assistant
        } else {
            Role::User
        };
        input.messages = vec![Message::new(role, vec![content])];
        assert!(matches!(
            ollama_types::ChatRequest::from_completion(&LanguageModel::new("test-model"), input),
            Err(Error::Unsupported(_))
        ));
    }
    let mut input = request();
    input.messages[0].replay.push(ReplayData {
        provider: "other".into(),
        model: None,
        format: "unknown".into(),
        version: 1,
        payload: Value::Null,
    });
    assert!(matches!(
        ollama_types::ChatRequest::from_completion(&LanguageModel::new("test-model"), input),
        Err(Error::Unsupported(_))
    ));
    for key in [
        "model",
        "messages",
        "stream",
        "tools",
        "authorization",
        "unknown",
    ] {
        let mut input = request();
        input.options.insert(key.into(), json!("override"));
        assert!(matches!(
            ollama_types::ChatRequest::from_completion(&LanguageModel::new("test-model"), input),
            Err(Error::Unsupported(_))
        ));
    }
    let mut input = request();
    input.messages.push(Message::new(
        Role::Tool,
        vec![Content::ToolResult(ToolResult {
            call: ToolCallId::new(),
            provider_id: None,
            name: None,
            content: vec![],
            is_error: false,
        })],
    ));
    assert!(matches!(
        ollama_types::ChatRequest::from_completion(&LanguageModel::new("test-model"), input),
        Err(Error::InvalidRequest(_))
    ));
    let calls: Vec<_> = ["A", "B"]
        .into_iter()
        .map(|value| ToolCall {
            id: ToolCallId::new(),
            provider_id: None,
            name: "same_name".into(),
            arguments: ToolArguments::Object {
                value: json!({"input": value}).as_object().unwrap().clone(),
            },
        })
        .collect();
    let results = calls
        .iter()
        .rev()
        .map(|call| {
            Content::ToolResult(ToolResult {
                call: call.id.clone(),
                provider_id: None,
                name: Some(call.name.clone()),
                content: vec![ToolResultContent::Text {
                    text: call.id.0.clone(),
                }],
                is_error: false,
            })
        })
        .collect();
    let mut input = request();
    input.messages.push(Message::new(
        Role::Assistant,
        calls.into_iter().map(Content::ToolCall).collect(),
    ));
    input.messages.push(Message::new(Role::Tool, results));
    assert!(matches!(
        ollama_types::ChatRequest::from_completion(
            &LanguageModel::new("test-model"),
            input.clone()
        ),
        Err(Error::InvalidRequest(_))
    ));
    input.messages.last_mut().unwrap().content.swap(0, 1);
    assert!(
        ollama_types::ChatRequest::from_completion(&LanguageModel::new("test-model"), input)
            .is_ok()
    );
    assert!(Client::new("https://user:secret@example.invalid").is_err());
    assert!(
        client("http://localhost")
            .with_api_key("secret\r\nheader: value")
            .is_err()
    );
}

#[test]
fn malformed_arguments_are_retained_and_duplicate_provider_ids_rejected() {
    let mut fold = crate::streaming::Fold::default();
    let response = json!({"message":{"role":"assistant", "tool_calls":[
        {"function":{"name":"scalar","arguments":42}},
        {"function":{"name":"missing"}},
        {"function":{"name":"string","arguments":"{broken"}}
    ]},"done":true});
    let events = fold
        .apply(serde_json::from_value(response).unwrap())
        .unwrap();
    let StreamEvent::Finished(completion) = events.back().unwrap() else {
        panic!("expected finished")
    };
    for (content, raw) in completion
        .message
        .content
        .iter()
        .zip(["42", "", "\"{broken\""])
    {
        assert!(
            matches!(content, Content::ToolCall(ToolCall { arguments:ToolArguments::Invalid { raw: value }, .. }) if value == raw)
        );
    }
    let mut fold = crate::streaming::Fold::default();
    let repeated = json!({"message":{"role":"assistant","tool_calls":[
        {"id":"same", "function":{"name":"a","arguments":{}}},
        {"id":"same", "function":{"name":"b","arguments":{}}}
    ]},"done":true});
    assert!(matches!(
        fold.apply(serde_json::from_value(repeated).unwrap()),
        Err(Error::InvalidResponse(_))
    ));
}
