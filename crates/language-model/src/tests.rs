use super::*;
use futures::{executor::block_on, stream};
use serde_json::json;

fn image() -> Image {
    Image {
        media_type: "image/png".into(),
        source: ImageSource::Base64 {
            data: "YWJj".into(),
        },
    }
}

#[test]
fn frozen_record_migrates_without_losing_content_or_provider_replay() {
    // A literal historical record exercises decoding independently of the
    // current encoder; changing both sides must not erase compatibility.
    let stored: Envelope = serde_json::from_value(json!({
        "type_id": "cowork.llm.message.v1",
        "payload": {
            "id": "historical-user-message",
            "role": "user",
            "content": [{"kind": "text", "text": "Hello"}],
            "generation": null,
            "replay": []
        }
    }))
    .unwrap();
    let upgraded = stored.decode_message().unwrap();
    assert_eq!(upgraded.id.0, "historical-user-message");
    assert_eq!(upgraded.content, [Content::text("Hello")]);
    assert_eq!(Envelope::encode_message(&upgraded).unwrap(), stored);

    let call = ToolCallId::new();
    let mut reply = Message::new(
        Role::Assistant,
        vec![
            Content::Reasoning(Reasoning::Text {
                text: "consider".into(),
            }),
            Content::Reasoning(Reasoning::Redacted {
                data: "opaque".into(),
            }),
            Content::text("answer"),
            Content::Image(image()),
            Content::ToolCall(ToolCall {
                id: call.clone(),
                provider_id: Some("provider-1".into()),
                name: "weather".into(),
                arguments: ToolArguments::from_raw("{broken"),
            }),
        ],
    );
    reply.generation = Some(GenerationMetadata {
        provider: "future-provider".into(),
        model: Some("m".into()),
        finish_reason: Some("future-reason".into()),
        usage: Some(Usage {
            output_tokens: Some(2),
            ..Usage::default()
        }),
        extra: json!({"unknown": {"nested": [1, 2]}})
            .as_object()
            .unwrap()
            .clone(),
    });
    reply.replay.push(ReplayData {
        provider: "future-provider".into(),
        model: Some("m".into()),
        format: "opaque-items".into(),
        version: 93,
        payload: json!([{"signature": "s"}]),
    });
    let result = Message::new(
        Role::Tool,
        vec![Content::ToolResult(ToolResult {
            call,
            provider_id: Some("provider-1".into()),
            name: Some("weather".into()),
            content: vec![
                ToolResultContent::Text {
                    text: "{\"looks\":\"json\"}".into(),
                },
                ToolResultContent::Json {
                    value: json!({"temperature": 2}),
                },
                ToolResultContent::Image(image()),
            ],
            is_error: true,
        })],
    );
    for original in [reply, result] {
        let envelope = Envelope::encode_message(&original).unwrap();
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let loaded: Envelope = serde_json::from_slice(&bytes).unwrap();
        let latest = loaded.decode_message().unwrap();
        assert_eq!(latest, original);
        assert_eq!(Envelope::encode_message(&latest).unwrap(), envelope);
        let historical: v1::Message = serde_json::from_value(envelope.payload).unwrap();
        assert_eq!(Message::from(historical), original);
    }
}

#[test]
fn unknown_record_or_nested_field_is_refused_and_original_can_be_retained() {
    let original = Envelope {
        type_id: "cowork.llm.message.v200".into(),
        payload: json!({"future": [1, 2, 3]}),
    };
    assert!(matches!(
        original.decode_message(),
        Err(SchemaError::UnsupportedType(_))
    ));
    assert_eq!(original.payload, json!({"future": [1, 2, 3]}));

    let valid =
        Envelope::encode_message(&Message::new(Role::User, vec![Content::text("hi")])).unwrap();
    let mut nested = valid.clone();
    nested.payload["content"][0]["future_semantics"] = json!(true);
    assert!(matches!(
        nested.decode_message(),
        Err(SchemaError::InvalidPayload(_))
    ));
    assert_eq!(nested.payload["content"][0]["future_semantics"], true);
    let mut unknown_content = valid;
    unknown_content.payload["content"][0] = json!({"kind":"future_content", "data":"opaque"});
    assert!(unknown_content.decode_message().is_err());
}

#[test]
fn arguments_and_role_validation_do_not_repair_malformed_observations() {
    for raw in ["{broken", "[1,2]", "null", "\"json string\"", " 4 "] {
        assert_eq!(
            ToolArguments::from_raw(raw),
            ToolArguments::Invalid { raw: raw.into() }
        );
    }
    assert!(matches!(
        ToolArguments::from_raw("{\"x\":1}"),
        ToolArguments::Object { .. }
    ));
    let invalid_role = Message::new(
        Role::User,
        vec![Content::Reasoning(Reasoning::Text {
            text: "hidden".into(),
        })],
    );
    assert!(matches!(
        Envelope::encode_message(&invalid_role),
        Err(SchemaError::InvalidMessage(_))
    ));
}

struct Scripted {
    events: Vec<Result<StreamEvent, Error>>,
}

#[test]
fn request_validation_rejects_ambiguous_tools_and_out_of_range_controls() {
    let tool = ToolDefinition {
        name: "calculate".into(),
        description: None,
        parameters: JsonObject::new(),
    };
    let mut request = CompletionRequest {
        tools: vec![tool.clone()],
        ..Default::default()
    };
    assert!(request.validate().is_ok());
    request.tools.push(tool);
    assert!(matches!(request.validate(), Err(Error::InvalidRequest(_))));
    request.tools.pop();
    request.tools[0].name = " \t".into();
    assert!(matches!(request.validate(), Err(Error::InvalidRequest(_))));

    let valid = CompletionRequest {
        max_output_tokens: Some(1),
        temperature: Some(0.0),
        top_p: Some(1.0),
        stop: vec!["end".into()],
        ..Default::default()
    };
    assert!(valid.validate().is_ok());
    for invalid in [
        CompletionRequest {
            max_output_tokens: Some(0),
            ..valid.clone()
        },
        CompletionRequest {
            temperature: Some(f64::NAN),
            ..valid.clone()
        },
        CompletionRequest {
            temperature: Some(-0.1),
            ..valid.clone()
        },
        CompletionRequest {
            top_p: Some(1.5),
            ..valid.clone()
        },
        CompletionRequest {
            stop: vec![String::new()],
            ..valid.clone()
        },
    ] {
        assert!(matches!(invalid.validate(), Err(Error::InvalidRequest(_))));
    }
}

#[test]
fn thinking_efforts_use_model_defined_choices_without_guessing_unknown_capabilities() {
    let mut capabilities = ThinkingCapabilities::default();
    assert!(capabilities.validate_effort("future-level").is_ok());
    capabilities.effort_levels = Some(vec![ThinkingLevel {
        value: "custom-budget".into(),
        label: "Custom budget".into(),
    }]);
    assert!(capabilities.validate_effort("custom-budget").is_ok());
    assert!(matches!(
        capabilities.validate_effort("medium"),
        Err(Error::Unsupported(_))
    ));
    capabilities.effort_levels = Some(Vec::new());
    assert!(matches!(
        capabilities.validate_effort("custom-budget"),
        Err(Error::Unsupported(_))
    ));
    let request = CompletionRequest {
        thinking: Some(Thinking::Effort(" \t".into())),
        ..Default::default()
    };
    assert!(matches!(request.validate(), Err(Error::InvalidRequest(_))));
}

impl LanguageModelProvider for Scripted {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, Error>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn stream<'a>(
        &'a self,
        _: &'a LanguageModel,
        _: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, Error>> {
        let events = self.events.clone();
        Box::pin(async move { Ok(Box::pin(stream::iter(events)) as CompletionStream) })
    }
}

#[test]
fn object_safe_completion_requires_terminal_content_and_rejects_late_events() {
    async fn collect(provider: &dyn LanguageModelProvider) -> Result<Completion, Error> {
        let model = LanguageModel::new("scripted");
        collect_completion(
            provider
                .stream(&model, CompletionRequest::default())
                .await?,
        )
        .await
    }
    let delta = StreamEvent::Delta {
        index: 0,
        delta: ContentDelta::Text("preview".into()),
    };
    let accepted = Completion {
        message: Message::new(Role::Assistant, vec![Content::text("authoritative")]),
    };
    let provider: Box<dyn LanguageModelProvider> = Box::new(Scripted {
        events: vec![
            Ok(delta.clone()),
            Ok(StreamEvent::Finished(Box::new(accepted.clone()))),
        ],
    });
    assert_eq!(block_on(collect(provider.as_ref())).unwrap(), accepted);
    let interrupted = Scripted {
        events: vec![Ok(delta.clone())],
    };
    assert_eq!(
        block_on(collect(&interrupted)),
        Err(Error::IncompleteStream)
    );
    let late = Scripted {
        events: vec![Ok(StreamEvent::Finished(Box::new(accepted))), Ok(delta)],
    };
    assert!(matches!(
        block_on(collect(&late)),
        Err(Error::InvalidResponse(_))
    ));
    let wrong_role = Scripted {
        events: vec![Ok(StreamEvent::Finished(Box::new(Completion {
            message: Message::new(Role::User, vec![Content::text("not an assistant")]),
        })))],
    };
    assert!(matches!(
        block_on(collect(&wrong_role)),
        Err(Error::InvalidResponse(_))
    ));
}
