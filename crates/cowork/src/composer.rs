//! The composer: the draft's blocks and comments, and the bottom bar
//! with the send button.

use std::{cell::Cell, rc::Rc, time::Duration};

use draft::{DraftItemKind, ItemId};
use gpui::{
    Animation, AnimationExt as _, App, Bounds, Context, Edges, Entity, ExternalPaths, Focusable,
    Hsla, IntoElement, MouseButton, Rgba, SharedString, Task, TextRun, Window, canvas, div,
    prelude::*, px, rems, rgb,
};
use gpui_base::{GlobalState, Textarea, input::TextareaState};
use gpui_component::{
    ActiveTheme, Disableable as _, Icon, Selectable as _, Sizable as _,
    attachment::Attachment,
    button::{Button, ButtonVariants as _},
    combobox::Combobox,
    kbd::Kbd,
    searchable_list::SearchableListItem,
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use crate::{
    Cowork, QueueComposer, StopGeneration, SubmitComposer,
    composer_attachments::{AttachmentCard, PendingAttachment},
    model_picker::{LanguageModel, UNAVAILABLE_MODEL_TOOLTIP},
    participant::ParticipantId,
    thread_draft::{AttachmentTarget, EditorSlot, ItemPresence, ThreadDraft},
    timeline::{UserComment, UserCommentBody},
    top_bar::TOP_BAR_HEIGHT,
};

const BOTTOM_BAR_DIVIDER_THRESHOLD: gpui::Pixels = px(24.);

/// Placeholder on the local user's empty trailing draft line.
pub(crate) const DRAFT_PLACEHOLDER: &str = "Write a message";

/// How long the send-key cap takes to fade from primary back to its resting colors.
pub(crate) const SEND_KEY_FADE: Duration = Duration::from_millis(1_200);

/// How long the join/leave label stays beside the cap.
pub(crate) const SEND_KEY_CUE: Duration = Duration::from_secs(3);

const SEND_KEY_GAP: gpui::Pixels = px(8.);
const SEND_KEY_PAD_X: gpui::Pixels = px(6.);
const SEND_KEY_PAD_Y: gpui::Pixels = px(2.);
const SEND_KEY_BORDER: gpui::Pixels = px(1.);

/// Which key submits, following [`Cowork::enter_sends_draft`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendKey {
    /// Enter sends. Nobody else is in the chat.
    Enter,
    /// Cmd-Enter or Ctrl-Enter sends. Someone else is here.
    ModifierEnter,
}

impl SendKey {
    pub(crate) fn from_enter_sends(enter_sends: bool) -> Self {
        if enter_sends {
            Self::Enter
        } else {
            Self::ModifierEnter
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Enter => "↵",
            Self::ModifierEnter => {
                cfg_select! {
                    target_os = "macos" => "⌘↵",
                    _ => "Ctrl ↵",
                }
            }
        }
    }
}

/// What the cap's tooltip names. The glyph stays [`SendKey`]; generating only
/// changes the word, because Enter and Cmd/Ctrl-Enter steer a running agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendAction {
    Send(SendKey),
    Steer(SendKey),
}

impl SendAction {
    pub(crate) fn new(enter_sends: bool, generating: bool) -> Self {
        let key = SendKey::from_enter_sends(enter_sends);
        if generating {
            Self::Steer(key)
        } else {
            Self::Send(key)
        }
    }

    pub(crate) fn key(self) -> SendKey {
        match self {
            Self::Send(key) | Self::Steer(key) => key,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        self.key().label()
    }

    pub(crate) fn tooltip(self) -> &'static str {
        match self {
            Self::Send(_) => "Send message",
            Self::Steer(_) => "Steer",
        }
    }
}

/// The label shown when a collaborator joining or leaving changes [`SendKey`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendKeyCue {
    OthersHere,
    JustYou,
}

impl SendKeyCue {
    fn from_key(key: SendKey) -> Self {
        match key {
            SendKey::Enter => Self::JustYou,
            SendKey::ModifierEnter => Self::OthersHere,
        }
    }

    pub(crate) fn text(self) -> String {
        match self {
            Self::OthersHere => {
                format!("Others are here: {} sends", SendKey::ModifierEnter.label())
            }
            Self::JustYou => format!("Just you: {} sends", SendKey::Enter.label()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SendKeyWatch {
    draft_id: Uuid,
    key: SendKey,
}

/// The cap's pulse. The timer is stored so a new join or leave drops it
/// instead of leaving a detached task that would clear the replacement.
enum SendKeyPulse {
    Settled,
    Playing {
        draft_id: Uuid,
        cue: SendKeyCue,
        generation: u64,
        /// Held so a newer pulse drops this timer instead of leaving it running.
        _timer: Task<()>,
    },
}

impl SendKeyPulse {
    fn playing(&self, draft_id: Uuid) -> Option<(SendKeyCue, u64)> {
        match self {
            Self::Playing {
                draft_id: pulsing,
                cue,
                generation,
                ..
            } if *pulsing == draft_id => Some((*cue, *generation)),
            _ => None,
        }
    }
}

pub(crate) struct DraftAffordance {
    watch: Option<SendKeyWatch>,
    pulse: SendKeyPulse,
    /// Increments on every pulse, so a replacement is distinct from the one it drops.
    pulse_generation: u64,
}

impl Default for DraftAffordance {
    fn default() -> Self {
        Self {
            watch: None,
            pulse: SendKeyPulse::Settled,
            pulse_generation: 0,
        }
    }
}

pub(crate) struct ComposerModel {
    pub(crate) draft_id: Uuid,
    pub(crate) comments: Vec<UserComment>,
    pub(crate) comments_folded: bool,
    pub(crate) blocks: Vec<ComposerBlock>,
    pub(crate) draft_position: Option<Entity<TextareaState>>,
    draft_row_visible: bool,
    /// The avatar the draft position row leads with, and those layered on it.
    draft_position_people: (ParticipantId, Vec<ParticipantId>),
    draft_position_presence: ItemPresence,
    /// Files being read, by the block they will land in; `None` is the draft
    /// position.
    pub(crate) pending: Vec<(Option<ItemId>, Attachment)>,
}

pub(crate) struct ComposerBlock {
    pub(crate) id: ItemId,
    pub(crate) creator: ParticipantId,
    pub(crate) presence: ItemPresence,
    pub(crate) editor: Entity<TextareaState>,
    pub(crate) attachments: Vec<AttachmentCard>,
}

impl Cowork {
    /// Check the entire collaborative draft, including comments and attachment
    /// only blocks; the focused editor is not necessarily the submission.
    pub(crate) fn composer_has_content(&self, cx: &App) -> bool {
        self.readable_draft_id(cx)
            .and_then(|draft_id| {
                self.read_draft(draft_id, cx, |draft| {
                    draft.items().iter().any(|item| !item.is_empty())
                })
            })
            .unwrap_or(false)
    }

    fn render_composer_action(
        &self,
        generating: bool,
        has_content: bool,
        resume_pending: bool,
        disabled_reason: Option<&'static str>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        if generating && !has_content {
            return div()
                .id("stop-generation-tooltip")
                .tooltip(|window, cx| {
                    Tooltip::new("Stop generating")
                        .action(&StopGeneration, None)
                        .build(window, cx)
                })
                .child(
                    Button::new("stop-generation")
                        .debug_selector(|| "stop-generation".to_owned())
                        .icon(Icon::new(AssetIconName::Square))
                        .danger()
                        .small()
                        .accessibility_label("Stop generating")
                        .on_click(cx.listener(Self::composer_button_clicked)),
                )
                .into_any_element();
        }
        let ready = disabled_reason.is_none();
        let enter_sends = self
            .readable_draft_id(cx)
            .is_none_or(|draft_id| self.enter_sends_draft(draft_id, cx));
        div()
            .id("send-message-tooltip")
            .tooltip(move |window, cx| {
                if let Some(reason) = disabled_reason {
                    return Tooltip::new(reason).build(window, cx);
                }
                Tooltip::element(move |window, cx| {
                    let mut content = div().py_1().flex().flex_col().gap_2().child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_6()
                            .child(if generating {
                                "Steer"
                            } else if resume_pending {
                                "Resume messages"
                            } else {
                                "Send message"
                            })
                            // Enter in the draft sends, which no key binding
                            // names, except while others are in the chat.
                            .children(if enter_sends {
                                gpui::Keystroke::parse("enter").ok().map(Kbd::new)
                            } else {
                                Kbd::binding_for_action(&SubmitComposer, None, window)
                            }),
                    );
                    if generating {
                        content = content
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_6()
                                    .child("Queue")
                                    .children(Kbd::binding_for_action(
                                        &QueueComposer,
                                        None,
                                        window,
                                    )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Click to steer · Shift-click to queue"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_6()
                                    .child("Stop generating")
                                    .children(Kbd::binding_for_action(
                                        &StopGeneration,
                                        None,
                                        window,
                                    )),
                            );
                    }
                    content
                })
                .build(window, cx)
            })
            .child(
                Button::new("send-message")
                    .debug_selector(move || {
                        if generating {
                            "steer-message"
                        } else if resume_pending {
                            "resume-messages"
                        } else {
                            "send-message"
                        }
                        .to_owned()
                    })
                    .icon(Icon::new(if generating {
                        AssetIconName::CornerDownLeft
                    } else if resume_pending {
                        AssetIconName::Play
                    } else {
                        AssetIconName::SendHorizontal
                    }))
                    .when(resume_pending, |button| button.label("Resume messages"))
                    .small()
                    .disabled(!ready)
                    .when(ready, |button| {
                        button.bg(cx.theme().accent).text_color(cx.theme().primary)
                    })
                    .accessibility_label(if generating {
                        "Steer message; Shift-click to queue"
                    } else if resume_pending {
                        "Resume messages"
                    } else {
                        "Send message"
                    })
                    .on_click(cx.listener(Self::composer_button_clicked)),
            )
            .into_any_element()
    }

    pub(crate) fn render_composer_comment(
        &self,
        comment: &UserComment,
        cx: &App,
    ) -> gpui::AnyElement {
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(cx.theme().secondary_foreground)
                .child(body.clone())
                .into_any_element(),
            UserCommentBody::Editing { composer, .. } => div()
                .id(format!("comment-editor-composer-{}", comment.id))
                .relative()
                .flex_1()
                .min_w_0()
                .child(Textarea::new(composer))
                .child(self.render_remote_carets(composer, comment.presence.carets.clone()))
                .into_any_element(),
        };

        div()
            .id(format!("composer-comment-{}", comment.id))
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .gap_1()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(
                div()
                    .w_full()
                    .px_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .line_clamp(2)
                    .child(comment.reference.quote.clone()),
            )
            .child(
                div().w_full().child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_sm()
                        .bg(cx.theme().popover)
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_1p5()
                                .py_1()
                                .border_l_2()
                                .border_color(rgb(self.color_of(comment.author)))
                                .child(self.render_layered_avatars(
                                    comment.author,
                                    &comment.presence.editors,
                                    comment.id,
                                    cx,
                                ))
                                .child(body),
                        ),
                ),
            )
            .into_any_element()
    }

    pub(crate) fn render_bottom_bar(
        &self,
        composer: Option<Entity<TextareaState>>,
        read_only_line_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_scroll_handle = self.timeline_scroll_handle.clone();
        let active_thread = self.active_thread(cx);
        let can_control = active_thread
            .as_ref()
            .is_none_or(|thread| thread.read(cx).can_control_generation());
        let can_change_model = active_thread
            .as_ref()
            .is_none_or(|thread| thread.read(cx).can_change_model());

        let loading_attachments = self
            .writable_draft_id(cx)
            .is_some_and(|draft_id| self.draft_is_loading_attachments(draft_id, cx));
        let generating = active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).is_generating());
        let active_model = self.active_model(cx);
        let model_unavailable = active_model
            .as_ref()
            .is_some_and(|model| !self.active_catalog(cx).contains(model));
        let has_model = active_model.is_some() && !model_unavailable;
        let context_indicator =
            has_model.then(|| self.render_context_indicator(active_thread.as_ref(), cx));

        let model_picker = div()
            .id("model-picker-container")
            .debug_selector(|| "model-picker".to_owned())
            .w_auto()
            .min_w_0()
            .h(px(28.))
            .mt(px(3.))
            .flex_none()
            .flex()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .child(
                Combobox::new(&self.model_picker)
                    .disabled(!can_change_model)
                    .search_placeholder("Search models…")
                    .menu_width(px(360.))
                    .menu_max_h(rems(24.))
                    .appearance(false)
                    .small()
                    .p_0()
                    .render_trigger(move |trigger, _, cx| {
                        let selected_model = trigger.selection().first().map(|(_, model)| model);
                        let title = selected_model
                            .map(LanguageModel::title)
                            .unwrap_or_else(|| "Select a model…".into());
                        let provider = selected_model.map(|model| model.model.provider);
                        let unavailable = selected_model.is_some_and(|model| !model.available);

                        div().h_full().flex().items_center().child(
                            Button::new("model-picker-trigger")
                                .debug_selector(|| "model-picker-trigger".to_owned())
                                .ghost()
                                .small()
                                .disabled(!can_change_model)
                                .selected(trigger.is_open())
                                .accessibility_label(title.clone())
                                .flex_none()
                                .px_1p5()
                                .when(unavailable, |this| this.tooltip(UNAVAILABLE_MODEL_TOOLTIP))
                                .when(!can_change_model, |this| {
                                    this.tooltip("Your access to this chat doesn't include changing the model")
                                })
                                .when_some(provider, |this, provider| {
                                    this.child(
                                        provider
                                            .icon(px(16.), cx)
                                            .when(unavailable, |this| this.opacity(0.5)),
                                    )
                                })
                                .child(
                                    div()
                                        .when(unavailable, |this| {
                                            this.text_color(
                                                cx.theme().muted_foreground.opacity(0.7),
                                            )
                                        })
                                        .child(title),
                                ),
                        )
                    }),
            );
        let has_content = self.composer_has_content(cx);
        let resume_pending = !generating
            && !has_content
            && active_thread
                .as_ref()
                .is_some_and(|thread| !thread.read(cx).pending_submissions().is_empty());
        let disabled_reason = if model_unavailable {
            Some("The selected model is unavailable")
        } else if !has_model {
            Some("Select a model to send")
        } else if loading_attachments {
            Some("Waiting for attachments")
        } else if !has_content && !resume_pending {
            Some("Write a message to send")
        } else {
            None
        };
        let button = can_control.then(|| {
            self.render_composer_action(
                generating,
                has_content,
                resume_pending,
                disabled_reason,
                cx,
            )
        });

        div()
            .id("bottom-bar")
            .debug_selector(|| "bottom-bar".to_owned())
            .relative()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .px_2()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let content_bottom = if let Some(composer) = &composer {
                            let composer = composer.read(cx);
                            let text_end = composer.value().len();
                            composer
                                .range_to_bounds(&(text_end..text_end))
                                .map(|bounds| bounds.bottom())
                        } else {
                            read_only_line_bounds.get().map(|bounds| bounds.bottom())
                        };
                        let Some(content_bottom) = content_bottom else {
                            return;
                        };
                        let scroll_offset = timeline_scroll_handle.offset().y;
                        let max_scroll_offset = timeline_scroll_handle.max_offset().y;
                        let is_scrolled_to_bottom = max_scroll_offset > px(0.)
                            && scroll_offset <= -max_scroll_offset + px(1.);
                        let divider_visible = !is_scrolled_to_bottom
                            && content_bottom >= bounds.top() - BOTTOM_BAR_DIVIDER_THRESHOLD;

                        if divider_visible {
                            window.paint_quad(gpui::fill(bounds, cx.theme().border));
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(1.)),
            )
            .child(self.render_project_folders(cx))
            .child(div().flex_1())
            .children(context_indicator)
            .child(self.render_project_mode(cx))
            .child(model_picker)
            .children(button)
    }

    pub(crate) fn render_composer_input(
        composer: &Entity<TextareaState>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(("composer", composer.entity_id()))
            .debug_selector(|| "composer".to_owned())
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(Textarea::new(composer))
    }

    /// A composer editor. The last one is tall, so there is room to click
    /// into, and it stays that tall when typing turns the draft position
    /// into a block.
    fn render_composer_editor(
        editor: &Entity<TextareaState>,
        last: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        Self::render_composer_input(editor)
            .when(last, |this| this.min_h(px(110.)))
            .on_click({
                let editor = editor.clone();
                cx.listener(move |_, _, window, cx| {
                    editor.focus_handle(cx).focus(window, cx);
                })
            })
    }

    /// Whether the draft position is shown below the prompt blocks: always
    /// when there are none, otherwise only while someone is there or files
    /// are being read for a new block.
    fn draft_row_visible(&self, draft: &ThreadDraft, window: &Window, cx: &App) -> bool {
        !draft.items().iter().any(|item| item.is_prompt())
            || draft
                .draft_position
                .as_ref()
                .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
            || !draft.others_at_draft_position(&[]).is_empty()
            || self
                .pending_reads(draft)
                .iter()
                .any(|pending| draft.pending_block(pending.target).is_none())
    }

    pub(crate) fn pending_reads(&self, draft: &ThreadDraft) -> Vec<PendingAttachment> {
        let local = self
            .pending_attachments
            .iter()
            .filter(|pending| pending.draft_id == draft.id)
            .cloned();
        let remote = draft
            .presence
            .iter()
            .filter(|(participant, _)| **participant != draft.author)
            .flat_map(|(_, (presence, _))| &presence.pending_reads)
            .map(|read| PendingAttachment {
                id: Uuid::from_bytes(read.id),
                draft_id: draft.id,
                target: match read.block {
                    Some(block) => {
                        AttachmentTarget::Block(ItemId::from_uuid(Uuid::from_bytes(block)))
                    }
                    // Never matches a batch, so it is shown at the draft
                    // position like the local user's reads for new blocks.
                    None => AttachmentTarget::NewBlock(Uuid::nil()),
                },
                name: read.name.clone(),
                is_image: read.is_image,
                progress: read.progress.map(f32::from),
            });
        local.chain(remote).collect()
    }

    /// Whose avatars the draft position row shows: everyone while the draft
    /// has no blocks, otherwise those at it. The local user comes first when
    /// included, and stands alone in an unshared thread.
    fn draft_position_people(
        draft: &ThreadDraft,
        participants: &[ParticipantId],
        window: &Window,
        cx: &App,
    ) -> (ParticipantId, Vec<ParticipantId>) {
        let local_there = draft
            .draft_position
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
        let others = if draft.items().iter().any(|item| item.is_prompt()) {
            draft.others_at_draft_position(participants)
        } else {
            participants
                .iter()
                .copied()
                .filter(|participant| *participant != draft.author)
                .collect()
        };
        let local_included = local_there
            || participants.is_empty()
            || !draft.items().iter().any(|item| item.is_prompt());
        match (local_included, others.split_first()) {
            (false, Some((first, rest))) => (*first, rest.to_vec()),
            _ => (draft.author, others),
        }
    }

    fn draft_participants(&self, draft_id: Uuid, cx: &App) -> Vec<ParticipantId> {
        self.thread_store
            .read(cx)
            .loaded_recents(cx)
            .map(|thread| thread.read(cx))
            .find(|thread| thread.draft().id == draft_id)
            .map(|thread| thread.participants().to_vec())
            .unwrap_or_default()
    }

    pub(crate) fn last_composer_editor(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<Entity<TextareaState>> {
        let draft_id = self.readable_draft_id(cx)?;
        self.read_draft(draft_id, cx, |draft| {
            if self.draft_can_edit(draft_id, cx) && self.draft_row_visible(draft, window, cx) {
                return draft.draft_position.clone();
            }
            let last_block = draft.items().into_iter().rfind(|item| item.is_prompt())?;
            draft.editor(EditorSlot::Prompt(last_block.id))
        })?
    }

    /// The trailing editor the local user is composing in: the draft position
    /// when that row is showing, otherwise the last prompt block. Read-only
    /// peers get neither the placeholder nor the send-key cap.
    fn end_editor(
        &self,
        draft_id: Uuid,
        window: &Window,
        cx: &App,
    ) -> Option<Entity<TextareaState>> {
        if !self.draft_can_edit(draft_id, cx) {
            return None;
        }
        self.read_draft(draft_id, cx, |draft| {
            if self.draft_row_visible(draft, window, cx) {
                draft.draft_position.clone()
            } else {
                let last = draft.items().into_iter().rfind(|item| item.is_prompt())?;
                draft.editor(EditorSlot::Prompt(last.id))
            }
        })
        .flatten()
    }

    /// Keeps the placeholder string and the cap's right gutter in step with
    /// the draft, and starts the pulse when the send key changes.
    ///
    /// The placeholder string stays on the trailing editor while its buffer
    /// is non-empty, marked text included. The editor paints that string only
    /// for an empty buffer, so the last character disappearing shows it on
    /// that same frame.
    pub(crate) fn sync_draft_affordance(
        &mut self,
        draft_id: Uuid,
        enter_sends: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.observe_send_key(draft_id, enter_sends, cx);
        let end = self.end_editor(draft_id, window, cx);
        let end_id = end.as_ref().map(|editor| editor.entity_id());
        let gutter = end
            .as_ref()
            .map(|_| {
                send_key_chip_outer_width(SendKey::from_enter_sends(enter_sends).label(), window)
                    + SEND_KEY_GAP
            })
            .unwrap_or(px(0.));
        let editors = self
            .read_draft(draft_id, cx, |draft| {
                draft
                    .all_editors()
                    .map(|(_, editor)| editor.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for editor in editors {
            let on_end = Some(editor.entity_id()) == end_id;
            let desired = if on_end {
                SharedString::from(DRAFT_PLACEHOLDER)
            } else {
                SharedString::default()
            };
            let current = editor.read(cx).presentation().placeholder().clone();
            let right = if on_end { gutter } else { px(0.) };
            editor.update(cx, |state, cx| {
                state.set_editor_paddings(Edges {
                    top: px(0.),
                    right,
                    bottom: px(0.),
                    left: px(0.),
                });
                if current.as_ref() != desired.as_ref() {
                    state.set_placeholder(desired, window, cx);
                }
            });
        }
    }

    fn observe_send_key(&mut self, draft_id: Uuid, enter_sends: bool, cx: &mut Context<Self>) {
        let key = SendKey::from_enter_sends(enter_sends);
        if let Some(watch) = &self.draft_affordance.watch
            && watch.draft_id == draft_id
            && watch.key == key
        {
            return;
        }
        let changed = self
            .draft_affordance
            .watch
            .as_ref()
            .is_some_and(|watch| watch.draft_id == draft_id);
        self.draft_affordance.watch = Some(SendKeyWatch { draft_id, key });
        if changed {
            self.begin_send_key_pulse(draft_id, SendKeyCue::from_key(key), cx);
        } else if self.draft_affordance.pulse.playing(draft_id).is_none() {
            // A different draft: drop a pulse that belonged to the one we left.
            self.draft_affordance.pulse = SendKeyPulse::Settled;
        }
    }

    fn begin_send_key_pulse(&mut self, draft_id: Uuid, cue: SendKeyCue, cx: &mut Context<Self>) {
        self.draft_affordance.pulse_generation =
            self.draft_affordance.pulse_generation.wrapping_add(1);
        let generation = self.draft_affordance.pulse_generation;
        // Replacing the previous pulse drops its task.
        self.draft_affordance.pulse = SendKeyPulse::Playing {
            draft_id,
            cue,
            generation,
            _timer: cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SEND_KEY_CUE).await;
                _ = this.update(cx, |this, cx| {
                    let current = matches!(
                        &this.draft_affordance.pulse,
                        SendKeyPulse::Playing {
                            generation: playing,
                            ..
                        } if *playing == generation
                    );
                    if current {
                        this.draft_affordance.pulse = SendKeyPulse::Settled;
                        cx.notify();
                    }
                });
            }),
        };
    }

    /// The pulse currently showing for `draft_id`, if a join or leave changed
    /// its send key and the label is still up.
    pub(crate) fn send_key_pulse(&self, draft_id: Uuid) -> Option<(SendKeyCue, u64)> {
        self.draft_affordance.pulse.playing(draft_id)
    }

    pub(crate) fn composer_model(
        &self,
        draft_id: Uuid,
        window: &Window,
        cx: &App,
    ) -> Option<ComposerModel> {
        let participants = self.draft_participants(draft_id, cx);
        self.read_draft(draft_id, cx, |draft| {
            let blocks = draft
                .items()
                .into_iter()
                .filter_map(|item| {
                    let DraftItemKind::Prompt { attachments } = item.kind else {
                        return None;
                    };
                    let creator = ParticipantId::from_uuid(item.creator);
                    Some(ComposerBlock {
                        id: item.id,
                        creator,
                        presence: ItemPresence {
                            editors: draft
                                .editors_of(item.id, &participants)
                                .into_iter()
                                .filter(|editor| *editor != creator)
                                .collect(),
                            carets: draft.remote_carets(item.id, &participants),
                        },
                        editor: draft.editor(EditorSlot::Prompt(item.id))?,
                        attachments: attachments
                            .iter()
                            .map(|record| AttachmentCard::new(draft, record))
                            .collect(),
                    })
                })
                .collect();
            let pending = self
                .pending_reads(draft)
                .iter()
                .map(|pending| {
                    (
                        draft.pending_block(pending.target),
                        Self::render_pending_attachment(pending),
                    )
                })
                .collect();
            ComposerModel {
                draft_id,
                comments: draft.comment_views(&participants),
                comments_folded: draft.comments_folded,
                blocks,
                draft_position: self
                    .draft_can_edit(draft_id, cx)
                    .then(|| draft.draft_position.clone())
                    .flatten(),
                draft_row_visible: self.draft_row_visible(draft, window, cx)
                    && (self.draft_can_edit(draft_id, cx)
                        || !draft.others_at_draft_position(&participants).is_empty()
                        || self
                            .pending_reads(draft)
                            .iter()
                            .any(|pending| draft.pending_block(pending.target).is_none())),
                draft_position_people: Self::draft_position_people(
                    draft,
                    &participants,
                    window,
                    cx,
                ),
                draft_position_presence: ItemPresence {
                    editors: Vec::new(),
                    carets: draft.draft_position_carets(&participants),
                },
                pending,
            }
        })
    }

    /// The send-key cap, right-aligned on the first line. It is not a control:
    /// clicking it does not send and does not move the caret.
    fn render_send_key_chip(
        &self,
        draft_id: Uuid,
        editor: &Entity<TextareaState>,
        action: SendAction,
        window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        let resting_border = cx.theme().border;
        let resting_text = cx.theme().muted_foreground;
        let primary = cx.theme().primary;
        let label = action.label();
        let tooltip = action.tooltip();
        let enter_sends = matches!(action.key(), SendKey::Enter);
        let chip = div()
            .id("send-key-chip")
            .debug_selector(|| format!("send-key-chip:{label}:{tooltip}"))
            .flex_none()
            .px(SEND_KEY_PAD_X)
            .py(SEND_KEY_PAD_Y)
            .border_1()
            .rounded(px(4.))
            .text_xs()
            .whitespace_nowrap()
            .child(label)
            .tooltip(move |window, cx| {
                let binding = if enter_sends {
                    gpui::Keystroke::parse("enter").ok().map(Kbd::new)
                } else {
                    Kbd::binding_for_action(&SubmitComposer, None, window)
                };
                Tooltip::new(tooltip).key_binding(binding).build(window, cx)
            })
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            });
        let chip = if let Some((_, generation)) = self.send_key_pulse(draft_id) {
            chip.with_animation(
                ("send-key-pulse", generation),
                Animation::new(SEND_KEY_FADE),
                move |chip, t| {
                    chip.border_color(mix_color(primary, resting_border, t))
                        .text_color(mix_color(primary, resting_text, t))
                },
            )
            .into_any_element()
        } else {
            chip.border_color(resting_border)
                .text_color(resting_text)
                .into_any_element()
        };
        let cue = self.send_key_pulse(draft_id).map(|(cue, _)| {
            let text = cue.text();
            let selector_text = text.clone();
            div()
                .id("send-key-cue")
                .debug_selector(move || format!("send-key-cue:{selector_text}"))
                .flex_none()
                .px_2()
                .py_0p5()
                .text_sm()
                .whitespace_nowrap()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .text_color(cx.theme().popover_foreground)
                .shadow_md()
                .child(text.clone())
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
        });
        div()
            .absolute()
            .top_0()
            .right(SEND_KEY_GAP)
            .h(composer_line_height(editor.read(cx), window))
            .flex()
            .items_center()
            .gap(SEND_KEY_GAP)
            .children(cue)
            .child(chip)
            .into_any_element()
    }

    pub(crate) fn render_composer(
        &self,
        composer: ComposerModel,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let draft_id = composer.draft_id;
        let send_action = SendAction::new(
            self.enter_sends_draft(draft_id, cx),
            self.active_thread(cx)
                .is_some_and(|thread| thread.read(cx).is_generating()),
        );
        let end_id = self
            .end_editor(draft_id, window, cx)
            .map(|editor| editor.entity_id());
        let ComposerModel {
            draft_id: _,
            comments,
            comments_folded,
            blocks,
            draft_position,
            draft_row_visible,
            draft_position_people,
            draft_position_presence,
            mut pending,
        } = composer;
        let can_edit = self.draft_can_edit(draft_id, cx);
        let mut rows = Vec::new();
        if !comments.is_empty() {
            let mut content = vec![
                Self::render_comment_group_toggle(draft_id, comments.len(), comments_folded, cx)
                    .into_any_element(),
            ];
            if !comments_folded {
                content.extend(
                    comments
                        .iter()
                        .map(|comment| self.render_composer_comment(comment, cx)),
                );
            }
            rows.push(
                self.render_comment_group_row(&comments, content, cx)
                    .into_any_element(),
            );
        }

        let block_count = blocks.len();
        for (index, block) in blocks.into_iter().enumerate() {
            let last = !draft_row_visible && index + 1 == block_count;
            let block_pending = pending
                .extract_if(.., |(target, _)| *target == Some(block.id))
                .map(|(_, pending)| pending)
                .collect::<Vec<_>>();
            let mut content = Vec::new();
            if !block.attachments.is_empty() || !block_pending.is_empty() {
                content.push(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .children(block.attachments.iter().map(|attachment| {
                            self.render_attachment(
                                attachment,
                                Some((draft_id, block.id, attachment.id)),
                                cx,
                            )
                        }))
                        .children(block_pending)
                        .into_any_element(),
                );
            }
            let chip = (end_id == Some(block.editor.entity_id())).then(|| {
                self.render_send_key_chip(draft_id, &block.editor, send_action, window, cx)
            });
            content.push(
                Self::render_composer_editor(&block.editor, last, cx)
                    .child(self.render_remote_carets(&block.editor, block.presence.carets))
                    .children(chip)
                    .into_any_element(),
            );
            let block_id = block.id;
            rows.push(
                self.render_presence_row(
                    block.creator,
                    &block.presence.editors,
                    block_id.as_uuid(),
                    None,
                    content,
                    cx,
                )
                .can_drop(move |value, _, _| {
                    can_edit
                        && value
                            .downcast_ref::<ExternalPaths>()
                            .is_some_and(|paths| !paths.paths().is_empty())
                })
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                    this.drop_attachments_on(
                        draft_id,
                        AttachmentTarget::Block(block_id),
                        paths,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }))
                .into_any_element(),
            );
        }

        let errors = self
            .attachment_errors
            .iter()
            .filter(|error| error.draft_id == draft_id)
            .map(|error| {
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.message.clone())
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        if draft_row_visible {
            let mut content = Vec::new();
            if !pending.is_empty() {
                content.push(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .children(pending.into_iter().map(|(_, pending)| pending))
                        .into_any_element(),
                );
            }
            content.extend(errors);
            content.extend(draft_position.as_ref().map(|editor| {
                let chip = (end_id == Some(editor.entity_id()))
                    .then(|| self.render_send_key_chip(draft_id, editor, send_action, window, cx));
                Self::render_composer_editor(editor, true, cx)
                    .child(self.render_remote_carets(editor, draft_position_presence.carets))
                    .children(chip)
                    .into_any_element()
            }));
            let (primary, others) = draft_position_people;
            rows.push(
                // Nil: there is one draft position, unlike the items' ids.
                self.render_presence_row(primary, &others, Uuid::nil(), None, content, cx)
                    .into_any_element(),
            );
        } else if !errors.is_empty() {
            rows.push(self.render_message_row(None, errors).into_any_element());
        }

        div()
            .id("composer-area")
            .w_full()
            .flex_1()
            .flex()
            .flex_col()
            .child(div().w_full().flex().flex_col().gap_3().children(rows))
            .child(
                div()
                    .id("composer-empty-space")
                    .w_full()
                    .flex_1()
                    .min_h(px(24.))
                    .when(can_edit, |this| {
                        this.cursor_text()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if !this.draft_can_edit(draft_id, cx) {
                                    return;
                                }
                                this.focus_draft_editor(
                                    draft_id,
                                    EditorSlot::DraftPosition,
                                    None,
                                    window,
                                    cx,
                                );
                            }))
                    }),
            )
    }
}

fn composer_line_height(editor: &TextareaState, window: &Window) -> gpui::Pixels {
    editor.line_height().unwrap_or_else(|| {
        // The timeline column sets text_sm on the editor. Before the first
        // layout, match that size against the window's line-height ratio.
        let mut style = window.text_style();
        style.font_size = rems(0.875).into();
        style.line_height_in_pixels(window.rem_size())
    })
}

/// Text width at text_xs plus the cap's horizontal padding and border.
fn send_key_chip_outer_width(label: &str, window: &Window) -> gpui::Pixels {
    let mut style = window.text_style();
    style.font_size = rems(0.75).into();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let run = TextRun {
        len: label.len(),
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(label.into(), font_size, &[run], None)
        .width()
        + SEND_KEY_PAD_X * 2.
        + SEND_KEY_BORDER * 2.
}

fn mix_color(from: Hsla, to: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0., 1.);
    let from = Rgba::from(from);
    let to = Rgba::from(to);
    Hsla::from(Rgba {
        r: from.r + (to.r - from.r) * t,
        g: from.g + (to.g - from.g) * t,
        b: from.b + (to.b - from.b) * t,
        a: from.a + (to.a - from.a) * t,
    })
}
