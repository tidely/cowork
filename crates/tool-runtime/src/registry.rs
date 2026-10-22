use std::{collections::BTreeMap, sync::Arc};

use language_model::ToolDefinition;
use serde_json::Value;

use crate::{PreparedCall, Provides, Tool, ToolCapabilities, ToolDyn, ToolError, ToolErrorKind};

/// A registration's model-facing definition and host-facing execution metadata,
/// captured together when the tool is registered, so policy sees the same
/// identity the model was offered. Only `definition` is sent to a provider.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDescriptor {
    pub definition: ToolDefinition,
    pub capabilities: ToolCapabilities,
}

impl ToolDescriptor {
    pub fn name(&self) -> &str {
        &self.definition.name
    }
}

/// One registered tool: its captured descriptor and its implementation.
pub struct RegisteredTool<C: ?Sized, A> {
    pub descriptor: ToolDescriptor,
    pub tool: Arc<dyn ToolDyn<C, A>>,
}

impl<C: ?Sized, A> Clone for RegisteredTool<C, A> {
    fn clone(&self) -> Self {
        Self {
            descriptor: self.descriptor.clone(),
            tool: self.tool.clone(),
        }
    }
}

impl<C: ?Sized, A> RegisteredTool<C, A> {
    /// Captures the descriptor once; later reads never regenerate it.
    pub fn new(tool: Arc<dyn ToolDyn<C, A>>) -> Result<Self, ToolError> {
        let definition = tool.definition();
        if definition.name.trim().is_empty() {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "tool name must not be empty",
            ));
        }
        Ok(Self {
            descriptor: ToolDescriptor {
                definition,
                capabilities: tool.capabilities(),
            },
            tool,
        })
    }

    pub fn prepare(&self, args: Value, context: &Arc<C>) -> Result<PreparedCall<A>, ToolError> {
        self.tool.clone().prepare(args, context)
    }
}

/// An owned, heterogeneous toolset for host context `C` and host access `A`.
/// Cloning shares the implementations.
pub struct ToolRegistry<C: ?Sized, A> {
    tools: BTreeMap<String, RegisteredTool<C, A>>,
}

impl<C: ?Sized, A> Default for ToolRegistry<C, A> {
    fn default() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }
}

impl<C: ?Sized, A> Clone for ToolRegistry<C, A> {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
        }
    }
}

impl<C: ?Sized + 'static, A: 'static> ToolRegistry<C, A> {
    /// The compiler checks here that `C` provides the tool's context and that
    /// its access converts into `A`.
    pub fn register<T>(&mut self, tool: T) -> Result<(), ToolError>
    where
        T: Tool,
        C: Provides<T::Context>,
        A: From<T::Access>,
    {
        self.register_dyn(Arc::new(tool))
    }

    pub fn register_dyn(&mut self, tool: Arc<dyn ToolDyn<C, A>>) -> Result<(), ToolError> {
        self.insert(RegisteredTool::new(tool)?)
    }

    /// Refuses a name that is already registered rather than replacing it.
    pub fn insert(&mut self, tool: RegisteredTool<C, A>) -> Result<(), ToolError> {
        match self.tools.entry(tool.descriptor.name().to_owned()) {
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

impl<C: ?Sized, A> ToolRegistry<C, A> {
    pub fn get(&self, name: &str) -> Option<&RegisteredTool<C, A>> {
        self.tools.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<RegisteredTool<C, A>> {
        self.tools.remove(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Registrations in name order.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredTool<C, A>> {
        self.tools.values()
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.iter().map(|tool| &tool.descriptor)
    }

    /// Decodes arguments and computes access; nothing runs until `execute`.
    pub fn prepare(
        &self,
        name: &str,
        args: Value,
        context: &Arc<C>,
    ) -> Result<PreparedCall<A>, ToolError> {
        self.get(name)
            .ok_or_else(|| ToolError::new(ToolErrorKind::NotFound, format!("unknown tool {name}")))?
            .prepare(args, context)
    }
}
