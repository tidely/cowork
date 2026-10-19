# ollama

Ollama's native HTTP provider for `language_model::LanguageModelProvider`, alongside the
existing Rig application. It uses `ollama-types` for the native wire protocol;
the accepted messages and persistence schemas are owned by `language-model`.
No agent loop, tool executor, approvals, database, or retries are included.

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

The base URL is the server root (without `/api`). A reverse-proxy prefix is
preserved: `https://host/prefix` becomes `/prefix/api/chat`. Credentials,
queries, fragments, and non-HTTP schemes are rejected. `with_api_key` validates
a bearer header and keeps it outside stored data. `Client` is cloneable and
deliberately does not implement Debug. A default reqwest client is used unless
`with_http_client` supplies one (`reqwest` is re-exported) to control TLS,
redirects, proxies, connection pooling, and timeouts.

`list_models` uses `/api/tags` and returns `LanguageModel` metadata records,
retaining complete native model metadata. It
does not guess capabilities or context limits from model names. `show_model`
exposes native `/api/show` metadata, including model-defined thinking controls.
Discovery leaves shared thinking capabilities unknown instead of guessing effort
levels from model names or evolving native metadata. Callers may supply known
levels and a default through `LanguageModel.thinking`.
`stream` uses native NDJSON with `stream: true`. Awaiting `stream`
sends the request and checks its HTTP status before returning the body stream.
Dropping the future/stream stops
local I/O; server-side inference cancellation is not guaranteed.

`stream_native(ChatRequest)` accepts the native typed request directly, preserving
its provider controls and selecting `stream: true`. It returns the same shared
`CompletionStream` output as `LanguageModelProvider::stream`. The trait method takes
the selected `LanguageModel` separately from the shared completion request, converts
shared requests and delegates to this entry point; native requests do not require
conversion through the shared message schema. Use `collect_completion` to collect
the stream into its final shared `Completion` when individual events are unneeded.

## Requests and conversion

`ollama-types` owns conversion from the shared `language-model` foundation:
`ChatRequest::from_completion(&LanguageModel, CompletionRequest)` builds the
native typed request, `Tool::from(ToolDefinition)` projects tool definitions, and
each generic message expands into one or more native messages. The client
handles HTTP and response accumulation.
The request conversion validates sequence-level tool correlation first and
fills omitted result names and provider IDs from their preceding calls. It then
extends the output with each fallible per-message conversion.
Request options map directly to typed native fields,
without serializing a request into an intermediate JSON object. The HTTP
operation sets the native request's stream flag before sending it.

`CompletionRequest.thinking` selects common thinking behavior. `Enabled` sends
the model's configured default effort when present, otherwise native `think: true`.
`Disabled` sends native `think: false`; `Effort(value)` sends that model-specific
string. Advertised effort levels are validated; unknown capability metadata permits
any nonempty level for the server to validate. An omitted thinking setting leaves
the server default intact. Supplying both shared thinking and the raw `think`
option is an error, including a raw null value.

The typed controls become native runtime options: `max_output_tokens` is
`options.num_predict`, and `temperature`, `top_p`, `stop`, and `seed` keep their
names. Supplying a typed control and the same raw runtime option is an error.

`CompletionRequest.options` accepts these native top-level controls:

- `think`: raw boolean, model-defined string, or null (server default), when the
  shared thinking field is omitted.
- `format`: a string or JSON Schema object.
- `keep_alive`: string or number.
- `options`: object of native runtime options, passed through intact.
- `truncate`, `shift`, `logprobs`: booleans.
- `top_logprobs`: nonnegative 32-bit integer.

Other top-level controls are rejected. In particular, options cannot replace
`model`, `messages`, `tools`, `stream`, or authentication. For example,
`{"options":{"num_ctx":32768},"think":"custom-level"}`
passes runtime settings in their native nested location. Supported option
values and model capabilities are ultimately validated by the server; the
adapter does not hard-code model IDs, sampling ranges, or thinking levels.

Text and visible thinking are concatenated into their respective native
fields; base64 images and tool calls retain their order in native arrays.
Assistant thinking must precede text/images, which must precede tool calls;
backwards interleaving is rejected. Native fields still merge adjacent blocks
and cannot preserve text/image placement. The canonical input is not mutated,
and its stored representation keeps those distinctions.

Native tool calls use full JSON argument objects. Responses with scalar,
array, string, or explicit-null arguments remain canonical `Invalid` arguments,
preserving the JSON value's text. Missing arguments remain `Invalid` with empty
raw text. They are never repaired to `{}`. An unnamed finalized call is an
invalid response. Replaying malformed arguments is explicitly unsupported.

Native streaming calls are completed call occurrences, not OpenAI-style
argument fragments. Each occurrence receives a distinct local identity, even
when names, arguments, or native indexes repeat. Nonempty provider IDs remain
separate and must be unique within a reply. Native indexes and unknown tool
metadata are retained in generation metadata instead of being treated as
guaranteed identities. Canonical results must match a preceding unresolved
local call; supplied names/provider IDs must agree, and pending calls must be
resolved before another conversational message or request. Tool results must
follow their call order, including across separate tool messages: id-less
same-name calls otherwise have no unambiguous native result association. The
adapter rejects reordered results instead of silently changing their order.

Each canonical tool result becomes one native tool message. Literal text stays
literal; JSON parts become compact JSON text, with newline separators between
parts. Images use the native message's base64 image array. Error results carry
an `Error: ` text prefix because the native message has no error flag. These
are deliberate request projections; canonical text/JSON/image distinctions and
the error flag remain unchanged in storage.

Developer roles, URL images, redacted reasoning, and opaque replay extensions
are rejected rather than silently dropped. Output assistant images are also
unsupported because native image data supplies no declared media type. No
provider-native replay format is required or emitted by this adapter. Native
response diagnostics, message extras, raw call metadata, unknown finish reasons,
and optional usage counters survive in generation metadata. Missing usage
counts stay unknown; a total is retained only if the server reports one.

## Completion and error guarantees

NDJSON framing operates on bytes and supports split UTF-8, CRLF, blank lines,
and a final complete JSON object without a newline. Each line is bounded at 8 MiB;
oversized lines and malformed UTF-8 fail as invalid responses. Deltas are provisional;
only one terminal `done: true` response produces authoritative `Finished`.
Unexpected EOF produces `IncompleteStream`, including after visible text.
HTTP failures, malformed JSON, in-band errors, and invalid assistant roles
never become successful empty completions.

HTTP diagnostics parse only the documented `error` field of the body;
arbitrary proxy bodies are not displayed. The server is trusted, so the message
is kept whole, with the configured bearer secret redacted. `Retry-After` (or
`retry-after-ms`) becomes `Error::Http.retry_after`. Transport errors keep their
full cause chain (e.g. a refused connection) with the request URL removed.

Loopback HTTP tests cover discovery/path prefixes, request conversion and auth,
split-byte streaming, thinking/tools/usage, repeated native tool occurrences,
malformed arguments, unsupported content, truncation and provider errors, HTTP
startup, body timeouts, line-length limits, and cancellation on drop. No Ollama
installation is required.

Research references (reviewed 2026-10-19):

- [Ollama chat API](https://docs.ollama.com/api/chat)
- [Native streaming](https://docs.ollama.com/api/streaming)
- [Tool calling and streaming accumulation](https://docs.ollama.com/capabilities/tool-calling)
- [Model discovery](https://docs.ollama.com/api/tags)
- [Provider errors](https://docs.ollama.com/api/errors)
- [Native wire declarations](https://github.com/ollama/ollama/blob/main/api/types.go)
- [Native parser call events and indexes](https://github.com/ollama/ollama/blob/main/model/parsers/deepseek3.go)


