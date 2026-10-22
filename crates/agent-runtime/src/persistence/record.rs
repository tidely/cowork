//! Log records. Conversation messages use their own versioned envelopes, and
//! entry origins are stored beside them. Observation payloads are opaque JSON
//! so tool-specific UI data need not become conversation schema. Unknown
//! identifiers and fields are refused, never rewritten.

use agent_input::InputId;
use language_model::{Envelope, v1};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::LogError;
use crate::{
    AgentEvent, AgentHost, Compaction, Conversation, DefinitionRevision, Entry, EventKind, Origin,
};

pub const RECORD_TYPE: &str = "cowork.agent.log.v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Initial {
        entries: Vec<StoredEntry>,
        compactions: Vec<StoredCompaction>,
        next_sequence: u64,
    },
    Event {
        event: StoredEvent,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredEntry {
    pub message: Envelope,
    pub origin: StoredOrigin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredOrigin {
    Input { id: String, source: Value },
    Model { revision: u32 },
    Tool,
    Context { sections: Vec<String> },
}

impl StoredEntry {
    pub fn encode<S: Serialize>(entry: &Entry<S>) -> Result<Self, LogError> {
        Ok(Self {
            message: Envelope::encode_message(&entry.message)?,
            origin: match &entry.origin {
                Origin::Input { id, source } => StoredOrigin::Input {
                    id: id.to_string(),
                    source: serde_json::to_value(source)?,
                },
                Origin::Model { revision } => StoredOrigin::Model {
                    revision: revision.0,
                },
                Origin::Tool => StoredOrigin::Tool,
                Origin::Context { sections } => StoredOrigin::Context {
                    sections: sections.clone(),
                },
            },
        })
    }

    pub fn decode<S: DeserializeOwned>(&self) -> Result<Entry<S>, LogError> {
        Ok(Entry {
            message: self.message.decode_message()?,
            origin: match &self.origin {
                StoredOrigin::Input { id, source } => Origin::Input {
                    id: id
                        .parse::<InputId>()
                        .map_err(|_| LogError::Invalid("invalid input identity".into()))?,
                    source: S::deserialize(source)?,
                },
                StoredOrigin::Model { revision } => Origin::Model {
                    revision: DefinitionRevision(*revision),
                },
                StoredOrigin::Tool => Origin::Tool,
                StoredOrigin::Context { sections } => Origin::Context {
                    sections: sections.clone(),
                },
            },
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCompaction {
    pub previous_revision: usize,
    pub end: usize,
    pub summary: Envelope,
}

impl StoredCompaction {
    pub fn encode(value: &Compaction) -> Result<Self, language_model::SchemaError> {
        Ok(Self {
            previous_revision: value.previous_revision,
            end: value.end,
            summary: Envelope::encode_message(&value.summary)?,
        })
    }

    pub fn decode(&self) -> Result<Compaction, language_model::SchemaError> {
        Ok(Compaction {
            previous_revision: self.previous_revision,
            end: self.end,
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
        entry: StoredEntry,
    },
    Compacted {
        compaction: StoredCompaction,
    },
    /// UI/runtime observations never become accepted conversation entries.
    Observation {
        name: String,
        data: Value,
    },
}

impl StoredEvent {
    pub fn encode<H: AgentHost>(value: &AgentEvent<H>) -> Result<Self, LogError>
    where
        H::Source: Serialize,
    {
        let observation = |name: &str, data: Value| StoredEventKind::Observation {
            name: name.into(),
            data,
        };
        let call = |call: &language_model::ToolCall| v1::ToolCall::from(call.clone());
        let event = match &value.kind {
            EventKind::MessageCommitted { index, entry } => StoredEventKind::MessageCommitted {
                index: *index,
                entry: StoredEntry::encode(entry)?,
            },
            EventKind::Compacted(compaction) => StoredEventKind::Compacted {
                compaction: StoredCompaction::encode(compaction)?,
            },
            EventKind::Preview { index, delta } => {
                let delta = match delta {
                    language_model::ContentDelta::Text(text) => json!({"kind":"text", "text":text}),
                    language_model::ContentDelta::Reasoning(text) => {
                        json!({"kind":"reasoning", "text":text})
                    }
                    language_model::ContentDelta::ToolCall { id, name, arguments } => {
                        json!({"kind":"tool_call", "id":id, "name":name, "arguments":arguments})
                    }
                };
                observation("preview", json!({"index":index, "delta":delta}))
            }
            EventKind::InputAdmitted { id, source, delivery, location } => observation(
                "input_admitted",
                json!({
                    "id": id.to_string(), "source": serde_json::to_value(source)?,
                    "delivery": format!("{delivery:?}"), "location": format!("{location:?}"),
                }),
            ),
            EventKind::InputsDiscarded { inputs } => observation(
                "inputs_discarded",
                Value::Array(
                    inputs
                        .iter()
                        .map(|input| {
                            Ok(json!({"id": input.id.to_string(), "source": serde_json::to_value(&input.source)?}))
                        })
                        .collect::<Result<_, LogError>>()?,
                ),
            ),
            EventKind::RunStarted => observation("run_started", Value::Null),
            EventKind::PhaseChanged(phase) => {
                observation("phase_changed", json!({"phase": format!("{phase:?}")}))
            }
            EventKind::UsageReported(usage) => observation(
                "usage_reported",
                serde_json::to_value(v1::Usage::from(usage.clone()))?,
            ),
            EventKind::ApprovalRequested { call: value, tool, access } => observation(
                "approval_requested",
                json!({"call": call(value), "tool": tool, "access": format!("{access:?}")}),
            ),
            EventKind::ToolStarted { call: value, tool } => {
                observation("tool_started", json!({"call": call(value), "tool": tool}))
            }
            EventKind::ToolProgress { call: value, progress } => {
                let progress = match progress {
                    tool_runtime::ToolProgress::Text { text } => json!({"kind":"text", "text":text}),
                    tool_runtime::ToolProgress::Content { content } => json!({
                        "kind":"content",
                        "content": content.iter().cloned().map(v1::ToolResultContent::from).collect::<Vec<_>>(),
                    }),
                    tool_runtime::ToolProgress::Custom { kind, value } => {
                        json!({"kind":"custom", "type":kind, "value":value})
                    }
                };
                observation("tool_progress", json!({"call": call(value), "progress": progress}))
            }
            EventKind::ToolFinished { call: value, tool, result } => {
                let result = match result {
                    Ok(output) => json!({
                        "ok": true, "tool_name": output.tool_name, "output": output.output,
                        "model_output": output.model_output.iter().cloned().map(v1::ToolResultContent::from).collect::<Vec<_>>(),
                    }),
                    Err(error) => json!({"ok": false, "error": error}),
                };
                observation(
                    "tool_finished",
                    json!({"call": call(value), "tool": tool, "result": result}),
                )
            }
            EventKind::DefinitionReplaced { revision } => {
                observation("definition_replaced", json!({"revision": revision.0}))
            }
            EventKind::ConfigurationChanged(change) => {
                observation("configuration_changed", json!({"change": format!("{change:?}")}))
            }
            EventKind::DynamicToolAdded(name) => observation("dynamic_tool_added", json!({"name": name})),
            EventKind::DynamicToolRemoved(name) => {
                observation("dynamic_tool_removed", json!({"name": name}))
            }
            EventKind::RunFinished { outcome, error } => observation(
                "run_finished",
                json!({"outcome": format!("{outcome:?}"), "error": error.as_ref().map(ToString::to_string)}),
            ),
            EventKind::CompactionStarted { end, previous_revision } => observation(
                "compaction_started",
                json!({"end": end, "previous_revision": previous_revision}),
            ),
            EventKind::CompactionFailed(error) => {
                observation("compaction_failed", json!({"error": error.to_string()}))
            }
        };
        Ok(Self {
            sequence: value.sequence,
            run: value.run.map(|run| run.to_string()),
            event,
        })
    }

    /// Fold authoritative changes into a replica. Observations are for the UI;
    /// they never synthesize accepted entries from provisional deltas.
    pub fn apply<S: Clone + DeserializeOwned>(
        &self,
        conversation: &mut Conversation<S>,
    ) -> Result<(), LogError> {
        match &self.event {
            StoredEventKind::MessageCommitted { index, entry } => {
                conversation.apply(*index, entry.decode()?)?
            }
            StoredEventKind::Compacted { compaction } => {
                conversation.apply_compaction(compaction.decode()?)?
            }
            StoredEventKind::Observation { .. } => {}
        }
        Ok(())
    }
}

pub(crate) fn initial<S: Clone + Serialize>(
    conversation: &Conversation<S>,
    next_sequence: u64,
) -> Result<Record, LogError> {
    Ok(Record::Initial {
        entries: conversation
            .entries()
            .iter()
            .map(StoredEntry::encode)
            .collect::<Result<_, _>>()?,
        compactions: conversation
            .compactions()
            .iter()
            .map(StoredCompaction::encode)
            .collect::<Result<_, _>>()?,
        next_sequence,
    })
}
