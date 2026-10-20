use std::{num::NonZeroUsize, ops::Range};

use language_model::{CompletionRequest, Content, Envelope, Message, Role};

use crate::ConversationError;

/// When to compact and how much. Sizes are the serialized shared messages of
/// the model context, not claimed token estimates. Automatic compaction is
/// opt-in; manual requests use the same policy.
///
/// A compaction summarizes the older part of the model context and keeps the
/// newer part verbatim after the summary: the next request is the
/// instructions, the summary, and the messages past the cut. The summary
/// request sends the context up to the cut, which is how the last request
/// began, so the provider serves it from its prompt cache.
#[derive(Clone, Debug)]
pub struct CompactionPolicy {
    /// Compact automatically once the model context is at least this large.
    pub trigger_bytes: NonZeroUsize,
    /// The part of the model context a compaction summarizes, from 0 to 1,
    /// counting the instructions and any previous summary. The cut is the
    /// last point at or before it where every tool call has its result, or
    /// if there is none, the first after it. The last message always stays.
    pub summarized_fraction: f64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        const {
            Self {
                // No automatic auxiliary requests without an explicit threshold.
                trigger_bytes: NonZeroUsize::new(usize::MAX).unwrap(),
                summarized_fraction: 0.7,
            }
        }
    }
}

/// A context replacement, never deletion of transcript messages. range indexes
/// the original full history; the leading instructions and the tail stay intact.
#[derive(Clone, Debug, PartialEq)]
pub struct Compaction {
    pub previous_revision: usize,
    pub range: Range<usize>,
    pub summary: Message,
}

#[derive(Clone, Debug)]
pub struct CompactionPlan {
    pub previous_revision: usize,
    /// The messages the summary replaces, indexing the full history: from the
    /// end of the leading instructions to the cut.
    pub range: Range<usize>,
    /// The model context up to the cut, as the summary request sends it: the
    /// instructions, any previous summary, and the messages after it up to
    /// the cut. It begins every request since the previous compaction.
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
    #[error("session owner has closed")]
    Closed,
    #[error("compaction policy is not configured")]
    Disabled,
    #[error(transparent)]
    Model(#[from] language_model::Error),
    #[error(transparent)]
    Conversation(#[from] ConversationError),
}

pub(crate) fn encoded_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            // Conversation validation already guarantees encodable message content.
            serde_json::to_vec(&Envelope::encode_message(message).expect("validated message"))
                .expect("message envelope is serializable")
                .len()
        })
        .sum()
}

/// The summary request: `template`, the run's request, with the plan's
/// context and the summary instruction after it. Nothing else differs from
/// a turn's request, and the context is how the last turn's messages began,
/// so the provider serves it from the prompt cache that turn left; any
/// change before the instruction (tools, thinking, images, reasoning) would
/// miss it. That includes the output cap, which a thinking budget must stay
/// below; a reply calling tools anyway is an invalid summary.
///
/// The model sees exactly what the summary replaces, plus the instructions
/// it keeps, so nothing past the cut is summarized twice.
pub(crate) fn summary_request(
    plan: &CompactionPlan,
    template: &CompletionRequest,
) -> CompletionRequest {
    let mut request = template.clone();
    request.messages = plan.context.clone();
    request
        .messages
        .push(Message::user(include_str!("../prompts/compaction.md")));
    request
}

pub(crate) fn finish_summary(
    plan: &CompactionPlan,
    reply: &Message,
) -> Result<Compaction, CompactionError> {
    let reason = reply
        .generation
        .as_ref()
        .and_then(|meta| meta.finish_reason.as_deref());
    if reply.role != Role::Assistant
        || !matches!(reason, Some("stop" | "end_turn"))
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
        range: plan.range.clone(),
        summary: Message::user(format!(
            "Earlier conversation summary (context, not a new request):\n{text}"
        )),
    })
}
