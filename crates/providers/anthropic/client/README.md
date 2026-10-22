# anthropic

An Anthropic Messages implementation of `language_model::LanguageModelProvider`.
One cloneable client is one connection, with credentials and transport policy
separate from model metadata and stored messages. No agent loop, automatic
retries, approvals, or background tasks are included.

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

The facade exports this client as `agent::Anthropic`. Clients implement
the object-safe provider trait and can be stored as `Box<dyn LanguageModelProvider>`
or `Arc<dyn LanguageModelProvider>` alongside the other providers.

The base includes `/v1`; reverse-proxy prefixes are preserved. URL credentials,
queries, fragments, and non-HTTP schemes are rejected. Authentication uses a
sensitive `x-api-key` header with `anthropic-version: 2023-06-01`. No environment
variables are read. Debug omits credentials and endpoint addresses. Provider
errors redact the configured key; transport errors preserve their cause chain
without the URL. HTTP errors expose status and parsed Retry-After hints.
`with_http_client` supplies optional TLS, proxy, redirect, default-header,
connection-pool, and timeout policy (`reqwest` is re-exported).

`list_models` follows cursor pagination and retains native metadata, including
capabilities. Explicit `max_input_tokens` becomes the context-window value;
effort levels and defaults remain unknown rather than being guessed from IDs.
Malformed or repeating pagination is an error. A known ID can be used without
discovery.

`stream` converts shared input through `anthropic-types`, starts HTTP, and checks
status before returning. `stream_native(MessagesRequest)` bypasses generic
conversion and preserves native controls, setting only `stream: true`. Native
callers own request validity. Both return the same shared completion stream.

SSE framing handles byte-split UTF-8, CR/LF/CRLF, comments, and multiline data.
The parser checks message/block start, delta, stop ordering and block types.
Tool-call previews keep the same local identity as their final call; fragmented
arguments remain provisional and malformed final JSON stays `ToolArguments::Invalid`.
Text and reasoning preserve indexed block order. Signatures and redactions
survive through native replay data (see the types crate).

Only `message_stop`, after a stop reason and closed blocks, emits authoritative
`Finished`. EOF, invalid event ordering, missing signatures, unknown content,
and provider errors never become a successful partial response. Unknown stop
reason strings survive. Shared input usage includes Anthropic's base, cache-read,
and cache-creation input tokens; output is cumulative, and total is derived only
when both counts are known. Original counters and completion diagnostics remain
in generation metadata.

Dropping the startup future or returned stream cancels local HTTP work; remote
inference cancellation is not guaranteed. Response sizes and timeouts follow the
configured transport and trusted endpoint, as with the existing compatible client.

Tests use loopback HTTP servers, including one-byte response writes. No API key,
Anthropic account, live model, or paid request is needed:

```text
cargo test -p anthropic-types -p anthropic
```
