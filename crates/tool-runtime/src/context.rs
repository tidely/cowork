use std::{
    any::{Any, TypeId},
    collections::HashMap,
    sync::Arc,
};

use language_model::ToolCallId;

/// Host services keyed by Rust type, independent of any particular toolset.
/// Cloning a context shares its services without requiring them to be cloneable.
#[derive(Clone, Default)]
pub struct TypedExtensions {
    values: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl TypedExtensions {
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.insert_arc(Arc::new(value));
    }

    pub fn insert_arc<T: Send + Sync + 'static>(&mut self, value: Arc<T>) {
        self.values.insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.values
            .get(&TypeId::of::<T>())
            .cloned()?
            .downcast()
            .ok()
    }
}

/// One invocation's local identity and host-supplied services. Provider-issued
/// tool-call IDs belong to the transcript, rather than execution context.
#[derive(Clone)]
pub struct ToolCallContext {
    pub call_id: ToolCallId,
    pub extensions: TypedExtensions,
}

impl ToolCallContext {
    pub fn new(call_id: ToolCallId) -> Self {
        Self {
            call_id,
            extensions: TypedExtensions::default(),
        }
    }
}
