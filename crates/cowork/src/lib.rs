//! The Cowork app as a library, so that `src/main.rs` runs it with [`run`]
//! and examples (`cargo run -p cowork --example component_preview`) can
//! reach its components.

use std::{
    cell::Cell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, OnceLock},
};

use gpui::{
    App, AppContext, Bounds, Context, Entity, EntityId, ExternalPaths, IntoElement, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, QuitMode, Render,
    ScrollHandle, SharedString, Subscription, TextRun, TitlebarOptions, Window, WindowBounds,
    WindowDecorations, WindowOptions, actions, canvas, div, prelude::*, px, size,
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
    provider_setup::ProviderStep,
    settings::SettingsPage,
    sharing::JoinDialog,
    sidebar::SIDEBAR_WIDTH,
    submission::{ActiveGeneration, ThreadAgent},
    thread::{Thread, ThreadStore},
    thread_draft::ThreadDraft,
    timeline::{PromptBlock, ThreadMessageId, TimelineMessage},
    timeline_view::{SegmentTextView, ShownSegments, TimelineRow},
    timeline_virtualization::{BuildRow, TimelineLayout, TimelinePress, VirtualTimeline},
    top_bar::macos_traffic_light_position,
    usage::ActivityRange,
};

mod archive;
mod assets;
mod attachments;
mod avatars;
mod caret;
#[doc(hidden)]
pub mod component_preview;
mod composer;
mod composer_attachments;

mod draft_editing;
mod file_change_view;

mod highlight;
mod inline_icons;

mod context_settings;
mod create_project;
mod model_picker;
mod models;
mod participant;
mod peer_access;
mod pending_view;
mod profile;
mod profile_page;
mod project_folders;
mod project_mode;
mod projects;
mod prompt;
mod protocol;
mod provider_setup;
mod providers;
mod scheduled_task;
mod scheduler;
mod schedules;
mod search_palette;
mod settings;

mod settings_file;

mod sidebar;
mod statistics;

#[cfg(test)]
mod test_support;
mod theme;
mod thread;
mod thread_files;
use thread::{draft as thread_draft, sharing, submission};
mod timeline;
mod timeline_projection;
mod timeline_selection;
mod timeline_view;
mod timeline_virtualization;
mod tool_approval;
mod tool_call_card;
mod top_bar;
mod transcript;
mod turn_navigation;
mod usage;
mod welcome;

static TOKIO_RUNTIME: OnceLock<Runtime> = OnceLock::new();

actions!(
    cowork,
    [
        Quit,
        SubmitComposer,
        QueueComposer,
        StopGeneration,
        OpenSearchPalette
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainStage {
    /// Choosing a provider before the first chat.
    Welcome,
    /// Choosing one of the provider's models before the first chat.
    ProviderSetup(models::ModelProvider),
    Thread,
    /// A settings page, with the sidebar showing settings; see `settings.rs`.
    Settings(SettingsPage),
    /// The scheduled prompts.
    Schedules,
}

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    new_thread_draft: ThreadDraft,
    attachment_errors: Vec<AttachmentError>,
    pending_attachments: Vec<PendingAttachment>,
    timeline_scroll_handle: ScrollHandle,
    /// A pressed pointer can anchor a drag before GPUI reports a nonempty
    /// selection; see [`TimelinePress`].
    timeline_press: Option<TimelinePress>,
    /// The measured heights of the active thread's timeline rows.
    timeline_layout: Rc<std::cell::RefCell<TimelineLayout>>,
    /// The rows holding the ends of a selection in the timeline.
    timeline_selection: Rc<std::cell::RefCell<timeline_selection::TimelineSelection>>,
    /// The conversation navigation rail's local state; see
    /// `turn_navigation`.
    turn_rail: turn_navigation::TurnRail,
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
    /// Where "Back to app" returns to from settings.
    stage_before_settings: MainStage,
    /// The provider card selected on the welcome screen or the Providers
    /// settings page, to continue to its models.
    selected_provider: Option<models::ModelProvider>,
    /// Which step the Providers settings page shows.
    settings_provider_step: ProviderStep,
    /// The connections to providers, with their API keys; see
    /// `providers.rs`.
    providers: providers::Providers,
    /// The API key field of the provider whose models show, made when it
    /// first shows; see `provider_setup.rs`.
    api_key_input: Option<provider_setup::ApiKeyInput>,
    context_settings: context_settings::ContextSettings,
    profile: Profile,
    /// Everyone's profile as the active thread shows them, refreshed at the
    /// start of every render; see [`Cowork::profiles_for`].
    shown_profiles: HashMap<ParticipantId, Profile>,
    profile_error: Option<SharedString>,
    /// Watches the open profile name dialog's input.
    profile_name_subscription: Option<Subscription>,
    tokio_handle: tokio::runtime::Handle,
    /// Where the agent's shell commands run and its file tools reach, with
    /// each hosted thread's project and scratch folder.
    sandboxes: Arc<sandbox::Sandboxes>,
    active_generations: HashMap<Uuid, ActiveGeneration>,
    /// The first chat's greeting while it streams in; see
    /// `thread/onboarding.rs`.
    onboarding_intro: Option<thread::OnboardingIntro>,
    /// The agent session of each local or hosted thread that has run.
    agents: HashMap<Uuid, ThreadAgent>,
    /// What the profile's statistics still count of the user's deleted
    /// threads; the rest they count from the threads themselves. See
    /// [`Cowork::delete_archived_thread`] and `statistics.rs`.
    deleted: statistics::Statistics,
    /// Lazily rebuilt on Profile, not on unrelated streaming redraws.
    profile_statistics: Option<statistics::ProfileStatistics>,
    /// Where the profile's statistics are saved; `None` keeps them in
    /// memory, as in tests.
    statistics_file: Option<std::path::PathBuf>,
    /// Where the user's own threads are saved; `None` keeps them in memory,
    /// as in tests. See `thread_files.rs`.
    thread_files: Option<thread_files::ThreadFiles>,
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
    /// The model new threads start with: the last one selected locally,
    /// saved for the next launch; see [`Cowork::set_new_thread_model`].
    new_thread_model: Option<ModelRef>,
    /// Where the user's settings are saved; `None` keeps them in memory, as
    /// in tests. See `settings_file.rs`.
    settings_file: Option<std::path::PathBuf>,
    /// The folders added before the thread exists, which move into it with
    /// the draft; see [`Thread::project_folders`].
    new_thread_project_folders: Vec<ProjectFolder>,
    /// The mode chosen before the thread exists, which moves into it with
    /// the folders; see [`Thread::project_mode`].
    new_thread_project_mode: ProjectMode,
    /// The sidebar project the thread about to be created is composed in,
    /// which it is filed under when it is created; see
    /// [`Cowork::compose_in_project`].
    new_thread_project: Option<projects::ProjectId>,
    /// The sidebar's projects; see `projects.rs`.
    projects: projects::Projects,
    /// Always shows the active thread's model; see [`Cowork::sync_model_picker`].
    model_picker: Entity<ModelPickerState>,
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

    /// What a launch opens on, once the saved threads and settings are
    /// loaded. Models are discovered either way, once the API keys are
    /// read. A new user is onboarded (see `thread/onboarding.rs`); anyone
    /// else gets a new chat, with the saved model or none yet, to pick in
    /// the composer.
    fn open_start_stage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let discovered = self.start_providers(cx);
        if self.needs_onboarding(cx) {
            self.offer_onboarding(discovered, window, cx);
        } else {
            self.main_stage = MainStage::Thread;
            discovered.detach();
        }
        self.stage_before_settings = self.main_stage;
        cx.notify();
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
        let pointer_owner = cx.entity().downgrade();
        self.render_generation = self.render_generation.wrapping_add(1);
        let active_thread_id = self.active_thread_id;
        // The thread is read in place, never copied whole: a row clones only
        // its own message, and only when it is built.
        let active_thread = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .zip(active_thread_id);
        let composer = self
            .readable_draft_id(cx)
            .and_then(|draft_id| self.composer_model(draft_id, window, cx));
        let (message_count, comments) = match &active_thread {
            Some((thread, _)) => {
                let thread = thread.read(cx);
                let comments = thread
                    .timeline
                    .iter()
                    .filter_map(|message| match message {
                        TimelineMessage::User(group) => Some(group.record.comments.as_slice()),
                        TimelineMessage::Agent(_) => None,
                    })
                    .flatten()
                    .map(timeline::SubmittedComment::to_view)
                    .chain(thread.draft().comment_views(thread.participants()))
                    .collect::<Vec<_>>();
                (thread.timeline.len(), comments)
            }
            None => (0, self.new_thread_draft.comment_views(&[])),
        };
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
        // A selection's rows stay built while it lasts; see
        // `timeline_selection`.
        if self.timeline_press.is_none() && !gpui_base::TextSelection::has_selection(window, cx) {
            self.timeline_selection.borrow_mut().clear();
        }
        let comments = Rc::new(comments);
        // Over the stage, beside the scrolling timeline rather than in it.
        let navigation = match &active_thread {
            Some((thread, thread_id)) => {
                let turns = turn_navigation::conversation_turns(&thread.read(cx).timeline);
                self.render_turn_navigation(*thread_id, thread, &turns, window, cx)
            }
            None => Vec::new(),
        };
        let (rows, build): (_, BuildRow) = match active_thread {
            Some((thread, thread_id)) => {
                let (rows, steering_groups) = self.plan_timeline_rows(&thread.read(cx).timeline);
                let owner = cx.entity().downgrade();
                let build = move |index: usize,
                                  message_id: Uuid,
                                  first: bool,
                                  window: &mut Window,
                                  cx: &mut App| {
                    owner
                        .update(cx, |this, cx| {
                            // Built in this frame's prepaint, from the thread
                            // as it rendered.
                            let row = TimelineRow::capture(
                                &thread.read(cx).timeline,
                                index,
                                message_id,
                                &steering_groups,
                            );
                            div()
                                .id(gpui::ElementId::Uuid(message_id))
                                .w_full()
                                .when(!first, |this| this.pt_6())
                                .children(row.map(|row| {
                                    this.render_timeline_message(
                                        thread_id, &row, &comments, wrap_width, window, cx,
                                    )
                                }))
                                .into_any_element()
                        })
                        .expect("the timeline owner is mounted")
                };
                (rows, Rc::new(build))
            }
            None => (
                Vec::new(),
                Rc::new(|_, _, _, _: &mut Window, _: &mut App| div().into_any_element()),
            ),
        };

        // Keep what the previous frame used, including rows built in its
        // prepaint, after this render.
        let previous_generation = self.render_generation.wrapping_sub(1);
        self.segment_text_views
            .retain(|_, text_view| text_view.rendered_at >= previous_generation);
        self.shown_segments
            .retain(|_, shown| shown.rendered_at >= previous_generation);

        let can_write = composer.is_some();
        let composer = composer.map(|composer| self.render_composer(composer, cx));
        let pending_rows = active_thread_id
            .map(|thread_id| self.render_pending_messages(thread_id, message_count, cx))
            .unwrap_or_default();
        // What follows the rows in the scroll flow, laid out natively.
        let tail = div()
            .w_full()
            .flex()
            .flex_col()
            .children(pending_rows)
            .children(composer.map(|composer| composer.when(message_count > 0, |this| this.mt_6())))
            .when(!can_write, |this| {
                this.child(
                    div()
                        .id("read-only-thread")
                        .when(message_count > 0, |this| this.mt_6())
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
            });

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
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _cx| {
                        // Window-level capture includes release outside the timeline
                        // and runs before a child can consume the pointer event.
                        let owner = pointer_owner.clone();
                        // A press alone changes no layout, so it doesn't
                        // redraw; starting or ending a drag switches history
                        // between culled and native layout.
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                            if phase.capture() && event.button == MouseButton::Left {
                                _ = owner.update(cx, |this, _| {
                                    this.timeline_press = Some(TimelinePress::new(event.position));
                                });
                            }
                        });
                        let owner = pointer_owner.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase.capture() && event.pressed_button == Some(MouseButton::Left) {
                                _ = owner.update(cx, |this, cx| {
                                    if let Some(press) = &mut this.timeline_press
                                        && press.moved_to(event.position)
                                    {
                                        cx.notify();
                                    }
                                });
                            }
                        });
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase.capture() && event.button == MouseButton::Left {
                                _ = pointer_owner.update(cx, |this, cx| {
                                    if this
                                        .timeline_press
                                        .take()
                                        .is_some_and(|press| press.dragging)
                                    {
                                        cx.notify();
                                    }
                                });
                            }
                        });
                    },
                )
                .absolute(),
            )
            .child(
                div()
                    .id("timeline-scroll")
                    .debug_selector(|| "timeline-scroll".to_owned())
                    .track_focus(&self.timeline_focus_handle)
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.timeline_scroll_handle)
                    // Keep the focus scope for Copy, but let message rows
                    // and editors take focus themselves. The scroll area's
                    // blank space must not take it away from the composer.
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_scroll(cx.listener(Self::timeline_scrolled))
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
                            .child(VirtualTimeline::new(
                                self.timeline_layout.clone(),
                                active_thread_id,
                                rows,
                                build,
                                tail.into_any_element(),
                                self.timeline_selection.clone(),
                                self.timeline_scroll_handle.clone(),
                                cx.entity().downgrade(),
                            )),
                    ),
            )
            .children(navigation)
    }
}

impl Render for Cowork {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.main_stage != MainStage::Thread {
            self.timeline_press = None;
        }
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
            .on_action(cx.listener(Self::queue_composer_action))
            .on_action(cx.listener(|this, _: &StopGeneration, _, cx| this.stop_generation(cx)))
            .on_action(cx.listener(|this, _: &OpenSearchPalette, window, cx| {
                this.open_search_palette(window, cx);
            }))
            .on_key_down(cx.listener(Self::begin_inline_comment))
            // Run before the focused editor's own handling, which they
            // extend to the composer as a whole.
            .capture_action(cx.listener(Self::paste_attachments))
            .capture_action(cx.listener(Self::copy_timeline_selection))
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
            // Closing the turn card is all Escape does while it shows.
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.dismiss_turn_card(cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.dismiss_turn_card(cx) || this.escape_empty_draft_item(window, cx) {
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
                                MainStage::Settings(page) => {
                                    this.child(self.render_settings_page(page, window, cx))
                                }
                                MainStage::Schedules => this.child(self.render_schedules_page(cx)),
                                MainStage::Welcome => this.child(self.render_welcome(window, cx)),
                                MainStage::ProviderSetup(provider) => {
                                    this.child(self.render_welcome_models(provider, window, cx))
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
    // Keep both existing submit bindings, registering the native one last so
    // shortcut tooltips show Command on macOS and Control elsewhere.
    let (alternate_submit, native_submit, alternate_queue, native_queue) = cfg_select! {
        target_os = "macos" => ("ctrl-enter", "cmd-enter", "ctrl-shift-enter", "cmd-shift-enter"),
        _ => ("cmd-enter", "ctrl-enter", "cmd-shift-enter", "ctrl-shift-enter"),
    };
    cx.bind_keys([
        KeyBinding::new(alternate_submit, SubmitComposer, None),
        KeyBinding::new(native_submit, SubmitComposer, None),
        KeyBinding::new(alternate_queue, QueueComposer, None),
        KeyBinding::new(native_queue, QueueComposer, None),
        KeyBinding::new(
            cfg_select! {
                target_os = "macos" => "cmd-.",
                _ => "ctrl-.",
            },
            StopGeneration,
            None,
        ),
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

/// What every window of the app needs before it opens: components, the
/// theme, and quitting with cmd-q on macOS.
fn init_app(cx: &mut App) {
    gpui_component::init(cx);
    theme::init(cx);
    #[cfg(target_os = "macos")]
    {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
    }
}

/// Runs the app until its last window closes.
pub fn run() -> anyhow::Result<()> {
    // Run by the user's login shell to read its environment for the agent's
    // commands; see `sandbox::environment`.
    if std::env::args().nth(1).as_deref() == Some(sandbox::PRINT_ENV_FLAG) {
        sandbox::print_env();
        return Ok(());
    }
    let ephemeral = std::env::args_os().skip(1).any(|arg| arg == "--ephemeral");
    // Own the persistent store before starting any services or reading other
    // saved state. Only an explicit --ephemeral launch may skip the lock.
    let (root, mut statistics_file, mut schedule_file, mut settings_file, mut projects_file) =
        if ephemeral {
            (None, None, None, None, None)
        } else {
            (
                Some(thread_files::threads_dir().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cannot find the home directory. Use --ephemeral to run without saving."
                    )
                })?),
                statistics::statistics_file(),
                scheduler::schedule_file(),
                settings_file::settings_file(),
                projects::projects_file(),
            )
        };
    let (thread_files, saved_threads) = thread_files::ThreadFiles::open(root)?;
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
        thread_files.root().to_owned(),
        command_environment(),
    ));
    let mut thread_files = Some((thread_files, saved_threads));

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            init_app(cx);
            bind_app_keys(cx);
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
                        timeline_press: None,
                        timeline_layout: Default::default(),
                        turn_rail: Default::default(),
                        timeline_selection: Default::default(),
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
                        stage_before_settings: MainStage::Welcome,
                        selected_provider: None,
                        settings_provider_step: ProviderStep::default(),
                        providers: Default::default(),
                        api_key_input: None,
                        context_settings: context_settings::ContextSettings::default(),
                        profile: Profile::local(local_participant_id),
                        shown_profiles: HashMap::new(),
                        profile_error: None,
                        profile_name_subscription: None,
                        tokio_handle,
                        sandboxes,
                        active_generations: HashMap::new(),
                        onboarding_intro: None,
                        agents: HashMap::new(),
                        deleted: Default::default(),
                        profile_statistics: None,
                        statistics_file: None,
                        thread_files: None,
                        scheduler: Default::default(),
                        activity_range: ActivityRange::default(),
                        local_participant_id,
                        typing_in: None,
                        published_presence: HashMap::new(),
                        caret_label_refresh: None,
                        working_refresh: None,
                        new_thread_model: None,
                        settings_file: None,
                        new_thread_project_folders: Vec::new(),
                        new_thread_project_mode: ProjectMode::default(),
                        new_thread_project: None,
                        projects: projects::Projects::load(projects_file.take()),
                        model_picker,
                        models: Arc::default(),
                        picker_rows: None,
                        _model_picker_subscription: model_picker_subscription,
                        _window_activation_subscription: window_activation_subscription,
                    }
                });
                let window_handle = window.window_handle();
                cowork.update(cx, |cowork, cx| {
                    if let Some((files, saved)) = thread_files.take() {
                        cowork.start_thread_files(files, saved, cx);
                    }
                    cowork.start_statistics(statistics_file.take());
                    cowork.start_settings(settings_file.take(), cx);
                    cowork.start_scheduler(schedule_file.take(), Some(window_handle), cx);
                    cowork.open_start_stage(window, cx);
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

/// Where the agent's commands get their environment: an app started from a
/// terminal already has the user's shell environment, while one started from
/// the Dock or a launcher, whose standard output is not a terminal, reads it
/// from a login shell.
fn command_environment() -> sandbox::EnvironmentSource {
    use std::io::IsTerminal as _;
    if cfg!(unix) && !std::io::stdout().is_terminal() {
        sandbox::EnvironmentSource::LoginShell
    } else {
        sandbox::EnvironmentSource::Process
    }
}

#[cfg(test)]
mod tests;
