use std::collections::{BTreeMap, HashMap, HashSet};

use agent_input::InputId;
use language_model::{Content, Message, MessageId, Role, ToolCall, ToolCallId};
use serde_json::Value;

use crate::{
    Compaction, CompactionError, CompactionPlan, DefinitionRevision, compaction::encoded_bytes,
};

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

/// Where an entry came from. Provenance only: authority was checked when the
/// request that produced it was admitted.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin<S> {
    /// A committed input and the source that sent it.
    Input { id: InputId, source: S },
    /// A model reply, produced under this definition revision.
    Model { revision: DefinitionRevision },
    /// A tool result; the message names its call.
    Tool,
    /// Context notes from these sections (see `ContextSection`).
    Context { sections: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry<S> {
    pub message: Message,
    pub origin: Origin<S>,
}

#[derive(Clone, Debug)]
struct Delivered {
    snapshot: Value,
    /// The entry count when it was delivered; a compaction summarizing past
    /// this point makes the model forget it.
    at: usize,
}

/// Ordered, accepted conversation entries. Previews never enter this owner.
/// Appends validate a whole batch before changing history or call correlation.
/// Native replay data is retained verbatim; this layer never edits messages.
///
/// Entries contain only user, assistant, and tool messages: instructions
/// belong to the definition, so they are never part of the history.
#[derive(Clone, Debug)]
pub struct Conversation<S> {
    entries: Vec<Entry<S>>,
    message_ids: HashSet<MessageId>,
    call_ids: HashSet<ToolCallId>,
    pending: HashMap<ToolCallId, ToolCall>,
    compactions: Vec<Compaction>,
    delivered: BTreeMap<String, Delivered>,
}

impl<S> Default for Conversation<S> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            message_ids: HashSet::new(),
            call_ids: HashSet::new(),
            pending: HashMap::new(),
            compactions: Vec::new(),
            delivered: BTreeMap::new(),
        }
    }
}

impl<S: Clone> Conversation<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[Entry<S>] {
        &self.entries
    }

    /// Permanent context transitions. Neither this ledger nor `entries` is
    /// truncated by compaction.
    pub fn compactions(&self) -> &[Compaction] {
        &self.compactions
    }

    fn compacted_end(&self) -> usize {
        self.compactions
            .last()
            .map_or(0, |compaction| compaction.end)
    }

    /// The current model context: the latest summary, then the entries after it.
    pub fn model_messages(&self) -> Vec<Message> {
        let summary = self
            .compactions
            .last()
            .map(|compaction| compaction.summary.clone());
        summary
            .into_iter()
            .chain(
                self.entries[self.compacted_end()..]
                    .iter()
                    .map(|entry| entry.message.clone()),
            )
            .collect()
    }

    pub fn context_bytes(&self) -> usize {
        encoded_bytes(&self.model_messages())
    }

    /// The snapshot of a context section the model last received, unless a
    /// compaction has since summarized the point where it was delivered.
    pub fn delivered(&self, section: &str) -> Option<&Value> {
        self.delivered
            .get(section)
            .filter(|delivered| delivered.at >= self.compacted_end())
            .map(|delivered| &delivered.snapshot)
    }

    /// Records what the model now knows about a section, delivered by the
    /// entry at index `at` (or, without a note, as of that point).
    pub(crate) fn deliver(&mut self, section: String, snapshot: Value, at: usize) {
        self.delivered.insert(section, Delivered { snapshot, at });
    }

    /// Plans summarizing about `summarized_fraction` of the model context,
    /// measured in the same serialized bytes as [`Self::context_bytes`]. The
    /// cut is the last point at or before that size where every tool call has
    /// its result, or the first after it when there is none, so one large
    /// exchange still compacts. It never covers the last entry.
    ///
    /// Entries appended later stay outside this snapshot and survive its commit.
    pub fn plan_compaction(
        &self,
        summarized_fraction: f64,
    ) -> Result<CompactionPlan, CompactionError> {
        let old_end = self.compacted_end();
        let mut context: Vec<Message> = self
            .compactions
            .last()
            .map(|compaction| compaction.summary.clone())
            .into_iter()
            .collect();
        let mut size = encoded_bytes(&context);
        let total =
            size + encoded_bytes(self.entries[old_end..].iter().map(|entry| &entry.message));
        let target = (total as f64 * summarized_fraction.clamp(0.0, 1.0)) as usize;
        // Every call before the previous compaction's end has its result.
        let mut pending = HashSet::new();
        let mut end = None;
        let last = self.entries.len().saturating_sub(1);
        for (index, entry) in self.entries.iter().enumerate().take(last).skip(old_end) {
            for content in &entry.message.content {
                match content {
                    Content::ToolCall(call) => {
                        pending.insert(&call.id);
                    }
                    Content::ToolResult(result) => {
                        pending.remove(&result.call);
                    }
                    _ => {}
                }
            }
            size += encoded_bytes([&entry.message]);
            if pending.is_empty() {
                let fits = size <= target;
                if fits || end.is_none() {
                    end = Some(index + 1);
                }
                if !fits {
                    break;
                }
            }
        }
        let end = end.ok_or(CompactionError::NothingToCompact)?;
        context.extend(
            self.entries[old_end..end]
                .iter()
                .map(|entry| entry.message.clone()),
        );
        Ok(CompactionPlan {
            previous_revision: self.compactions.len(),
            end,
            context,
        })
    }

    pub fn apply_compaction(&mut self, compaction: Compaction) -> Result<(), CompactionError> {
        if compaction.previous_revision != self.compactions.len()
            || compaction.end <= self.compacted_end()
            || compaction.end > self.entries.len()
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
        let mut pending = HashSet::new();
        for entry in &self.entries[..compaction.end] {
            for content in &entry.message.content {
                match content {
                    Content::ToolCall(call) => {
                        pending.insert(&call.id);
                    }
                    Content::ToolResult(result) => {
                        pending.remove(&result.call);
                    }
                    _ => {}
                }
            }
        }
        if !pending.is_empty() {
            return Err(ConversationError::UnsettledCalls.into());
        }
        let candidate = encoded_bytes(
            std::iter::once(&compaction.summary).chain(
                self.entries[compaction.end..]
                    .iter()
                    .map(|entry| &entry.message),
            ),
        );
        if candidate >= self.context_bytes() {
            return Err(CompactionError::NotSmaller);
        }
        self.message_ids.insert(compaction.summary.id.clone());
        self.compactions.push(compaction);
        Ok(())
    }

    /// Outstanding calls in their original content order, independent of tool
    /// completion order. Used to settle cancelled or undispatched calls.
    pub fn pending_calls(&self) -> Vec<ToolCall> {
        self.entries
            .iter()
            .flat_map(|entry| &entry.message.content)
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

    /// Returns the first new entry's index. No partial append on validation
    /// failure, including duplicate IDs or a mismatched provider call identity.
    pub fn append(&mut self, entries: Vec<Entry<S>>) -> Result<usize, ConversationError> {
        let mut message_ids = self.message_ids.clone();
        let mut call_ids = self.call_ids.clone();
        let mut pending = self.pending.clone();
        for entry in &entries {
            let message = &entry.message;
            message
                .validate()
                .map_err(|error| ConversationError::InvalidMessage(error.to_string()))?;
            if matches!(message.role, Role::System | Role::Developer) {
                return Err(ConversationError::InvalidMessage(
                    "instructions belong to the definition, not the conversation".into(),
                ));
            }
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
        let index = self.entries.len();
        self.entries.extend(entries);
        self.message_ids = message_ids;
        self.call_ids = call_ids;
        self.pending = pending;
        Ok(index)
    }

    /// Apply a `MessageCommitted` event to a replica with the same history.
    /// Rejects duplicates and gaps instead of silently diverging.
    pub fn apply(&mut self, index: usize, entry: Entry<S>) -> Result<(), ConversationError> {
        if index != self.entries.len() {
            return Err(ConversationError::OutOfOrder);
        }
        self.append(vec![entry])?;
        Ok(())
    }
}
