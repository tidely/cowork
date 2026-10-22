# Language models

The new provider stack lives alongside Cowork's Rig integration. Cowork does not
use these crates yet; its agent loop, database, and sharing protocol are unchanged.
The examples run the [agent runtime](../agent-runtime/README.md) on these
providers.

## Examples

Both use local Ollama at `http://localhost:11434` with `gemma4:e4b`, thinking
disabled:

```text
cargo run -p language-models --example prompt   # define an agent, prompt it
cargo run -p language-models --example agent    # host, tools, approvals, dynamic tools
```

`prompt` is the managed API: an `AgentDefinition`, `Agent::builder`, `prompt`,
and `close`. `agent` implements its own `AgentHost`: note tools reach an
application service through their typed `Context`, writing needs approval
(answered with a `ResolveApproval` request from a UI task), a `Configure`
request changes the policy and the model is told in a context note, and a
dynamic tool is added and found through `search_tools` and `call_tool`.

| Crate | Responsibility |
| --- | --- |
| `provider-sse` | Byte-level SSE framing shared by the HTTP providers. |
| `anthropic-types` | Native Messages wire types, request conversion, and signed thinking replay. |
| `anthropic` | Anthropic model discovery and streamed Messages HTTP provider. |
| `language-model` | Owned conversation types, versioned message encoding, and the provider-independent completion contract. |
| `ollama-types` | Native Ollama request and response data, with Serde and no HTTP dependencies. |
| `ollama` | Native Ollama HTTP communication and conversion to the shared contract. |
| `openai-compatible-types` | Chat-completions request and response data for compatible servers. |
| `openai-compatible` | HTTP communication with configurable compatible endpoints, including vLLM and Ollama. |
| `language-models` | Construction and selection of those implementations through the shared contract. |

The protocol crates are in `crates/providers/<provider>/types`; their HTTP clients are
in `crates/providers/<provider>/client`. Provider clients depend on `language-model`,
never the reverse. Applications can use one client directly or use this facade.

```rust,no_run
use language_models::{CompletionRequest, Envelope, LanguageModel, LanguageModelProvider, Message, Ollama, collect_completion};

# async fn example(model: String) -> Result<(), Box<dyn std::error::Error>> {
let provider = Ollama::new("http://localhost:11434")?;
// Alternatively, use OpenAiCompatible::new with an API root such as
// http://localhost:8000/v1 (vLLM) or http://localhost:11434/v1 (Ollama).
let model = LanguageModel::new(model);
let request = CompletionRequest {
    max_output_tokens: Some(1024),
    ..CompletionRequest::new(vec![Message::user("Hello")])
};
let stream = provider.stream(&model, request).await?;
let reply = collect_completion(stream).await?;
let record = Envelope::encode_message(&reply.message)?;
# let _ = record;
# Ok(())
# }
```

Both clients use a default HTTP client unless `with_http_client` supplies a
`reqwest::Client` for transport policy (`language_models::reqwest` is
re-exported, so callers needn't match its version), and have an optional
`with_api_key` builder. Neither credentials nor connection objects
are part of a persisted message. `LanguageModelProvider` is the connection trait;
`LanguageModel` contains model information returned by its `list_models` method.
The constructors return concrete clients so callers can configure authentication,
use native requests, and inspect provider-specific metadata. Applications choose
`Box<dyn LanguageModelProvider>` or `Arc<dyn LanguageModelProvider>` when they need dynamic selection or
sharing. There is no provider enum or manually forwarded dispatch.

## Persistence boundary

Persist the shared message envelope, not a provider's request or response.
Every persisted message has a fixed type identifier containing its version.
`latest` names the in-memory representation and is never a persisted version.
Reading an envelope selects its historical schema and converts it to the current
representation; writing selects the current fixed schema. The caller owns any
database transaction that replaces an older record. Unknown versions must remain
intact rather than be silently interpreted or overwritten.

Provider wire types follow their endpoint's protocol and depend on the generic
foundation. Their `from_completion(&model, request)` constructors build native
typed requests; the clients set streaming mode and send those requests.
Per-message conversion expands grouped tool results, while whole-history
correlation is checked at the request boundary. No provider format becomes Cowork's persisted schema.

Provider-specific request options and replay data must not be mistaken for
portable capabilities. Reverse conversions are separate operations and cannot
promise a lossless round trip through a provider's more limited format.

Use `CompletionRequest.thinking` and the typed controls (`max_output_tokens`,
`temperature`, `top_p`, `stop`, `seed`) where the provider supports them,
leaving `options` for provider-specific settings. Effort values remain model-defined strings.
`LanguageModel.thinking` can advertise value/label choices and explicit defaults;
unknown metadata stays absent. `None` preserves server defaults, and adapters
reject unsupported explicit controls or conflicting raw overrides.

Concrete clients also accept native requests through `stream_native`, returning
the same shared completion events. This lets callers
set typed provider controls after generic conversion. These methods bypass the
generic history validator; raw wire requests remain the caller's responsibility.

All generation requests stream. `collect_completion` only drains an existing
stream when previews are unnecessary; there is no separate non-streaming API.

## Initial scope

Chat completion, streamed output, tool calls/results, reasoning, images, usage,
and model discovery cover the existing app's provider needs. The compatible
provider uses the chat-completions API; it does not require OpenAI's SDK, account,
or Responses API. Optional server-specific fields belong at the provider boundary.

An accepted completion is the authoritative message. Streamed previews do not
become a completed reply if the stream ends early. Dropping a request or stream
cancels local HTTP work; it does not promise remote computation has stopped.

The opt-in tests in `tests/live_ollama.rs` check streaming, versioned message
encoding, and a second request using the returned history, through both native
and compatible Ollama APIs. Run with a local server and installed model:

```text
cargo test -p language-models --test live_ollama -- --ignored --nocapture --test-threads=1
```

`tests/live_agent.rs` runs an agent with a base tool and a dynamic tool reached
through `call_tool`:

```text
cargo test -p language-models --test live_agent -- --ignored --nocapture
```

They default to `gemma4:e4b`. `OLLAMA_URL` and `OLLAMA_MODEL` select another
server or model; the model must support tools and the chosen controls.


## Anthropic

`Anthropic::new("https://api.anthropic.com/v1")?.with_api_key(key)?` supplies
another implementation of the same provider trait. Set `max_output_tokens`
explicitly. Shared thinking uses adaptive mode; `CompletionRequest::cache`
points become `cache_control` breakpoints, and legacy budgets are available
through `anthropic-types::MessagesRequest`. Signed
thinking replay survives the shared message envelope. See
[the client documentation](../providers/anthropic/client/README.md) for the
supported subset and transport behavior.


