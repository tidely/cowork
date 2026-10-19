# Tool runtime

The tool primitives for Cowork's provider-independent stack. This crate has no
Rig, GPUI, networking, sandbox policy, or agent-loop dependency. The existing Rig
tools have not migrated yet.

`Tool` has associated `Args` and `Output` types. Arguments deserialize from JSON
and supply a JSON Schema for the model-facing definition. Outputs implement
`ToolOutput`, retaining structured data separately from the model-facing content.
The blanket `ToolDyn` implementation lets unrelated tool implementations share
one registry through dynamic dispatch.

```rust
use language_model::{ToolCallId, ToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tool_runtime::{
    Tool, ToolCallContext, ToolCapabilities, ToolDispatch, ToolError, ToolOutput, ToolRegistry,
    collect_output,
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs { a: i32, b: i32 }

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
    fn name(&self) -> &str { "add" }
    fn description(&self) -> String { "Add two integers".into() }
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities { is_read_only: true, ..Default::default() }
    }
    async fn run(&self, _: ToolCallContext, args: AddArgs) -> Result<Sum, ToolError> {
        Ok(Sum { value: i64::from(args.a) + i64::from(args.b) })
    }
}

# async fn example() -> Result<(), ToolError> {
let mut registry = ToolRegistry::default();
registry.register(Add)?;
let context = ToolCallContext::new(ToolCallId::new());
let stream = registry.call("add", serde_json::json!({"a": 2, "b": 3}), context).await;
let result = collect_output(stream).await?;
// result.output retains {"value": 5}; result.model_output contains text "5".
# Ok(())
# }
```

`BuiltinToolOutput` is the enum application code will match for known built-in
results. It currently has only `Dynamic(Value)`, which also supports custom and
runtime-discovered tools. As built-ins migrate, they can add named variants and
override `ToolOutput::into_output` to select them. No schema versions or tool
persistence formats are introduced here.

Custom outputs usually need only `impl ToolOutput for MyOutput {}`. The default
serializes the result once into `Dynamic` and presents its JSON to the model.
An explicit model projection can instead return text, JSON, or images through
the existing `language_model::ToolResultContent`. An empty explicit projection
is an error; `None` selects the default. No JSON-shape heuristics interpret
arbitrary result fields as special content or UI instructions.

Simple tools implement `run`. Streaming tools implement `execute`, returning
`Progress` events followed by exactly one `Finished(Result<Output, ToolError>)`.
`finished_only`, `deferred_result`, and `with_progress` construct common stream
shapes. `with_progress` polls the final-result future only after progress ends.
`collect_output` discards progress and validates the complete stream, rejecting
missing or repeated final results and events after completion. Streaming callers
must consume and check that same contract; progress is never an accepted result.

`ToolCallContext` retains the shared local `ToolCallId`. Its `TypedExtensions`
holds host-provided services behind `Arc`, such as permission grants, cancellation
handles, or an output sink. Neither tool definitions nor context introduce a
provider identity. A host can implement `ToolDispatch` around a registry to add
approval, tracing, or other execution policy. Tool names and definitions must
remain stable for the life of their registration.

`ToolError` distinguishes dispatch/execution failures from successful domain
results. For example, a shell command's nonzero exit code can remain structured
command output, while an approval refusal is `PermissionDenied`. Host code
chooses how failures become model-facing error results.


## Execution metadata

`Tool::capabilities()` declares host-facing `ToolCapabilities`. The `ToolDyn`
adapter forwards them, including for heterogeneous toolsets; manually implemented
dynamic tools can declare them as well. Capabilities describe cancellation and
progress support, read-only behavior, maximum concurrency, and an optional
suggested timeout. Concurrency limits use `NonZeroU32`; unspecified limits and
timeouts leave the decision to the host. No timeout or limiter runs implicitly.

The registry captures a `ToolDescriptor { definition, capabilities }` when a tool
is registered. `ToolDispatch::descriptors()` lists those snapshots, and
`descriptor(name)` looks one up before execution. Dispatch wrappers can forward
or filter descriptors alongside calls. `definitions()` projects only their
model-facing definitions, keeping execution metadata out of model API schemas.
Listing or querying metadata never regenerates a registered tool's definition or
capabilities.

A permission/scheduling wrapper should inspect the descriptor before awaiting
`inner.call(...)`: simple tools can execute during that await, before their
stream is returned. The host can then acquire concurrency permits, wait for
approval, or attach cancellation services and grants to `ToolCallContext`.
Capabilities are declarations rather than permission grants. Read-only calls
still require authorization to access data. Cancellation support means the tool
cooperates with its host's cancellation service, not that dropping a future
rolls back work or stops a spawned process. The library does not synthesize a
cancellation service or automatically police progress events.

Metadata has no behavior versions, hub routing scopes, lifecycle hooks, or
transport frame limits. Those mechanisms are not part of this runtime.
