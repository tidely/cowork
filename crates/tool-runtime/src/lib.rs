#![doc = include_str!("../README.md")]

mod capabilities;
mod context;
mod dispatch;
mod error;
mod output;
mod tool;

pub use capabilities::ToolCapabilities;
pub use context::{ToolCallContext, TypedExtensions};
pub use dispatch::{ToolDescriptor, ToolDispatch, ToolRegistry};
pub use error::{ToolError, ToolErrorKind};
pub use output::{BuiltinToolOutput, ToolOutput, TypedToolOutput};
pub use tool::{
    ArcTool, Tool, ToolDyn, ToolProgress, ToolStream, ToolStreamItem, collect_output,
    deferred_result, finished_only, with_progress,
};

#[cfg(test)]
mod tests;
