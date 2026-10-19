# openai-compatible-types

Serde protocol data for `/v1/chat/completions` (request, response, SSE JSON
payloads), `/v1/models`, and structured error responses. It depends on the base
`language-model` crate for generic conversion, plus Serde, serde_json and base64.
No HTTP, SDK or agent runtime.

This deliberately models an interoperable subset rather than enforcing the
complete OpenAI API. Roles, content-part kinds, tool types, tool-choice modes,
and finish reasons are strings. Text and image URL input parts are represented
explicitly; unfamiliar part payloads and server extensions remain in `extra`
maps. Image URLs accept both object and bare-string forms. Tool arguments remain
raw strings, preserving fragments and malformed model output. Reasoning and
reasoning_content are separate optional fields because servers can emit both.

Stream payloads preserve choice indexes and tool indexes independently. IDs,
function names, arguments, and terminal deltas can be absent/null. Empty choices
with usage are valid. These types do not frame SSE, recognize `[DONE]`, assemble
fragments, or decide when an incomplete call is usable. Those are client concerns.
Known optional null fields normalize to omission on serialization; unknown JSON
fields retain their values. This is not a byte-exact JSON archive.

`ChatCompletionRequest::extra` supports server options such as vLLM's top_k and
chat_template_kwargs without an exhaustive vendor parameter schema. Constructed
extension maps must not duplicate explicit field names: Serde flattening does
not enforce that invariant. Generic request conversion checks reserved-key collisions
before sending configurable extension fields. The same rule applies to nested
extension maps. Raw wire containers do not validate role/content combinations,
JSON schemas, tool history linkage, or supported server features.

The initial scope excludes Responses, embeddings, legacy function_call,
audio/video handling, logprob interpretation, authentication, and retries.
Unknown fields for those features may be retained but are not supported behavior.
These provider wire types are not Cowork's versioned persistence format.

Primary sources consulted on 2026-10-19:

- [Ollama OpenAI compatibility](https://docs.ollama.com/api/openai-compatibility)
- [vLLM serving documentation](https://docs.vllm.ai/en/latest/serving/online_serving/)
- [vLLM chat-completion protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/openai/chat_completion/protocol.py)
- [vLLM shared generation protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/generate/base/protocol.py)
- [vLLM model, usage, and error protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/serve/engine/protocol.py)

Fixtures are small synthetic examples shaped after these protocols, not captured
traffic or a promise that every server/model supports every represented option.

`ChatCompletionRequest::from_completion(&model, request)`, with a shared
`language_model::LanguageModel` and `CompletionRequest`, performs
strict conversion into the native request. Sequence validation resolves tool
IDs and checks ordering, missing results and duplicate or mismatched results.
Each generic message then converts on its own, grouped tool results expanding
into one message each. Generic tool definitions convert with
`ToolDefinition::from`. The shared typed controls populate `max_tokens`,
`temperature`, `top_p`, `stop`, and `seed`, and conflict with the same raw
option. Known request options populate explicit typed fields; unknown server
options populate `extra`. No JSON serialization roundtrip
is used to construct requests. Unsupported replay data or content combinations
return an error rather than silently dropping information.

The client sets `stream: true` before sending, and requests usage unless the
caller set `stream_options.include_usage`.

Shared thinking intent maps to the native `reasoning_effort` field. Explicit
efforts are checked against configured model levels when known; unknown metadata
allows model-defined values through. Enabling requires a configured default
effort and disabling requires a configured off code; neither is guessed. Omitted
thinking leaves server defaults unchanged. A raw `reasoning_effort` option remains
available when shared thinking is absent, but combining the two is rejected.
Vendor template controls remain independent options: an effort setting alone
does not establish whether a server's template enables thinking.
