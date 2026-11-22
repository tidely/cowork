//! The Cowork app as a library, so `src/main.rs` runs it with [`run`] and
//! examples can reach its components.

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
mod avatar_card;
mod avatars;
mod caret;
mod chat_db;
mod chat_meta;
mod chat_store;
#[doc(hidden)]
pub mod component_preview;
mod composer;
mod composer_attachments;
pub mod share_preview;

mod draft_editing;
mod file_change_view;

mod highlight;
mod inline_icons;

mod context_settings;
mod create_project;
mod model_picker;
mod models;
mod opening;
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
mod saved_settings;
mod scheduled_task;
mod scheduler;
mod schedules;
mod search_palette;
mod settings;

mod sidebar;
mod statistics;

#[cfg(test)]
mod test_support;
mod theme;
mod thread;
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
mod unload;
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
        OpenSearchPalette,
        NewChat,
        OpenSettings,
        JoinSharedThread,
        OpenScheduled,
        ArchiveChat
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainStage {
    Welcome,
    ProviderSetup(models::ModelProvider),
    Thread,
    /// Standing in for a chat that is slow to load or failed to; see
    /// `opening.rs`.
    Opening,
    /// A settings page, with the sidebar showing settings; see `settings.rs`.
    Settings(SettingsPage),
    Schedules,
}

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    recents_shown: usize,
    new_thread_draft: ThreadDraft,
    attachment_errors: Vec<AttachmentError>,
    pending_attachments: Vec<PendingAttachment>,
    timeline_scroll_handle: ScrollHandle,
    /// A pressed pointer can anchor a drag before GPUI reports a nonempty
    /// selection; see [`TimelinePress`].
    timeline_press: Option<TimelinePress>,
    timeline_layout: Rc<std::cell::RefCell<TimelineLayout>>,
    timeline_selection: Rc<std::cell::RefCell<timeline_selection::TimelineSelection>>,
    /// The conversation navigation rail's local state.
    turn_rail: turn_navigation::TurnRail,
    /// Taken when someone presses on a user message, as agent text takes
    /// focus itself, so that Copy reaches the window's text selection
    /// instead of an editor with nothing selected.
    timeline_focus_handle: gpui::FocusHandle,
    follow_generation: bool,
    thread_store: Entity<ThreadStore>,
    selection_message_id: Option<Uuid>,
    segment_text_views: HashMap<(ThreadMessageId, usize), SegmentTextView>,
    shown_segments: HashMap<ThreadMessageId, ShownSegments>,
    render_generation: u64,
    copied_endpoint_id: Option<Uuid>,
    join_dialog: Option<Entity<JoinDialog>>,
    main_stage: MainStage,
    stage_before_settings: MainStage,
    opening_stage: opening::OpeningStage,
    /// The provider card selected on the welcome screen or the Providers
    /// settings page, to continue to its models.
    selected_provider: Option<models::ModelProvider>,
    /// Looking for Ollama again while its setup step shows it isn't ready;
    /// see `Cowork::watch_ollama_setup`.
    ollama_watch: Option<gpui::Task<()>>,
    copied_pull_command: bool,
    settings_provider_step: ProviderStep,
    /// The connections to providers, with their API keys.
    providers: providers::Providers,
    /// The API key field of the provider whose models show, made when it
    /// first shows; see `provider_setup.rs`.
    api_key_input: Option<provider_setup::ApiKeyInput>,
    context_settings: context_settings::ContextSettings,
    profile: Profile,
    /// Everyone's profile as the active thread shows them, refreshed at the
    /// start of every render; see [`Cowork::profiles_for`].
    shown_profiles: HashMap<ParticipantId, Profile>,
    /// This view, for cards built away from its render, such as tooltips.
    weak: gpui::WeakEntity<Self>,
    profile_error: Option<SharedString>,
    profile_name_subscription: Option<Subscription>,
    tokio_handle: tokio::runtime::Handle,
    /// Where the agent's commands run, with each hosted thread's project.
    sandboxes: Arc<sandbox::Sandboxes>,
    active_generations: HashMap<Uuid, ActiveGeneration>,
    /// The first chat's greeting while it streams in; see
    /// `thread/onboarding.rs`.
    onboarding_intro: Option<thread::OnboardingIntro>,
    agents: HashMap<Uuid, ThreadAgent>,
    /// What the profile's statistics still count of the user's deleted
    /// threads besides their runs' usage; the rest they count from the
    /// threads themselves. See [`Cowork::delete_archived_thread`] and
    /// `statistics.rs`.
    deleted: statistics::DeletedChats,
    /// The usage of every run of the user's own chats, deleted ones
    /// included; see `statistics.rs`.
    token_activity: Vec<usage::TokenActivity>,
    /// Lazily rebuilt on Profile, not on unrelated streaming redraws.
    profile_statistics: Option<statistics::ProfileStatistics>,
    /// What the search palette found in unloaded chats; see `search_palette.rs`.
    saved_chat_search: search_palette::SavedChatSearch,
    /// Where the user's own threads and other persistent state are saved;
    /// `None` keeps them in memory, as in tests and ephemeral launches.
    chat_db: Option<chat_db::ChatDb>,
    /// The scheduled tasks, and the queue running them; see `scheduler.rs`.
    scheduler: scheduler::Scheduler,
    activity_range: ActivityRange,
    local_participant_id: ParticipantId,
    /// The draft editor the user is typing in, so the caret can move to the
    /// draft position when its item is removed instead of vanishing.
    typing_in: Option<(Uuid, EntityId, gpui::FocusHandle)>,
    published_presence: HashMap<Uuid, protocol::Presence>,
    caret_label_refresh: Option<gpui::Task<()>>,
    working_refresh: Option<gpui::Task<()>>,
    /// The model new threads start with: the last one selected locally,
    /// saved for the next launch; see [`Cowork::set_new_thread_model`].
    new_thread_model: Option<ModelRef>,
    /// The folders added before the thread exists, which move into it with
    /// the draft; see [`Thread::project_folders`].
    new_thread_project_folders: Vec<ProjectFolder>,
    /// The mode chosen before the thread exists, which moves into it with
    /// the folders; see [`Thread::project_mode`].
    new_thread_project_mode: ProjectMode,
    /// The sidebar project the thread about to be created is composed in; it
    /// is filed under it on creation.
    new_thread_project: Option<projects::ProjectId>,
    /// The sidebar's projects; see `projects.rs`.
    projects: projects::Projects,
    /// Always shows the active thread's model; see [`Cowork::sync_model_picker`].
    model_picker: Entity<ModelPickerState>,
    models: Arc<ModelCatalog>,
    picker_rows: Option<(Arc<ModelCatalog>, Option<ModelRef>)>,
    _model_picker_subscription: Subscription,
    _window_activation_subscription: Subscription,
    /// How long each loaded chat has been idle; see `unload.rs`.
    idle_clock: unload::IdleClock,
    _thread_store_subscription: Subscription,
}

impl Cowork {
    fn active_thread(&self, cx: &App) -> Option<Entity<Thread>> {
        self.thread_store.read(cx).active(cx)
    }

    fn active_thread_id(&self, cx: &App) -> Option<Uuid> {
        self.thread_store.read(cx).active_id()
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
            "New chat".into(),
            Vec::new(),
            draft,
            participant_id,
            models,
            model,
            cx,
        )
    }

    /// What a launch opens on, once saved threads and settings are loaded. A
    /// new user is onboarded; anyone else gets a new chat with the saved model
    /// or none yet. Models are discovered either way.
    fn open_start_stage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let discovered = self.start_providers(cx);
        if self.needs_onboarding(cx) {
            self.offer_onboarding(discovered, window, cx);
        } else {
            self.main_stage = MainStage::Thread;
            self.focus_composer(window, cx);
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
        let active_thread_id = self.active_thread_id(cx);
        // The thread is read in place, never copied whole: a row clones only
        // its own message, and only when it is built.
        let active_thread = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).loaded(thread_id, cx))
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
        let text_width = timeline_text_width(window, cx);
        let row_width = timeline_row_width(window, cx);
        let available_width = window.viewport_size().width - sidebar_width - px(82.);
        let wrap_width = available_width.max(px(120.)).min(text_width);
        // A selection's rows stay built while it lasts; see
        // `timeline_selection`.
        if self.timeline_press.is_none() && !gpui_base::TextSelection::has_selection(window, cx) {
            self.timeline_selection.borrow_mut().clear();
        }
        let comments = Rc::new(comments);
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
                        .child("Read-only chat")
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

/// The timeline's text column: 120 characters of its 14 px font, so it
/// stays that wide when the available stage is wider.
fn timeline_text_width(window: &Window, cx: &App) -> gpui::Pixels {
    let character = "0";
    let run = TextRun {
        len: character.len(),
        font: window.text_style().font(),
        color: cx.theme().foreground,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(character.into(), px(14.), &[run], None)
        .width()
        * 120.
}

/// A timeline row at its widest: the text column, with 40 px for the
/// avatar and 40 px on the right.
pub(crate) fn timeline_row_width(window: &Window, cx: &App) -> gpui::Pixels {
    timeline_text_width(window, cx) + px(80.)
}

impl Render for Cowork {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.main_stage != MainStage::Thread {
            self.timeline_press = None;
        }
        let opening = self.shown_opening(cx);
        if self.main_stage == MainStage::Opening && opening.is_none() {
            // Its chat was archived: back to the chat shown before.
            self.main_stage = MainStage::Thread;
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
            .on_action(cx.listener(|this, _: &NewChat, window, cx| this.start_new_chat(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_settings(SettingsPage::Profile, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JoinSharedThread, window, cx| {
                this.open_join_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenScheduled, window, cx| {
                this.open_schedules(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ArchiveChat, window, cx| {
                if let Some(thread_id) = this.active_thread_id(cx) {
                    this.request_archive_thread(thread_id, window, cx);
                }
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
                                MainStage::Opening => this.children(
                                    opening.map(|opening| self.render_opening(opening, window, cx)),
                                ),
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
        KeyBinding::new(
            cfg_select! {
                target_os = "macos" => "cmd-n",
                _ => "ctrl-n",
            },
            NewChat,
            None,
        ),
        KeyBinding::new(
            cfg_select! {
                target_os = "macos" => "cmd-,",
                _ => "ctrl-,",
            },
            OpenSettings,
            None,
        ),
        KeyBinding::new(
            cfg_select! {
                target_os = "macos" => "cmd-shift-a",
                _ => "ctrl-shift-a",
            },
            ArchiveChat,
            None,
        ),
    ]);
    // Every command is in the menu bar, as macOS expects, showing its
    // shortcut. Elsewhere the sidebar and the shortcuts reach them.
    #[cfg(target_os = "macos")]
    cx.set_menus([
        gpui::Menu::new("Cowork").items([
            gpui::MenuItem::action("Settings…", OpenSettings),
            gpui::MenuItem::separator(),
            gpui::MenuItem::action("Quit Cowork", Quit),
        ]),
        gpui::Menu::new("File").items([
            gpui::MenuItem::action("New Chat", NewChat),
            gpui::MenuItem::action("Join Shared Chat…", JoinSharedThread),
            gpui::MenuItem::separator(),
            gpui::MenuItem::action("Archive Chat", ArchiveChat),
        ]),
        gpui::Menu::new("Chat").items([
            gpui::MenuItem::action("Send", SubmitComposer),
            gpui::MenuItem::action("Queue", QueueComposer),
            gpui::MenuItem::action("Stop", StopGeneration),
        ]),
        gpui::Menu::new("Go").items([
            gpui::MenuItem::action("Search Chats…", OpenSearchPalette),
            gpui::MenuItem::action("Scheduled", OpenScheduled),
        ]),
    ]);
}

fn init_app(cx: &mut App) {
    gpui_component::init(cx);
    theme::init(cx);
    #[cfg(target_os = "macos")]
    {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
    }
}

/// The size the window opens at, in logical pixels.
pub(crate) const DEFAULT_WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(1200.), px(760.));

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
    let cowork_dir = if ephemeral {
        None
    } else {
        Some(chat_db::cowork_dir().ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot find the home directory. Use --ephemeral to run without saving."
            )
        })?)
    };
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
    // What is saved is read before the window opens, so a first launch is told
    // apart from one with chats before anything is shown.
    let (chat_db, scratch_root) = match cowork_dir {
        Some(dir) => {
            let (db, saved) = chat_db::ChatDb::open(dir, &tokio_handle)?;
            let root = db.root().to_owned();
            (Some((db, saved)), root)
        }
        None => (None, chat_db::ephemeral_root()),
    };
    let ephemeral_root = chat_db.is_none().then(|| scratch_root.clone());
    let sandboxes = Arc::new(sandbox::Sandboxes::new(scratch_root, command_environment()));
    let mut chat_db = chat_db;

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            init_app(cx);
            bind_app_keys(cx);
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    DEFAULT_WINDOW_SIZE,
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
                        recents_shown: sidebar::RECENTS_PAGE,
                        new_thread_draft,
                        attachment_errors: Vec::new(),
                        pending_attachments: Vec::new(),
                        timeline_scroll_handle: ScrollHandle::new(),
                        timeline_press: None,
                        timeline_layout: Default::default(),
                        turn_rail: Default::default(),
                        timeline_selection: Default::default(),
                        timeline_focus_handle: cx.focus_handle(),
                        weak: cx.weak_entity(),
                        follow_generation: true,
                        thread_store: thread_store.clone(),
                        selection_message_id: None,
                        segment_text_views: HashMap::new(),
                        shown_segments: HashMap::new(),
                        render_generation: 0,
                        copied_endpoint_id: None,
                        join_dialog: None,
                        main_stage: MainStage::Welcome,
                        stage_before_settings: MainStage::Welcome,
                        opening_stage: Default::default(),
                        selected_provider: None,
                        ollama_watch: None,
                        copied_pull_command: false,
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
                        token_activity: Vec::new(),
                        profile_statistics: None,
                        saved_chat_search: Default::default(),
                        chat_db: None,
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
                        new_thread_project: None,
                        projects: Default::default(),
                        model_picker,
                        models: Arc::default(),
                        picker_rows: None,
                        idle_clock: Default::default(),
                        _model_picker_subscription: model_picker_subscription,
                        _window_activation_subscription: window_activation_subscription,
                        _thread_store_subscription: Cowork::subscribe_thread_store(
                            &thread_store,
                            window,
                            cx,
                        ),
                    }
                });
                let window_handle = window.window_handle();
                cowork.update(cx, |cowork, cx| {
                    if let Some((db, saved)) = chat_db.take() {
                        cowork.start_chat_db(db, saved, cx);
                    }
                    cowork.start_scheduler(Some(window_handle), cx);
                    cowork.start_unload_sweep(cx);
                    cowork.open_start_stage(window, cx);
                });
                let root = cx.new(|cx| Root::new(cowork, window, cx));
                // App activation alone does not focus a window on every platform.
                window.activate_window();
                root
            }) {
                eprintln!("failed to open Cowork window: {error}");
                cx.quit();
                return;
            }

            if let Some(root) = ephemeral_root.clone() {
                // An ephemeral instance's scratch folders are for this run only.
                cx.on_app_quit(move |_| {
                    if let Err(error) = std::fs::remove_dir_all(&root)
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        eprintln!("could not remove {}: {error}", root.display());
                    }
                    async {}
                })
                .detach();
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
