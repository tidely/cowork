# ollama-types

Serde wire data for Ollama's native `POST /api/chat`, `GET /api/tags`, and
`POST /api/show`, with conversions from the shared `language-model` foundation.
There is no HTTP client, stream reader, or agent runtime here. The shared
foundation owns generic conversation data and its independently versioned
storage schema; this provider crate owns its native projection.
`ChatRequest::from_completion(&model, request)`, with a shared
`language_model::LanguageModel` and `CompletionRequest`, validates conversation
correlation, resolves omitted result names and provider IDs, then collects and
flattens per-message conversions: one generic tool message may contain multiple
results, each requiring its own native message. `Tool::from(ToolDefinition)`
preserves full parameter schemas. Options map directly to native typed fields;
the shared typed controls become runtime `options` (`max_output_tokens` as
`num_predict`) and conflict with the same raw runtime option. Base64 image data
is validated here. The HTTP client chooses the request's
`stream` flag after conversion.

Assistant history must keep reasoning before text/images and tool calls after
them. Interleaving those groups cannot be represented by native message fields
and is rejected rather than silently regrouped.

`ChatResponse` is one JSON response/chunk. `ChatStreamItem` distinguishes a
chunk from an in-band error. Framing NDJSON, accumulating fragments, requiring
a final chunk, validating completed tool arguments, and interpreting model
capabilities are the communication adapter's responsibilities. A streamed
function may lack its name/arguments until completed; native calls may have no
ID. Missing/null optional fields do not imply zero usage. Durations are
nanoseconds and image strings are base64 payloads.

Native thinking accepts booleans and model-defined strings.
The shared request's `thinking` field maps enabled/disabled to native booleans,
or uses the model's configured default effort for enabled thinking. Explicit
effort strings are checked against advertised levels when known; unknown metadata
allows arbitrary nonempty model-defined levels. Omission preserves server defaults.
Raw `options.think` remains available only when shared thinking is absent; supplying
both is an error even when the raw value is null. Discovery does not guess levels.

Runtime options and JSON Schemas remain objects. Unexpected tool argument JSON is retained as
`ToolArguments::Invalid`; it is not repaired or parsed as a second JSON string.
Missing arguments are absent; explicit null is retained as invalid arguments.
Unknown response/message metadata is
retained in `extra` maps. This does not promise exact JSON byte preservation or
wire compatibility for arbitrary invalid values of known fields. Callers must
not put known field names into `extra`, which would produce duplicate keys.

Scope excludes model management, embeddings, `/api/generate`, and experimental
endpoints. Show metadata remains mostly optional because older servers and
remote models expose different detail sets.

Research references (reviewed 2026-10-19):

- [Ollama chat](https://docs.ollama.com/api/chat)
- [Ollama model list](https://docs.ollama.com/api/tags)
- [Ollama model details](https://docs.ollama.com/api-reference/show-model-details)
- [Ollama streaming](https://docs.ollama.com/api/streaming)
- [Ollama errors](https://docs.ollama.com/api/errors)
- [Ollama authoritative wire declarations](https://github.com/ollama/ollama/blob/main/api/types.go)


