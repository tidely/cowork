//! Frozen v1 log records. Conversation messages use their own versioned
//! envelopes. Observation payloads are retained as opaque JSON so tool-specific
//! UI data need not become conversation schema. New incompatible shapes need a
//! new record type identifier; unknown identifiers/fields are never rewritten.

use language_model::{Envelope, v1};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{AgentEvent, Compaction, Conversation, EventKind};

pub const RECORD_TYPE: &str = "cowork.agent.log.v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Initial {
        messages: Vec<Envelope>,
        compactions: Vec<StoredCompaction>,
        next_sequence: u64,
    },
    Event {
        event: StoredEvent,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCompaction {
    pub previous_revision: usize,
    pub start: usize,
    pub end: usize,
    pub summary: Envelope,
}

impl StoredCompaction {
    pub fn encode(value: &Compaction) -> Result<Self, language_model::SchemaError> {
        Ok(Self {
            previous_revision: value.previous_revision,
            start: value.range.start,
            end: value.range.end,
            summary: Envelope::encode_message(&value.summary)?,
        })
    }
    pub fn decode(&self) -> Result<Compaction, language_model::SchemaError> {
        Ok(Compaction {
            previous_revision: self.previous_revision,
            range: self.start..self.end,
            summary: self.summary.decode_message()?,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredEvent {
    pub sequence: u64,
    pub run: Option<String>,
    pub event: StoredEventKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredEventKind {
    MessageCommitted {
        index: usize,
        message: Envelope,
        input: Option<Value>,
    },
    Compacted {
        compaction: StoredCompaction,
    },
    /// UI/runtime observations never become accepted conversation messages.
    Observation {
        name: String,
        data: Value,
    },
}

impl StoredEvent {
    pub fn encode<S: Serialize>(value: &AgentEvent<S>) -> Result<Self, super::LogError> {
        let observation = |name: &str, data: Value| StoredEventKind::Observation {
            name: name.into(),
            data,
        };
        let input = |origin: &crate::InputOrigin<S>| -> Result<Value, serde_json::Error> {
            Ok(
                json!({"id": origin.id.to_string(), "source": serde_json::to_value(&origin.source)?}),
            )
        };
        let event = match &value.kind {
            EventKind::MessageCommitted { index, message, input: origin } => StoredEventKind::MessageCommitted {
                index: *index, message: Envelope::encode_message(message)?, input: origin.as_ref().map(input).transpose()?,
            },
            EventKind::Compacted(compaction) => StoredEventKind::Compacted { compaction: StoredCompaction::encode(compaction)? },
            EventKind::Preview { index, delta } => {
                let delta = match delta {
                    language_model::ContentDelta::Text(text) => json!({"kind":"text", "text":text}),
                    language_model::ContentDelta::Reasoning(text) => json!({"kind":"reasoning", "text":text}),
                    language_model::ContentDelta::ToolCall { id, name, arguments } => json!({"kind":"tool_call", "id":id, "name":name, "arguments":arguments}),
                };
                observation("preview", json!({"index":index, "delta":delta}))
            }
            EventKind::InputAdmitted { input: origin, delivery, location } => observation("input_admitted", json!({
                "input": input(origin)?, "delivery": format!("{delivery:?}"), "location":format!("{location:?}"),
            })),
            EventKind::InputsDiscarded { inputs } => observation("inputs_discarded", Value::Array(inputs.iter().map(|value| {
                Ok(json!({"id":value.id.to_string(), "source":serde_json::to_value(&value.source)?, "message":Envelope::encode_message(&value.payload)?}))
            }).collect::<Result<_, super::LogError>>()?)),
            EventKind::RunStarted => observation("run_started", Value::Null),
            EventKind::PhaseChanged(phase) => observation("phase_changed", json!({"phase":format!("{phase:?}")})),
            EventKind::UsageReported(usage) => observation("usage_reported", serde_json::to_value(v1::Usage::from(usage.clone()))?),
            EventKind::ToolStarted(call) => observation("tool_started", serde_json::to_value(v1::ToolCall::from(call.clone()))?),
            EventKind::ToolProgress { call, progress } => {
                let progress = match progress {
                    tool_runtime::ToolProgress::Text { text } => json!({"kind":"text", "text":text}),
                    tool_runtime::ToolProgress::Content { content } => json!({"kind":"content", "content":content.iter().cloned().map(v1::ToolResultContent::from).collect::<Vec<_>>()}),
                    tool_runtime::ToolProgress::Custom { kind, value } => json!({"kind":"custom", "type":kind, "value":value}),
                };
                observation("tool_progress", json!({"call":v1::ToolCall::from(call.clone()), "progress":progress}))
            }
            EventKind::ToolFinished { call, result } => {
                let result = match result {
                    Ok(output) => json!({"ok":true, "tool_name":output.tool_name, "output":output.output,
                        "model_output":output.model_output.iter().cloned().map(v1::ToolResultContent::from).collect::<Vec<_>>()}),
                    Err(error) => json!({"ok":false, "error":error}),
                };
                observation("tool_finished", json!({"call":v1::ToolCall::from(call.clone()), "result":result}))
            }
            EventKind::RunFinished { outcome, error } => observation("run_finished", json!({"outcome":format!("{outcome:?}"), "error":error.as_ref().map(ToString::to_string)})),
            EventKind::CompactionStarted { range, previous_revision } => observation("compaction_started", json!({"start":range.start, "end":range.end,"previous_revision":previous_revision})),
            EventKind::CompactionFailed(error) => observation("compaction_failed", json!({"error":error.to_string()})),
        };
        Ok(Self {
            sequence: value.sequence,
            run: value.run.map(|run| run.to_string()),
            event,
        })
    }

    /// Fold authoritative changes into a replica. Observations are for the UI;
    /// they never synthesize accepted messages from provisional deltas.
    pub fn apply(&self, conversation: &mut Conversation) -> Result<(), super::LogError> {
        match &self.event {
            StoredEventKind::MessageCommitted { index, message, .. } => {
                conversation.apply(*index, message.decode_message()?)?
            }
            StoredEventKind::Compacted { compaction } => {
                conversation.apply_compaction(compaction.decode()?)?
            }
            StoredEventKind::Observation { .. } => {}
        }
        Ok(())
    }
}

pub(crate) fn initial(
    conversation: &Conversation,
    next_sequence: u64,
) -> Result<Record, super::LogError> {
    Ok(Record::Initial {
        messages: conversation
            .messages()
            .iter()
            .map(Envelope::encode_message)
            .collect::<Result<_, _>>()?,
        compactions: conversation
            .compactions()
            .iter()
            .map(StoredCompaction::encode)
            .collect::<Result<_, _>>()?,
        next_sequence,
    })
}
