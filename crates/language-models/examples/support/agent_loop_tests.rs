use super::*;
use futures::{StreamExt as _, future::BoxFuture, stream};
use language_models::{
    Completion, CompletionStream, GenerationMetadata, StreamEvent, ToolCall, ToolCallId,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tool_runtime::{ToolDescriptor, ToolStream, TypedToolOutput};

struct Scripted {
    replies: Mutex<VecDeque<Message>>,
    requests: Mutex<Vec<CompletionRequest>>,
    incomplete: bool,
}

impl LanguageModelProvider for Scripted {
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<LanguageModel>, language_models::Error>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn stream<'a>(
        &'a self,
        _: &'a LanguageModel,
        request: CompletionRequest,
    ) -> BoxFuture<'a, Result<CompletionStream, language_models::Error>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            let events = if self.incomplete {
                vec![Err(language_models::Error::IncompleteStream)]
            } else {
                let message = self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected model turn");
                vec![Ok(StreamEvent::Finished(Box::new(Completion { message })))]
            };
            Ok(stream::iter(events).boxed())
        })
    }
}

fn reply(content: Vec<Content>, reason: &str) -> Message {
    let mut message = Message::new(Role::Assistant, content);
    message.generation = Some(GenerationMetadata {
        provider: "scripted".into(),
        model: None,
        finish_reason: Some(reason.into()),
        usage: None,
        extra: Default::default(),
    });
    message
}

fn call(name: &str, raw: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        provider_id: Some(format!("call_{name}")),
        name: name.into(),
        arguments: ToolArguments::from_raw(raw),
    }
}

#[tokio::test]
async fn dispatches_calls_and_sends_correlated_results_on_the_next_turn() {
    let add = call("add", r#"{"a":2,"b":3}"#);
    let missing = call("missing", "{}");
    let assistant = reply(
        vec![
            Content::ToolCall(add.clone()),
            Content::ToolCall(missing.clone()),
        ],
        "tool_calls",
    );
    let provider = Scripted {
        replies: Mutex::new(VecDeque::from([
            assistant.clone(),
            reply(vec![Content::text("5")], "stop"),
        ])),
        requests: Mutex::new(vec![]),
        incomplete: false,
    };
    let mut tools = ToolRegistry::default();
    tools.register(Add).unwrap();
    let result = agent_loop(
        &provider,
        &LanguageModel::new("scripted"),
        &tools,
        vec![Message::user("Add two and three")],
    )
    .await
    .unwrap();
    assert_eq!(result.content, vec![Content::text("5")]);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages[1], assistant);
    let Content::ToolResult(result) = &requests[1].messages[2].content[0] else {
        panic!("missing successful tool result")
    };
    assert_eq!(result.call, add.id);
    assert_eq!(result.provider_id, add.provider_id);
    assert!(!result.is_error);
    assert_eq!(
        result.content,
        vec![ToolResultContent::Json {
            value: json!({"value":5})
        }]
    );
    let Content::ToolResult(error) = &requests[1].messages[3].content[0] else {
        panic!("missing error result")
    };
    assert_eq!(error.call, missing.id);
    assert_eq!(error.provider_id, missing.provider_id);
    assert!(error.is_error);
    assert!(
        matches!(&error.content[0], ToolResultContent::Json { value } if value["kind"] == "not_found")
    );
}

struct CountDispatch {
    tools: ToolRegistry,
    calls: Arc<AtomicUsize>,
}
impl ToolDispatch for CountDispatch {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools.descriptors()
    }
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        context: ToolCallContext,
    ) -> BoxFuture<'a, ToolStream<TypedToolOutput>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.tools.call(name, args, context)
    }
}

#[tokio::test]
async fn rejects_unusable_responses_before_dispatching_any_calls() {
    for (reason, raw, incomplete) in [
        ("length", "{}", false),
        ("stop", "{bad", false),
        ("stop", "{}", true),
    ] {
        let assistant = reply(
            vec![
                Content::ToolCall(call("add", r#"{"a":2,"b":3}"#)),
                Content::ToolCall(call("other", raw)),
            ],
            reason,
        );
        let provider = Scripted {
            replies: Mutex::new(VecDeque::from([assistant])),
            requests: Mutex::new(vec![]),
            incomplete,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        registry.register(Add).unwrap();
        let tools = CountDispatch {
            tools: registry,
            calls: calls.clone(),
        };
        assert!(
            agent_loop(
                &provider,
                &LanguageModel::new("scripted"),
                &tools,
                vec![Message::user("hi")]
            )
            .await
            .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
