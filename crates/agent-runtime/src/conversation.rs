use std::collections::{HashMap, HashSet};

use crate::{Compaction, CompactionError, CompactionPlan, compaction::encoded_bytes};
use language_model::{Content, Message, MessageId, Role, ToolCall, ToolCallId};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    #[error("invalid message: {0}")]
    InvalidMessage(String),
    #[error("message identity is already committed")]
    DuplicateMessage,
    #[error("tool call identity is already committed")]
    DuplicateCall,
    #[error("outstanding tool calls must be settled before another message")]
    UnsettledCalls,
    #[error("tool result does not match an outstanding call")]
    UncorrelatedResult,
    #[error("message record is out of order")]
    OutOfOrder,
}

/// Ordered, accepted conversation data. Preview deltas never enter this owner.
/// Appends validate a whole batch before changing history or call correlation.
/// Native replay data is retained verbatim; this layer never edits messages.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    messages: Vec<Message>,
    message_ids: HashSet<MessageId>,
    call_ids: HashSet<ToolCallId>,
    pending: HashMap<ToolCallId, ToolCall>,
    compactions: Vec<Compaction>,
}

impl Conversation {
    pub fn new(messages: Vec<Message>) -> Result<Self, ConversationError> {
        let mut conversation = Self::default();
        conversation.append(messages)?;
        Ok(conversation)
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Includes synthetic context-summary identities, which are not ordinary
    /// messages in the permanent transcript but must not be reused by inputs.
    pub(crate) fn contains_message_id(&self, id: &MessageId) -> bool {
        self.message_ids.contains(id)
    }

    /// Permanent context transitions. Neither this ledger nor messages() is
    /// truncated by compaction; UI replay retains all original tool/message data.
    pub fn compactions(&self) -> &[Compaction] {
        &self.compactions
    }

    pub fn model_messages(&self) -> Vec<Message> {
        if let Some(compaction) = self.compactions.last() {
            self.messages[..compaction.range.start]
                .iter()
                .cloned()
                .chain(std::iter::once(compaction.summary.clone()))
                .chain(self.messages[compaction.range.end..].iter().cloned())
                .collect()
        } else {
            self.messages.clone()
        }
    }

    pub fn context_bytes(&self) -> usize {
        encoded_bytes(&self.model_messages())
    }

    /// Capture an older complete segment. Never split a tool-call/result batch.
    /// Subsequent appends remain outside this snapshot and survive its commit.
    pub fn plan_compaction(&self, keep_recent: usize) -> Result<CompactionPlan, CompactionError> {
        let prefix = self
            .messages
            .iter()
            .take_while(|message| matches!(message.role, Role::System | Role::Developer))
            .count();
        let old_end = self
            .compactions
            .last()
            .map_or(prefix, |compaction| compaction.range.end);
        let limit = self.messages.len().saturating_sub(keep_recent.max(1));
        let mut pending = HashSet::new();
        let mut end = prefix;
        for (index, message) in self.messages.iter().enumerate().take(limit).skip(prefix) {
            for content in &message.content {
                match content {
                    Content::ToolCall(call) => {
                        pending.insert(call.id.clone());
                    }
                    Content::ToolResult(result) => {
                        pending.remove(&result.call);
                    }
                    _ => {}
                }
            }
            if pending.is_empty() {
                end = index + 1;
            }
        }
        if end <= old_end || end - prefix < 2 {
            return Err(CompactionError::NothingToCompact);
        }
        let mut messages = Vec::new();
        if let Some(previous) = self.compactions.last() {
            messages.push(previous.summary.clone());
        }
        messages.extend_from_slice(&self.messages[old_end..end]);
        Ok(CompactionPlan {
            previous_revision: self.compactions.len(),
            range: prefix..end,
            messages,
        })
    }

    pub fn apply_compaction(&mut self, compaction: Compaction) -> Result<(), CompactionError> {
        let prefix = self
            .messages
            .iter()
            .take_while(|message| matches!(message.role, Role::System | Role::Developer))
            .count();
        let old_end = self
            .compactions
            .last()
            .map_or(prefix, |previous| previous.range.end);
        if compaction.previous_revision != self.compactions.len()
            || compaction.range.start != prefix
            || compaction.range.end <= old_end
            || compaction.range.end > self.messages.len()
        {
            return Err(CompactionError::Stale);
        }
        if compaction.summary.role != Role::User
            || !compaction.summary.replay.is_empty()
            || compaction.summary.content.is_empty()
            || compaction.summary.content.iter().any(
                |content| !matches!(content, Content::Text { text } if !text.trim().is_empty()),
            )
        {
            return Err(CompactionError::InvalidSummary);
        }
        compaction
            .summary
            .validate()
            .map_err(|_| CompactionError::InvalidSummary)?;
        if self.message_ids.contains(&compaction.summary.id) {
            return Err(ConversationError::DuplicateMessage.into());
        }
        let before = Conversation::new(self.messages[..compaction.range.end].to_vec())?;
        if !before.is_settled() {
            return Err(ConversationError::UnsettledCalls.into());
        }
        let candidate = self.messages[..prefix]
            .iter()
            .cloned()
            .chain(std::iter::once(compaction.summary.clone()))
            .chain(self.messages[compaction.range.end..].iter().cloned())
            .collect::<Vec<_>>();
        if encoded_bytes(&candidate) >= self.context_bytes() {
            return Err(CompactionError::NotSmaller);
        }
        self.message_ids.insert(compaction.summary.id.clone());
        self.compactions.push(compaction);
        Ok(())
    }

    /// Outstanding calls in their original content order, independent of tool
    /// completion order. Used to settle cancelled or undispatched calls.
    pub fn pending_calls(&self) -> Vec<ToolCall> {
        self.messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::ToolCall(call) if self.pending.contains_key(&call.id) => {
                    Some(call.clone())
                }
                _ => None,
            })
            .collect()
    }

    pub fn is_settled(&self) -> bool {
        self.pending.is_empty()
    }

    /// Returns the first new message's index. No partial append on validation
    /// failure, including duplicate IDs or a mismatched provider call identity.
    pub fn append(&mut self, messages: Vec<Message>) -> Result<usize, ConversationError> {
        let mut message_ids = self.message_ids.clone();
        let mut call_ids = self.call_ids.clone();
        let mut pending = self.pending.clone();
        for message in &messages {
            message
                .validate()
                .map_err(|error| ConversationError::InvalidMessage(error.to_string()))?;
            if !message_ids.insert(message.id.clone()) {
                return Err(ConversationError::DuplicateMessage);
            }
            if message.role != Role::Tool && !pending.is_empty() {
                return Err(ConversationError::UnsettledCalls);
            }
            for content in &message.content {
                match content {
                    Content::ToolCall(call) => {
                        if !call_ids.insert(call.id.clone()) {
                            return Err(ConversationError::DuplicateCall);
                        }
                        pending.insert(call.id.clone(), call.clone());
                    }
                    Content::ToolResult(result) => {
                        let Some(call) = pending.remove(&result.call) else {
                            return Err(ConversationError::UncorrelatedResult);
                        };
                        if result.provider_id != call.provider_id
                            || result.name.as_ref().is_some_and(|name| name != &call.name)
                        {
                            return Err(ConversationError::UncorrelatedResult);
                        }
                    }
                    _ => {}
                }
            }
        }
        let index = self.messages.len();
        self.messages.extend(messages);
        self.message_ids = message_ids;
        self.call_ids = call_ids;
        self.pending = pending;
        Ok(index)
    }

    /// Apply a MessageCommitted event to a replica seeded with the same initial
    /// history. Reject duplicates and gaps instead of silently diverging.
    pub fn apply(&mut self, index: usize, message: Message) -> Result<(), ConversationError> {
        if index != self.messages.len() {
            return Err(ConversationError::OutOfOrder);
        }
        self.append(vec![message])?;
        Ok(())
    }
}
