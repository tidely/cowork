//! Tests of the app as a whole: threads, the composer, collaboration
//! between app instances, and layout. Helpers shared between areas are
//! here.

use super::*;

use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use draft::{
    AttachmentId, AttachmentKind, AttachmentRecord, CommentTarget, Draft, DraftItem, DraftItemKind,
    ItemId, TextEdit,
};
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EntityInputHandler as _, ExternalPaths,
    Focusable, IntoElement, MouseButton, Render, ScrollHandle, SharedString, Window, div, point,
    px,
};
use gpui_base::{
    TextSelectionLayer, TextViewState, Textarea,
    input::{InputState, Paste, TextareaState},
};
use gpui_component::Root;
use iroh::{Endpoint, EndpointId, endpoint::presets};
use itertools::Itertools;
use rig::completion::{Message as RigMessage, Usage, message::UserContent};
use tools::TurnComments;
use uuid::Uuid;

use crate::{
    Cowork,
    attachments::{
        AttachmentSource, FileAttachment, FileAttachmentContent, MAX_TEXT_ATTACHMENT_BYTES,
    },
    composer_attachments::PendingAttachment,
    model_picker::{ModelPickerItems, OLLAMA_CONTEXT_TOKENS, language_model_groups},
    models::{ModelCatalog, ModelInfo, ModelProvider, ModelRef},
    participant::ParticipantId,
    profile::{Profile, participant_name, profile_picture},
    prompt::agent_message,
    sharing::endpoint_id_input_is_complete,
    sidebar::SIDEBAR_WIDTH,
    submission::ActiveGeneration,
    test_support::*,
    thread::{
        ControlGeneration, EditDraft, HostPeer, ManageAccess, PeerMode, PeerPermissions, Thread,
        ThreadHost, ThreadOwnership, ThreadSharing, ThreadStore,
    },
    thread_draft::{AttachmentTarget, EditorSlot, ItemPresence, RemoteCaret, ThreadDraft},
    timeline::{
        AgentCommentResponse, AgentMessage, AgentStep, CommentReference, PromptBlock, StepView,
        ThreadMessageId, TimelineMessage, UserComment, UserCommentBody, UserMessageGroup,
    },
    usage::{ActivityRange, TokenActivity},
};

mod attachments;
mod collaboration;
mod comments;
mod composer;
mod file_transfer;
mod layout;
mod model_picker;
mod permissions;
mod search_palette;
mod threads;
mod tool_approval;

impl Thread {
    fn from_welcome(
        welcome: protocol::Welcome,
        draft: ThreadDraft,
        sharing: ThreadSharing,
        cx: &mut impl AppContext,
    ) -> Self {
        Self::from_prepared_welcome(
            Self::prepare_welcome(welcome).expect("a valid thread snapshot"),
            draft,
            sharing,
            cx,
        )
    }

    fn validate_welcome(welcome: &protocol::Welcome) -> anyhow::Result<()> {
        Self::prepare_welcome(welcome.clone()).map(|_| ())
    }

    fn rebase(&mut self, welcome: protocol::Welcome, cx: &mut impl AppContext) {
        self.try_apply_for_test(protocol::HostMessage::Welcome(Box::new(welcome)), cx)
            .expect("a valid thread snapshot");
    }
}

fn ollama_model(id: &str) -> ModelRef {
    ModelRef {
        provider: ModelProvider::Ollama,
        id: id.into(),
    }
}

fn recommended_qwen() -> ModelRef {
    ollama_model("test-default")
}

fn ollama_qwen() -> ModelRef {
    ollama_model("test-other")
}

/// A catalog offering `models`, named after their ids.
fn catalog_of(models: &[ModelRef]) -> ModelCatalog {
    let mut catalog = ModelCatalog::default();
    for provider in models.iter().map(|model| model.provider).unique() {
        catalog.set_provider(
            provider,
            models
                .iter()
                .filter(|model| model.provider == provider)
                .map(|model| {
                    let info = ModelInfo {
                        name: model.id.clone(),
                        max_tokens: OLLAMA_CONTEXT_TOKENS,
                    };
                    (model.id.clone(), info)
                }),
        );
    }
    catalog
}

/// A catalog offering both test models.
fn test_catalog() -> ModelCatalog {
    catalog_of(&[recommended_qwen(), ollama_qwen()])
}

/// Every row of the picker, as (group, name, available).
fn picker_rows(items: &ModelPickerItems) -> Vec<(usize, String, bool)> {
    use gpui_component::searchable_list::SearchableListDelegate as _;

    // Groups are never empty, so the first empty one is past the end.
    (0..)
        .map_while(|section| (items.items_count(section) > 0).then_some(section))
        .flat_map(|section| {
            (0..items.items_count(section)).map(move |row| {
                let item = items
                    .item(gpui_base::IndexPath::new(row).section(section))
                    .expect("picker row");
                (section, item.name.to_string(), item.available)
            })
        })
        .collect()
}

struct AttachmentTestRoot {
    cowork: Entity<Cowork>,
}

impl Render for AttachmentTestRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A `Cowork` showing `active_thread_id` out of `thread_store`, for tests
/// that do not go through the app's window setup.
fn test_cowork(
    thread_store: Entity<ThreadStore>,
    active_thread_id: Option<Uuid>,
    tokio_handle: tokio::runtime::Handle,
    window: &mut Window,
    cx: &mut Context<Cowork>,
) -> Cowork {
    bind_app_keys(cx);
    let (model_picker, model_picker_subscription) = Cowork::new_model_picker(window, cx);
    let local_participant_id = active_thread_id
        .and_then(|id| thread_store.read(cx).thread(id, cx))
        .filter(|thread| thread.read(cx).is_host())
        .map_or_else(ParticipantId::new, |thread| {
            thread.read(cx).participant_id()
        });
    Cowork {
        sidebar_open: true,
        recents_open: true,
        new_thread_draft: ThreadDraft::new(local_participant_id),
        attachment_errors: Vec::new(),
        pending_attachments: Vec::new(),
        timeline_scroll_handle: ScrollHandle::new(),
        timeline_focus_handle: cx.focus_handle(),
        follow_generation: true,
        thread_store,
        active_thread_id,
        selection_message_id: None,
        segment_text_views: HashMap::new(),
        shown_segments: HashMap::new(),
        render_generation: 0,
        copied_endpoint_id: None,
        join_dialog: None,
        main_stage: MainStage::Thread,
        selected_welcome_provider: None,
        profile: Profile::local(local_participant_id),
        shown_profiles: HashMap::new(),
        profile_error: None,
        profile_name_subscription: None,
        tokio_handle,
        sandboxes: crate::test_support::unused_sandboxes(),
        active_generations: HashMap::new(),
        tokens_used: 0,
        token_activity: Vec::new(),
        activity_range: ActivityRange::default(),
        local_participant_id,
        typing_in: None,
        published_presence: HashMap::new(),
        caret_label_refresh: None,
        working_refresh: None,
        new_thread_model: None,
        model_picker,
        model_picker_hovered: false,
        models: Arc::default(),
        picker_rows: None,
        _model_picker_subscription: model_picker_subscription,
        _window_activation_subscription: cx.observe_window_activation(window, |_, _, _| {}),
    }
}

/// The agent starting message `id` in reply to `prompt`.
fn agent_started(
    id: Uuid,
    comment_group_id: Option<uuid::Bytes>,
    prompt: &str,
) -> protocol::HostMessage {
    protocol::HostMessage::AgentStarted {
        id: id.into_bytes(),
        comment_group_id,
        started_at: SystemTime::UNIX_EPOCH,
        prompt: protocol::Json::from_rig(&RigMessage::user(prompt)),
    }
}

/// What the host sends for an event of the agent producing message `id`.
fn agent_event(id: Uuid, event: agent::AgentEvent) -> protocol::HostMessage {
    protocol::HostMessage::AgentEvent {
        id: id.into_bytes(),
        event: protocol::Json::shared(event),
    }
}

/// The events of a model turn whose provider streams `script`, then ends the
/// reply having used `total` tokens. See [`agent::test_support::turn`].
fn model_turn(
    script: impl IntoIterator<Item = agent::test_support::MockStreamEvent>,
    total: u64,
) -> Vec<agent::AgentEvent> {
    use agent::test_support::{MockStreamEvent, mock_final_with_total_tokens};

    agent::test_support::turn(script.into_iter().chain([MockStreamEvent::FinalResponse(
        mock_final_with_total_tokens(total),
    )]))
}

/// A streamed call of the `name` tool with `arguments`.
fn streamed_tool_call(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> agent::test_support::MockStreamEvent {
    agent::test_support::MockStreamEvent::tool_call(id, name, arguments)
}

/// A fragment of thinking, streamed as reasoning item `id`, or of the
/// answer.
fn streamed_text(id: &str, text: &str, thinking: bool) -> agent::test_support::MockStreamEvent {
    use agent::test_support::MockStreamEvent;

    if thinking {
        MockStreamEvent::ReasoningDelta {
            id: id.into(),
            reasoning: text.into(),
        }
    } else {
        MockStreamEvent::text(text)
    }
}

/// The results of every tool the turn in `events` called, as the agent
/// loop would report them.
fn tool_results(events: &[agent::AgentEvent], output: &str) -> Vec<agent::AgentEvent> {
    let mut fold = agent::TurnFold::default();
    for event in events {
        fold.apply(event).expect("a valid event");
    }
    fold.pending_calls()
        .iter()
        .map(|call| agent::AgentEvent::ToolResult {
            call: call.id.clone(),
            result: rig::tool::ToolResult::success(rig::tool::ToolOutput::text(output)),
        })
        .collect()
}

/// `participant` joining with the profile derived from their id.
fn joined(participant: ParticipantId) -> protocol::HostMessage {
    protocol::HostMessage::ParticipantJoined {
        participant: participant.into_bytes(),
        profile: protocol::Profile::default(),
    }
}

fn test_thread(thread_id: Uuid, timeline: Vec<TimelineMessage>, draft: ThreadDraft) -> Thread {
    let author = draft.author;
    let mut thread =
        Thread::new_local("Test".into(), timeline, draft, author, Arc::default(), None);
    thread.instance_id = thread_id;
    thread.summary.id = thread_id;
    thread
}

fn attachment_test_cowork(
    cx: &mut gpui::TestAppContext,
    tokio_handle: tokio::runtime::Handle,
) -> (Entity<Cowork>, Uuid, &mut gpui::VisualTestContext) {
    cx.update(gpui_component::init);
    let thread_id = Uuid::new_v4();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let draft = ThreadDraft::new(ParticipantId::new());
        let thread = cx.new(|_| test_thread(thread_id, Vec::new(), draft));
        let thread_store = cx.new(|_| ThreadStore {
            threads: VecDeque::from([thread]),
        });
        let cowork =
            cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
        AttachmentTestRoot { cowork }
    });
    let cowork = view.read_with(cx, |root, _| root.cowork.clone());
    (cowork, thread_id, cx)
}

fn draft_attachments(draft: &ThreadDraft) -> Vec<FileAttachment> {
    draft
        .items()
        .into_iter()
        .flat_map(|item| match item.kind {
            DraftItemKind::Prompt { attachments } => attachments,
            DraftItemKind::Comment { .. } => Vec::new(),
        })
        .map(|record| {
            draft
                .files
                .get(&record.id)
                .cloned()
                .expect("attachment bytes")
        })
        .collect()
}

struct EmptyThreadTestView {
    thread: Entity<Thread>,
    draft_id: Uuid,
}

impl Render for EmptyThreadTestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A full `Cowork` window showing the draft of a new thread.
fn composer_test_cowork(
    cx: &mut gpui::TestAppContext,
) -> (
    Entity<Cowork>,
    tokio::runtime::Runtime,
    &mut gpui::VisualTestContext,
) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let tokio_handle = runtime.handle().clone();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let thread_store = cx.new(|_| ThreadStore::default());
        let cowork = cx.new(|cx| test_cowork(thread_store, None, tokio_handle, window, cx));
        Root::new(cowork, window, cx)
    });
    let cowork = root.read_with(cx, |root, _| {
        root.view()
            .clone()
            .downcast::<Cowork>()
            .expect("root shows cowork")
    });
    // Focus and blur events are only delivered to an active window.
    cx.update(|window, _| window.activate_window());
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.set_models(test_catalog(), cx);
            cowork.select_model(ollama_qwen(), cx);
            cowork.focus_composer(window, cx);
        })
    });
    cx.run_until_parked();
    (cowork, runtime, cx)
}

fn new_thread_items(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) -> Vec<DraftItem> {
    cowork.read_with(cx, |cowork, _| cowork.new_thread_draft.items())
}

fn prompt_bodies(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
    new_thread_items(cowork, cx)
        .into_iter()
        .filter(|item| item.is_prompt())
        .map(|item| item.body)
        .collect()
}

fn focused_slot(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) -> Option<EditorSlot> {
    cx.update(|window, cx| {
        cowork
            .read(cx)
            .focused_draft_editor(window, cx)
            .map(|(_, slot, _)| slot)
    })
}

/// A host and a collaborator side by side in one window, connected over
/// an in-memory stream through the real protocol code.
struct Collaboration<'a> {
    host: Entity<Cowork>,
    collaborator: Entity<Cowork>,
    host_thread: Entity<Thread>,
    cx: &'a mut gpui::VisualTestContext,
    _runtime: tokio::runtime::Runtime,
}

struct PairRoot {
    host: Entity<Cowork>,
    collaborator: Entity<Cowork>,
}

/// A connected host and collaborator end, relayed on the test's own
/// executor, with every message encoded and decoded like on the wire.
fn in_memory_peers(cx: &mut App) -> (HostPeer, ThreadHost) {
    fn relay<Message: serde::Serialize + serde::de::DeserializeOwned + 'static>(
        from: async_channel::Receiver<Message>,
        to: async_channel::Sender<Message>,
        cx: &mut App,
    ) {
        cx.spawn(async move |_| {
            while let Ok(message) = from.recv().await {
                let bytes = postcard::to_stdvec(&message).expect("encode message");
                let message = postcard::from_bytes(&bytes).expect("decode message");
                if to.send(message).await.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    let (host_out, host_out_relay) = async_channel::unbounded();
    let (host_in_relay, host_in) = async_channel::unbounded();
    let (collaborator_out, collaborator_out_relay) = async_channel::unbounded();
    let (collaborator_in_relay, collaborator_in) = async_channel::unbounded();
    // Bulk messages get their own relay, so they may arrive before
    // control messages sent earlier, like on the wire.
    let (host_bulk, host_bulk_relay) = async_channel::bounded(2);
    let (collaborator_bulk, collaborator_bulk_relay) = async_channel::bounded(2);
    relay::<protocol::HostMessage>(host_out_relay, collaborator_in_relay.clone(), cx);
    relay::<protocol::HostMessage>(host_bulk_relay, collaborator_in_relay, cx);
    relay::<protocol::CollaboratorMessage>(collaborator_out_relay, host_in_relay.clone(), cx);
    relay::<protocol::CollaboratorMessage>(collaborator_bulk_relay, host_in_relay, cx);
    (
        protocol::Peer {
            outgoing: host_out,
            bulk: host_bulk,
            incoming: host_in,
        },
        protocol::Peer {
            outgoing: collaborator_out,
            bulk: collaborator_bulk,
            incoming: collaborator_in,
        },
    )
}

impl Render for PairRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .child(div().w_1_2().h_full().child(self.host.clone()))
            .child(div().w_1_2().h_full().child(self.collaborator.clone()))
    }
}

impl<'a> Collaboration<'a> {
    /// Starts with the host sharing a thread whose draft has one block,
    /// and the collaborator joined to it.
    fn start(cx: &'a mut gpui::TestAppContext) -> Self {
        Self::start_with_history(cx, false)
    }

    fn start_with_history(cx: &'a mut gpui::TestAppContext, long_history: bool) -> Self {
        cx.update(gpui_component::init);
        // Never driven, so nothing ever runs on it: the test scheduler
        // rejects wake-ups from other threads. The agent never answers.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let endpoint = runtime
            .block_on(Endpoint::builder(presets::Minimal).bind())
            .expect("bind endpoint");
        let tokio_handle = runtime.handle().clone();
        let thread_id = Uuid::new_v4();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let mut draft = ThreadDraft::new(ParticipantId::new());
            draft.create_prompt(draft.author.as_uuid(), "from the host");
            let timeline = if long_history {
                let text = (0..60)
                    .map(|index| format!("Paragraph {index}: a response to scroll past.\n\n"))
                    .collect::<String>();
                vec![TimelineMessage::Agent(AgentMessage {
                    id: Uuid::new_v4(),
                    comment_group_id: None,
                    started_at: SystemTime::UNIX_EPOCH,
                    comment_responses: Vec::new(),
                    prompt: 0,
                    pending_events: Vec::new(),
                    run: crate::protocol::AgentRun::Ended {
                        outcome: crate::protocol::RunOutcome::Completed,
                        duration: std::time::Duration::ZERO,
                    },
                    output: crate::timeline::AgentOutput {
                        thinking_complete: true,
                        text: text.clone(),
                        ..Default::default()
                    },
                    committed: Default::default(),
                    comment_calls_checked: 0,
                    awaiting_approval: None,
                    step_views: Vec::new(),
                    work_expanded: false,
                    text_view: cx.new(|cx| TextViewState::markdown(&text, cx)),
                })]
            } else {
                Vec::new()
            };
            let mut thread = test_thread(thread_id, timeline, draft);
            if long_history {
                // What a joiner derives the message's text from.
                let TimelineMessage::Agent(message) = &thread.timeline[0] else {
                    unreachable!("the long history's agent message");
                };
                let reply = RigMessage::assistant(message.output.text.clone());
                thread
                    .transcript
                    .extend([RigMessage::user("Long history prompt"), reply]);
            }
            assert!(thread.set_model_catalog(test_catalog(), cx));
            thread
                .select_model(ollama_qwen(), cx)
                .expect("local host can select a model");
            assert!(thread.start_hosting(endpoint));
            let thread = cx.new(|_| thread);
            let host_store = cx.new(|_| ThreadStore {
                threads: VecDeque::from([thread]),
            });
            let host = cx.new(|cx| {
                test_cowork(
                    host_store,
                    Some(thread_id),
                    tokio_handle.clone(),
                    window,
                    cx,
                )
            });
            let collaborator_store = cx.new(|_| ThreadStore::default());
            let collaborator = cx.new(|cx| {
                let mut collaborator =
                    test_cowork(collaborator_store, None, tokio_handle.clone(), window, cx);
                if long_history {
                    collaborator.follow_generation = false;
                }
                collaborator
            });
            let pair = cx.new(|_| PairRoot { host, collaborator });
            Root::new(pair, window, cx)
        });
        cx.update(|window, _| window.activate_window());
        let pair = root.read_with(cx, |root, _| {
            root.view()
                .clone()
                .downcast::<PairRoot>()
                .expect("root shows the pair")
        });
        let (host, collaborator) =
            pair.read_with(cx, |pair, _| (pair.host.clone(), pair.collaborator.clone()));
        let host_thread = host.read_with(cx, |host, cx| host.active_thread(cx).expect("thread"));

        let (host_end, collaborator_end) = cx.update(|_, cx| in_memory_peers(cx));
        cx.update(|_, cx| {
            let host_weak = host.downgrade();
            let thread_weak = host_thread.downgrade();
            cx.spawn(async move |cx| {
                if let Err(error) = Cowork::serve_peer(host_weak, thread_weak, host_end, cx).await {
                    eprintln!("stopped serving collaborator: {error:#}");
                }
            })
            .detach();
            let collaborator = collaborator.clone();
            cx.spawn(async move |cx| {
                collaborator_end
                    .send(protocol::CollaboratorMessage::Join {
                        protocol_version: protocol::PROTOCOL_VERSION,
                    })
                    .await
                    .expect("send join");
                let profile = collaborator
                    .read_with(cx, |collaborator, _| collaborator.profile.to_protocol());
                collaborator_end
                    .send(protocol::CollaboratorMessage::Profile(profile))
                    .await
                    .expect("send profile");
                let Some(protocol::HostMessage::Welcome(welcome)) =
                    collaborator_end.receive().await
                else {
                    panic!("expected a welcome");
                };
                collaborator.update(cx, |collaborator, cx| {
                    collaborator
                        .mirror_thread(*welcome, collaborator_end, None, cx)
                        .expect("valid welcome");
                });
            })
            .detach();
        });

        let mut collaboration = Self {
            host,
            collaborator,
            host_thread,
            cx,
            _runtime: runtime,
        };
        collaboration.wait_until("the collaborator joins", |this| {
            this.collaborator_thread().is_some()
        });
        collaboration
    }

    /// Pumps both sides, drawing frames so editors catch up, until `done`.
    fn wait_until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        for _ in 0..20 {
            self.cx.run_until_parked();
            self.cx.update(|window, cx| window.draw(cx).clear(cx));
            self.cx.run_until_parked();
            if done(self) {
                return;
            }
        }
        panic!("gave up waiting until {what}");
    }

    /// Lets everything in flight settle.
    fn settle(&mut self) {
        for _ in 0..5 {
            self.cx.run_until_parked();
            self.cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        self.cx.run_until_parked();
    }

    fn collaborator_thread(&mut self) -> Option<Entity<Thread>> {
        self.collaborator
            .read_with(self.cx, |collaborator, cx| collaborator.active_thread(cx))
    }

    fn items(&mut self, thread: &Entity<Thread>) -> Vec<DraftItem> {
        thread.read_with(self.cx, |thread, _| thread.draft().items())
    }

    fn bodies(&mut self, thread: &Entity<Thread>) -> Vec<String> {
        self.items(thread)
            .into_iter()
            .map(|item| item.body)
            .collect()
    }

    fn focus(&mut self, cowork: &Entity<Cowork>) {
        self.cx.update(|window, cx| {
            cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx));
        });
    }
}

/// Sets the host's default peer mode and waits for the mirror to see it.
/// Collaborators join as `Write`, so tests that submit or stop runs from
/// the collaborator grant `Admin` first.
fn set_default(session: &mut Collaboration<'_>, mode: PeerMode) {
    session.host_thread.update(session.cx, |thread, cx| {
        thread
            .with_authorized::<ManageAccess, _>(thread.participant_id(), |mut auth| {
                auth.set_default_mode(mode, cx);
            })
            .expect("host manages access");
    });
    session.wait_until("the mirror receives the default mode", |this| {
        let mirror = this.collaborator_thread().expect("joined");
        mirror.read_with(this.cx, |thread, _| {
            thread.peer_permissions().default_mode() == mode
        })
    });
}
