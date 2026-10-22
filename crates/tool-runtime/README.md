# Tool runtime

The tool primitives for Cowork's provider-independent stack. This crate has no
Rig, GPUI, networking, sandbox policy, or agent-loop dependency; authorization
and scheduling belong to [agent-runtime](../agent-runtime/README.md). The
existing Rig tools have not migrated yet.

A `Tool` has four associated types:

- `Args`: deserialized from the model's JSON, with a generated JSON Schema for
  the model-facing definition.
- `Output`: a `ToolOutput`, keeping structured data apart from the content the
  model sees.
- `Context`: the host services the tool needs, `()` for none. A registry's host
  context must provide it (`Provides<T::Context>`), which the compiler checks
  when the tool is registered, rather than a type-map lookup on every call.
- `Access`: what one call needs to be allowed, `()` when calls differ only by
  tool. `Tool::access` computes it from the arguments; the registry converts it
  into the host's access type (`A: From<T::Access>`) for authorization.

```rust
use std::sync::Arc;

use language_model::{ToolCallId, ToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tool_runtime::{
    CallControl, Provides, Tool, ToolCall, ToolError, ToolOutput, ToolRegistry, collect_output,
};

/// A service the host owns and hands to the tools that ask for it.
struct Ledger;

/// The host context of this registry.
struct Host {
    ledger: Arc<Ledger>,
}

impl Provides<Ledger> for Host {
    fn provide(self: &Arc<Self>) -> Arc<Ledger> {
        self.ledger.clone()
    }
}

/// The host's access type: here, whether a call writes.
#[derive(Clone, Debug, Default, PartialEq)]
struct Writes(bool);

#[derive(Deserialize, JsonSchema)]
struct AddArgs { a: i32, b: i32, record: bool }

#[derive(Serialize)]
struct Sum { value: i64 }

impl ToolOutput for Sum {
    fn model_output(&self) -> Option<Vec<ToolResultContent>> {
        Some(vec![ToolResultContent::Text { text: self.value.to_string() }])
    }
}

struct Add;

impl Tool for Add {
    type Args = AddArgs;
    type Output = Sum;
    type Context = Ledger;
    type Access = Writes;

    fn name(&self) -> &str { "add" }
    fn description(&self) -> String { "Add two integers".into() }
    fn access(&self, args: &AddArgs, _: &Ledger) -> Writes { Writes(args.record) }
    async fn run(&self, call: ToolCall<Writes>, _ledger: Arc<Ledger>, args: AddArgs) -> Result<Sum, ToolError> {
        // `call.access` is exactly what authorization allowed.
        let _ = call.access;
        Ok(Sum { value: i64::from(args.a) + i64::from(args.b) })
    }
}

# async fn example() -> Result<(), ToolError> {
let mut registry = ToolRegistry::<Host, Writes>::default();
registry.register(Add)?;
let host = Arc::new(Host { ledger: Arc::new(Ledger) });
// Preparing decodes the arguments and computes access; nothing runs yet.
let prepared = registry.prepare("add", serde_json::json!({"a": 2, "b": 3, "record": true}), &host)?;
assert_eq!(prepared.access(), &Writes(true));
let stream = prepared.execute(ToolCallId::new(), CallControl::default()).await;
let result = collect_output(stream).await?;
// result.output keeps {"value": 5}; result.model_output contains text "5".
# Ok(())
# }
```

Preparing and executing are separate so a host can authorize a call between
them: `prepare` never runs the tool, and awaiting `execute` may already run a
simple one. The prepared call keeps the access value it computed and passes it
to the tool in `ToolCall::access`, so a tool runs with exactly what was allowed
even if its context changed meanwhile.

`ToolCall` also carries the local `ToolCallId` and `CallControl`: cooperative
`cancellation` when the run ends, and `yield_wait` when new input should wake a
blocking wait without stopping what it observes. Neither promises rollback or
that external work stopped. Provider-issued call IDs belong to the transcript.

## Outputs and streams

`BuiltinToolOutput` is the enum application code matches for known built-in
results. It has only `Dynamic(Value)` so far, which also serves custom and
dynamic tools. Custom outputs usually need only `impl ToolOutput for MyOutput {}`:
the default serializes the result once and presents its JSON to the model. An
explicit `model_output` returns text, JSON, or images; an empty projection is an
error. No JSON-shape heuristics turn result fields into special content.

Simple tools implement `run`. Streaming tools implement `execute`, returning
`Progress` events followed by exactly one `Finished(Result<Output, ToolError>)`.
`finished_only`, `deferred_result`, and `with_progress` build common stream
shapes. `collect_output` discards progress and rejects missing or repeated final
results and events after completion; progress is never an accepted result.

`ToolError` separates dispatch and execution failures from successful domain
results: a shell command's nonzero exit can be structured output, while a
refused approval is `PermissionDenied`. Hosts decide how failures become
model-facing error results.

## Registries and metadata

`ToolRegistry<C, A>` holds tools for host context `C` and host access `A`.
Registering captures a `ToolDescriptor { definition, capabilities }` once;
listing and lookups never regenerate it. Only `definition` is sent to a model.
`ToolDyn` is the object-safe form; typed tools implement it through a blanket
impl, and dynamic tools (from MCP, for example) can implement it by hand.

`ToolCapabilities` declares cancellation and progress support, read-only
behavior, maximum concurrency, and a suggested timeout. They describe the
implementation, not one call, and grant nothing: read-only calls still need
authorization. No timeout or limiter runs implicitly.
