use std::{
    cell::Cell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, OnceLock},
};

use anyhow::Context as _;
use gpui::{
    App, AppContext, Bounds, Context, Entity, EntityId, ExternalPaths, IntoElement, KeyBinding,
    MouseButton, MouseUpEvent, PlatformInput, QuitMode, Render, ScrollHandle, SharedString,
    Subscription, TextRun, TitlebarOptions, Window, WindowBounds, WindowDecorations, WindowOptions,
    actions, canvas, div, prelude::*, px, size,
};
use gpui_base::input::{Backspace, Escape, MoveDown, MoveUp};
use gpui_component::{ActiveTheme as _, Root, TitleBar};
use tokio::runtime::Runtime;
use uuid::Uuid;

use crate::{
    assets::Assets,
    composer_attachments::{AttachmentError, PendingAttachment},
    model_picker::ModelPickerState,
    models::{ModelCatalog, ModelRef},
    participant::ParticipantId,
    profile::Profile,
    project_folders::ProjectFolder,
    project_mode::ProjectMode,
    sharing::JoinDialog,
    sidebar::SIDEBAR_WIDTH,
    submission::ActiveGeneration,
    thread::{Thread, ThreadStore},
    thread_draft::ThreadDraft,
    timeline::{PromptBlock, ThreadMessageId, TimelineMessage},
    timeline_view::{SegmentTextView, ShownSegments},
    top_bar::macos_traffic_light_position,
    usage::{ActivityRange, TokenActivity},
};

mod archive;
mod assets;
mod attachments;
mod avatars;
mod caret;
mod component_preview;
mod composer;
mod composer_attachments;
mod draft_editing;

mod highlight;
mod model_picker;
mod models;
mod participant;
mod peer_access;
mod profile;
mod profile_page;
mod project_folders;
mod project_mode;
mod prompt;
mod protocol;
mod scheduled_task;
mod scheduler;
mod schedules;
mod search_palette;

mod sidebar;
mod statistics;

#[cfg(test)]
mod test_support;
mod theme;
mod thread;
use thread::{draft as thread_draft, sharing, submission};
mod timeline;
mod timeline_view;
mod tool_approval;
mod tool_call_card;
mod top_bar;
mod transcript;
mod usage;
mod welcome;

static TOKIO_RUNTIME: OnceLock<Runtime> = OnceLock::new();

actions!(cowork, [Quit, SubmitComposer, OpenSearchPalette]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainStage {
    Welcome,
    ProviderSetup(ProviderSetupStage),
    Thread,
    Profile,
    /// The archived threads, to restore or delete for good.
    Archive,
    /// The scheduled prompts.
    Schedules,
}

/// The provider whose setup page occupies the main stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderSetupStage {
    Ollama,
}

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    new_thread_draft: ThreadDraft,
    attachment_errors: Vec<AttachmentError>,
    pending_attachments: Vec<PendingAttachment>,
    timeline_scroll_handle: ScrollHandle,
    /// Taken when someone presses on a user message, as agent text takes
    /// focus itself, so that Copy reaches the window's text selection
    /// instead of an editor with nothing selected.
    timeline_focus_handle: gpui::FocusHandle,
    follow_generation: bool,
    thread_store: Entity<ThreadStore>,
    active_thread_id: Option<Uuid>,
    selection_message_id: Option<Uuid>,
    segment_text_views: HashMap<(ThreadMessageId, usize), SegmentTextView>,
    shown_segments: HashMap<ThreadMessageId, ShownSegments>,
    render_generation: u64,
    copied_endpoint_id: Option<Uuid>,
    join_dialog: Option<Entity<JoinDialog>>,
    /// Which page occupies the center stage.
    main_stage: MainStage,
    selected_welcome_provider: Option<models::ModelProvider>,
    profile: Profile,
    /// Everyone's profile as the active thread shows them, refreshed at the
    /// start of every render; see [`Cowork::profiles_for`].
    shown_profiles: HashMap<ParticipantId, Profile>,
    profile_error: Option<SharedString>,
    /// Watches the open profile name dialog's input.
    profile_name_subscription: Option<Subscription>,
    tokio_handle: tokio::runtime::Handle,
    /// Where the agent's shell commands run, one microVM per hosted thread.
    sandboxes: Arc<sandbox::Sandboxes>,
    active_generations: HashMap<Uuid, ActiveGeneration>,
    /// Tokens used across local threads, excluding ones joined from someone
    /// else; see [`Cowork::record_turn_usage`].
    tokens_used: u64,
    /// When those tokens were used, turn by turn, in the order turns ended.
    token_activity: Vec<TokenActivity>,
    /// How many chats were deleted from the archive or are gone since an
    /// earlier session, and the longest the agent generated in one of them,
    /// which the profile's statistics still count; see
    /// [`Cowork::delete_archived_thread`] and `statistics.rs`.
    deleted_chats: usize,
    longest_deleted_chat: std::time::Duration,
    /// Where the profile's statistics are saved; `None` keeps them in
    /// memory, as in tests.
    statistics_file: Option<std::path::PathBuf>,
    /// The scheduled tasks, and the queue running them; see `scheduler.rs`.
    scheduler: scheduler::Scheduler,
    /// The period the profile page's token activity chart shows.
    activity_range: ActivityRange,
    /// Who the local user is in the threads this app creates and hosts.
    local_participant_id: ParticipantId,
    /// The draft editor the user is typing in, so that when its item is
    /// removed, by a submission or by someone else, the caret can move to
    /// the draft position instead of vanishing.
    typing_in: Option<(Uuid, EntityId, gpui::FocusHandle)>,
    /// The presence last sent for each shared thread, by instance id.
    published_presence: HashMap<Uuid, protocol::Presence>,
    caret_label_refresh: Option<gpui::Task<()>>,
    /// The next redraw of a running agent's "Working for" line.
    working_refresh: Option<gpui::Task<()>>,
    /// The model new threads start with: the last one selected locally.
    new_thread_model: Option<ModelRef>,
    /// The folders added before the thread exists, which move into it with
    /// the draft; see [`Thread::project_folders`].
    new_thread_project_folders: Vec<ProjectFolder>,
    /// The mode chosen before the thread exists, which moves into it with
    /// the folders; see [`Thread::project_mode`].
    new_thread_project_mode: ProjectMode,
    /// Always shows the active thread's model; see [`Cowork::sync_model_picker`].
    model_picker: Entity<ModelPickerState>,
    model_picker_hovered: bool,
    /// The models this app can run, as last discovered.
    models: Arc<ModelCatalog>,
    /// The catalog and selection the picker's rows were built from.
    picker_rows: Option<(Arc<ModelCatalog>, Option<ModelRef>)>,
    _model_picker_subscription: Subscription,
    _window_activation_subscription: Subscription,
}

impl Cowork {
    fn active_thread(&self, cx: &App) -> Option<Entity<Thread>> {
        self.active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
    }

    fn new_local_thread(
        title: String,
        timeline: Vec<TimelineMessage>,
        draft: ThreadDraft,
        participant_id: ParticipantId,
        models: Arc<ModelCatalog>,
        model: Option<ModelRef>,
        cx: &mut App,
    ) -> Entity<Thread> {
        cx.new(|_| Thread::new_local(title, timeline, draft, participant_id, models, model))
    }

    fn new_empty_local_thread(
        draft: ThreadDraft,
        participant_id: ParticipantId,
        models: Arc<ModelCatalog>,
        model: Option<ModelRef>,
        cx: &mut App,
    ) -> Entity<Thread> {
        Self::new_local_thread(
            "New thread".into(),
            Vec::new(),
            draft,
            participant_id,
            models,
            model,
            cx,
        )
    }

    fn end_stale_mouse_drag(window: &mut Window, cx: &mut App) {
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: window.mouse_position(),
                modifiers: window.modifiers(),
                click_count: 1,
            }),
            cx,
        );
    }

    fn render_main_editor(
        &mut self,
        read_only_line_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_generation = self.render_generation.wrapping_add(1);
        let active_thread_id = self.active_thread_id;
        let (messages, draft_comments) = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .map(|thread| {
                let thread = thread.read(cx);
                (
                    thread.timeline.clone(),
                    thread.draft().comment_views(thread.participants()),
                )
            })
            .unwrap_or_else(|| (Vec::new(), self.new_thread_draft.comment_views(&[])));
        let composer = self
            .readable_draft_id(cx)
            .and_then(|draft_id| self.composer_model(draft_id, window, cx));
        let comments = messages
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => Some(group.comments.as_slice()),
                TimelineMessage::Agent(_) => None,
            })
            .flatten()
            .cloned()
            .chain(draft_comments)
            .collect::<Vec<_>>();
        let sidebar_width = if self.sidebar_open {
            SIDEBAR_WIDTH
        } else {
            px(0.)
        };
        // Measure a character in the timeline's 14 px font so the text column
        // stays 120 characters wide when the available stage is wider.
        let character = "0";
        let run = TextRun {
            len: character.len(),
            font: window.text_style().font(),
            color: cx.theme().foreground,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let text_width = window
            .text_system()
            .shape_line(character.into(), px(14.), &[run], None)
            .width()
            * 120.;
        // Message rows reserve 40 px for the avatar and 40 px on the right.
        let row_width = text_width + px(80.);
        let available_width = window.viewport_size().width - sidebar_width - px(82.);
        let wrap_width = available_width.max(px(120.)).min(text_width);
        let mut timeline_messages = Vec::new();
        if let Some(thread_id) = active_thread_id {
            for (index, message) in messages.iter().enumerate() {
                let submitted_comments = match message {
                    TimelineMessage::Agent(message) => message
                        .comment_group_id
                        .and_then(|group_id| {
                            messages.iter().find_map(|entry| match entry {
                                TimelineMessage::User(group) if group.id == group_id => {
                                    Some(group.comments.as_slice())
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default(),
                    TimelineMessage::User(_) => &[],
                };
                timeline_messages.push(self.render_timeline_message(
                    thread_id,
                    index,
                    message,
                    &comments,
                    submitted_comments,
                    wrap_width,
                    window,
                    cx,
                ));
            }
        }

        self.segment_text_views
            .retain(|_, text_view| text_view.rendered_at == self.render_generation);
        self.shown_segments
            .retain(|_, shown| shown.rendered_at == self.render_generation);

        let can_write = composer.is_some();
        let composer = composer.map(|composer| self.render_composer(composer, cx));

        div()
            .id("main-editor")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                div()
                    .id("timeline-scroll")
                    .track_focus(&self.timeline_focus_handle)
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.timeline_scroll_handle)
                    .on_scroll_wheel(cx.listener(Self::timeline_scrolled))
                    .child(
                        div()
                            .w_full()
                            .max_w(row_width)
                            .mx_auto()
                            .pt_6()
                            .min_h_full()
                            .flex()
                            .flex_col()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .children(timeline_messages.into_iter().enumerate().map(
                                |(index, message)| {
                                    div().when(index != 0, |this| this.mt_6()).child(message)
                                },
                            ))
                            .children(composer.map(|composer| {
                                composer.when(!messages.is_empty(), |this| this.mt_6())
                            }))
                            .when(!can_write, |this| {
                                this.child(
                                    div()
                                        .id("read-only-thread")
                                        .when(!messages.is_empty(), |this| this.mt_6())
                                        .relative()
                                        .w_full()
                                        .flex()
                                        .justify_center()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground.opacity(0.7))
                                        .child("Read-only thread")
                                        .child(
                                            canvas(
                                                move |bounds, _, _| {
                                                    read_only_line_bounds.set(Some(bounds));
                                                },
                                                |_, _, _, _| {},
                                            )
                                            .absolute()
                                            .size_full(),
                                        ),
                                )
                            }),
                    ),
            )
    }
}

impl Render for Cowork {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_model_picker(window, cx);
        let active_thread = self.active_thread(cx);
        self.shown_profiles =
            self.profiles_for(active_thread.as_ref().map(|thread| thread.read(cx)));
        if let Some(draft_id) = self.readable_draft_id(cx) {
            self.prepare_draft(draft_id, window, cx);
        }
        self.publish_presence(cx);
        self.schedule_caret_label_refresh(cx);
        self.schedule_working_refresh(cx);
        let composer = self.last_composer_editor(window, cx);
        let can_write = composer.is_some();
        let read_only_line_bounds = Rc::new(Cell::new(None));

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::submit_composer_action))
            .on_action(cx.listener(|this, _: &OpenSearchPalette, window, cx| {
                this.open_search_palette(window, cx);
            }))
            .on_key_down(cx.listener(Self::begin_inline_comment))
            // Run before the focused editor's own handling, which they
            // extend to the composer as a whole.
            .capture_action(cx.listener(Self::paste_attachments))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.move_between_draft_editors(true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.move_between_draft_editors(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.escape_empty_draft_item(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Backspace, window, cx| {
                if this.backspace_out_of_empty_draft_item(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(self.render_top_bar(window, cx))
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .h_full()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .when(can_write && self.main_stage == MainStage::Thread, |this| {
                                this.can_drop(|value, _, _| {
                                    value
                                        .downcast_ref::<ExternalPaths>()
                                        .is_some_and(|paths| !paths.paths().is_empty())
                                })
                                .on_drop(cx.listener(Self::drop_attachments))
                            })
                            .map(|this| match self.main_stage {
                                MainStage::Profile => this.child(self.render_profile_page(cx)),
                                MainStage::Archive => this.child(self.render_archive_page(cx)),
                                MainStage::Schedules => this.child(self.render_schedules_page(cx)),
                                MainStage::Welcome => this.child(self.render_welcome(cx)),
                                MainStage::ProviderSetup(provider) => {
                                    this.child(self.render_provider_setup(provider, cx))
                                }
                                MainStage::Thread => this
                                    .child(self.render_main_editor(
                                        read_only_line_bounds.clone(),
                                        window,
                                        cx,
                                    ))
                                    .child(self.render_bottom_bar(
                                        composer,
                                        read_only_line_bounds,
                                        window,
                                        cx,
                                    )),
                            }),
                    ),
            )
    }
}

fn bind_app_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-enter", SubmitComposer, None),
        KeyBinding::new("cmd-enter", SubmitComposer, None),
        KeyBinding::new(
            cfg_select! {
                target_os = "macos" => "cmd-k",
                _ => "ctrl-k",
            },
            OpenSearchPalette,
            None,
        ),
    ]);
}

fn main() -> anyhow::Result<()> {
    let worker_threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(2, 8);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("cowork-agent")
        .enable_all()
        .build()?;
    let tokio_handle = runtime.handle().clone();
    anyhow::ensure!(
        TOKIO_RUNTIME.set(runtime).is_ok(),
        "the agent runtime was already initialized",
    );
    let sandboxes = Arc::new(sandbox::Sandboxes::new(
        sandbox_home()?,
        Uuid::new_v4().simple().to_string(),
    ));

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::init(cx);
            bind_app_keys(cx);
            #[cfg(target_os = "macos")]
            {
                cx.on_action(|_: &Quit, cx| cx.quit());
                cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            }
            if component_preview::requested() {
                if let Err(error) = component_preview::open(cx) {
                    eprintln!("failed to open the component preview: {error}");
                    cx.quit();
                    return;
                }
                cx.set_quit_mode(QuitMode::LastWindowClosed);
                cx.activate(true);
                return;
            }
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(760.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Cowork".into()),
                    traffic_light_position: Some(macos_traffic_light_position()),
                    ..TitleBar::title_bar_options()
                }),
                window_decorations: Some(WindowDecorations::Client),
                ..TitleBar::window_options()
            };

            // microsandbox also stops the VMs once Cowork has exited; this
            // stops them sooner where quitting leaves time for it.
            cx.on_app_quit({
                let sandboxes = sandboxes.clone();
                let tokio_handle = tokio_handle.clone();
                move |_| {
                    let sandboxes = sandboxes.clone();
                    let stopped = tokio_handle.spawn(async move { sandboxes.shutdown().await });
                    async move {
                        _ = stopped.await;
                    }
                }
            })
            .detach();

            if let Err(error) = cx.open_window(window_options, move |window, cx| {
                let tokio_handle = tokio_handle.clone();
                let sandboxes = sandboxes.clone();
                let thread_store = cx.new(|_| ThreadStore::default());
                let local_participant_id = ParticipantId::new();
                let new_thread_draft = ThreadDraft::new(local_participant_id);
                let cowork = cx.new(|cx| {
                    let window_activation_subscription =
                        cx.observe_window_activation(window, |_, window, _cx| {
                            if window.is_window_active() {
                                window.on_next_frame(Cowork::end_stale_mouse_drag);
                            }
                        });
                    let (model_picker, model_picker_subscription) =
                        Cowork::new_model_picker(window, cx);
                    Cowork {
                        sidebar_open: true,
                        recents_open: true,
                        new_thread_draft,
                        attachment_errors: Vec::new(),
                        pending_attachments: Vec::new(),
                        timeline_scroll_handle: ScrollHandle::new(),
                        timeline_focus_handle: cx.focus_handle(),
                        follow_generation: true,
                        thread_store,
                        active_thread_id: None,
                        selection_message_id: None,
                        segment_text_views: HashMap::new(),
                        shown_segments: HashMap::new(),
                        render_generation: 0,
                        copied_endpoint_id: None,
                        join_dialog: None,
                        main_stage: MainStage::Welcome,
                        selected_welcome_provider: None,
                        profile: Profile::local(local_participant_id),
                        shown_profiles: HashMap::new(),
                        profile_error: None,
                        profile_name_subscription: None,
                        tokio_handle,
                        sandboxes,
                        active_generations: HashMap::new(),
                        tokens_used: 0,
                        token_activity: Vec::new(),
                        deleted_chats: 0,
                        longest_deleted_chat: Default::default(),
                        statistics_file: None,
                        scheduler: Default::default(),
                        activity_range: ActivityRange::default(),
                        local_participant_id,
                        typing_in: None,
                        published_presence: HashMap::new(),
                        caret_label_refresh: None,
                        working_refresh: None,
                        new_thread_model: None,
                        new_thread_project_folders: Vec::new(),
                        new_thread_project_mode: ProjectMode::default(),
                        model_picker,
                        model_picker_hovered: false,
                        models: Arc::default(),
                        picker_rows: None,
                        _model_picker_subscription: model_picker_subscription,
                        _window_activation_subscription: window_activation_subscription,
                    }
                });
                let window_handle = window.window_handle();
                cowork.update(cx, |cowork, cx| {
                    cowork.start_statistics(statistics::statistics_file());
                    cowork.start_scheduler(scheduler::schedule_file(), Some(window_handle), cx);
                });
                cx.new(|cx| Root::new(cowork, window, cx))
            }) {
                eprintln!("failed to open Cowork window: {error}");
                cx.quit();
                return;
            }

            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });

    Ok(())
}

/// Cowork's own microsandbox home, apart from any of the user's. Short, as
/// microsandbox puts Unix sockets beneath it.
fn sandbox_home() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::home_dir().context("the home directory is unknown")?;
    Ok(home.join(".cowork").join("microsandbox"))
}

#[cfg(test)]
mod tests;
