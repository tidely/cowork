# anthropic-types

`MessagesRequest::from_completion(&model, request)` validates the whole history
and produces typed native input. Leading system messages become `system` blocks;
consecutive conversation roles are merged. Tool results become user content with
resolved `tool_use_id` and native `is_error`. Text, JSON-as-text, and image tool
results are supported. Provider IDs are separate from local tool-call identities.
Unanswered, duplicate, or mismatched results are errors. Malformed arguments can
be observed in responses but cannot be submitted as native tool input.

`max_output_tokens` (or raw `options.max_tokens`) must be positive and fit a
32-bit integer. It is never inferred from a context-window limit. Temperature,
top-p, and stop sequences map directly. Seed and developer messages are
unsupported. Shared `Thinking::Enabled` selects adaptive thinking and uses a
configured default effort when present; `Effort(value)` also sets
`output_config.effort`. No model-name heuristics select a thinking mode. Legacy
budgeted thinking is available through the typed native request or the raw
`thinking` option. Explicit controls conflict with equivalent raw options rather
than silently replacing them.

Accepted raw options are `max_tokens`, `temperature`, `top_p`, `top_k`,
`stop_sequences`, `thinking`, `output_config`, `tool_choice`, and `metadata`.
Other options are rejected, including attempts to overwrite model, messages,
tools, or stream. Native request users can set cache controls and other declared
protocol fields directly.

Images support JPEG, PNG, GIF, and WebP base64 input and HTTP(S) URLs. Assistant
images and citation-bearing or unrecognized output blocks are unsupported.

Signed thinking and redacted thinking are preserved as shared reasoning plus
versioned `anthropic.messages.content` replay data containing the native content
blocks. The client binds replay to the requested model, including when the server
resolves an alias. Conversion verifies the replay's provider, format, version,
model, and projected content. Edited messages must clear or regenerate replay;
unsigned reasoning cannot be sent as signed Anthropic reasoning. Persist through
`language_model::Envelope` so signatures and opaque redactions survive.

References:

- [Messages streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Models API](https://platform.claude.com/docs/en/api/models/list)
- [Thinking controls](https://platform.claude.com/docs/en/build-with-claude/thinking-steering-and-cost)
