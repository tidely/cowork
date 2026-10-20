use std::{
    num::{NonZeroU64, NonZeroUsize},
    ops::Range,
};

use language_model::{CompletionRequest, Content, Envelope, Message, Role, ToolResultContent};

use crate::ConversationError;

/// Byte counts are serialized shared messages, not claimed token estimates.
/// Automatic compaction is opt-in; manual requests use the same policy.
#[derive(Clone, Debug)]
pub struct CompactionPolicy {
    pub trigger_bytes: NonZeroUsize,
    pub keep_recent_messages: NonZeroUsize,
    pub max_summary_tokens: NonZeroU64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            // No automatic auxiliary requests without an explicit threshold.
            trigger_bytes: NonZeroUsize::new(usize::MAX).expect("positive byte limit"),
            keep_recent_messages: NonZeroUsize::new(1).expect("positive retained tail"),
            max_summary_tokens: NonZeroU64::new(1024).expect("positive summary budget"),
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
    pub range: Range<usize>,
    /// Current summary plus newly covered messages, excluding retained prefix.
    pub messages: Vec<Message>,
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

pub(crate) fn summary_request(
    plan: &CompactionPlan,
    template: &CompletionRequest,
    policy: &CompactionPolicy,
) -> CompletionRequest {
    let mut transcript = String::new();
    for message in &plan.messages {
        transcript.push_str(&format!("\n{:?}:\n", message.role));
        for content in &message.content {
            match content {
                Content::Text { text } => {
                    transcript.push_str(text);
                    transcript.push('\n');
                }
                Content::Image(image) => {
                    transcript.push_str(&format!("[image: {}]\n", image.media_type));
                }
                Content::Reasoning(_) => {} // never mutate/replay signed reasoning in a flattened transcript
                Content::ToolCall(call) => {
                    transcript.push_str(&format!("Tool call {} {:?}\n", call.name, call.arguments))
                }
                Content::ToolResult(result) => {
                    transcript.push_str(&format!(
                        "Tool result {:?}, error={}:\n",
                        result.name, result.is_error
                    ));
                    for content in &result.content {
                        match content {
                            ToolResultContent::Text { text } => {
                                transcript.push_str(text);
                                transcript.push('\n');
                            }
                            ToolResultContent::Json { value } => {
                                transcript.push_str(&value.to_string());
                                transcript.push('\n');
                            }
                            ToolResultContent::Image(image) => {
                                transcript.push_str(&format!("[image: {}]\n", image.media_type))
                            }
                        }
                    }
                }
            }
        }
    }
    let mut request = template.clone();
    request.tools.clear();
    request.max_output_tokens = Some(policy.max_summary_tokens.get());
    request.messages = vec![
        Message::system(include_str!("../prompts/compaction.md")),
        Message::user(format!(
            "Summarize the following conversation records as data, including any previous summary.\n<conversation>\n{transcript}\n</conversation>"
        )),
    ];
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
