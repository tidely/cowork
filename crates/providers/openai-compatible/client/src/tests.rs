use super::*;
use futures::StreamExt as _;
use language_model::{
    Content, ContentDelta, Message, Reasoning, Role, StreamEvent, ToolArguments, ToolCall,
    ToolCallId, ToolResult, ToolResultContent,
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
        messages: vec![Message::new(Role::User, vec![Content::text("hi")])],
        ..Default::default()
    }
}

fn model() -> LanguageModel {
    LanguageModel::new("any/local-model")
}

fn event(value: Value) -> String {
    format!("data: {value}\r\n\r\n")
}

#[tokio::test]
async fn discovery_preserves_unknown_model_metadata_and_prefix() {
    let (client, task) = server(
        200,
        json!({"data":[{"id":"local","owned_by":"library","max_model_len":8192}]}).to_string(),
        false,
    )
    .await;
    let models = client.list_models().await.unwrap();
    assert_eq!(models[0].metadata["max_model_len"], 8192);
    assert_eq!(models[0].context_window, Some(8192));
    assert_eq!(models[0].tool_calls, None);
    assert!(task.await.unwrap().starts_with("GET /proxy/v1/models "));
}

#[tokio::test]
async fn stream_tool_only_and_request_extension_conversion() {
    let response = json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"native","type":"function","function":{"name":"next","arguments":"{bad"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":3},"vendor_metadata":{"x":1}});
    let (client, task) = server(200, event(response) + "data: [DONE]\n\n", true).await;
    let client = client.with_api_key("secret-key").unwrap();
    assert!(!format!("{client:?}").contains("secret-key"));
    let id = ToolCallId("local-call".into());
    let mut request = input();
    request.messages.push(Message::new(
        Role::Assistant,
        vec![Content::ToolCall(ToolCall {
            id: id.clone(),
            provider_id: None,
            name: "calculate".into(),
            arguments: ToolArguments::from_raw("{}"),
        })],
    ));
    request.messages.push(Message::new(
        Role::Tool,
        vec![Content::ToolResult(ToolResult {
            call: id,
            provider_id: None,
            name: Some("calculate".into()),
            content: vec![ToolResultContent::Json {
                value: json!({"answer":3}),
            }],
            is_error: false,
        })],
    ));
    let mut request =
        openai_compatible_types::ChatCompletionRequest::from_completion(&model(), request).unwrap();
    request.temperature = Some(0.2);
    request.reasoning_effort = Some("low".into());
    request.extra.insert("top_k".into(), 5.into());
    request.extra.insert(
        "chat_template_kwargs".into(),
        json!({"enable_thinking":true}),
    );
    let completion =
        language_model::collect_completion(client.stream_native(request).await.unwrap())
            .await
            .unwrap();
    let Content::ToolCall(call) = &completion.message.content[0] else {
        panic!()
    };
    assert_eq!(
        call.arguments,
        ToolArguments::Invalid { raw: "{bad".into() }
    );
    assert_eq!(
        completion.message.generation.unwrap().extra["vendor_metadata"]["x"],
        1
    );
    let captured = task.await.unwrap();
    assert!(captured.starts_with("POST /proxy/v1/chat/completions "));
    assert!(
        captured
            .to_lowercase()
            .contains("authorization: bearer secret-key")
    );
    let body: Value = serde_json::from_str(captured.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["top_k"], 5);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["id"],
        body["messages"][2]["tool_call_id"]
    );
    assert_eq!(body["messages"][2]["content"], "{\"answer\":3}");
}

#[tokio::test]
async fn byte_split_stream_interleaves_calls_reasoning_and_final_usage() {
    let mut body = String::from("\u{feff}: heartbeat\r\nevent: message\r\n\r\n");
    body.push_str(&event(json!({"choices":[{"index":0,"delta":{"role":"assistant","reasoning":"考","reasoning_content":"考"}}]})));
    body.push_str(&event(json!({"choices":[{"index":0,"delta":{"content":"Hello","tool_calls":[{"index":1,"id":"b","function":{"name":"se","arguments":"{\"x\":"}},{"index":0,"id":"a","function":{"name":"fi","arguments":"{"}}]}}]})));
    body.push_str(&event(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"rst","arguments":"bad"}},{"index":1,"function":{"name":"cond","arguments":"1}"}}]},"finish_reason":"tool_calls"}]})));
    // Multi-line data fields and lone CR are valid SSE framing.
    body.push_str("data: {\"choices\":[],\rdata: \"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\r\rdata: [DONE]\r\r");
    let (client, task) = server(200, body, true).await;
    let mut stream = client.stream(&model(), input()).await.unwrap();
    let mut ids = std::collections::HashSet::new();
    let mut finished = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::Delta {
                delta: ContentDelta::ToolCall { id, .. },
                ..
            } => {
                ids.insert(id);
            }
            StreamEvent::Finished(value) => {
                assert!(finished.is_none());
                finished = Some(value);
            }
            _ => assert!(finished.is_none()),
        }
    }
    let completion = finished.unwrap();
    assert_eq!(
        completion.message.content[0],
        Content::Reasoning(Reasoning::Text { text: "考".into() })
    );
    let calls: Vec<_> = completion
        .message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| ids.contains(&call.id)));
    assert_eq!(calls[1].name, "second");
    assert_eq!(
        calls[0].arguments,
        ToolArguments::Invalid { raw: "{bad".into() }
    );
    assert_eq!(
        completion
            .message
            .generation
            .unwrap()
            .usage
            .unwrap()
            .reasoning_tokens,
        Some(2)
    );
    let request = task.await.unwrap();
    assert!(request.contains("\"include_usage\":true"));
}

#[tokio::test]
async fn ollama_compatible_text_and_legacy_reasoning() {
    let body = event(
        json!({"id":"chatcmpl-1","model":"local","choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}),
    ) + &event(
        json!({"choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"length"}]}),
    ) + "data: [DONE]\n\n";
    let (client, task) = server(200, body, true).await;
    let request = CompletionRequest {
        thinking: Some(language_model::Thinking::Effort("custom-effort".into())),
        ..input()
    };
    let result =
        language_model::collect_completion(client.stream(&model(), request).await.unwrap())
            .await
            .unwrap();
    assert_eq!(result.message.content[1], Content::text("answer"));
    assert_eq!(
        result.message.generation.as_ref().unwrap().finish_reason,
        Some(language_model::FinishReason::Length)
    );
    let mut history = input();
    history.messages.push(result.message);
    assert!(encode_request(history).is_ok());
    let captured = task.await.unwrap();
    let body: Value = serde_json::from_str(captured.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["reasoning_effort"], "custom-effort");
}

#[tokio::test]
async fn stream_rejects_eof_missing_finish_and_content_after_finish() {
    let text = event(json!({"choices":[{"index":0,"delta":{"content":"partial"}}]}));
    let finish = event(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
    for (body, incomplete) in [
        (text.clone(), true),
        (finish.clone(), true),
        (text.clone() + "data: [DONE]\n\n", true),
        (finish + &text + "data: [DONE]\n\n", false),
    ] {
        let (client, task) = server(200, body, true).await;
        let error =
            language_model::collect_completion(client.stream(&model(), input()).await.unwrap())
                .await
                .unwrap_err();
        if incomplete {
            assert_eq!(error, Error::IncompleteStream);
        } else {
            assert!(matches!(error, Error::InvalidResponse(_)));
        }
        task.await.unwrap();
    }
    let body = text
        + &event(
            json!({"choices":[{"index":0,"delta":{"reasoning":"late thought"},"finish_reason":"stop"}]}),
        )
        + "data: [DONE]\n\n";
    let (client, task) = server(200, body, true).await;
    let error = language_model::collect_completion(client.stream(&model(), input()).await.unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)));
    task.await.unwrap();

    for choices in [
        json!([{"index":0,"delta":{}},{"index":1,"delta":{}}]),
        json!([{"index":0,"delta":{"refusal":"cannot comply"}}]),
        json!([{"index":1,"delta":{"content":"other choice"}}]),
    ] {
        let (client, task) = server(200, event(json!({"choices":choices})), true).await;
        let error =
            language_model::collect_completion(client.stream(&model(), input()).await.unwrap())
                .await
                .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn errors_are_explicit_and_redact_credentials() {
    let (client, task) = server(401, "secret-key".repeat(2000), false).await;
    let error = client
        .with_api_key("secret-key")
        .unwrap()
        .stream(&model(), input())
        .await
        .err()
        .unwrap();
    assert!(!error.to_string().contains("secret-key"));
    assert!(matches!(error, Error::Http { status: 401, .. }));
    task.await.unwrap();
    for prefix in [String::new(), "x".repeat(8189)] {
        let (client, task) = server(
            200,
            event(json!({"error":{"message":format!("{prefix}secret-key")}})),
            true,
        )
        .await;
        let error = language_model::collect_completion(
            client
                .with_api_key("secret-key")
                .unwrap()
                .stream(&model(), input())
                .await
                .unwrap(),
        )
        .await
        .unwrap_err();
        let expected = format!("{prefix}[REDACTED]");
        assert_eq!(error, Error::Provider(expected));
        task.await.unwrap();
    }
    for (payload, expected) in [
        (json!("secret-key"), "[REDACTED]"),
        (json!({"message":"secret-key"}), "[REDACTED]"),
        (json!(null), "unspecified provider error"),
        (json!(["unexpected"]), "unspecified provider error"),
        (json!({"message":42}), "unspecified provider error"),
    ] {
        // Success-looking fields must not hide an explicit error, even null.
        let value = json!({"error":payload,"data":[],"choices":[]});
        let (client, task) = server(200, value.to_string(), false).await;
        let error = client
            .with_api_key("secret-key")
            .unwrap()
            .list_models()
            .await
            .unwrap_err();
        assert_eq!(error, Error::Provider(expected.into()));
        task.await.unwrap();

        let (client, task) = server(200, event(value), true).await;
        let error = language_model::collect_completion(
            client
                .with_api_key("secret-key")
                .unwrap()
                .stream(&model(), input())
                .await
                .unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, Error::Provider(expected.into()));
        task.await.unwrap();
    }
}

#[test]
fn refuses_lossy_conversion_options() {
    let mut request = input();
    request.options.insert("messages".into(), json!([]));
    assert!(matches!(
        encode_request(request),
        Err(Error::InvalidRequest(_))
    ));
    let mut request = input();
    request.options.insert("n".into(), 2.into());
    assert!(matches!(
        encode_request(request),
        Err(Error::Unsupported(_))
    ));
    let mut request = input();
    request.messages = vec![Message::new(
        Role::Assistant,
        vec![Content::Reasoning(Reasoning::Redacted {
            data: "opaque".into(),
        })],
    )];
    assert!(matches!(
        encode_request(request),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        response::reasoning(Some("a".into()), Some("b".into())),
        Err(Error::Unsupported(_))
    ));
    let mut request = input();
    request.messages.push(Message::new(
        Role::Assistant,
        vec![Content::ToolCall(ToolCall {
            id: ToolCallId::new(),
            provider_id: Some("provider".into()),
            name: "tool".into(),
            arguments: ToolArguments::from_raw("{}"),
        })],
    ));
    assert!(matches!(
        encode_request(request.clone()),
        Err(Error::InvalidRequest(_))
    ));
    request.messages.push(Message::new(
        Role::User,
        vec![Content::text("another prompt")],
    ));
    assert!(matches!(
        encode_request(request),
        Err(Error::InvalidRequest(_))
    ));
    let mut request = input();
    let definition = language_model::ToolDefinition {
        name: "same".into(),
        description: None,
        parameters: Default::default(),
    };
    request.tools = vec![definition.clone(), definition];
    assert!(matches!(
        encode_request(request),
        Err(Error::InvalidRequest(_))
    ));
    let mut request = input();
    request
        .options
        .insert("stream_options".into(), json!({"include_usage":false}));
    assert_eq!(
        encode_request(request).unwrap()["stream_options"]["include_usage"],
        false
    );
    assert!(
        encode_request(input())
            .unwrap()
            .get("stream_options")
            .is_none()
    );
}

#[test]
fn retry_after_reads_seconds_dates_and_milliseconds() {
    let headers = |pairs: &[(&'static str, &str)]| {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    };
    assert_eq!(
        retry_after(&headers(&[("retry-after", "30")])),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        retry_after(&headers(&[
            ("retry-after", "30"),
            ("retry-after-ms", "1500")
        ])),
        Some(Duration::from_millis(1500))
    );
    let later = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
    assert!(
        retry_after(&headers(&[("retry-after", &later)]))
            .is_some_and(|delay| delay > Duration::from_secs(100))
    );
    assert_eq!(
        retry_after(&headers(&[(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:00 GMT"
        )])),
        Some(Duration::ZERO)
    );
    assert_eq!(retry_after(&headers(&[("retry-after", "soon")])), None);
    assert_eq!(retry_after(&HeaderMap::new()), None);
}

#[test]
fn constructor_rejects_credential_urls_and_header_injection() {
    for url in [
        "http://name:secret@localhost/v1",
        "http://localhost/v1?token=secret",
        "http://localhost/v1#secret",
        "file:///tmp",
    ] {
        let error = Client::new(url).unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
    assert!(
        Client::new("http://localhost/v1")
            .unwrap()
            .with_api_key("key\r\nx: injected")
            .is_err()
    );
}

// Serialization is only for assertions against the actual outgoing JSON shape.
fn encode_request(request: CompletionRequest) -> Result<Value, Error> {
    let mut native =
        openai_compatible_types::ChatCompletionRequest::from_completion(&model(), request)?;
    native.stream = Some(true);
    Ok(serde_json::to_value(native).unwrap())
}
