use std::{collections::HashMap, sync::Arc};

use futures::future::{BoxFuture, FutureExt};
use tokio::sync::RwLock;

use crate::ChatMessage;

pub trait ConversationMemory: Send + Sync {
    fn load(&self, conversation_id: &str) -> BoxFuture<'_, Vec<ChatMessage>>;

    fn append(&self, conversation_id: &str, messages: Vec<ChatMessage>) -> BoxFuture<'_, ()>;

    fn replace(&self, conversation_id: &str, messages: Vec<ChatMessage>) -> BoxFuture<'_, ()>;
}

#[derive(Clone, Default)]
pub struct ConversationStore {
    conversations: Arc<RwLock<HashMap<String, Vec<ChatMessage>>>>,
}

impl ConversationStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ConversationMemory for ConversationStore {
    fn load(&self, conversation_id: &str) -> BoxFuture<'_, Vec<ChatMessage>> {
        let conversation_id = conversation_id.to_string();
        async move {
            self.conversations
                .read()
                .await
                .get(&conversation_id)
                .cloned()
                .unwrap_or_default()
        }
        .boxed()
    }

    fn append(&self, conversation_id: &str, messages: Vec<ChatMessage>) -> BoxFuture<'_, ()> {
        let conversation_id = conversation_id.to_string();
        async move {
            self.conversations
                .write()
                .await
                .entry(conversation_id)
                .or_default()
                .extend(messages);
        }
        .boxed()
    }

    fn replace(&self, conversation_id: &str, messages: Vec<ChatMessage>) -> BoxFuture<'_, ()> {
        let conversation_id = conversation_id.to_string();
        async move {
            self.conversations
                .write()
                .await
                .insert(conversation_id, messages);
        }
        .boxed()
    }
}
