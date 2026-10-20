use std::sync::{Arc, Mutex};

use rig::{
    AgentBuilder,
    agent::{DispatchAction, DispatchEvent, RunSettled},
    test_utils::{MockCompletionModel, MockStreamEvent, mock_final_with_total_tokens},
    tool::ToolOutput,
};

use super::*;
use crate::test_support::turn;

/// A model event read from its JSON form, as one arrives from elsewhere.
fn event(value: serde_json::Value) -> AgentEvent {
    AgentEvent::Model(serde_json::from_value(value).expect("a stream event"))
}

fn ended() -> AgentEvent {
    AgentEvent::TurnEnded {
        origin: None,
        stop: Some(StopReason::Stop),
        usage: Usage::default(),
    }
}

/// A turn that thinks, answers and calls a tool, as Rig streams it.
fn thinking_turn() -> Vec<AgentEvent> {
    turn([
        MockStreamEvent::ReasoningDelta {
            id: "r".into(),
            reasoning: "Hmm.".into(),
        },
        MockStreamEvent::text("Checking."),
        MockStreamEvent::tool_call("c", "lookup", serde_json::json!({"q": 1})),
        MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
    ])
}

fn fold_all(fold: &mut TurnFold, events: &[AgentEvent]) -> Vec<Folded> {
    events
        .iter()
        .map(|event| fold.apply(event).expect("a valid event"))
        .collect()
}

fn result_of(call: &ToolCall, output: &str) -> AgentEvent {
    AgentEvent::tool_result(
        call.id.clone(),
        call.function.name.clone(),
        &ToolResult::success(ToolOutput::text(output)),
    )
}

#[test]
fn folding_rebuilds_replies_and_tool_results() {
    let mut fold = TurnFold::default();
    let folded = fold_all(&mut fold, &thinking_turn());
    let blocks = folded
        .iter()
        .filter_map(|folded| folded.block.clone())
        .collect::<Vec<_>>();
    let messages = folded
        .into_iter()
        .filter_map(|folded| folded.message)
        .collect::<Vec<_>>();
    // The tool call is complete as soon as its part ends.
    let Some(AssistantContent::ToolCall(call)) = blocks
        .iter()
        .find(|block| matches!(block, AssistantContent::ToolCall(_)))
    else {
        panic!("expected the completed tool call, got {blocks:?}");
    };
    assert_eq!(call.function.name.as_str(), "lookup");
    let [Message::Assistant(reply)] = messages.as_slice() else {
        panic!("expected the reply");
    };
    assert!(reply.origin.is_some());
    assert_eq!(reply.stop, Some(StopReason::ToolUse));
    assert!(matches!(
        reply.content.as_slice(),
        [
            AssistantContent::Reasoning(_),
            AssistantContent::Text(_),
            AssistantContent::ToolCall(_)
        ]
    ));

    // Resuming after the reply, as someone joining now would.
    assert_eq!(fold.partial(), None);
    let mut resumed = TurnFold::after(&messages[0]);
    let result = result_of(call, "found");
    let expected = fold
        .apply(&result)
        .expect("a result")
        .message
        .expect("the results prompt");
    assert_eq!(
        resumed.apply(&result).expect("a result").message,
        Some(expected.clone())
    );
    assert!(matches!(
        expected,
        Message::User { content } if matches!(content.as_slice(), [UserContent::ToolResult(_)])
    ));
    // A call answers once.
    assert!(resumed.apply(&result).expect("a result").message.is_none());
}

/// Mid-turn, the partial message is the reply so far, and just before
/// the turn ends it is the reply the turn adds.
#[test]
fn partial_messages_grow_into_the_messages_folded() {
    let events = thinking_turn();
    let (turn_ended, streamed) = events.split_last().expect("a turn");
    let mut fold = TurnFold::default();
    assert_eq!(fold.partial(), None);
    fold_all(&mut fold, &streamed[..2]);
    let Some(Message::Assistant(partial)) = fold.partial() else {
        panic!("expected the reply so far");
    };
    assert!(matches!(
        partial.content.as_slice(),
        [AssistantContent::Reasoning(_)]
    ));

    fold_all(&mut fold, &streamed[2..]);
    let Some(Message::Assistant(partial)) = fold.partial() else {
        panic!("expected the whole reply");
    };
    let Some(Message::Assistant(reply)) = fold.apply(turn_ended).expect("an end").message else {
        panic!("expected the reply");
    };
    assert_eq!(partial.content, reply.content);
}

/// The reply is the content Rig finalized, not what streamed before it:
/// an end that states other content supersedes the fragments the
/// preview showed.
#[test]
fn replies_hold_the_content_rig_finalized() {
    let events = [
        event(serde_json::json!({"event": "start", "part": 0, "kind": "reasoning"})),
        event(serde_json::json!({"event": "reasoning", "part": 0, "text": "Hm"})),
        event(serde_json::json!({"event": "end", "part": 0,
            "content": {"type": "reasoning", "text": "Considered."}})),
        ended(),
    ];
    let mut fold = TurnFold::default();
    fold_all(&mut fold, &events[..2]);
    let Some(Message::Assistant(partial)) = fold.partial() else {
        panic!("expected the preview");
    };
    assert!(matches!(
        partial.content.as_slice(),
        [AssistantContent::Reasoning(reasoning)] if reasoning.text == "Hm"
    ));

    let Some(AssistantContent::Reasoning(finalized)) =
        fold.apply(&events[2]).expect("an end").block
    else {
        panic!("expected the finalized reasoning");
    };
    assert_eq!(finalized.text, "Considered.");
    let Some(Message::Assistant(reply)) = fold.apply(&events[3]).expect("an end").message else {
        panic!("expected the reply");
    };
    let [AssistantContent::Reasoning(reasoning)] = reply.content.as_slice() else {
        panic!("expected the reasoning, got {:?}", reply.content);
    };
    assert_eq!(reasoning.text, "Considered.");
}

/// A part's place in the preview is its place in the reply, so parts
/// don't move when one ends: text that began before a tool call stays
/// before it, even though the call ends first.
#[test]
fn parts_keep_their_place_when_they_end() {
    let events = [
        event(serde_json::json!({"event": "start", "part": 0, "kind": "text"})),
        event(serde_json::json!({"event": "text", "part": 0, "text": "Checking"})),
        event(
            serde_json::json!({"event": "start", "part": 1, "kind": "tool_call",
            "name": "lookup"}),
        ),
        event(serde_json::json!({"event": "arguments", "part": 1, "json": "{}"})),
        event(serde_json::json!({"event": "end", "part": 1, "content": {
            "type": "toolcall", "id": {"provider": "c"},
            "function": {"name": "lookup", "arguments": {}}}})),
        event(serde_json::json!({"event": "text", "part": 0, "text": " twice."})),
        event(serde_json::json!({"event": "end", "part": 0,
            "content": {"type": "text", "text": "Checking twice."}})),
        ended(),
    ];
    let (turn_ended, streamed) = events.split_last().expect("a turn");
    let mut fold = TurnFold::default();
    for event in streamed {
        fold.apply(event).expect("a valid event");
        if let Some(Message::Assistant(partial)) = fold.partial()
            && partial.content.len() == 2
        {
            assert!(matches!(
                partial.content.as_slice(),
                [AssistantContent::Text(_), AssistantContent::ToolCall(_)]
            ));
        }
    }
    let Some(Message::Assistant(reply)) = fold.apply(turn_ended).expect("an end").message else {
        panic!("expected the reply");
    };
    let [AssistantContent::Text(text), AssistantContent::ToolCall(_)] = reply.content.as_slice()
    else {
        panic!("expected the text, then the call, got {:?}", reply.content);
    };
    assert_eq!(text.text, "Checking twice.");
}

/// Events Rig's stream could not have produced are refused, as Rig
/// refuses them in a relayed stream: here, text for a part that never
/// started, and a part that ends twice.
#[test]
fn events_out_of_order_are_refused() {
    let mut fold = TurnFold::default();
    assert!(
        fold.apply(&event(
            serde_json::json!({"event": "text", "part": 0, "text": "Hi"})
        ))
        .is_err()
    );

    let start = event(serde_json::json!({"event": "start", "part": 0, "kind": "text"}));
    let end = event(serde_json::json!({"event": "end", "part": 0,
        "content": {"type": "text", "text": "Hi"}}));
    let mut fold = TurnFold::default();
    fold_all(&mut fold, &[start.clone(), end.clone()]);
    assert!(fold.apply(&end).is_err());

    // Positions are per turn: the next turn starts over.
    fold.apply(&ended()).expect("an end");
    fold_all(&mut fold, &[start, end]);
}

/// A failed turn's calls never run, so nothing waits for their results.
#[test]
fn a_failed_turns_calls_are_not_pending() {
    let reply = Message::Assistant(
        AssistantMessage::new(vec![AssistantContent::ToolCall(ToolCall::from_wire(
            "c",
            rig::message::ToolFunction::new(
                rig::message::ToolName::new("lookup").expect("a name"),
                serde_json::json!({}),
            ),
        ))])
        .with_stop(StopReason::Error("refused".into())),
    );
    assert!(TurnFold::after(&reply).pending_calls().is_empty());
}

/// Denies the calls adding to 10, and keeps the messages the run commits.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Option<Vec<Message>>>>);

impl AgentHook for Recorder {
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let denied = event
            .tool_args()
            .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok())
            .is_some_and(|args| args["a"] == 10);
        if denied {
            DispatchAction::skip("Not now.")
        } else {
            DispatchAction::proceed()
        }
    }

    async fn on_run_settled(&self, _: &HookContext, event: RunSettled<'_>) {
        *self.0.lock().unwrap() = event
            .messages
            .map(|messages| messages.iter().map(|message| (**message).clone()).collect());
    }
}

/// Folding a run's events, one at a time from their JSON form as someone
/// they were sent to does, rebuilds exactly the messages Rig commits: here
/// for a reply that thinks, answers, and makes a call that runs, one a hook
/// denies, one whose arguments are not a JSON object, and one of a tool that
/// doesn't exist, which Rig drops from the reply.
#[tokio::test]
async fn folding_a_runs_events_rebuilds_what_rig_commits() {
    let calculate = |id: &str, a: u64| {
        MockStreamEvent::tool_call(
            id,
            "calculate",
            serde_json::json!({"operation": "add", "a": a, "b": 2}),
        )
    };
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::ReasoningDelta {
                id: "r".into(),
                reasoning: "Add them.".into(),
            },
            MockStreamEvent::Text("Adding.".into()),
            // Its result comes only once the others have returned.
            MockStreamEvent::ToolCallNameDelta {
                id: "malformed".into(),
                name: "calculate".into(),
            },
            MockStreamEvent::ToolCallArgumentsDelta {
                id: "malformed".into(),
                arguments: "{\"a\": ".into(),
            },
            MockStreamEvent::ToolCallEnd {
                id: "malformed".into(),
            },
            calculate("ran", 1),
            calculate("denied", 10),
            MockStreamEvent::tool_call("unknown", "unknown_tool", serde_json::json!({})),
            MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
        ],
        vec![
            MockStreamEvent::Text("It is 3.".into()),
            MockStreamEvent::FinalResponse(mock_final_with_total_tokens(20)),
        ],
    ]);
    let recorder = Recorder::default();
    let agent = AgentBuilder::new(model).tool(tools::Calculate).build();
    let prompt = Message::user("1 + 2?");
    let mut events = Vec::new();
    run(
        agent
            .prompt(prompt.clone())
            .max_turns(10)
            .unhandled_invalid_tool_call(rig::run::UnhandledInvalidToolCall::Ignore)
            .add_hook(recorder.clone()),
        |event| events.push(serde_json::to_string(&event).unwrap()),
    )
    .await
    .expect("the run");

    let mut fold = TurnFold::default();
    let replayed = std::iter::once(prompt)
        .chain(events.iter().filter_map(|event| {
            fold.apply(&serde_json::from_str(event).unwrap())
                .unwrap()
                .message
        }))
        .collect::<Vec<_>>();
    let committed = recorder.0.lock().unwrap().clone().expect("the run settled");
    assert_eq!(replayed, committed);
    let [_, _, Message::User { content: results }, _] = committed.as_slice() else {
        panic!("expected prompt, reply, tool results, reply, got {committed:?}");
    };
    let errors = results
        .iter()
        .map(|result| matches!(result, UserContent::ToolResult(result) if result.is_error))
        .collect::<Vec<_>>();
    assert_eq!(errors, [true, false, true]);
}

/// Stops the run at the end of its first model turn, and keeps the
/// messages it commits.
#[derive(Clone, Default)]
struct Stopper(Recorder);

impl AgentHook for Stopper {
    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        _: rig::agent::ModelTurnFinished<'_>,
    ) -> rig::agent::ModelTurnAction {
        rig::agent::ModelTurnAction::stop("Enough.")
    }

    async fn on_run_settled(&self, context: &HookContext, event: RunSettled<'_>) {
        self.0.on_run_settled(context, event).await;
    }
}

/// Rig reports the outcome of a turn whose answer arrived even when the run
/// then refuses it; such a turn's end is not reported, as Rig never commits
/// its reply.
#[tokio::test]
async fn a_refused_turn_does_not_end() {
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::Text("Done.".into()),
        MockStreamEvent::FinalResponse(mock_final_with_total_tokens(10)),
    ]]);
    let stopper = Stopper::default();
    let agent = AgentBuilder::new(model).build();
    let prompt = Message::user("Hi");
    let mut events = Vec::new();
    run(
        agent.prompt(prompt.clone()).add_hook(stopper.clone()),
        |event| events.push(event),
    )
    .await
    .expect_err("the hook stops the run");

    let mut fold = TurnFold::default();
    let replayed = std::iter::once(prompt)
        .chain(
            fold_all(&mut fold, &events)
                .into_iter()
                .filter_map(|folded| folded.message),
        )
        .collect::<Vec<_>>();
    let committed = stopper
        .0
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("the run settled");
    assert_eq!(replayed, committed);
}
