# anthropic

An Anthropic Messages implementation of `language_model::LanguageModelProvider`,
exported by the facade as `agent::Anthropic`. One cloneable client is one
connection, with credentials and transport policy separate from model metadata
and stored messages. Conversion to native requests is in `anthropic-types`.

```rust,no_run
use language_model::{CompletionRequest, LanguageModel, LanguageModelProvider, Message, collect_completion};

# async fn example(key: &str, model_id: &str) -> Result<(), language_model::Error> {
let provider = anthropic::Client::new("https://api.anthropic.com/v1")?
    .with_api_key(key)?;
let model = LanguageModel::new(model_id);
let request = CompletionRequest {
    max_output_tokens: Some(4096),
    ..CompletionRequest::new(vec![Message::user("Hello")])
};
let completion = collect_completion(provider.stream(&model, request).await?).await?;
# let _ = completion;
# Ok(())
# }
```

The base URL includes `/v1`, and reverse-proxy prefixes are preserved. URL
credentials, queries, fragments, and non-HTTP schemes are rejected. The key goes
in a sensitive `x-api-key` header with `anthropic-version: 2023-06-01`, and no
environment variables are read. Debug output omits credentials and addresses, and
errors redact the key. `with_http_client` supplies TLS, proxy, redirect, and
timeout policy.

`list_models` follows cursor pagination and keeps native metadata. Explicit
`max_input_tokens` becomes the context window; nothing is guessed from IDs.
`stream_native(MessagesRequest)` bypasses generic conversion and sets only
`stream: true`; native callers own request validity.

Only `message_stop`, after a stop reason and closed blocks, emits the
authoritative `Finished`. EOF, invalid event ordering, missing signatures, unknown
content, and provider errors never become a successful partial response. Tool-call
previews keep the same local identity as their final call, and malformed final
arguments stay `ToolArguments::Invalid`. Shared input usage sums Anthropic's base,
cache-read, and cache-creation input tokens, and the original counters stay in
generation metadata.

Tests use loopback HTTP servers and need no API key.
