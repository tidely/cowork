# anthropic-types

Native Anthropic Messages wire data and `MessagesRequest::from_completion(&model, request)`,
which validates the whole history and produces typed native input. Instructions
become `system` blocks, consecutive conversation roles are merged, and tool
results become user content with a resolved `tool_use_id` and native `is_error`.
Malformed tool arguments can be read from responses but cannot be submitted.

- `max_output_tokens` (or raw `options.max_tokens`) is required to be positive
  and is never inferred from a context window. Seed and developer messages are
  unsupported.
- Shared `Thinking::Enabled` selects adaptive thinking, using a configured
  default effort when present; `Effort(value)` also sets `output_config.effort`.
  No model-name heuristics pick a thinking mode. Legacy budgeted thinking is
  available through the typed native request or the raw `thinking` option.
- Explicit controls conflict with equivalent raw options rather than replacing
  them. Only the raw options the converter lists are accepted; others, including
  attempts to overwrite model, messages, tools, or stream, are errors. Native
  request users can set cache controls and other protocol fields directly.
- Signed and redacted thinking are kept as shared reasoning plus versioned
  `anthropic.messages.content` replay data holding the native blocks, bound to
  the requested model (also when the server resolves an alias). Conversion
  verifies the replay against the content, so edited messages must clear or
  regenerate it, and unsigned reasoning is never sent as signed. Persist through
  `language_model::Envelope` so signatures and redactions survive.
