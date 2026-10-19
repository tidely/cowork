use std::{collections::BTreeMap, sync::Arc};

use futures::future::BoxFuture;
use language_model::ToolDefinition;
use serde_json::Value;

use crate::{
    ArcTool, Tool, ToolCallContext, ToolCapabilities, ToolDyn, ToolError, ToolErrorKind,
    ToolStream, TypedToolOutput, finished_only,
};

/// A registration's model-facing definition and host-facing execution metadata.
/// These are captured together so policy sees the same tool identity the model
/// was offered. Only `definition` is sent to a model provider.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDescriptor {
    pub definition: ToolDefinition,
    pub capabilities: ToolCapabilities,
}

/// A host may wrap dispatch with permission checks, cancellation, or tracing.
/// Inspect the descriptor before calling the inner dispatcher: awaiting `call`
/// can already execute a tool, even before its result stream is returned.
pub trait ToolDispatch: Send + Sync {
    fn descriptors(&self) -> Vec<ToolDescriptor>;

    /// Lookup for per-call policy. Wrappers can forward or filter descriptors;
    /// registries override this default with an indexed lookup.
    fn descriptor(&self, name: &str) -> Option<ToolDescriptor> {
        self.descriptors()
            .into_iter()
            .find(|descriptor| descriptor.definition.name == name)
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.descriptors()
            .into_iter()
            .map(|descriptor| descriptor.definition)
            .collect()
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        context: ToolCallContext,
    ) -> BoxFuture<'a, ToolStream<TypedToolOutput>>;
}

struct RegisteredTool {
    descriptor: ToolDescriptor,
    tool: ArcTool,
}

/// An owned, heterogeneous toolset. Register before sharing it with concurrent
/// callers; execution borrows no mutable registry state or locks. Descriptors
/// are snapshots: listing and policy lookup never recompute a tool's metadata.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

impl ToolRegistry {
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<(), ToolError> {
        self.register_dyn(Arc::new(tool))
    }

    pub fn register_dyn(&mut self, tool: Arc<dyn ToolDyn>) -> Result<(), ToolError> {
        let definition = tool.definition();
        let name = definition.name.clone();
        if name.trim().is_empty() {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "tool name must not be empty",
            ));
        }
        match self.tools.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                let descriptor = ToolDescriptor {
                    definition,
                    capabilities: tool.capabilities(),
                };
                entry.insert(RegisteredTool { descriptor, tool });
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(entry) => Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                format!("tool {} is already registered", entry.key()),
            )),
        }
    }
}

impl ToolDispatch for ToolRegistry {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    fn descriptor(&self, name: &str) -> Option<ToolDescriptor> {
        self.tools
            .get(name)
            .map(|registered| registered.descriptor.clone())
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        context: ToolCallContext,
    ) -> BoxFuture<'a, ToolStream<TypedToolOutput>> {
        Box::pin(async move {
            match self.tools.get(name) {
                Some(registered) => registered.tool.execute(context, args).await,
                None => finished_only(Err(ToolError::new(
                    ToolErrorKind::NotFound,
                    format!("unknown tool {name}"),
                ))),
            }
        })
    }
}
