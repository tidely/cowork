# agent

Rig's agent runs, reported as events that anyone can fold into the run's
history, here or on another computer.

`agent::run` drives a Rig `AgentRunner` and reports what it does as
`AgentEvent`s, as it happens:

```text
provider streams the reply
  -> AgentEvent::Model: each part starts, grows, and ends with its content;
     a tool call's end completes the call, while the turn streams on
  -> AgentEvent::TurnEnded: the reply joins the history, with its usage and
     the origin and stop reason Rig gave it
  -> tools run, each reporting AgentEvent::ToolResult as soon as it returns
  -> once all have returned, their results become the next prompt
```

Rig's stream carries the model's events, but not a turn's origin and stop
reason, and its tool results only once every call of a turn has returned.
`run` adds a hook to the runner that observes those as Rig settles them
(`on_outcome`), and reports them in that order among the stream's events.
Rig answers some calls without dispatching them, such as one whose arguments
are not a JSON object; their results come from the stream instead, whose tool
items are Rig's committed history. Rig reports an outcome for every turn
whose answer arrived, including one the run then refuses, so a turn's end is
reported only once the run goes on with it.

A run is fully described by its events. `TurnFold` folds them into the
messages they add to the history, and anyone folding the same events, in
this process or after sending them elsewhere one at a time (each is
serializable on its own), ends up with exactly the messages Rig's agent
committed.

Each part of Rig's stream ends with the content Rig finalized, at the part's
position in the reply. A reply is exactly that content, with the origin and
stop reason Rig gave the turn. Nothing re-accumulates the stream. Fragments
only feed `TurnFold::partial`, a preview of the message being folded:
finished parts as Rig finalized them, and open ones as their fragments so
far, each where the reply will have it. The preview never joins the history.
A tool result is the message Rig commits for the call
(`AgentEvent::tool_result`).

The fold checks each turn's events as Rig checks a relayed stream, with
`StreamRecord::push`, and refuses one Rig's stream could not have produced,
such as a fragment of a part that never started. Events from elsewhere are
checked before they are folded.

Everything else about a run is the runner's: its tools, hooks (such as one
deciding in `on_dispatch` whether each call may run), turn budget, and tool
concurrency.

With the `test-support` feature, `agent::test_support::turn` scripts a turn
through Rig's mock model, so its events are exactly what Rig emits.

## Sketch

```rust,no_run
use agent::{AgentEvent, TurnFold};
use rig::{AgentBuilder, completion::Message, providers::ollama::Ollama};
use tools::Calculate;

# async fn example() -> anyhow::Result<()> {
let model = Ollama::new().native_completion("qwen3.8:27b");
let runner = AgentBuilder::new(model)
    .additional_params(serde_json::json!({
        "num_ctx": 131_072,
        "think": "medium"
    }))
    .tool(Calculate)
    .build()
    .prompt(Message::user("What is 1 + 2?"))
    .max_turns(10);

let mut history = vec![Message::user("What is 1 + 2?")];
let mut fold = TurnFold::default();
agent::run(runner, |event| {
    let folded = fold.apply(&event).expect("Rig's events are in order");
    if let Some(rig::completion::AssistantContent::ToolCall(call)) = folded.block {
        // This occurs as soon as the provider closes this call.
        println!("{}: {}", call.function.name, call.function.arguments_value());
    }
    if let AgentEvent::TurnEnded { usage, .. } = event {
        println!("turn used {usage:?}");
    }
    history.extend(folded.message);
})
.await?;
# Ok(())
# }
```

The callback is synchronous by design. A UI or server should forward events to
its own channel instead of doing expensive work in the callback.
