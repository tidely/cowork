# agent

A deliberately small multi-turn agent loop built on Rig's low-level
`CompletionModel` stream.

The important distinction from Rig's standard multi-turn stream is when a
completed tool call becomes observable:

```text
provider ToolCall block ends
  -> AgentEvent::ToolCall is emitted immediately
  -> the rest of the model turn is consumed
  -> the complete assistant message is committed to history
  -> tools execute
  -> tool results become the next prompt
```

This preserves valid conversation history without hiding completed calls until
the whole model turn commits.

## Sketch

```rust,no_run
use agent::{Agent, AgentEvent};
use rig::{completion::Message, prelude::*, providers::ollama::wire::Ollama, tool::ToolSet};
use tools::{RespondToComment, TurnComments};

# async fn example() -> anyhow::Result<()> {
let client = Ollama::new().bound()?;
let model = client.completion("qwen3.8:27b");

let comments = std::sync::Arc::new(TurnComments::new(2));
let mut tools = ToolSet::default();
tools.add_tool(RespondToComment::new(comments));

let mut history = Vec::new();
let response = Agent::new(model, tools)
    .additional_params(serde_json::json!({
        "num_ctx": 131_072,
        "think": "medium"
    }))
    .run(Message::user("Reply to comment_1 and comment_2."), &mut history, |event| {
        match event {
            AgentEvent::ToolCall(call) => {
                // This occurs as soon as Ollama's record for this call arrives.
                println!("{}: {}", call.function.name, call.function.arguments);
            }
            AgentEvent::Model(_) | AgentEvent::ToolResult { .. } => {}
        }
    })
    .await?;

println!("final response: {:?}", response.choice);
# Ok(())
# }
```

The callback is synchronous by design. A UI or server should forward events to
its own channel instead of doing expensive work in the callback.
