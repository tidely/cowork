# agent

A deliberately small multi-turn agent loop built on Rig's low-level
`CompletionModel` stream.

The important distinction from Rig's standard multi-turn stream is that
everything is observable as it streams, including a completed tool call:

```text
provider ToolCall block ends
  -> AgentEvent::Model(BlockEnd) completes the call, while the turn streams on
  -> the rest of the model turn is consumed
  -> AgentEvent::TurnEnded: the reply joins the history
  -> tools execute, each reporting AgentEvent::ToolResult
  -> once all have returned, their results become the next prompt
```

This preserves valid conversation history without hiding completed calls until
the whole model turn commits.

A run is fully described by its events. `TurnFold` folds them into the
messages they add to the history, and the loop records its own history by
folding them, so anyone folding the same events, in this process or after
sending them elsewhere (they are serializable), ends up with exactly the same
history.

## Sketch

```rust,no_run
use agent::{Agent, AgentEvent, TurnFold};
use rig::{completion::Message, prelude::*, providers::ollama::wire::Ollama, tool::ToolSet};
use tools::{RespondToComment, TurnComments};

# async fn example() -> anyhow::Result<()> {
let client = Ollama::new().bound()?;
let model = client.completion("qwen3.8:27b");

let comments = std::sync::Arc::new(TurnComments::new(2));
let mut tools = ToolSet::default();
tools.add_tool(RespondToComment::new(comments));

let mut history = Vec::new();
let mut fold = TurnFold::default();
Agent::new(model, tools)
    .additional_params(serde_json::json!({
        "num_ctx": 131_072,
        "think": "medium"
    }))
    .run(Message::user("Reply to comment_1 and comment_2."), &mut history, |event| {
        let folded = fold.apply(&event);
        if let Some(rig::completion::AssistantContent::ToolCall(call)) = folded.block {
            // This occurs as soon as the provider's record for this call arrives.
            println!("{}: {}", call.function.name, call.function.arguments);
        }
        if let AgentEvent::TurnEnded { usage, .. } = event {
            println!("turn used {usage:?}");
        }
    })
    .await?;
# Ok(())
# }
```

The callback is synchronous by design. A UI or server should forward events to
its own channel instead of doing expensive work in the callback.
