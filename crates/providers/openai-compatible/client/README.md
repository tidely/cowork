# openai-compatible

One `language_model::LanguageModelProvider` connection to a configurable
chat-completions server, aimed at vLLM and Ollama's compatible endpoint, without
an OpenAI account, SDK, or fixed model names. Conversion is in
`openai-compatible-types`.

```rust
use language_model::{CompletionRequest, LanguageModel, LanguageModelProvider, Message, Thinking};
use openai_compatible::Client;

# async fn example() -> Result<(), language_model::Error> {
let client = Client::new("http://localhost:8000/v1")?;
let models = client.list_models().await?;
let model = LanguageModel::new("local-model");
let request = CompletionRequest {
    thinking: Some(Thinking::Effort("custom-effort".into())),
    temperature: Some(0.2),
    ..CompletionRequest::new(vec![Message::user("Hello")])
};
let mut native =
    openai_compatible_types::ChatCompletionRequest::from_completion(&model, request)?;
native.extra.insert("top_k".into(), 5.into());
let stream = client.stream_native(native).await?;
let completion = language_model::collect_completion(stream).await?;
# Ok(())
# }
```

The API base includes `/v1`, and a reverse-proxy prefix is kept. URLs with
credentials, queries, or fragments are rejected. `with_api_key` sets a bearer
header; Debug omits address and credentials, errors are redacted, and
`Retry-After` (or `retry-after-ms`) becomes `Error::Http.retry_after`. Use a
no-redirect `with_http_client` when redirects are not trusted. Responses are
trusted: the client imposes no size limits on bodies, events, or error messages.

`stream` posts to `chat/completions` and supports one choice (`n=1`). It requires
a nonempty finish reason and `[DONE]`, including for length and tool stops, and
EOF before the marker is `IncompleteStream`. Usage-only chunks are folded in before
the one authoritative `Finished`. `stream_native` takes a typed
`ChatCompletionRequest`, sets `stream: true`, and sets
`stream_options.include_usage` unless the caller did (set it false to leave
counts unknown). Native callers own request validity.

- Typed controls map to `max_tokens` (accepted by both vLLM and Ollama, unlike
  `max_completion_tokens`), `temperature`, `top_p`, `stop`, and `seed`.
- `Thinking::Effort(value)` maps to `reasoning_effort`, checked against advertised
  levels. `Enabled` requires the model's `thinking.default_effort`, `Disabled`
  requires `thinking.disabled_effort`, and missing mappings fail. Discovery leaves
  these unknown, so callers configure them per server and model.
- Reasoning is sent as `reasoning` and read from `reasoning` or the legacy
  `reasoning_content`; identical fields are deduplicated and conflicting ones fail.
- Structured tool results become JSON text, and errors are prefixed `Tool error:`
  because the shared message has no error flag. Result IDs must match a preceding
  call, and missing provider IDs get a stable ID from the local call identity.
  Malformed arguments stay `Invalid`.
- Replay data, redacted reasoning, assistant and tool-result images, audio and
  video, legacy function calls, refusals, and non-text output parts fail
  explicitly.
- Tool indexes must form a contiguous range, and text or reasoning first appearing
  after tool calls, or reasoning after text, is rejected, since that history could
  not be replayed in its original block order.

Tests use synthetic local TCP fixtures and need no model server.
