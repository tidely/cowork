# openai-compatible-types

Serde protocol data for `/v1/chat/completions` (request, response, SSE payloads),
`/v1/models`, and error responses, plus
`ChatCompletionRequest::from_completion(&model, request)` converting from the
shared `language-model` types. There is no HTTP or SDK here; SSE framing, `[DONE]`,
and fragment assembly are client concerns, and these are not Cowork's persisted
schema.

This is an interoperable subset, not the whole OpenAI API. Roles, part kinds, and
finish reasons are strings; unfamiliar payloads and server extensions stay in
`extra` maps, so vLLM's `top_k` or `chat_template_kwargs` need no schema. Tool
arguments stay raw strings to preserve fragments and malformed output. Known
optional nulls drop on serialization, so this is not a byte-exact archive.
Extension maps must not repeat explicit field names, because Serde flattening does
not prevent it; conversion checks reserved keys. Responses, embeddings, legacy
`function_call`, and audio or video are out of scope.

Conversion validates tool correlation across the history, then converts messages
one at a time, expanding grouped tool results. The shared typed controls fill
`max_tokens`, `temperature`, `top_p`, `stop`, and `seed` and conflict with the same
raw option. Known options fill typed fields and unknown ones go to `extra`, and
unsupported replay data or content is an error rather than dropped. Shared thinking
maps to `reasoning_effort`; enabling needs a configured default effort and
disabling a configured off value, and neither is guessed. A raw `reasoning_effort`
is accepted only when shared thinking is absent. Template controls such as
`enable_thinking` stay independent options.
