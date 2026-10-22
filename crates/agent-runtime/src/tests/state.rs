//! Request assembly and the session's state: definition, configuration,
//! dynamic tools, and the context notes that tell the model about changes.

use super::*;
use language_model::{CachePoint, ToolDefinition};

fn names(tools: &[ToolDefinition]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

fn note(message: &Message, index: usize) -> &str {
    let Content::Text { text } = &message.content[index] else {
        panic!("context notes are text")
    };
    text
}

/// Each request is built from the definition, the configuration, and the
/// conversation. Configuration changes keep the prefix; a replaced definition
/// is a new revision; the model is told about both through context notes.
#[tokio::test]
async fn requests_are_assembled_from_definition_configuration_and_conversation() {
    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = fixture(Mode::Allow);
    let mut harness = Harness::start(session, handle);
    harness.submit("first", Delivery::Queue).await;
    let first = recv(&mut requests).await;
    let request = first.request.clone();
    assert_eq!(request.instructions.as_deref(), Some("system"));
    assert_eq!(names(&request.tools), [SEARCH_TOOLS, CALL_TOOL, "work"]);
    assert_eq!(request.max_output_tokens, Some(4096));
    assert_eq!(
        request.cache,
        vec![
            CachePoint::AfterTools,
            CachePoint::AfterInstructions,
            CachePoint::AfterMessage(request.messages.last().unwrap().id.clone()),
        ]
    );
    // The context note comes before the input it accompanies.
    assert_eq!(
        note(&request.messages[0], 0),
        "<tool_policy>\nMode: Allow\n</tool_policy>"
    );
    assert_eq!(request.messages[1].content, [Content::text("first")]);
    respond(first, text("one"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;

    harness
        .request(RequestKind::Configure(ConfigurationChange::Sampling(
            Sampling {
                temperature: Some(0.2),
                ..Sampling::default()
            },
        )))
        .await
        .unwrap();
    harness
        .request(RequestKind::Configure(ConfigurationChange::Permissions(
            Mode::Deny,
        )))
        .await
        .unwrap();
    harness.submit("second", Delivery::Queue).await;
    let second = recv(&mut requests).await;
    assert_eq!(second.request.temperature, Some(0.2));
    assert_eq!(second.request.tools, request.tools);
    // Only the changed section is told again, after what the model saw.
    let messages = &second.request.messages;
    assert_eq!(messages[..2], request.messages[..]);
    assert_eq!(
        messages[3].content,
        [Content::text("<tool_policy>\nMode: Deny\n</tool_policy>")]
    );
    assert_eq!(messages[4].content, [Content::text("second")]);
    respond(second, text("two"));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;

    let unknown = definition().tool("missing");
    assert!(matches!(
        harness
            .request(RequestKind::ReplaceDefinition {
                definition: unknown,
                provider: None
            })
            .await,
        Err(RequestError::Rejected(_))
    ));
    let replaced = AgentDefinition::new(LanguageModel::new("other")).instructions("system");
    let response = harness
        .request(RequestKind::ReplaceDefinition {
            definition: replaced,
            provider: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        response,
        Response::DefinitionReplaced(DefinitionRevision(1))
    ));
    harness.submit("third", Delivery::Queue).await;
    let third = recv(&mut requests).await;
    assert_eq!(third.model.id, "other");
    assert_eq!(names(&third.request.tools), [SEARCH_TOOLS, CALL_TOOL]);
    let messages = &third.request.messages;
    assert!(note(&messages[messages.len() - 2], 0).contains("different model, other"));
    answer_with(&mut harness, third, "three").await;

    let (result, _) = harness.finish().await;
    let revisions: Vec<_> = result
        .conversation
        .entries()
        .iter()
        .filter_map(|entry| match entry.origin {
            Origin::Model { revision } => Some(revision.0),
            _ => None,
        })
        .collect();
    assert_eq!(revisions, [0, 0, 1]);
}

async fn answer_with(harness: &mut Harness, request: SamplingRequest, text_: &str) {
    respond(request, text(text_));
    harness
        .event(|kind| matches!(kind, EventKind::RunFinished { .. }))
        .await;
}

#[derive(Deserialize, JsonSchema)]
pub(super) struct EchoArgs {
    text: String,
}

/// Needs nothing from the host: `()` context and access.
pub(super) struct Echo;

impl Tool for Echo {
    type Args = EchoArgs;
    type Output = Value;
    type Context = ();
    type Access = ();

    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> String {
        "Repeat text back".into()
    }
    async fn run(
        &self,
        _: tool_runtime::ToolCall<()>,
        _: Arc<()>,
        args: EchoArgs,
    ) -> Result<Value, tool_runtime::ToolError> {
        Ok(json!({"echo": args.text}))
    }
}

/// Dynamic tools never change the request's tools: the model finds them with
/// `search_tools` and calls them with `call_tool`, which authorization and
/// events see as the real tool.
#[tokio::test]
async fn dynamic_tools_are_found_with_search_and_run_through_call_tool() {
    let Fixture {
        session,
        handle,
        mut requests,
        ..
    } = fixture(Mode::Ask);
    let mut harness = Harness::start(session, handle);
    harness
        .request(RequestKind::DynamicTools(DynamicToolChange::add(Echo)))
        .await
        .unwrap();
    assert!(matches!(
        harness
            .request(RequestKind::DynamicTools(DynamicToolChange::add(Work)))
            .await,
        Err(RequestError::Rejected(reason)) if reason.contains("offered directly")
    ));
    harness.submit("use echo", Delivery::Queue).await;
    let first = recv(&mut requests).await;
    let tools = first.request.tools.clone();
    assert_eq!(names(&tools), [SEARCH_TOOLS, CALL_TOOL, "work"]);
    assert!(note(&first.request.messages[0], 1).contains("- echo: Repeat text back"));
    respond(
        first,
        calls(vec![tool_call(
            SEARCH_TOOLS,
            json!({"query": "repeat text"}),
        )]),
    );

    // Searching is the runtime's own read-only built-in: no approval.
    let second = recv(&mut requests).await;
    let found = result_of(second.request.messages.last().unwrap());
    let ToolResultContent::Json { value } = &found.content[0] else {
        panic!()
    };
    assert_eq!(value["tools"][0]["name"], "echo");
    assert_eq!(
        value["tools"][0]["parameters"]["properties"]["text"]["type"],
        "string"
    );
    respond(
        second,
        calls(vec![
            tool_call(
                CALL_TOOL,
                json!({"name": "echo", "arguments": {"text": "hi"}}),
            ),
            tool_call(
                CALL_TOOL,
                json!({"name": "work", "arguments": {"path": "x"}}),
            ),
        ]),
    );
    let asked = harness
        .event(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested { call, tool, access } = asked.kind else {
        unreachable!()
    };
    assert_eq!((tool.as_str(), access), ("echo", Access("nothing".into())));
    harness
        .request(RequestKind::ResolveApproval {
            call: call.id,
            decision: ApprovalDecision::AllowOnce,
        })
        .await
        .unwrap();
    let third = recv(&mut requests).await;
    let messages = &third.request.messages;
    assert_eq!(third.request.tools, tools);
    assert_eq!(
        result_of(&messages[messages.len() - 2]).content,
        [ToolResultContent::Json {
            value: json!({"echo": "hi"})
        }]
    );
    // Base tools are called directly, never through call_tool.
    assert_eq!(
        error_kind(&messages[messages.len() - 1]),
        "invalid_arguments"
    );
    answer_with(&mut harness, third, "done").await;

    harness
        .request(RequestKind::DynamicTools(DynamicToolChange::Remove(
            "echo".into(),
        )))
        .await
        .unwrap();
    harness.submit("again", Delivery::Queue).await;
    let fourth = recv(&mut requests).await;
    let messages = &fourth.request.messages;
    assert!(note(&messages[messages.len() - 2], 0).contains("no longer available: echo"));
    answer_with(&mut harness, fourth, "ok").await;
    let (_, events) = harness.finish().await;
    let started: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(started, [SEARCH_TOOLS, "echo"]);
}

#[tokio::test]
async fn sessions_without_call_tool_have_no_dynamic_tools() {
    let mut definition = definition();
    definition.tools = vec!["work".into()];
    let Fixture {
        session, handle, ..
    } = fixture_with(Mode::Allow, definition);
    let harness = Harness::start(session, handle);
    assert!(matches!(
        harness
            .request(RequestKind::DynamicTools(DynamicToolChange::add(Echo)))
            .await,
        Err(RequestError::Rejected(_))
    ));
    harness.finish().await;

    let Fixture {
        session, handle, ..
    } = fixture(Mode::Allow);
    let harness = Harness::start(session, handle);
    harness
        .request(RequestKind::DynamicTools(DynamicToolChange::add(Echo)))
        .await
        .unwrap();
    let mut without = super::definition();
    without.tools = vec!["work".into()];
    assert!(matches!(
        harness
            .request(RequestKind::ReplaceDefinition {
                definition: without,
                provider: None
            })
            .await,
        Err(RequestError::Rejected(_))
    ));
    harness.finish().await;
}
