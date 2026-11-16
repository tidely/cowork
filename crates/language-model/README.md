# language-model

Cowork-owned conversation data and an object-safe `LanguageModelProvider` boundary. It
has no dependency on GPUI, HTTP, a database, or a provider wire schema.

`latest::Message` is the current in-memory type. It deliberately has no Serde
implementation. `v1::Message` is the first frozen storage DTO and implements
`VersionedType` with `cowork.llm.message.v1`. Nested v1 DTOs are frozen too.
Explicit `From` conversions connect the two. There is no fabricated earlier
schema and no serialized `latest` identifier.

```rust
use language_model::{Envelope, Message};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let message = Message::user("Hello");
let record = Envelope::encode_message(&message)?;
let bytes = serde_json::to_vec(&record)?;
let loaded: Envelope = serde_json::from_slice(&bytes)?;
let latest = loaded.decode_message()?;
// A future current-version encoder upgrades an older successfully decoded DTO.
let replacement = Envelope::encode_message(&latest)?;
# Ok(())
# }
```

The owner decides whether and how to replace a database record atomically.
Adding v2 means a new module, identifier, explicit conversion, and decode
dispatch branch; v1 remains intact. `decode_message` borrows its envelope and
rejects unknown identifiers, content kinds, or fields, leaving the original
available for retention or export; unknown fields are never silently dropped on
re-saving. Opaque replay payloads and provider metadata stay full JSON values.

Only JSON objects are executable tool arguments; arrays, scalars, null, and
broken JSON stay `Invalid`. Adapters reject unsupported content, invalid
correlation, and unrecognized replay data rather than dropping them. Unknown
token counts stay `None`. Base64 images persist inline; URL images keep only the
reference. Replay data is bound by convention to unchanged original content and
its provider and model, so editing content must clear or regenerate it, and exact
native replay across providers is not promised.

## Model and provider

`LanguageModel` is model information: ID, display name, and the facts code above
the provider acts on, plus everything else the provider reported in `metadata`.
Its ID is scoped to the connection that offers it. Discovery returns these
records; `LanguageModel::new(id)` selects a known ID without discovery. Each typed
field is `None` when unknown, which is not the same as unsupported. Why model
metadata is typed and provider behavior is not is in
[docs/agent-architecture.md](../../docs/agent-architecture.md).

`LanguageModelProvider` owns the connection; `provider.stream(&model, request)`
generates with a selected model. The model is borrowed during startup and the
returned stream owns its work. A request is flat plain data assembled once per
model call, without a duplicate model ID. Authentication belongs to the
connection, never to requests or stored messages.

- `instructions` is the system prompt in its own field, and requests with
  `System` messages are invalid. `cache` lists where the stable prefix ends.
- Typed controls (`max_output_tokens`, `temperature`, `top_p`, `stop`, `seed`)
  preserve provider defaults when `None` or empty. Adapters reject a typed control
  combined with its raw wire option; provider-only settings stay in `options`.
- Thinking is `Some(Thinking::Effort("medium".into()))`, `Enabled`, or `Disabled`;
  `None` preserves provider defaults. Effort strings are model-defined.
  `LanguageModel.thinking` optionally advertises `effort_levels`, `default_effort`,
  and `disabled_effort`. Discovery leaves them unknown rather than inferring them
  from model names, and explicit values with unknown capabilities reach the server
  for validation. How each adapter maps them is in its client README.

`stream` returns an owned stream of provisional indexed deltas and exactly one
final authoritative `Finished(Box<Completion>)`. A length stop still finishes a
request; agent policy decides whether tools may execute. Providers must yield
`IncompleteStream` on unexpected EOF, even after text was observed. All generation
streams; `collect_completion` only drains an existing stream and rejects missing
finals or events after a final. Dropping a future or stream cancels local work,
not necessarily server inference. Awaiting `stream` reports startup errors (the
HTTP providers check the response status first), and reading the stream reports
later ones. Timeouts and authorization are configured outside this trait.

`Error::Http.status` is the numeric status, so this crate needs no HTTP library,
and `retry_after` carries the server's requested delay. Provider diagnostics are
kept whole with credentials redacted, and transport errors keep their cause
chain. Errors are runtime values, not persisted schemas.

No agent loop, event journal, database, or tool executor belongs here.
