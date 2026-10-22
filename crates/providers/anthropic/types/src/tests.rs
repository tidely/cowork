use super::*;
use language_model::{
    CachePoint, CompletionRequest, Content, Error, Image, ImageSource as SharedImageSource,
    LanguageModel, Message as SharedMessage, ReplayData, Role, Thinking, ToolArguments, ToolCall,
    ToolCallId, ToolDefinition, ToolResult, ToolResultContent as SharedResultContent,
};
use serde_json::json;

fn input(messages: Vec<SharedMessage>) -> CompletionRequest {
    CompletionRequest {
        max_output_tokens: Some(2048),
        ..CompletionRequest::new(messages)
    }
}
fn model() -> LanguageModel {
    LanguageModel::new("selected-model")
}

#[test]
fn converts_instructions_multimodal_tools_correlated_results_and_cache_points() {
    let id = ToolCallId::new();
    let mut request = input(vec![
        SharedMessage::new(
            Role::User,
            vec![
                Content::text("look"),
                Content::Image(Image {
                    media_type: "image/png".into(),
                    source: SharedImageSource::Base64 {
                        data: "aGk=".into(),
                    },
                }),
            ],
        ),
        SharedMessage::new(
            Role::Assistant,
            vec![Content::ToolCall(ToolCall {
                id: id.clone(),
                provider_id: Some("toolu_a".into()),
                name: "read".into(),
                arguments: ToolArguments::Object {
                    value: json!({"path":"foo"}).as_object().unwrap().clone(),
                },
            })],
        ),
        SharedMessage::new(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call: id,
                provider_id: None,
                name: None,
                content: vec![SharedResultContent::Json {
                    value: json!({"error":true}),
                }],
                is_error: true,
            })],
        ),
        SharedMessage::user("continue"),
    ]);
    request.instructions = Some("system".into());
    request.tools = vec![ToolDefinition {
        name: "read".into(),
        description: None,
        parameters: Default::default(),
    }];
    // The assistant's call is marked on its own block, before the tool
    // result merges into the next user turn.
    request.cache = vec![
        CachePoint::AfterTools,
        CachePoint::AfterInstructions,
        CachePoint::AfterMessage(request.messages[1].id.clone()),
    ];
    let original = request.clone();
    let native = MessagesRequest::from_completion(&model(), request).unwrap();
    let value = json!(native);
    assert_eq!(value["system"][0]["text"], "system");
    assert_eq!(value["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(value["tools"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        value["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(
        value["messages"][0]["content"][1]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(
        value["messages"][0]["content"][1]["source"]["type"],
        "base64"
    );
    assert_eq!(
        value["messages"][1]["content"][0]["input"],
        json!({"path":"foo"})
    );
    assert_eq!(value["messages"][2]["content"][0]["tool_use_id"], "toolu_a");
    assert_eq!(value["messages"][2]["content"][0]["is_error"], true);
    assert_eq!(value["messages"][2]["content"][1]["text"], "continue");
    // Resolution is confined to the owned request, not caller history.
    assert!(
        matches!(&original.messages[2].content[0], Content::ToolResult(result) if result.provider_id.is_none())
    );
}

#[test]
fn validates_controls_without_overwriting_or_guessing() {
    let mut request = input(vec![SharedMessage::user("hi")]);
    request.thinking = Some(Thinking::Effort("custom".into()));
    let native = MessagesRequest::from_completion(&model(), request.clone()).unwrap();
    assert_eq!(json!(native)["output_config"]["effort"], "custom");
    assert_eq!(json!(native)["thinking"]["type"], "adaptive");
    for (key, value) in [
        ("thinking", json!(null)),
        ("output_config", json!({"effort":"high"})),
        ("max_tokens", json!(100)),
        ("messages", json!([])),
    ] {
        let mut conflict = request.clone();
        conflict.options.insert(key.into(), value);
        assert!(
            MessagesRequest::from_completion(&model(), conflict).is_err(),
            "{key}"
        );
    }
    request.thinking = None;
    request.options.insert(
        "thinking".into(),
        json!({"type":"enabled","budget_tokens":1024}),
    );
    assert!(MessagesRequest::from_completion(&model(), request.clone()).is_ok());
    request.max_output_tokens = Some(1024);
    assert!(MessagesRequest::from_completion(&model(), request).is_err());
    assert!(
        MessagesRequest::from_completion(
            &model(),
            CompletionRequest::new(vec![SharedMessage::user("hi")])
        )
        .is_err()
    );
}

#[test]
fn rejects_unrepresentable_content_and_invalid_tool_sequences() {
    let call = ToolCall {
        id: ToolCallId::new(),
        provider_id: None,
        name: "tool".into(),
        arguments: ToolArguments::Object {
            value: Default::default(),
        },
    };
    for messages in [
        vec![SharedMessage::new(
            Role::Developer,
            vec![Content::text("instructions")],
        )],
        vec![
            SharedMessage::user("hi"),
            SharedMessage::new(Role::System, vec![Content::text("late")]),
        ],
        vec![SharedMessage::new(
            Role::Assistant,
            vec![Content::Reasoning(language_model::Reasoning::Text {
                text: "unsigned".into(),
            })],
        )],
        vec![SharedMessage::new(
            Role::Assistant,
            vec![Content::ToolCall(call.clone())],
        )],
        vec![SharedMessage::new(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call: call.id.clone(),
                provider_id: None,
                name: None,
                content: vec![],
                is_error: false,
            })],
        )],
    ] {
        assert!(MessagesRequest::from_completion(&model(), input(messages)).is_err());
    }
    let mut request = input(vec![SharedMessage::user("hi")]);
    request.seed = Some(1);
    assert!(matches!(
        MessagesRequest::from_completion(&model(), request),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn signed_replay_is_bound_to_content_format_and_model() {
    let blocks: Vec<ContentBlock> = serde_json::from_value(json!([
        {"type":"thinking","thinking":"reason","signature":"signed"},
        {"type":"redacted_thinking","data":"opaque"},
        {"type":"text","text":"answer"}
    ]))
    .unwrap();
    let mut message = SharedMessage::new(Role::Assistant, project_content(&blocks).unwrap());
    message.replay.push(ReplayData {
        provider: "anthropic".into(),
        model: Some(model().id),
        format: REPLAY_FORMAT.into(),
        version: 1,
        payload: json!(blocks),
    });
    let native = MessagesRequest::from_completion(&model(), input(vec![message.clone()])).unwrap();
    assert_eq!(
        json!(native)["messages"][0]["content"][0]["signature"],
        "signed"
    );
    assert!(
        MessagesRequest::from_completion(
            &LanguageModel::new("different-model"),
            input(vec![message.clone()])
        )
        .is_err()
    );
    message.content[2] = Content::text("edited");
    assert!(MessagesRequest::from_completion(&model(), input(vec![message])).is_err());
}

#[test]
fn signed_replay_cannot_bypass_tool_argument_validation() {
    let blocks: Vec<ContentBlock> = serde_json::from_value(json!([
        {"type":"thinking","thinking":"reason","signature":"signed"},
        {"type":"tool_use","id":"toolu_a","name":"read","input":null}
    ]))
    .unwrap();
    let mut message = SharedMessage::new(Role::Assistant, project_content(&blocks).unwrap());
    let Content::ToolCall(call) = &message.content[1] else {
        panic!("missing call")
    };
    let result = SharedMessage::new(
        Role::Tool,
        vec![Content::ToolResult(ToolResult {
            call: call.id.clone(),
            provider_id: None,
            name: None,
            content: vec![],
            is_error: true,
        })],
    );
    message.replay.push(ReplayData {
        provider: "anthropic".into(),
        model: Some(model().id),
        format: REPLAY_FORMAT.into(),
        version: 1,
        payload: json!(blocks),
    });
    assert!(
        matches!(MessagesRequest::from_completion(&model(), input(vec![message, result])), Err(Error::Unsupported(reason)) if reason.contains("malformed"))
    );
}
