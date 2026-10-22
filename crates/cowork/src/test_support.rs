//! Helpers shared by the tests of several modules.

use std::{collections::HashMap, sync::Arc};

use draft::{AttachmentId, AttachmentRecord};
use uuid::Uuid;

use agent::{
    Compaction, DefinitionRevision, Entry, InputId, Origin,
    model::{
        Content, FinishReason, GenerationMetadata, Message, Reasoning, Role, ToolCall, ToolCallId,
        ToolResult, ToolResultContent,
    },
};

use crate::{
    attachments::{FileAttachment, FileAttachmentContent},
    protocol::{self, Preview, PreviewDelta, RunEvent},
    thread::{Thread, ThreadSharing},
    transcript::{RunSource, shared_compaction, shared_entry},
};

impl Thread {
    /// UI fixtures use the same execution lifecycle as real runs rather than
    /// assigning independent activity flags.
    pub(crate) fn set_generating_for_test(
        &mut self,
        generating: bool,
        cx: &mut impl gpui::AppContext,
    ) {
        let id = self.timeline.iter().find_map(|entry| match entry {
            crate::timeline::TimelineMessage::Agent(run) if run.is_generating() => {
                Some(run.record.id)
            }
            _ => None,
        });
        if generating && !self.is_generating() {
            if let Some(id) = id {
                self.restore_execution(Some((id, Default::default())), false);
            } else {
                self.apply_for_test(
                    protocol::HostMessage::AgentStarted {
                        id: Uuid::new_v4().into_bytes(),
                        comment_group_id: None,
                        started_at: std::time::SystemTime::now(),
                    },
                    cx,
                );
            }
        } else if !generating && let Some(id) = id {
            self.end_agent_run(
                id.into_bytes(),
                protocol::RunOutcome::Stopped,
                std::time::Duration::ZERO,
                cx,
            );
        }
    }
}

/// Simulates a client that does not honor its local permission or epoch guards.
/// The real host receiver still validates every request delivered this way.
pub(crate) fn request_unchecked(thread: &Thread, request: protocol::CollaboratorMessage) -> bool {
    let ThreadSharing::Connected { host, .. } = &thread.sharing else {
        panic!("a connected test peer");
    };
    host.try_send(request).is_ok()
}

/// Sandboxes for a test `Cowork`. Nothing is set up until a command runs, and
/// tests run none, so the scratch root is never created.
pub(crate) fn unused_sandboxes() -> Arc<sandbox::Sandboxes> {
    Arc::new(sandbox::Sandboxes::new(
        std::env::temp_dir().join("cowork-test-sandboxes-unused"),
        Uuid::new_v4().simple().to_string(),
        sandbox::EnvironmentSource::Process,
    ))
}

pub(crate) fn encoded_image(width: u32, format: image::ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(width, 2))
        .write_to(&mut std::io::Cursor::new(&mut bytes), format)
        .expect("encode test image");
    bytes
}

pub(crate) fn text_attachment(name: &str, text: &str) -> FileAttachment {
    FileAttachment {
        name: name.into(),
        content: FileAttachmentContent::Text(text.into()),
    }
}

/// Records for `files`, and the files by id, as a thread holds them.
pub(crate) fn attached(
    files: impl IntoIterator<Item = FileAttachment>,
) -> (Vec<AttachmentRecord>, HashMap<AttachmentId, FileAttachment>) {
    files
        .into_iter()
        .map(|file| {
            let record = AttachmentRecord {
                id: AttachmentId::new(),
                name: file.name.clone(),
                kind: file.kind(),
                size: file.len(),
                creator: Uuid::new_v4(),
            };
            (record.clone(), (record.id, file))
        })
        .unzip()
}

/// A fragment a scripted model streams: part `id` of the reply.
#[derive(Clone)]
pub(crate) enum Streamed {
    Text {
        id: String,
        text: String,
    },
    Thinking {
        id: String,
        text: String,
    },
    Call {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

/// A fragment of thinking, streamed as part `id`, or of the answer.
pub(crate) fn streamed_text(id: &str, text: &str, thinking: bool) -> Streamed {
    if thinking {
        Streamed::Thinking {
            id: id.into(),
            text: text.into(),
        }
    } else {
        Streamed::Text {
            id: id.into(),
            text: text.into(),
        }
    }
}

/// A call of the `name` tool with `arguments`, as part `id`.
pub(crate) fn streamed_tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Streamed {
    Streamed::Call {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

/// Plays a host's agent session for tests: the messages the host sends for
/// its runs, numbering committed entries as its conversation does. One per
/// thread, as entries count across runs.
#[derive(Default)]
pub(crate) struct ScriptedHost {
    entries: u64,
    compactions: usize,
    calls: Vec<ToolCall>,
    minted: usize,
}

impl ScriptedHost {
    /// The run producing message `id` starts.
    pub(crate) fn started(
        &self,
        id: Uuid,
        comment_group_id: Option<uuid::Bytes>,
    ) -> protocol::HostMessage {
        protocol::HostMessage::AgentStarted {
            id: id.into_bytes(),
            comment_group_id,
            started_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// The run producing message `id` ends with `outcome`.
    pub(crate) fn ended(&self, id: Uuid, outcome: protocol::RunOutcome) -> protocol::HostMessage {
        protocol::HostMessage::AgentEnded {
            id: id.into_bytes(),
            outcome,
            duration: std::time::Duration::from_secs(1),
        }
    }

    fn event(id: Uuid, event: RunEvent) -> protocol::HostMessage {
        protocol::HostMessage::AgentEvent {
            id: id.into_bytes(),
            event,
        }
    }

    fn commit(&mut self, id: Uuid, entry: Entry<RunSource>) -> protocol::HostMessage {
        let index = self.entries;
        self.entries += 1;
        Self::event(
            id,
            RunEvent::Committed {
                index,
                entry: shared_entry(&entry),
            },
        )
    }

    /// The run producing message `id` commits `text` as its prompt.
    pub(crate) fn prompt(&mut self, id: Uuid, text: &str) -> protocol::HostMessage {
        self.commit(
            id,
            Entry {
                message: Message::user(text),
                origin: Origin::Input {
                    id: InputId::new(),
                    source: RunSource::Schedule,
                },
            },
        )
    }

    /// The run producing message `id` starts, with `text` as its prompt.
    pub(crate) fn start(&mut self, id: Uuid, text: &str) -> Vec<protocol::HostMessage> {
        vec![self.started(id, None), self.prompt(id, text)]
    }

    /// A model reply streaming `script`, then reporting it used `total`
    /// tokens and joining the transcript.
    pub(crate) fn turn(
        &mut self,
        id: Uuid,
        script: impl IntoIterator<Item = Streamed>,
        total: u64,
    ) -> Vec<protocol::HostMessage> {
        let mut parts: Vec<(String, Content)> = Vec::new();
        let mut events = Vec::new();
        for streamed in script {
            let (part, content) = match &streamed {
                Streamed::Text { id, text } => (id.clone(), Content::text(text.clone())),
                Streamed::Thinking { id, text } => (
                    id.clone(),
                    Content::Reasoning(Reasoning::Text { text: text.clone() }),
                ),
                Streamed::Call {
                    id,
                    name,
                    arguments,
                } => {
                    self.minted += 1;
                    let mut call = crate::timeline::tool_call(id, name, arguments.clone());
                    call.id = ToolCallId(format!("{id}-{}", self.minted));
                    (id.clone(), Content::ToolCall(call))
                }
            };
            let index = match parts.iter().position(|(existing, _)| *existing == part) {
                Some(index) => {
                    match (&mut parts[index].1, &content) {
                        (Content::Text { text }, Content::Text { text: more })
                        | (
                            Content::Reasoning(Reasoning::Text { text }),
                            Content::Reasoning(Reasoning::Text { text: more }),
                        ) => text.push_str(more),
                        _ => panic!("part {part} changes kind"),
                    }
                    index
                }
                None => {
                    parts.push((part, content.clone()));
                    parts.len() - 1
                }
            };
            let delta = match streamed {
                Streamed::Text { text, .. } => PreviewDelta::Text(text),
                Streamed::Thinking { text, .. } => PreviewDelta::Reasoning(text),
                Streamed::Call { .. } => continue,
            };
            events.push(Self::event(
                id,
                RunEvent::Preview(Preview {
                    index: index as u32,
                    delta,
                }),
            ));
        }
        let content = parts
            .into_iter()
            .map(|(_, content)| content)
            .collect::<Vec<_>>();
        self.calls = content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        let mut message = Message::new(Role::Assistant, content);
        message.generation = Some(GenerationMetadata {
            provider: "test".into(),
            model: None,
            finish_reason: Some(if self.calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            }),
            usage: None,
            extra: Default::default(),
        });
        events.push(Self::event(id, RunEvent::Usage { tokens: total }));
        events.push(self.commit(
            id,
            Entry {
                message,
                origin: Origin::Model {
                    revision: DefinitionRevision::default(),
                },
            },
        ));
        events
    }

    /// Each call of the last reply returning `output`, in order.
    pub(crate) fn results(&mut self, id: Uuid, output: &str) -> Vec<protocol::HostMessage> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .map(|call| {
                self.commit(
                    id,
                    Entry {
                        message: Message::new(
                            Role::Tool,
                            vec![Content::ToolResult(ToolResult {
                                call: call.id,
                                provider_id: None,
                                name: Some(call.name),
                                content: vec![ToolResultContent::Text {
                                    text: output.into(),
                                }],
                                is_error: false,
                            })],
                        ),
                        origin: Origin::Tool,
                    },
                )
            })
            .collect()
    }

    /// The calls of the last reply still waiting for their results.
    pub(crate) fn pending_calls(&self) -> &[ToolCall] {
        &self.calls
    }

    /// A summary of the first `end` entries replacing them, as the run
    /// producing message `id` compacts its context.
    pub(crate) fn compaction(
        &mut self,
        id: Uuid,
        end: usize,
        summary: &str,
    ) -> Vec<protocol::HostMessage> {
        let compaction = Compaction {
            previous_revision: self.compactions,
            end,
            summary: Message::user(summary),
        };
        self.compactions += 1;
        vec![
            Self::event(id, RunEvent::CompactionStarted),
            Self::event(id, RunEvent::Compacted(shared_compaction(&compaction))),
        ]
    }
}
