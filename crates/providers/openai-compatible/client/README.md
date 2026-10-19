# openai-compatible

One `language_model::LanguageModelProvider` connection to a configurable compatible
chat-completions server. This initial implementation targets the common vLLM
and Ollama endpoints, without an OpenAI account, SDK, or fixed model names.

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

The API base includes `/v1`; a reverse-proxy prefix is retained. HTTP(S) URLs
with credentials, queries or fragments are rejected. Optional `with_api_key`
uses a sensitive bearer header. Debug omits both address and credentials;
transport failures keep their full cause chain but omit URLs, and provider error
messages are redacted. `Retry-After` (or `retry-after-ms`) on an HTTP error
becomes `Error::Http.retry_after`. A default reqwest client is used unless
`with_http_client` supplies one (`reqwest` is re-exported) to configure
timeouts, TLS, proxies and redirects. Use a no-redirect client when endpoint
redirects are not trusted. No automatic
retries, provider discovery, tool execution or background tasks are included.

`stream` uses SSE through POST `chat/completions`, supporting one choice only
(`n=1`). Streaming accepts byte-split UTF-8, CR/LF/CRLF,
comments and multiline data. It requires a nonempty finish reason and `[DONE]`,
including length/tool stops. Usage-only chunks are incorporated before the one
authoritative `Finished`; EOF before the marker is `IncompleteStream`. Dropping
the stream drops its body and provisional assembly. This cancels local I/O,
not necessarily server inference. Provider responses are trusted; the client does
not impose size limits on JSON bodies, SSE events or error messages.

`stream_native` accepts typed `openai_compatible_types::ChatCompletionRequest`
values and returns the same shared events as the generic `stream` method. It
selects streaming mode, sets `stream_options.include_usage: true` unless the
caller set it, and preserves other native controls. HTTP status is checked before returning
the stream. Native callers own request validity and must avoid duplicate extension
keys; the client checks the model name. Output remains the
supported single-choice subset described here.
The generic `LanguageModelProvider::stream(&model, request)` method accepts a
separate `LanguageModel` metadata record and converts with
`ChatCompletionRequest::from_completion`, validating tool correlation before mapping individual messages and
flattening tool results. Discovery returns these metadata records; a known model
can also be selected with `LanguageModel::new(id)`.
HTTP and SSE framing remain in this client. The typed controls map to
`max_tokens` (accepted by both vLLM and Ollama, unlike
`max_completion_tokens`), `temperature`, `top_p`, `stop`, and `seed`; supplying
one with its raw option (or `max_completion_tokens`) is an error.
Generic flat request options are forwarded, including `top_k`,
`chat_template_kwargs`, `reasoning_effort`, sampling and structured-output
settings. Structural `model/messages/tools/stream` overrides are errors;
recognized options populate typed fields and unknown options populate extensions,
so explicit and extension keys cannot duplicate. Invalid recognized option shapes
are rejected before HTTP. Optional null controls are treated as omitted.
Final usage is requested by default; `stream_options: {include_usage: false}`
opts out, leaving the counts unknown.
Legacy `functions/function_call` requests are unsupported.
Server/model support for vendor options is the caller's responsibility.

Shared `Thinking::Effort(value)` maps to `reasoning_effort`, checking advertised
effort levels when present and otherwise forwarding the model-defined value.
`Enabled` requires the model's configured `thinking.default_effort`; `Disabled`
requires its explicit `thinking.disabled_effort` off code. Missing mappings fail
as unsupported. Discovery leaves thinking metadata unknown; callers may configure
it for their server and model. `thinking: None` preserves server defaults and
permits the raw `reasoning_effort` option. Combining shared thinking with that raw
option, including null, is an error.

An effort value does not configure vendor-specific template flags. Servers may
also require controls such as `chat_template_kwargs.enable_thinking`; these stay
explicit provider options and are never inferred from the shared intent.

Text and user image parts are supported. Base64 images become data URLs.
Reasoning uses `reasoning` on input; output accepts `reasoning` and legacy
`reasoning_content`. Identical coexisting fields are deduplicated; conflicting
ones fail. The protocol groups text, reasoning and calls into separate fields,
so requests with unrepresentable cross-kind interleaving are rejected.
Structured tool results deliberately become JSON text, joined with other result
parts by newlines; errors are prefixed `Tool error:` because shared tool messages
have no error flag. Result IDs/names must match a preceding canonical call;
missing provider IDs get a stable ID based on the canonical call identity.
Malformed call arguments remain `ToolArguments::Invalid`, never repaired.

Native replay data, redacted reasoning, assistant images, tool-result images,
audio/video, legacy function calls, refusals and non-text output parts fail
explicitly. Unknown response metadata is retained in generation extras, but is
not replayed as request content. The adapter does not promise exact native
continuation across servers. Discovery reports only supplied metadata and leaves
unreported capabilities unknown.

Tool indexes must form a contiguous range. Out-of-order
fragments reserve stable preview slots in that range. A new text/reasoning block
first appearing after calls is unsupported; existing text/reasoning blocks can
still receive fragments. First reasoning appearing after text is also rejected.
These checks prevent accepting history that cannot be replayed without changing
indexed block order.

Primary sources consulted on 2026-10-19:

- [Ollama OpenAI compatibility](https://docs.ollama.com/api/openai-compatibility)
- [vLLM online serving](https://docs.vllm.ai/en/latest/serving/online_serving/)
- [vLLM chat protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/openai/chat_completion/protocol.py)
- [vLLM shared generation protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/generate/base/protocol.py)

Tests use synthetic local TCP fixtures, including vLLM tool fragments, Ollama
text/thinking, late usage, errors and truncated streams. They need no model server.

