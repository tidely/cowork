use super::*;
use crate::conversion::messages;
use serde_json::json;

#[test]
fn multimodal_request_retains_server_options_and_tool_history() {
    let fixture = json!({
        "model": "qwen3-vl",
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "Describe and calculate."},
                {"type": "image_url", "image_url": {
                    "url": "data:image/png;base64,aGVsbG8=", "detail": "auto", "vendor": true
                }},
                {"type": "image_url", "image_url": "https://example.test/image.png"},
                {"type": "future_part", "payload": {"x": 2}}
            ]},
            {"role": "assistant", "tool_calls": [{
                "id": "call_a", "type": "function",
                "function": {"name": "calculate", "arguments": "{\"x\":2}"}
            }]},
            {"role": "tool", "tool_call_id": "call_a", "name": "calculate", "content": "4"}
        ],
        "tools": [{"type": "function", "function": {
            "name": "calculate", "parameters": {"type": "object"}, "strict": false
        }}],
        "tool_choice": {"type": "function", "function": {"name": "calculate"}},
        "stream": true, "stream_options": {"include_usage": true, "continuous_usage_stats": true},
        "stop": ["END"], "top_k": 10, "chat_template_kwargs": {"enable_thinking": false}
    });
    let request: ChatCompletionRequest = serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(request.extra["top_k"], 10);
    let Content::Parts(parts) = request.messages[0].content.as_ref().unwrap() else {
        panic!("expected content parts")
    };
    assert!(matches!(parts[1].image_url, Some(ImageUrl::Object(_))));
    assert!(matches!(parts[2].image_url, Some(ImageUrl::Url(_))));
    assert_eq!(parts[3].extra["payload"], json!({"x": 2}));
    assert_eq!(serde_json::to_value(request).unwrap(), fixture);
}

#[test]
fn full_responses_preserve_reasoning_raw_arguments_and_vendor_fields() {
    let fixture = json!({
        "id": "chatcmpl-vllm", "object": "chat.completion", "created": 1700000000,
        "model": "qwen3", "choices": [{
            "index": 0, "finish_reason": "tool_calls", "stop_reason": 151645,
            "message": {
                "role": "assistant", "content": "Calling tool.",
                "reasoning": "Reasoning one", "reasoning_content": "Reasoning two",
                "refusal": "", "citations": [{"id": 1}],
                "tool_calls": [{"type": "function", "function": {
                    "name": "calculate", "arguments": "{malformed"
                }}]
            }
        }],
        "usage": {
            "prompt_tokens": 10, "completion_tokens": 7, "total_tokens": 17,
            "completion_tokens_details": {"reasoning_tokens": 3}, "vendor_count": 9
        }, "prompt_token_ids": [2, 3]
    });
    let response: ChatCompletionResponse = serde_json::from_value(fixture.clone()).unwrap();
    let message = &response.choices[0].message;
    assert_eq!(message.reasoning.as_deref(), Some("Reasoning one"));
    assert_eq!(message.reasoning_content.as_deref(), Some("Reasoning two"));
    let call = &message.tool_calls.as_ref().unwrap()[0];
    assert_eq!(call.id, None);
    assert_eq!(call.function.arguments, "{malformed");
    assert_eq!(serde_json::to_value(response).unwrap(), fixture);

    let ollama: ChatCompletionResponse = serde_json::from_value(json!({
        "id": "chatcmpl-ollama", "object": "chat.completion", "model": "qwen3:8b",
        "choices": [{"index": 0, "message": {
            "role": "assistant", "content": "Hello", "reasoning": null, "tool_calls": null
        }, "finish_reason": "stop"}], "usage": null
    }))
    .unwrap();
    assert_eq!(
        ollama.choices[0].message.content,
        Some(Content::Text("Hello".into()))
    );
    assert_eq!(ollama.usage, None);
}

#[test]
fn chunks_keep_fragment_identity_and_accept_null_and_usage_only_payloads() {
    // Interleaved choice/tool indexes must survive without global call IDs.
    let fixtures = [
        json!({"choices": [{"index": 1, "delta": {
            "role": "assistant", "content": null, "tool_calls": [
                {"index": 2, "id": "call_c", "type": "function", "function": {
                    "name": "calculate", "arguments": "{\"x\":"
                }}
            ]
        }, "finish_reason": null}], "usage": null}),
        json!({"choices": [{"index": 1, "delta": {
            "reasoning": "a", "reasoning_content": "b", "tool_calls": [
                {"index": 2, "function": {"arguments": "3}"}},
                {"index": 0, "function": null}
            ]
        }}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "server_specific"}]}),
        json!({"choices": [{"index": 1, "delta": null, "finish_reason": "tool_calls"}]}),
        json!({"choices": [{"index": 2, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": null,
            "total_tokens": 14, "prompt_tokens_details": {"cached_tokens": 4}},
            "metrics": {"tokens_per_second": 22}}),
    ];
    let chunks: Vec<ChatCompletionChunk> = fixtures
        .into_iter()
        .map(|value| serde_json::from_value(value).unwrap())
        .collect();
    let calls = chunks[1].choices[0]
        .delta
        .as_ref()
        .unwrap()
        .tool_calls
        .as_ref()
        .unwrap();
    assert_eq!(calls[0].index, 2);
    assert_eq!(calls[0].id, None);
    assert_eq!(
        calls[0].function.as_ref().unwrap().arguments.as_deref(),
        Some("3}")
    );
    assert!(calls[1].function.is_none());
    assert!(chunks[3].choices[0].delta.is_none());
    assert!(chunks[4].choices[0].delta.is_none());
    assert!(chunks[5].choices.is_empty());
    assert_eq!(chunks[5].usage.as_ref().unwrap().completion_tokens, None);
    assert_eq!(chunks[5].extra["metrics"], json!({"tokens_per_second": 22}));
}

#[test]
fn model_cards_and_errors_preserve_server_variations() {
    for fixture in [
        json!({"object": "list", "data": [{"id": "qwen3:8b", "object": "model",
            "created": 1700000000, "owned_by": "library"}]}),
        json!({"object": "list", "data": [{"id": "Qwen/Qwen3", "object": "model",
            "owned_by": "vllm", "max_model_len": 32768, "permission": []}]}),
    ] {
        let models: ModelList = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(models).unwrap(), fixture);
    }
    for code in [json!(400), json!("model_not_found")] {
        let fixture = json!({"error": {"message": "Unknown model", "type": "invalid_request_error",
            "code": code, "details": {"retryable": false}}, "request_id": "req-a"});
        let error: ErrorResponse = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(error.error.code, Some(code));
        assert_eq!(serde_json::to_value(error).unwrap(), fixture);
    }
}

#[test]
fn generic_requests_expand_tool_results_and_keep_typed_options_separate() {
    use language_model::{
        Content as GenericContent, Message as GenericMessage, Role, ToolArguments, ToolCall,
        ToolCallId, ToolResult, ToolResultContent,
    };
    for media_type in ["image/png", "IMAGE/PNG"] {
        let projected: Vec<Message> = messages(GenericMessage::new(
            Role::User,
            vec![GenericContent::Image(language_model::Image {
                media_type: media_type.into(),
                source: language_model::ImageSource::Base64 {
                    data: "aGVsbG8=".into(),
                },
            })],
        ))
        .unwrap();
        let Some(Content::Parts(parts)) = &projected[0].content else {
            panic!()
        };
        let Some(ImageUrl::Object(image)) = &parts[0].image_url else {
            panic!()
        };
        assert_eq!(image.url, format!("data:{media_type};base64,aGVsbG8="));
    }
    for media_type in [
        "image/",
        "text/plain",
        "image/png; charset=utf-8",
        "image/png,extra",
        "image/png\r\n",
    ] {
        assert!(matches!(
            messages(GenericMessage::new(
                Role::User,
                vec![GenericContent::Image(language_model::Image {
                    media_type: media_type.into(),
                    source: language_model::ImageSource::Base64 {
                        data: "aGVsbG8=".into()
                    },
                })],
            )),
            Err(language_model::Error::InvalidRequest(_))
        ));
    }
    let calls: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|name| ToolCall {
            id: ToolCallId(name.into()),
            provider_id: None,
            name: name.into(),
            arguments: ToolArguments::from_raw("{}"),
        })
        .collect();
    let mut request = language_model::CompletionRequest {
        messages: vec![
            GenericMessage::new(Role::Assistant, calls.iter().cloned().map(GenericContent::ToolCall).collect()),
            GenericMessage::new(Role::Tool, calls.iter().map(|call| GenericContent::ToolResult(ToolResult {
                call: call.id.clone(), provider_id: None, name: None,
                content: vec![ToolResultContent::Json { value: json!({"ok":true}) }],
                is_error: false,
            })).collect()),
        ],
        options: json!({"temperature":0.2,"stop":["end"],"tool_choice":{"type":"function","function":{"name":"one","vendor":1}},"chat_template_kwargs":{"thinking":true},"stream_options":{"include_usage":false,"vendor":2}}).as_object().unwrap().clone(),
        ..Default::default()
    };
    let native = ChatCompletionRequest::from_completion(
        &language_model::LanguageModel::new("local"),
        request.clone(),
    )
    .unwrap();
    assert_eq!(native.model, "local");
    assert_eq!(native.messages.len(), 3);
    let calls = native.messages[0].tool_calls.as_ref().unwrap();
    for (index, message) in native.messages[1..].iter().enumerate() {
        assert_eq!(message.tool_call_id, calls[index].id);
        assert_eq!(message.content, Some(Content::Text("{\"ok\":true}".into())));
    }
    assert_eq!(native.temperature, Some(0.2));
    assert_eq!(native.stop, Some(Stop::Many(vec!["end".into()])));
    assert!(!native.extra.contains_key("temperature"));
    assert!(!native.extra.contains_key("tool_choice"));
    assert_eq!(
        native.extra["chat_template_kwargs"],
        json!({"thinking":true})
    );
    let Some(ToolChoice::Named(choice)) = native.tool_choice.as_ref() else {
        panic!()
    };
    assert_eq!(choice.function.extra["vendor"], 1);
    assert_eq!(native.stream_options.unwrap().include_usage, Some(false));
    let typed = language_model::CompletionRequest {
        max_output_tokens: Some(64),
        temperature: Some(0.5),
        top_p: Some(0.9),
        stop: vec!["end".into()],
        seed: Some(7),
        ..language_model::CompletionRequest::new(vec![GenericMessage::user("hi")])
    };
    let native = ChatCompletionRequest::from_completion(
        &language_model::LanguageModel::new("local"),
        typed.clone(),
    )
    .unwrap();
    assert_eq!(
        (
            native.max_tokens,
            native.temperature,
            native.top_p,
            native.seed
        ),
        (Some(64), Some(0.5), Some(0.9), Some(7))
    );
    assert_eq!(native.stop, Some(Stop::Many(vec!["end".into()])));
    let mut conflicting = typed;
    conflicting
        .options
        .insert("max_completion_tokens".into(), 1.into());
    assert!(matches!(
        ChatCompletionRequest::from_completion(
            &language_model::LanguageModel::new("local"),
            conflicting,
        ),
        Err(language_model::Error::InvalidRequest(_))
    ));
    let mut standalone = request.messages[1].clone();
    for content in &mut standalone.content {
        let GenericContent::ToolResult(result) = content else {
            unreachable!()
        };
        result.provider_id = Some(format!("call_{}", result.call.0));
    }
    let projected: Vec<Message> = messages(standalone.clone()).unwrap();
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0].tool_call_id.as_deref(), Some("call_one"));
    for provider_id in [None, Some(String::new())] {
        let GenericContent::ToolResult(result) = &mut standalone.content[0] else {
            unreachable!()
        };
        result.provider_id = provider_id;
        assert!(matches!(
            messages(standalone.clone()),
            Err(language_model::Error::InvalidRequest(_))
        ));
    }
    // The history pass must reject a duplicate even though each message is valid.
    request.messages.push(request.messages[1].clone());
    assert!(matches!(
        ChatCompletionRequest::from_completion(
            &language_model::LanguageModel::new("local"),
            request
        ),
        Err(language_model::Error::InvalidRequest(_))
    ));
}

#[test]
fn generic_options_reject_invalid_native_shapes_and_preserve_unknown_extensions() {
    assert!(matches!(
        ChatCompletionRequest::from_completion(
            &language_model::LanguageModel::new(""),
            language_model::CompletionRequest::default(),
        ),
        Err(language_model::Error::InvalidRequest(_))
    ));
    for (key, value) in [
        ("temperature", json!("warm")),
        ("max_tokens", json!(-1)),
        ("parallel_tool_calls", json!(1)),
        ("stop", json!([1])),
        ("tool_choice", json!({"function":{"name":"x"}})),
        ("tool_choice", json!(["function", {"name":"x"}])),
        ("tool_choice", json!({"type":"function","function":["x"]})),
        ("stream_options", json!({"include_usage":null})),
        ("stream_options", json!([true])),
        ("stream_options", json!(null)),
    ] {
        let request = language_model::CompletionRequest {
            options: [(key.into(), value)].into_iter().collect(),
            ..Default::default()
        };
        assert!(
            matches!(
                ChatCompletionRequest::from_completion(
                    &language_model::LanguageModel::new("local"),
                    request
                ),
                Err(language_model::Error::InvalidRequest(_))
            ),
            "{key}"
        );
    }
    let request = language_model::CompletionRequest {
        options: json!({"top_k":7,"temperature":null})
            .as_object()
            .unwrap()
            .clone(),
        ..Default::default()
    };
    let native = ChatCompletionRequest::from_completion(
        &language_model::LanguageModel::new("local"),
        request,
    )
    .unwrap();
    assert_eq!(native.extra["top_k"], 7);
    assert_eq!(native.temperature, None);
    assert_eq!(native.stream_options, None);
}

#[test]
fn shared_thinking_uses_advertised_efforts_and_explicit_enable_disable_mappings() {
    use language_model::{CompletionRequest, Error, LanguageModel, Thinking, ThinkingLevel};

    let convert = |model: &LanguageModel, thinking| {
        ChatCompletionRequest::from_completion(
            model,
            CompletionRequest {
                thinking,
                ..Default::default()
            },
        )
    };
    let mut model = LanguageModel::new("local");
    assert_eq!(convert(&model, None).unwrap().reasoning_effort, None);
    assert_eq!(
        convert(&model, Some(Thinking::Effort("model-defined".into())))
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("model-defined")
    );
    for thinking in [Thinking::Enabled, Thinking::Disabled] {
        assert!(matches!(
            convert(&model, Some(thinking)),
            Err(Error::Unsupported(_))
        ));
    }
    model.thinking.effort_levels = Some(vec![ThinkingLevel {
        value: "deep".into(),
        label: "Deep".into(),
    }]);
    assert!(matches!(
        convert(&model, Some(Thinking::Effort("unadvertised".into()))),
        Err(Error::Unsupported(_))
    ));
    model.thinking.default_effort = Some("deep".into());
    assert_eq!(
        convert(&model, Some(Thinking::Enabled))
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("deep")
    );
    model.thinking.default_effort = Some("unadvertised".into());
    assert!(matches!(
        convert(&model, Some(Thinking::Enabled)),
        Err(Error::Unsupported(_))
    ));
    // The off code is explicitly configured, not a selectable enabled level.
    model.thinking.disabled_effort = Some("off-code".into());
    assert_eq!(
        convert(&model, Some(Thinking::Disabled))
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("off-code")
    );
    model.thinking.disabled_effort = Some(" ".into());
    assert!(matches!(
        convert(&model, Some(Thinking::Disabled)),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn shared_thinking_conflicts_with_raw_effort_but_preserves_vendor_controls() {
    use language_model::{CompletionRequest, Error, LanguageModel, Thinking};
    let model = LanguageModel::new("local");
    for raw in [json!("raw-effort"), json!(null)] {
        let mut request = CompletionRequest {
            options: [("reasoning_effort".into(), raw)].into_iter().collect(),
            ..Default::default()
        };
        assert!(ChatCompletionRequest::from_completion(&model, request.clone()).is_ok());
        request.thinking = Some(Thinking::Effort("shared-effort".into()));
        assert!(matches!(
            ChatCompletionRequest::from_completion(&model, request),
            Err(Error::InvalidRequest(_))
        ));
    }
    let request = CompletionRequest {
        thinking: Some(Thinking::Effort("custom".into())),
        options: [(
            "chat_template_kwargs".into(),
            json!({"enable_thinking":false}),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let native = ChatCompletionRequest::from_completion(&model, request).unwrap();
    assert_eq!(native.reasoning_effort.as_deref(), Some("custom"));
    assert_eq!(
        native.extra["chat_template_kwargs"]["enable_thinking"],
        false
    );
}
