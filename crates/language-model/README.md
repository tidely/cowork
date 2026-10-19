# language-model

Cowork-owned conversation data and an object-safe `LanguageModelProvider` boundary. It
has no dependency on Rig, GPUI, HTTP, a database, or a provider wire schema.
This is parallel groundwork; the existing application still uses Rig.

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
Adding v2 means a new module, identifier, explicit migration/conversion, and
decode dispatch branch; v1 remains intact. Converting from v1 to latest is
infallible today because their semantics match; future migrations requiring
validation should use a fallible dispatch step instead of inventing defaults.
`decode_message` borrows its envelope. Unknown identifiers, content kinds, or
additive fields are rejected, leaving the original envelope available for
retention/export. Unknown fields are not silently removed during re-saving.
Opaque replay payloads and provider metadata remain full JSON values.

Content order, literal text versus JSON tool results, image MIME/source,
reasoning redactions, malformed tool argument text, provider IDs, and local
identities survive conversion. Only JSON objects are executable arguments;
arrays, scalars, null, and broken JSON stay `Invalid`. Generic validation checks
role/content combinations. Adapters additionally reject unsupported content,
invalid correlation, and unrecognized replay data rather than dropping them.
Local IDs are opaque strings generated as UUIDs; provider IDs are not local
identities. Unknown token counts stay `None`; finish reasons stay raw strings.

Base64 images can be persisted inline. URL images retain a reference, not the
remote bytes; offline durability requires the application to materialize them.
Replay data is bound by convention to unchanged original content and its
provider/model. Editing content must clear/regenerate replay data. Exact native
replay across providers is not promised by a generic message schema.

`LanguageModel` is model information: its ID, display name, context window,
capabilities, and metadata. Its ID is scoped to the provider connection that
offers it. Discovery returns these records; `LanguageModel::new(id)` selects a
known ID without discovery. Missing metadata stays unknown.

`LanguageModelProvider` owns the provider connection. Call
`provider.stream(&model, request)` to generate with a selected model. The model
is borrowed during startup; the returned stream owns its work. Requests contain
history, tools, typed controls, and adapter-specific options, without a
duplicate model ID. `CompletionRequest::new(messages)` and `Message::user`,
`system`, and `assistant` build the common cases.
Implementations document accepted option names and refuse unsupported conversion.

Typed controls exist for what every supported provider adopts:
`max_output_tokens`, `temperature`, `top_p`, `stop`, and `seed`. `None` (or an
empty `stop`) preserves provider defaults, and adapters reject a typed control
combined with its raw wire option. Provider-only settings, such as Ollama's
context window, stay in `options`.
Authentication belongs to the provider connection, never requests or stored messages.

Thinking is a named request control: `thinking: Some(Thinking::Effort("medium".into()))`.
`Thinking::Enabled` and `Thinking::Disabled` express explicit intent; `None`
preserves provider defaults. Effort strings are model-defined, not a universal
enum. Adapters translate this intent to wire controls or return an error.

`LanguageModel.thinking` contains optional `effort_levels` (value/label choices),
`default_effort`, and `disabled_effort`. Unknown levels stay `None`; an explicitly
empty list means no named levels. Providers validate known choices; explicit
values with unknown capabilities reach the server for validation. Metadata comes
from model configuration or discovery when available. Current discovery leaves
these fields unknown rather than inferring values from model names.

For native Ollama, enabling uses a configured default effort or `think: true`,
and disabling uses `think: false`. Compatible endpoints require an explicitly
known default effort for enabling and a `disabled_effort` for disabling; they
never assume every server accepts `"none"`. Vendor-specific template flags still
belong in provider options. Specifying both shared thinking and its raw wire
option is an error, including when that raw option is null.

`stream` returns an owned stream with provisional indexed deltas and exactly
one final authoritative `Finished(Box<Completion>)` event. A length stop
still finishes a request; agent policy decides whether tools may execute.
Providers must yield `IncompleteStream` on unexpected EOF, even after text
was observed. Raw streams cannot enforce this just through a type alias;
adapters must implement it. All generation requests are streamed. The optional
`collect_completion` utility drains an existing stream and rejects missing finals
or events after a final; it never makes another request or selects a different
endpoint. Dropping a future/stream cancels local work, not necessarily server
inference. Timeouts and authorization are configured outside this trait.

`stream(&model, request).await` performs request startup and reports initial transport
or provider-status errors. Reading the returned stream performs body parsing
and reports subsequent errors. Both HTTP providers send and check the response
status before returning their stream.

No agent loop, event journal, database, automatic migration rewrite, or tool
executor is included. Those policies do not belong in provider-neutral types.

`Error::Http.status` is the numeric HTTP status, so this crate needs no HTTP
library. `retry_after` carries the server's requested delay (`Retry-After`),
typically on 429 or 503. Provider diagnostics are kept whole, with credentials
redacted; transport errors keep their cause chain. Errors are runtime values,
not persisted schemas.

Implementation references:

- [Serde container attributes](https://serde.rs/container-attrs.html): tagged
  schemas and explicit rejection of fields an older reader cannot preserve.
- [Rust trait object compatibility](https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility)
  and [BoxFuture](https://docs.rs/futures/latest/futures/future/type.BoxFuture.html):
  boxed `Send` futures keep the provider trait usable through `dyn` without an
  async-trait dependency.
