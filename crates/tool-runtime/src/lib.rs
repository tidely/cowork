#![doc = include_str!("../README.md")]

mod call;
mod capabilities;
mod error;
mod output;
mod registry;
mod tool;

pub use call::{CallControl, ToolCall};
pub use capabilities::ToolCapabilities;
pub use error::{ToolError, ToolErrorKind};
pub use output::{BuiltinToolOutput, ToolOutput, TypedToolOutput};
pub use registry::{RegisteredTool, ToolDescriptor, ToolRegistry};
pub use tool::{
    PreparedCall, Tool, ToolDyn, ToolProgress, ToolStream, ToolStreamItem, collect_output,
    deferred_result, finished_only, with_progress,
};

#[cfg(test)]
mod tests;
