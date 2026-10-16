//! Scripted model/tool boundaries, including request-prefix and replay checks.
use super::*;
use rig::{
    completion::Message,
    test_utils::{MockCompletionModel, MockStreamEvent, mock_final_with_total_tokens},
};

fn tool_turn(id: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::Reasoning {
            id: "reasoning".into(),
            text: "Preserve this earlier reasoning exactly.".into(),
        },
        MockStreamEvent::tool_call(id, "calculate", json!({"operation":"add", "a":1, "b":2})),
        MockStreamEvent::FinalResponse(mock_final_with_total_tokens(100)),
    ]
}

fn text_turn(text: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::Text(text.into()),
        MockStreamEvent::FinalResponse(mock_final_with_total_tokens(100)),
    ]
}

fn hook(updates: mpsc::UnboundedSender<RunUpdate>) -> Arc<GenerationCompaction> {
    Arc::new(GenerationCompaction {
        threshold: Some(10),
        latest_context_tokens: Mutex::new(0),
        environment_note: "<environment>project path</environment>".into(),
        updates,
    })
}

fn agent(model: &MockCompletionModel, hook: Arc<GenerationCompaction>) -> StreamingAgent {
    let mut tools = ToolSet::default();
    tools.add_tool(Calculate);
    StreamingAgent::new(model.clone().erase(), tools)
        .preamble("Stable system")
        .additional_params(json!({"num_ctx":131072, "think":"medium"}))
        .context_hook(hook)
}

#[derive(Default)]
struct Steering(Mutex<Option<Message>>);

impl agent::SteeringHook for Steering {
    fn has_pending(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }
    fn take_pending(&self) -> Option<Message> {
        self.0.lock().unwrap().take()
    }
}

#[tokio::test]
async fn steering_is_delivered_after_loop_compaction() {
    let model = MockCompletionModel::from_stream_turns([
        tool_turn("real"),
        text_turn("Handoff"),
        text_turn("Steered reply"),
    ]);
    let (updates, mut receiver) = mpsc::unbounded_channel();
    let steering = Arc::new(Steering::default());
    let mut history = Vec::new();
    agent(&model, hook(updates.clone()))
        .steering_hook(steering.clone())
        .run(
            Message::user("Original task. ".repeat(100)),
            &mut history,
            |event| {
                if matches!(event, AgentEvent::ToolResult { .. }) {
                    *steering.0.lock().unwrap() = Some(Message::user("New instruction"));
                }
                _ = updates.send(RunUpdate::Event(event));
            },
        )
        .await
        .unwrap();
    assert_eq!(history.len(), 5);
    let mut finished = false;
    while let Ok(update) = receiver.try_recv() {
        match update {
            RunUpdate::CompactionFinished { context, .. } => {
                assert!(context.unwrap().contains("Handoff"));
                finished = true;
            }
            RunUpdate::Event(AgentEvent::Steered { .. }) => assert!(finished),
            _ => {}
        }
    }
    let requests = model.requests();
    let continuation = serde_json::to_string(&requests[2].chat_history).unwrap();
    assert!(continuation.contains("Handoff"));
    assert!(continuation.contains("New instruction"));
    assert!(!continuation.contains("Original task"));
}

#[tokio::test]
async fn compaction_runs_between_tool_turns_and_preserves_full_history() {
    let model = MockCompletionModel::from_stream_turns([
        tool_turn("first"),
        text_turn("First handoff"),
        tool_turn("second"),
        text_turn("Second handoff"),
        text_turn("Finished"),
    ]);
    let (updates, mut receiver) = mpsc::unbounded_channel();
    let agent = agent(&model, hook(updates.clone()));
    let prompt =
        Message::user("Keep this original task and its detailed constraints. ".repeat(100));
    let mut history = Vec::new();
    agent
        .run(prompt.clone(), &mut history, |event| {
            _ = updates.send(RunUpdate::Event(event));
        })
        .await
        .unwrap();
    assert_eq!(history.len(), 6);
    let mut fold = agent::TurnFold::default();
    let mut replayed = vec![prompt];
    let mut boundaries = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        match update {
            RunUpdate::Event(event) => replayed.extend(fold.apply(&event).unwrap().message),
            RunUpdate::CompactionStarted => assert!(fold.pending_calls().is_empty()),
            RunUpdate::CompactionFinished { context, .. } => {
                let context = context.expect("smaller summary");
                assert!(context.contains("<environment>project path</environment>"));
                boundaries.push(replayed.len());
            }
            _ => panic!("unexpected update"),
        }
    }
    assert_eq!(boundaries, [3, 5]);
    assert_eq!(replayed, history);
    let requests = model.requests();
    assert_eq!(requests.len(), 5);
    for request in &requests {
        assert_eq!(
            request.system_instructions(),
            requests[0].system_instructions()
        );
        assert_eq!(request.tools, requests[0].tools);
        assert_eq!(request.additional_params, requests[0].additional_params);
    }
    // Compare the real native /api/chat encoding, not only Rig's abstract
    // requests. Compaction must append to the earlier wire-message prefix.
    use rig::wire::{Body, Mode, Wire as _};
    let wire = rig::providers::ollama::Ollama::new()
        .native_completion("qwen3.8:27b")
        .wire;
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| {
            let encoded = wire.encode(request.clone(), Mode::Streaming).unwrap();
            let Body::Bytes(bytes) = encoded.request.body() else {
                panic!("JSON body");
            };
            serde_json::from_slice(bytes).unwrap()
        })
        .collect();
    for (ordinary, summary) in [(0, 1), (2, 3)] {
        for key in ["model", "tools", "options", "think", "stream"] {
            assert_eq!(bodies[ordinary][key], bodies[summary][key], "same {key}");
        }
        assert!(
            bodies[summary]["messages"]
                .as_array()
                .unwrap()
                .starts_with(bodies[ordinary]["messages"].as_array().unwrap())
        );
        assert!(
            bodies[summary]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["thinking"] == "Preserve this earlier reasoning exactly.")
        );
    }
    for (index, summary) in [(2, "First handoff"), (4, "Second handoff")] {
        let context = serde_json::to_string(&requests[index].chat_history).unwrap();
        assert!(context.contains(summary));
        assert!(context.contains("project path"));
        assert!(!context.contains("detailed constraints"));
        assert!(!context.contains("tool_result"));
    }
}

#[tokio::test]
async fn tool_call_during_loop_compaction_preserves_request_history() {
    let mut summary = tool_turn("summary");
    summary.insert(0, MockStreamEvent::Text("Invalid summary".into()));
    let model = MockCompletionModel::from_stream_turns([
        tool_turn("real"),
        summary,
        text_turn("Normal reply"),
    ]);
    let (updates, mut receiver) = mpsc::unbounded_channel();
    let mut history = Vec::new();
    agent(&model, hook(updates.clone()))
        .run(Message::user("Original task"), &mut history, |event| {
            _ = updates.send(RunUpdate::Event(event));
        })
        .await
        .unwrap();
    let mut results = 0;
    let mut failures = 0;
    while let Ok(update) = receiver.try_recv() {
        match update {
            RunUpdate::Event(AgentEvent::ToolResult { .. }) => results += 1,
            RunUpdate::CompactionFinished { context, .. } => {
                assert!(context.is_none());
                failures += 1;
            }
            _ => {}
        }
    }
    assert_eq!(results, 1, "only the ordinary tool call executes");
    assert_eq!(failures, 1);
    assert_eq!(history.len(), 4);
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    let continuation = serde_json::to_string(&requests[2].chat_history).unwrap();
    assert!(continuation.contains("Original task"));
    assert!(!continuation.contains("Invalid summary"));
}
