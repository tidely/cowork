use std::{collections::BTreeMap, sync::Arc};

use futures::future::BoxFuture;
use language_model::ToolDefinition;
use serde_json::Value;

use crate::{
    ArcTool, Tool, ToolCallContext, ToolDyn, ToolError, ToolErrorKind, ToolStream, TypedToolOutput,
    finished_only,
};

/// A host may wrap dispatch with permission checks, cancellation, or tracing.
/// Those policies stay outside the tool implementations and this contract.
pub trait ToolDispatch: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        context: ToolCallContext,
    ) -> BoxFuture<'a, ToolStream<TypedToolOutput>>;
}

/// An owned, heterogeneous toolset. Register before sharing it with concurrent
/// callers; execution borrows no mutable registry state or locks.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ArcTool>,
}

impl ToolRegistry {
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<(), ToolError> {
        self.register_dyn(Arc::new(tool))
    }

    pub fn register_dyn(&mut self, tool: Arc<dyn ToolDyn>) -> Result<(), ToolError> {
        let name = tool.definition().name;
        if name.trim().is_empty() {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "tool name must not be empty",
            ));
        }
        match self.tools.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(tool);
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
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        context: ToolCallContext,
    ) -> BoxFuture<'a, ToolStream<TypedToolOutput>> {
        Box::pin(async move {
            match self.tools.get(name) {
                Some(tool) => tool.execute(context, args).await,
                None => finished_only(Err(ToolError::new(
                    ToolErrorKind::NotFound,
                    format!("unknown tool {name}"),
                ))),
            }
        })
    }
}
