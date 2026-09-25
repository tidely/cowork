//! The composer: the draft's blocks and comments, and the bottom bar
//! with the send button.

use std::{cell::Cell, rc::Rc};

use draft::{DraftItemKind, ItemId};
use gpui::{
    App, Bounds, Context, Entity, ExternalPaths, Focusable, IntoElement, MouseButton, TextRun,
    Window, canvas, div, img, prelude::*, px, rems, rgb,
};
use gpui_base::{GlobalState, Textarea, input::TextareaState};
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    attachment::Attachment,
    button::{Button, ButtonVariants as _},
    combobox::Combobox,
    searchable_list::SearchableListItem,
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use crate::{
    Cowork,
    composer_attachments::{AttachmentCard, PendingAttachment},
    model_picker::{LanguageModel, UNAVAILABLE_MODEL_TOOLTIP},
    participant::ParticipantId,
    thread_draft::{AttachmentTarget, EditorSlot, ItemPresence, ThreadDraft},
    timeline::{UserComment, UserCommentBody},
    top_bar::TOP_BAR_HEIGHT,
};

const BOTTOM_BAR_DIVIDER_THRESHOLD: gpui::Pixels = px(24.);

/// What the composer shows of a draft, read out of it for rendering.
pub(crate) struct ComposerModel {
    pub(crate) draft_id: Uuid,
    pub(crate) comments: Vec<UserComment>,
    pub(crate) comments_folded: bool,
    pub(crate) blocks: Vec<ComposerBlock>,
    pub(crate) draft_position: Option<Entity<TextareaState>>,
    draft_row_visible: bool,
    /// The avatar the draft position row leads with, and those layered on it.
    draft_position_people: (ParticipantId, Vec<ParticipantId>),
    /// Everyone else's carets at the draft position.
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
    pub(crate) fn render_composer_comment(&self, comment: &UserComment) -> gpui::AnyElement {
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(rgb(0xe4e4e7))
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
            .rounded_lg()
            .border_1()
            .border_color(rgb(0x303036))
            .bg(rgb(0x202023))
            .child(
                div()
                    .w_full()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .text_color(rgb(0xd4d4d8))
                    .line_clamp(2)
                    .child(comment.reference.quote.clone()),
            )
            .child(
                div().w_full().px_3().pb_3().child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .border_1()
                        .border_color(rgb(0x303036))
                        .rounded_md()
                        .bg(rgb(0x1d1d20))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .border_l_2()
                                .border_color(rgb(self.color_of(comment.author)))
                                .child(self.render_layered_avatars(
                                    comment.author,
                                    &comment.presence.editors,
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_scroll_handle = self.timeline_scroll_handle.clone();
        let can_write = composer.is_some();
        let active_thread = self.active_thread(cx);
        // Picking the model and stopping the agent stay available to
        // participants who cannot write to the draft.
        let can_control = can_write
            || active_thread
                .as_ref()
                .is_some_and(|thread| thread.read(cx).sharing.is_collaborating());

        let loading_attachments = self
            .writable_draft_id(cx)
            .is_some_and(|draft_id| self.draft_is_loading_attachments(draft_id, cx));
        let generating = active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).generating);
        let active_model = self.active_model(cx);
        let model_unavailable = active_model
            .as_ref()
            .is_some_and(|model| !self.active_catalog(cx).contains(model));
        let has_model = active_model.is_some() && !model_unavailable;
        let context_indicator =
            has_model.then(|| self.render_context_indicator(active_thread.as_ref(), cx));
        let selected_model_title = self
            .model_picker
            .read(cx)
            .selection()
            .first()
            .map(|(_, model)| model.title())
            .unwrap_or_else(|| "Select a model...".into());
        let title_run = TextRun {
            len: selected_model_title.len(),
            font: window.text_style().font(),
            color: rgb(0xd4d4d8).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let title_width = window
            .text_system()
            .shape_line(selected_model_title, px(14.), &[title_run], None)
            .width();
        // Icon + chevron + two gaps + button padding + Combobox's custom-trigger slot gap.
        let model_picker_width = title_width + px(62.);
        let model_picker_hovered = self.model_picker_hovered;
        let model_picker = div()
            .id("model-picker-container")
            .debug_selector(|| "model-picker".to_owned())
            .w(model_picker_width)
            .min_w_0()
            .h(px(28.))
            .mt(px(3.))
            .flex_none()
            .flex()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if this.model_picker_hovered != *hovered {
                    this.model_picker_hovered = *hovered;
                    cx.notify();
                }
            }))
            .child(
                Combobox::new(&self.model_picker)
                    .search_placeholder("Search models...")
                    .menu_width(px(360.))
                    .menu_max_h(rems(24.))
                    .appearance(false)
                    .small()
                    .p_0()
                    .render_trigger(move |trigger, _, _| {
                        let selected_model = trigger.selection().first().map(|(_, model)| model);
                        let title = selected_model
                            .map(LanguageModel::title)
                            .unwrap_or_else(|| "Select a model...".into());
                        let provider = selected_model.map(|model| model.model.provider);
                        let unavailable = selected_model.is_some_and(|model| !model.available);

                        div()
                            .h_full()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .justify_end()
                            .child(
                                div()
                                    .id("model-picker-trigger")
                                    .h_full()
                                    .max_w_full()
                                    .min_w_0()
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(model_picker_hovered, |this| this.bg(rgb(0x2d2d30)))
                                    .text_sm()
                                    .text_color(if unavailable {
                                        rgb(0x71717a)
                                    } else {
                                        rgb(0xd4d4d8)
                                    })
                                    .when(unavailable, |this| {
                                        this.tooltip(|window, cx| {
                                            Tooltip::new(UNAVAILABLE_MODEL_TOOLTIP)
                                                .build(window, cx)
                                        })
                                    })
                                    .when_some(provider, |this, provider| {
                                        this.child(
                                            img(provider.icon_path())
                                                .size(px(18.))
                                                .flex_none()
                                                .rounded(px(4.))
                                                .when(unavailable, |this| this.opacity(0.5)),
                                        )
                                    })
                                    .child(div().child(title))
                                    .child(
                                        Icon::new(if trigger.is_open() {
                                            AssetIconName::ChevronUp
                                        } else {
                                            AssetIconName::ChevronDown
                                        })
                                        .size_4()
                                        .flex_none()
                                        .text_color(rgb(0xa1a1aa)),
                                    ),
                            )
                    }),
            );
        let button = if generating {
            Some(
                Button::new("stop-generation")
                    .icon(Icon::new(AssetIconName::Square))
                    .danger()
                    .small()
                    .accessibility_label("Stop generating")
                    .tooltip("Stop generating")
                    .on_click(cx.listener(Self::composer_button_clicked)),
            )
        } else if can_write {
            Some(
                Button::new("send-message")
                    .icon(Icon::new(AssetIconName::SendHorizontal))
                    .small()
                    .accessibility_label(if model_unavailable {
                        "Send message (the selected model is unavailable)"
                    } else if !has_model {
                        "Send message (select a model first)"
                    } else if loading_attachments {
                        "Send message (waiting for attachments)"
                    } else {
                        "Send message"
                    })
                    .tooltip(if model_unavailable {
                        "The selected model is unavailable"
                    } else if !has_model {
                        "Select a model to send"
                    } else if loading_attachments {
                        "Waiting for attachments"
                    } else if cfg!(target_os = "macos") {
                        "Send message (Cmd-Enter)"
                    } else {
                        "Send message (Ctrl-Enter)"
                    })
                    .disabled(loading_attachments || !has_model)
                    .on_click(cx.listener(Self::composer_button_clicked)),
            )
        } else {
            None
        };

        div()
            .id("bottom-bar")
            .debug_selector(|| "bottom-bar".to_owned())
            .relative()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .border_l_1()
            .border_color(rgb(0x2d2d30))
            .bg(rgb(0x18181b))
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .px_3()
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
                            window.paint_quad(gpui::fill(bounds, rgb(0x2d2d30)));
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(1.)),
            )
            .when(can_write, |this| {
                this.child(
                    Button::new("add-attachment")
                        .icon(Icon::new(AssetIconName::Paperclip))
                        .ghost()
                        .small()
                        .accessibility_label("Attach files")
                        .tooltip("Attach files")
                        .on_click(cx.listener(Self::pick_attachments)),
                )
            })
            .when(can_control, |this| {
                this.child(div().flex_1())
                    .children(context_indicator)
                    .child(model_picker)
                    .children(button)
            })
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
        !draft.doc.items().iter().any(|item| item.is_prompt())
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

    /// Every file being read into the draft: the local user's, and those
    /// others announce in their presence.
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
        let others = if draft.doc.items().iter().any(|item| item.is_prompt()) {
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
            || !draft.doc.items().iter().any(|item| item.is_prompt());
        match (local_included, others.split_first()) {
            (false, Some((first, rest))) => (*first, rest.to_vec()),
            _ => (draft.author, others),
        }
    }

    /// The thread's participants in join order, for the draft `draft_id`.
    fn draft_participants(&self, draft_id: Uuid, cx: &App) -> Vec<ParticipantId> {
        self.thread_store
            .read(cx)
            .threads
            .iter()
            .map(|thread| thread.read(cx))
            .find(|thread| thread.draft.id == draft_id)
            .map(|thread| thread.participants.clone())
            .unwrap_or_default()
    }

    /// The bottom-most editor of the composer.
    pub(crate) fn last_composer_editor(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<Entity<TextareaState>> {
        let draft_id = self.writable_draft_id(cx)?;
        self.read_draft(draft_id, cx, |draft| {
            if self.draft_row_visible(draft, window, cx) {
                return draft.draft_position.clone();
            }
            let last_block = draft
                .doc
                .items()
                .into_iter()
                .rfind(|item| item.is_prompt())?;
            draft.editor(EditorSlot::Prompt(last_block.id))
        })?
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
                .doc
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
                draft_position: draft.draft_position.clone(),
                draft_row_visible: self.draft_row_visible(draft, window, cx),
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

    /// The composer: the draft's comments, one row per prompt block, and the
    /// draft position, followed by empty space that leads to the draft
    /// position when clicked.
    pub(crate) fn render_composer(
        &self,
        composer: ComposerModel,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let ComposerModel {
            draft_id,
            comments,
            comments_folded,
            blocks,
            draft_position,
            draft_row_visible,
            draft_position_people,
            draft_position_presence,
            mut pending,
        } = composer;
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
                        .map(|comment| self.render_composer_comment(comment)),
                );
            }
            rows.push(
                self.render_comment_group_row(&comments, content)
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
            content.push(
                Self::render_composer_editor(&block.editor, last, cx)
                    .child(self.render_remote_carets(&block.editor, block.presence.carets))
                    .into_any_element(),
            );
            let block_id = block.id;
            rows.push(
                self.render_presence_row(block.creator, &block.presence.editors, content)
                    .can_drop(|value, _, _| {
                        value
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
                    .text_color(rgb(0xf87171))
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
                Self::render_composer_editor(editor, true, cx)
                    .child(self.render_remote_carets(editor, draft_position_presence.carets))
                    .into_any_element()
            }));
            let (primary, others) = draft_position_people;
            rows.push(
                self.render_presence_row(primary, &others, content)
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
                    .cursor_text()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus_draft_editor(
                            draft_id,
                            EditorSlot::DraftPosition,
                            None,
                            window,
                            cx,
                        );
                    })),
            )
    }
}
