# agent

The agent stack in one crate, which the Cowork app builds on. It re-exports:

- the session runtime ([agent-runtime](../agent-runtime/README.md)) at the root:
  `AgentDefinition`, `AgentSession`, `AgentHost`, the managed `Agent`;
- the provider-neutral model contract ([language-model](../language-model/README.md))
  as `agent::model`;
- typed tools ([tool-runtime](../tool-runtime/README.md)) as `agent::tool`;
- the provider connections `Ollama`, `OpenAiCompatible`, and `Anthropic`, whose
  protocol types and HTTP clients are in `crates/providers/<provider>/types` and
  `.../client`.

Layering and who owns which state are in
[docs/agent-architecture.md](../../docs/agent-architecture.md). Provider clients
depend on `language-model`, never the reverse, so an application can use one
client directly or this facade.

```rust,no_run
use agent::{Ollama, model::{CompletionRequest, LanguageModel, LanguageModelProvider, Message, collect_completion}};

# async fn example(model: String) -> Result<(), Box<dyn std::error::Error>> {
let provider = Ollama::new("http://localhost:11434")?;
// Or OpenAiCompatible::new with an API root such as http://localhost:8000/v1
// (vLLM), or Anthropic::new("https://api.anthropic.com/v1")?.with_api_key(key)?.
let model = LanguageModel::new(model);
let request = CompletionRequest {
    max_output_tokens: Some(1024),
    ..CompletionRequest::new(vec![Message::user("Hello")])
};
let reply = collect_completion(provider.stream(&model, request).await?).await?;
# let _ = reply;
# Ok(())
# }
```

The constructors return concrete clients, so callers can configure
authentication (`with_api_key`) and transport (`with_http_client`, with
`agent::reqwest` re-exported), use native requests (`stream_native`), and inspect
provider-specific metadata. Use `Box` or `Arc<dyn LanguageModelProvider>` for
dynamic selection or sharing; there is no provider enum.

## Examples and live tests

The examples and live tests use local Ollama at `http://localhost:11434` with
`gemma4:e4b`; `OLLAMA_URL` and `OLLAMA_MODEL` select another server or model,
which must support tools.

```text
cargo run -p agent --example prompt   # the managed API: define an agent, prompt it
cargo run -p agent --example agent    # own host: tools, approvals, policy change, dynamic tools
cargo run -p agent --example notebook # todo-list agent; a second source steers it (hardcodes its own model)
cargo test -p agent --test live_ollama -- --ignored --nocapture --test-threads=1
cargo test -p agent --test live_agent -- --ignored --nocapture
```
