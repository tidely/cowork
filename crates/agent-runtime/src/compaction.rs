use std::num::NonZeroUsize;

use language_model::{
    CachePoint, CompletionRequest, Content, Envelope, FinishReason, Message, Role,
};

use crate::ConversationError;

/// When to compact and how much. Sizes are the serialized shared messages of
/// the model context, not claimed token estimates. Manual compaction is
/// always available; automatic compaction needs a trigger.
///
/// A compaction summarizes the older part of the model context and keeps the
/// newer part verbatim after the summary: the next request is the summary,
/// then the entries past the cut. The summary request sends the context up
/// to the cut, which is how the last request began, so the provider serves
/// it from its prompt cache.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPolicy {
    /// Compact automatically once the model context is at least this large.
    pub trigger_bytes: Option<NonZeroUsize>,
    /// The part of the model context a compaction summarizes, from 0 to 1,
    /// counting any previous summary. The cut is the last point at or before
    /// it where every tool call has its result, or if there is none, the
    /// first after it. The last entry always stays.
    pub summarized_fraction: f64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            trigger_bytes: None,
            summarized_fraction: 0.7,
        }
    }
}

/// A context replacement, never deletion of transcript entries: entries
/// `..end` of the full history are replaced by `summary` for inference.
#[derive(Clone, Debug, PartialEq)]
pub struct Compaction {
    pub previous_revision: usize,
    pub end: usize,
    pub summary: Message,
}

#[derive(Clone, Debug)]
pub struct CompactionPlan {
    pub previous_revision: usize,
    /// The entries the summary replaces: `..end` of the full history.
    pub end: usize,
    /// The model context up to the cut, as the summary request sends it: any
    /// previous summary, then the entries after it up to the cut.
    pub context: Vec<Message>,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CompactionError {
    #[error("there is no complete older segment to compact")]
    NothingToCompact,
    #[error("compaction snapshot is stale or its range is invalid")]
    Stale,
    #[error("compaction produced an unusable summary")]
    InvalidSummary,
    #[error("summary would not reduce model context")]
    NotSmaller,
    #[error("compaction was cancelled")]
    Cancelled,
    #[error(transparent)]
    Model(#[from] language_model::Error),
    #[error(transparent)]
    Conversation(#[from] ConversationError),
}

pub(crate) fn encoded_bytes<'a>(messages: impl IntoIterator<Item = &'a Message>) -> usize {
    messages
        .into_iter()
        .map(|message| {
            // Conversation validation already guarantees encodable content.
            serde_json::to_vec(&Envelope::encode_message(message).expect("validated message"))
                .expect("message envelope is serializable")
                .len()
        })
        .sum()
}

/// The summary request: `template`, a step's request, with the plan's
/// context and the summary instruction as its messages. Nothing else differs
/// from a step's request, and the context is how the last step's messages
/// began, so the provider serves it from the prompt cache; any change before
/// the instruction (tools, thinking, instructions) would miss it.
pub(crate) fn summary_request(
    plan: &CompactionPlan,
    mut template: CompletionRequest,
) -> CompletionRequest {
    template.messages = plan.context.clone();
    template
        .cache
        .retain(|point| !matches!(point, CachePoint::AfterMessage(_)));
    if let Some(last) = template.messages.last() {
        template
            .cache
            .push(CachePoint::AfterMessage(last.id.clone()));
    }
    template
        .messages
        .push(Message::user(include_str!("../prompts/compaction.md")));
    template
}

pub(crate) fn finish_summary(
    plan: &CompactionPlan,
    reply: &Message,
) -> Result<Compaction, CompactionError> {
    let reason = reply
        .generation
        .as_ref()
        .and_then(|meta| meta.finish_reason.as_ref());
    if reply.role != Role::Assistant
        || reason != Some(&FinishReason::Stop)
        || reply
            .content
            .iter()
            .any(|content| !matches!(content, Content::Text { .. } | Content::Reasoning(_)))
    {
        return Err(CompactionError::InvalidSummary);
    }
    let text = reply
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(CompactionError::InvalidSummary);
    }
    Ok(Compaction {
        previous_revision: plan.previous_revision,
        end: plan.end,
        summary: Message::user(format!(
            "Earlier conversation summary (context, not a new request):\n{text}"
        )),
    })
}
