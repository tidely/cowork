# ollama

Ollama's native HTTP provider for `language_model::LanguageModelProvider`, which
the Cowork app runs its models on. Request conversion is in `ollama-types`.

```rust,no_run
use language_model::{CompletionRequest, LanguageModel, LanguageModelProvider, Message, collect_completion};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
// The custom HTTP client is optional.
let http = ollama::reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(120))
    .build()?;
let provider = ollama::Client::new("http://localhost:11434")?.with_http_client(http);
let model = LanguageModel::new("your-installed-model");
let request = CompletionRequest::new(vec![Message::user("Hello")]);
let stream = provider.stream(&model, request).await?;
let reply = collect_completion(stream).await?;
# Ok(())
# }
```

The base URL is the server root without `/api`; a reverse-proxy prefix is
preserved. Credentials, queries, fragments, and non-HTTP schemes are rejected.
`with_api_key` sets a bearer header, and `Client` deliberately does not implement
Debug.

`list_models` uses `/api/tags` and keeps complete native metadata without guessing
capabilities or context limits from model names; `show_model` exposes `/api/show`.
Thinking levels stay unknown unless the caller supplies them through
`LanguageModel.thinking`. `stream_native(ChatRequest)` sends a native request,
selecting `stream: true`, and returns the same shared stream as `stream`.

## Mapping

- `thinking`: `Enabled` sends the configured default effort, else `think: true`;
  `Disabled` sends `think: false`; `Effort(value)` sends that string, checked
  against advertised levels when known.
- `max_output_tokens` is `options.num_predict`. `CompletionRequest.options` takes
  only the listed native top-level controls (`think`, `format`, `keep_alive`,
  `options`, `truncate`, `shift`, `logprobs`, `top_logprobs`); others, including
  `model`, `messages`, `tools`, and `stream`, are rejected. Ollama's `num_ctx`
  goes in the nested `options` object.
- Native tool calls are complete occurrences, not fragments, so each gets a
  distinct local identity even when names and arguments repeat. Results must
  follow their calls in order, since id-less same-name calls have no other
  association, and reordered results are rejected. Scalar, array, string, null, or
  missing arguments stay `Invalid` and are never repaired.
- Each tool result becomes one native tool message: text stays literal, JSON
  becomes compact text, images use the base64 array, and errors get an `Error: `
  prefix because the native message has no error flag. The canonical record keeps
  these distinctions.
- Developer roles, URL images, redacted reasoning, and replay extensions are
  rejected rather than dropped. A finished reply puts thinking, then text and
  images, then tool calls in order whatever order the model streamed them, so it
  can be sent back.

## Streaming and errors

NDJSON is framed on bytes (split UTF-8, CRLF, a final object without a newline),
with each line capped at 8 MiB. Only the one `done: true` response produces the
authoritative `Finished`; EOF gives `IncompleteStream`, even after visible text,
and HTTP failures, malformed JSON, and in-band errors never become empty
completions. Error diagnostics parse only the documented `error` field, with the
bearer secret redacted, and `Retry-After` (or `retry-after-ms`) becomes
`Error::Http.retry_after`. Tests use loopback servers and need no Ollama.
