# ollama-types

Serde wire data for Ollama's native `POST /api/chat`, `GET /api/tags`, and
`POST /api/show`, with `ChatRequest::from_completion(&model, request)` converting
from the shared `language-model` types. There is no HTTP, stream reading, or agent
runtime here; framing, fragment accumulation, and final-chunk checks belong to the
client, and this is not Cowork's persisted schema.

Conversion validates tool correlation across the history, resolves omitted result
names and provider IDs, and expands each generic tool message into one native
message per result. Options map to typed native fields; the shared typed controls
become runtime `options` (`max_output_tokens` as `num_predict`) and conflict with
the same raw option. Assistant history must keep reasoning before text and images
and tool calls after them, because native fields cannot represent other
interleavings.

Thinking accepts booleans and model-defined strings. Shared thinking maps to
`think`, and supplying both it and raw `options.think` is an error, even for
null. Missing or null optional response fields do not imply zero usage. Tool
arguments that are not an object stay `ToolArguments::Invalid` rather than being
repaired, and unknown response fields are kept in `extra` maps, which callers must
not fill with known field names. Show metadata stays mostly optional because older
servers and remote models expose different details.
