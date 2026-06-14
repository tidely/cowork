use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::ChatMessage;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ConversationId(u128);

impl ConversationId {
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().as_u128())
    }

    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[async_trait]
pub trait ConversationMemory: Send + Sync {
    async fn load(&self, conversation_id: ConversationId) -> Vec<ChatMessage>;

    async fn append(&self, conversation_id: ConversationId, messages: Vec<ChatMessage>);

    async fn replace(&self, conversation_id: ConversationId, messages: Vec<ChatMessage>);
}

#[derive(Clone, Default)]
pub struct ConversationStore {
    conversations: Arc<RwLock<HashMap<ConversationId, Vec<ChatMessage>>>>,
}

impl ConversationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a store from a previously persisted snapshot of every
    /// conversation's model context.
    pub fn from_conversations(conversations: HashMap<ConversationId, Vec<ChatMessage>>) -> Self {
        Self {
            conversations: Arc::new(RwLock::new(conversations)),
        }
    }

    /// A clone of every conversation's model context, for persistence. The
    /// `conversations` field is private, so this is the only export seam.
    pub async fn export(&self) -> HashMap<ConversationId, Vec<ChatMessage>> {
        self.conversations.read().await.clone()
    }
}

#[async_trait]
impl ConversationMemory for ConversationStore {
    async fn load(&self, conversation_id: ConversationId) -> Vec<ChatMessage> {
        self.conversations
            .read()
            .await
            .get(&conversation_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn append(&self, conversation_id: ConversationId, messages: Vec<ChatMessage>) {
        self.conversations
            .write()
            .await
            .entry(conversation_id)
            .or_default()
            .extend(messages);
    }

    async fn replace(&self, conversation_id: ConversationId, messages: Vec<ChatMessage>) {
        self.conversations
            .write()
            .await
            .insert(conversation_id, messages);
    }
}
