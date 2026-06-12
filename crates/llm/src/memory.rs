use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::ChatMessage;

#[async_trait]
pub trait ConversationMemory: Send + Sync {
    async fn load(&self, conversation_id: &str) -> Vec<ChatMessage>;

    async fn append(&self, conversation_id: &str, messages: Vec<ChatMessage>);

    async fn replace(&self, conversation_id: &str, messages: Vec<ChatMessage>);
}

#[derive(Clone, Default)]
pub struct ConversationStore {
    conversations: Arc<RwLock<HashMap<String, Vec<ChatMessage>>>>,
}

impl ConversationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a store from a previously persisted snapshot of every
    /// conversation's model context.
    pub fn from_conversations(conversations: HashMap<String, Vec<ChatMessage>>) -> Self {
        Self {
            conversations: Arc::new(RwLock::new(conversations)),
        }
    }

    /// A clone of every conversation's model context, for persistence. The
    /// `conversations` field is private, so this is the only export seam.
    pub async fn export(&self) -> HashMap<String, Vec<ChatMessage>> {
        self.conversations.read().await.clone()
    }
}

#[async_trait]
impl ConversationMemory for ConversationStore {
    async fn load(&self, conversation_id: &str) -> Vec<ChatMessage> {
        self.conversations
            .read()
            .await
            .get(conversation_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn append(&self, conversation_id: &str, messages: Vec<ChatMessage>) {
        self.conversations
            .write()
            .await
            .entry(conversation_id.to_string())
            .or_default()
            .extend(messages);
    }

    async fn replace(&self, conversation_id: &str, messages: Vec<ChatMessage>) {
        self.conversations
            .write()
            .await
            .insert(conversation_id.to_string(), messages);
    }
}
