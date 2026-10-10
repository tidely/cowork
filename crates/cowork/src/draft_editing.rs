//! Editing a draft: the editors for its items, keeping them in sync with
//! the document, moving between them, and publishing presence.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use draft::{DraftItem, ItemId, TextEdit};
use gpui::{
    App, AppContext, Context, Entity, EntityInputHandler as _, Focusable, SharedString, Window,
};
use gpui_base::input::{InputEditorStyle, InputEvent, TextareaState};
use gpui_component::ActiveTheme;
use uuid::Uuid;

use crate::{
    Cowork,
    caret::{caret_x_on_edge_line, offset_near_x},
    protocol,
    thread::{EditDraft, Thread, ThreadSharing},
    thread_draft::{CARET_LABEL_DURATION, DraftEditorState, EditorSlot, ItemEditors, ThreadDraft},
};

impl Cowork {
    /// An auto-growing editor holding `text`, with the caret at its end.
    pub(crate) fn new_draft_editor(
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<TextareaState> {
        cx.new(|cx| {
            let mut editor = TextareaState::new(window, cx).auto_grow(1, usize::MAX);
            editor.set_editor_style(InputEditorStyle {
                caret: cx.theme().caret,
                ..Default::default()
            });
            if !text.is_empty() {
                editor.set_value(SharedString::from(text.to_owned()), window, cx);
                editor.set_selected_range(text.len()..text.len(), cx);
            }
            editor
        })
    }

    /// A draft editor whose input events are routed to the draft `draft_id`.
    fn new_routed_draft_editor(
        draft_id: Uuid,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let editor = Self::new_draft_editor(text, window, cx);
        // Ends by itself once the editor is dropped along with its item.
        cx.subscribe_in(&editor, window, move |this, editor, event, window, cx| {
            this.draft_editor_event(draft_id, editor, event, window, cx);
        })
        .detach();
        editor
    }

    /// Brings a draft's editors in line with its document: creates editors
    /// for new items and the draft position, drops those of removed items,
    /// and shows text changed by anything other than the editor itself.
    pub(crate) fn prepare_draft(
        &mut self,
        draft_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let can_edit = self.draft_can_edit(draft_id, cx);
        if !can_edit {
            self.cancel_draft_attachment_reads(draft_id, cx);
            if self
                .typing_in
                .as_ref()
                .is_some_and(|(id, _, _)| *id == draft_id)
            {
                self.typing_in = None;
            }
        }
        // Only when the removed editor still has focus: anything else the
        // user has moved to since keeps it.
        let focused = self
            .typing_in
            .as_ref()
            .filter(|(typing_draft, _, focus)| {
                *typing_draft == draft_id
                    && window.focused(cx).is_none_or(|focused| focused == *focus)
            })
            .map(|(_, editor, _)| *editor);
        let Some((missing, draft_position, shown, lost_focus)) =
            self.update_draft_editors(draft_id, cx, |items, draft| {
                draft
                    .editors
                    .retain(|id, _| items.iter().any(|item| item.id == *id));
                // The item being typed in is gone: submitted, or removed by
                // someone else.
                let lost_focus = focused.is_some_and(|editor| draft.slot_of(editor).is_none());
                let mut missing = Vec::new();
                let mut shown = Vec::new();
                for item in items {
                    match draft.editors.get(&item.id) {
                        Some(editors) => shown.extend(
                            editors
                                .all()
                                .into_iter()
                                .map(|editor| (editor.clone(), item.body.clone())),
                        ),
                        None => missing.push(item.clone()),
                    }
                }
                (missing, draft.draft_position.clone(), shown, lost_focus)
            })
        else {
            return;
        };
        if lost_focus {
            self.typing_in = None;
        }

        // Existing editors outlive theme changes; refresh their projected
        // caret style without touching text, selection, or collaboration state.
        let style = InputEditorStyle {
            caret: cx.theme().caret,
            ..Default::default()
        };
        let restore_position = !can_edit
            || draft_position
                .as_ref()
                .is_some_and(|editor| !editor.read(cx).is_editable());
        for editor in shown
            .iter()
            .map(|(editor, _)| editor)
            .chain(draft_position.iter())
        {
            editor.update(cx, |editor, cx| {
                editor.set_editor_style(style.clone());
                // A grant must not commit text or IME left in a readonly cache.
                if !can_edit || !editor.is_editable() {
                    editor.unmark_text(window, cx);
                }
                editor.set_readonly(!can_edit, cx);
            });
        }

        if restore_position && let Some(editor) = &draft_position {
            Self::show_text(editor, "", window, cx);
        }
        let synced = shown
            .into_iter()
            .filter(|(editor, body)| Self::show_text(editor, body, window, cx))
            .map(|(editor, body)| (editor.entity_id(), body))
            .collect::<Vec<_>>();
        let created = missing
            .into_iter()
            .map(|item| {
                let editors = if item.is_comment() {
                    ItemEditors::Comment {
                        inline: Self::new_routed_draft_editor(draft_id, &item.body, window, cx),
                        composer: Self::new_routed_draft_editor(draft_id, &item.body, window, cx),
                    }
                } else {
                    ItemEditors::Prompt(Self::new_routed_draft_editor(
                        draft_id, &item.body, window, cx,
                    ))
                };
                for editor in editors.all() {
                    editor.update(cx, |editor, cx| editor.set_readonly(!can_edit, cx));
                }
                (item.id, editors)
            })
            .collect::<Vec<_>>();
        let draft_position = draft_position.is_none().then(|| {
            let editor = Self::new_routed_draft_editor(draft_id, "", window, cx);
            editor.update(cx, |editor, cx| editor.set_readonly(!can_edit, cx));
            editor
        });
        self.update_draft_editors(draft_id, cx, |items, draft| {
            draft.synced_text.extend(synced);
            for (id, editors) in created {
                if let Some(body) = items
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| &item.body)
                {
                    for editor in editors.all() {
                        draft.synced_text.insert(editor.entity_id(), body.clone());
                    }
                }
                draft.editors.entry(id).or_insert(editors);
            }
            if let Some(draft_position) = draft_position {
                draft.draft_position.get_or_insert(draft_position);
            }
        });
        if lost_focus {
            self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
        }
    }

    /// Replaces an editor's text, keeping its selection on the same text.
    ///
    /// Waits while an IME composition is in progress: its marked text is in
    /// the editor but not yet in the document, and replacing it would cancel
    /// the composition. The text catches up once the composition commits.
    ///
    /// Returns whether the editor shows `text` afterwards.
    fn show_text(
        editor: &Entity<TextareaState>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let (value, selection) = {
            let editor = editor.read(cx);
            (editor.value(), editor.selected_range())
        };
        let Some(edit) = TextEdit::diff(&value, text) else {
            return true;
        };
        if editor
            .update(cx, |editor, cx| editor.marked_text_range(window, cx))
            .is_some()
        {
            return false;
        }
        let selection = edit.map_offset(selection.start)..edit.map_offset(selection.end);
        // Replacing the value also clears the editor's undo history, which
        // keeps undo from reverting anyone else's edits.
        editor.update(cx, |editor, cx| {
            editor.set_value(SharedString::from(text.to_owned()), window, cx);
            editor.set_selected_range(selection, cx);
        });
        true
    }

    fn draft_editor_event(
        &mut self,
        draft_id: Uuid,
        editor: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor_id = editor.entity_id();
        if self
            .read_draft(draft_id, cx, |draft| draft.slot_of(editor_id))
            .flatten()
            .is_none()
        {
            // Resets invalidate handles, not just text. Ignore every old event,
            // including Focus after Write is granted again before the render.
            if self
                .typing_in
                .as_ref()
                .is_some_and(|(id, typing_editor, _)| {
                    (*id, *typing_editor) == (draft_id, editor_id)
                })
            {
                self.typing_in = None;
            }
            editor.update(cx, |editor, cx| {
                editor.set_readonly(true, cx);
                editor.unmark_text(window, cx);
            });
            return;
        }
        if !self.draft_can_edit(draft_id, cx) {
            // Programmatic changes and queued events bypass Textarea's readonly
            // input guard. Restore the projection, never the canonical draft.
            self.prepare_draft(draft_id, window, cx);
            return;
        }
        if !editor.read(cx).is_editable() {
            self.prepare_draft(draft_id, window, cx);
            // Preserve a real focus on a restored editor, but discard text and
            // removal events queued while its projection was still readonly.
            if !matches!(event, InputEvent::Focus) {
                return;
            }
        }
        // Switching to another window blurs too, but the user will be back.
        if matches!(event, InputEvent::Blur)
            && window.is_window_active()
            && self
                .typing_in
                .as_ref()
                .is_some_and(|(typing_draft, typing_editor, _)| {
                    (*typing_draft, *typing_editor) == (draft_id, editor_id)
                })
        {
            self.typing_in = None;
        }
        match event {
            InputEvent::Change => {
                let value = editor.read(cx).value().to_string();
                let editor = editor.clone();
                let merged = self
                    .update_draft(draft_id, cx, {
                        let editor = editor.clone();
                        move |draft| match draft.slot_of(editor_id) {
                            // Typing at the draft position creates a block, and
                            // the editor typed into becomes that block's, so
                            // focus, caret and any IME composition carry on
                            // uninterrupted.
                            Some(EditorSlot::DraftPosition) if !value.is_empty() => {
                                let id = draft.create_prompt(draft.author.as_uuid(), &value);
                                draft.update_draft_editors(|_, state| {
                                    state.editors.insert(id, ItemEditors::Prompt(editor));
                                    state.synced_text.insert(editor_id, value);
                                    state.draft_position = None;
                                });
                                None
                            }
                            Some(slot) => slot
                                .item()
                                .and_then(|id| draft.apply_typing(id, editor_id, &value)),
                            None => None,
                        }
                    })
                    .flatten();
                // Show others' edits the keystroke was merged with right away.
                if let Some(body) = merged
                    && Self::show_text(&editor, &body, window, cx)
                {
                    self.update_draft_editors(draft_id, cx, |_, state| {
                        state.synced_text.insert(editor_id, body);
                    });
                }
                cx.notify();
            }
            // Empty items disappear once nobody is in them anymore. Switching
            // to another window also blurs, but the user has not left.
            InputEvent::Blur if window.is_window_active() => {
                let Some(id) = self
                    .read_draft(draft_id, cx, |draft| draft.slot_of(editor_id))
                    .flatten()
                    .and_then(EditorSlot::item)
                else {
                    return;
                };
                // Moving between the two editors of one comment, or leaving
                // a block whose files are still being read, is not leaving.
                let still_in_item = self
                    .read_draft(draft_id, cx, |draft| {
                        draft.editors.get(&id).is_some_and(|editors| {
                            editors
                                .all()
                                .into_iter()
                                .any(|editor| editor.focus_handle(cx).is_focused(window))
                        })
                    })
                    .unwrap_or(false);
                if still_in_item || self.has_pending_reads(draft_id, id, cx) {
                    return;
                }
                self.update_draft(draft_id, cx, |draft| draft.remove_if_unattended(id));
                cx.notify();
            }
            InputEvent::Focus => {
                self.typing_in = Some((draft_id, editor_id, editor.focus_handle(cx)));
            }
            InputEvent::Blur | InputEvent::PressEnter { .. } => {}
        }
    }

    /// Reads the draft `draft_id` wherever it lives, including read-only mirrors.
    pub(crate) fn read_draft<R>(
        &self,
        draft_id: Uuid,
        cx: &App,
        read: impl FnOnce(&ThreadDraft) -> R,
    ) -> Option<R> {
        if self.new_thread_draft.id == draft_id {
            return Some(read(&self.new_thread_draft));
        }
        self.thread_store
            .read(cx)
            .threads
            .iter()
            .map(|thread| thread.read(cx))
            .find(|thread| thread.draft().id == draft_id)
            .map(|thread| read(thread.draft()))
    }

    /// The focused editor of the draft the composer shows.
    pub(crate) fn focused_draft_editor(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<(Uuid, EditorSlot, Entity<TextareaState>)> {
        let draft_id = self.readable_draft_id(cx)?;
        self.read_draft(draft_id, cx, |draft| {
            draft
                .all_editors()
                .find(|(_, editor)| editor.focus_handle(cx).is_focused(window))
                .map(|(slot, editor)| (draft_id, slot, editor.clone()))
        })?
    }

    /// Focuses an editor of a draft, optionally moving its caret.
    pub(crate) fn focus_draft_editor(
        &mut self,
        draft_id: Uuid,
        slot: EditorSlot,
        caret: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if slot == EditorSlot::DraftPosition && !self.draft_can_edit(draft_id, cx) {
            return false;
        }
        self.prepare_draft(draft_id, window, cx);
        let Some(editor) = self
            .read_draft(draft_id, cx, |draft| draft.editor(slot))
            .flatten()
        else {
            return false;
        };
        if let Some(caret) = caret {
            let caret = caret.min(editor.read(cx).value().len());
            editor.update(cx, |editor, cx| editor.set_selected_range(caret..caret, cx));
        }
        editor.focus_handle(cx).focus(window, cx);
        // Recorded right away; the focus event only arrives with the next
        // frame, after the editor it replaces may already be gone.
        if self.draft_can_edit(draft_id, cx) {
            self.typing_in = Some((draft_id, editor.entity_id(), editor.focus_handle(cx)));
        }
        cx.notify();
        true
    }

    /// Focuses where the user most likely continues typing: the last prompt
    /// block if there is one, otherwise the draft position.
    pub(crate) fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        self.prepare_draft(draft_id, window, cx);
        let last_block = self
            .read_draft(draft_id, cx, |draft| {
                draft
                    .items()
                    .into_iter()
                    .rfind(|item| item.is_prompt())
                    .map(|item| (item.id, item.body.len()))
            })
            .flatten();
        match last_block {
            Some((id, end)) => {
                self.focus_draft_editor(draft_id, EditorSlot::Prompt(id), Some(end), window, cx)
            }
            None => self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx),
        };
    }

    /// Puts the caret into the composer unless it is already in the draft.
    pub(crate) fn ensure_composer_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focused_draft_editor(window, cx).is_none() {
            self.focus_composer(window, cx);
        }
    }

    /// Where the local user is in `thread`'s draft, as others should see it.
    fn local_presence(&self, thread: &Thread, cx: &App) -> protocol::Presence {
        if !thread.can_edit_draft() {
            return protocol::Presence::default();
        }
        let draft = thread.draft();
        let slot = self
            .typing_in
            .as_ref()
            .filter(|(typing_draft, _, _)| {
                *typing_draft == draft.id && self.active_thread_id == Some(thread.instance_id)
            })
            .and_then(|(_, editor, _)| draft.slot_of(*editor));
        let focus = slot.map(|slot| match slot.item() {
            Some(id) => protocol::PresenceFocus::Item(id.as_uuid().into_bytes()),
            None => protocol::PresenceFocus::DraftPosition,
        });
        let selection = slot
            .and_then(|slot| Some((slot.item()?, draft.editor(slot)?)))
            .and_then(|(id, editor)| {
                let editor = editor.read(cx);
                let range = editor.selected_range();
                let head = editor.cursor();
                let tail = if head == range.start {
                    range.end
                } else {
                    range.start
                };
                Some(protocol::PresenceSelection {
                    anchor: draft.anchor(id, tail)?,
                    head: draft.anchor(id, head)?,
                })
            });
        let pending_reads = self
            .pending_attachments
            .iter()
            .filter(|pending| pending.draft_id == draft.id)
            .map(|pending| protocol::PendingRead {
                id: pending.id.into_bytes(),
                name: pending.name.clone(),
                is_image: pending.is_image,
                progress: pending
                    .progress
                    .map(|progress| progress.clamp(0., 100.) as u8),
                block: draft
                    .pending_block(pending.target)
                    .map(|id| id.as_uuid().into_bytes()),
            })
            .collect();
        protocol::Presence {
            focus,
            selection,
            pending_reads,
        }
    }

    /// Sends the local user's presence in every shared thread whose has
    /// changed since it was last sent.
    pub(crate) fn publish_presence(&mut self, cx: &mut Context<Self>) {
        let threads = self.thread_store.read(cx).threads.clone();
        let mut collaborating = HashSet::new();
        for thread in threads {
            let (thread_id, presence) = {
                let thread = thread.read(cx);
                if !matches!(
                    thread.sharing,
                    ThreadSharing::Shared { .. } | ThreadSharing::Connected { .. }
                ) {
                    continue;
                }
                (thread.instance_id, self.local_presence(thread, cx))
            };
            collaborating.insert(thread_id);
            if self.published_presence.get(&thread_id) == Some(&presence) {
                continue;
            }
            self.published_presence.insert(thread_id, presence.clone());
            thread.update(cx, |thread, cx| thread.publish_presence(presence, cx));
        }
        // Sharing again starts over, with everyone joining learning it anew.
        self.published_presence
            .retain(|thread_id, _| collaborating.contains(thread_id));
    }

    /// Redraws once the caret labels shown now have expired.
    pub(crate) fn schedule_caret_label_refresh(&mut self, cx: &mut Context<Self>) {
        if self.caret_label_refresh.is_some() {
            return;
        }
        let Some(thread) = self.active_thread(cx) else {
            return;
        };
        let now = Instant::now();
        let Some(expires_in) = thread
            .read(cx)
            .draft()
            .presence
            .values()
            .filter_map(|(_, changed)| {
                (*changed + CARET_LABEL_DURATION).checked_duration_since(now)
            })
            .min()
        else {
            return;
        };
        self.caret_label_refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(expires_in + Duration::from_millis(20))
                .await;
            _ = this.update(cx, |this, cx| {
                this.caret_label_refresh = None;
                cx.notify();
            });
        }));
    }

    /// Whether files are still being read into the block `id`.
    fn has_pending_reads(&self, draft_id: Uuid, id: ItemId, cx: &App) -> bool {
        self.read_draft(draft_id, cx, |draft| {
            self.pending_attachments.iter().any(|pending| {
                pending.draft_id == draft_id && draft.pending_block(pending.target) == Some(id)
            })
        })
        .unwrap_or(false)
    }

    /// Moves between the composer's editors when Up or Down leaves the first
    /// or last visual line. Returns whether it moved.
    pub(crate) fn move_between_draft_editors(
        &mut self,
        up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((draft_id, slot, editor)) = self.focused_draft_editor(window, cx) else {
            return false;
        };
        // A selection collapses first, as it does inside an editor.
        if !editor.read(cx).selected_range().is_empty() {
            return false;
        }
        let Some(caret_x) = caret_x_on_edge_line(editor.read(cx), !up) else {
            return false;
        };
        let Some(mut chain) = self.read_draft(draft_id, cx, ThreadDraft::navigation_chain) else {
            return false;
        };
        if !self.draft_can_edit(draft_id, cx) {
            chain.retain(|(slot, _)| *slot != EditorSlot::DraftPosition);
        }
        let Some(index) = chain.iter().position(|(chain_slot, _)| *chain_slot == slot) else {
            return false;
        };
        let target = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1).filter(|next| *next < chain.len())
        };
        let Some((target_slot, target)) = target.map(|target| chain[target].clone()) else {
            return false;
        };
        let caret = offset_near_x(target.read(cx), caret_x, up);
        self.focus_draft_editor(draft_id, target_slot, Some(caret), window, cx)
    }

    /// Escape in an empty item removes it and returns to the draft position.
    pub(crate) fn escape_empty_draft_item(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((draft_id, slot, _)) = self.focused_draft_editor(window, cx) else {
            return false;
        };
        let Some(id) = slot.item() else {
            return false;
        };
        if !self.draft_can_edit(draft_id, cx) || !self.item_is_empty(draft_id, id, cx) {
            return false;
        }
        // Kept while someone else is in it, but the caret moves on either way.
        self.update_draft(draft_id, cx, |draft| draft.remove_if_unattended(id));
        self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
        true
    }

    fn item_is_empty(&self, draft_id: Uuid, id: ItemId, cx: &App) -> bool {
        self.read_draft(draft_id, cx, |draft| {
            draft.item(id).is_some_and(|item| item.is_empty())
        })
        .unwrap_or(false)
    }

    /// Backspace in an empty item removes it, and in the empty draft position
    /// steps back; either way the caret moves to the end of the editor above.
    pub(crate) fn backspace_out_of_empty_draft_item(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((draft_id, slot, editor)) = self.focused_draft_editor(window, cx) else {
            return false;
        };
        if !self.draft_can_edit(draft_id, cx) || !editor.read(cx).value().is_empty() {
            return false;
        }
        let Some(chain) = self.read_draft(draft_id, cx, ThreadDraft::navigation_chain) else {
            return false;
        };
        let previous = chain
            .iter()
            .position(|(chain_slot, _)| *chain_slot == slot)
            .and_then(|index| index.checked_sub(1))
            .map(|index| chain[index].clone());
        match slot.item() {
            Some(id) => {
                if !self.item_is_empty(draft_id, id, cx) {
                    return false;
                }
                // Kept while someone else is in it; the caret moves anyway.
                self.update_draft(draft_id, cx, |draft| draft.remove_if_unattended(id));
            }
            None if previous.is_none() => return false,
            None => {}
        }
        match previous {
            Some((slot, editor)) => {
                let end = editor.read(cx).value().len();
                self.focus_draft_editor(draft_id, slot, Some(end), window, cx)
            }
            None => self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx),
        }
    }

    /// The composer can display a draft regardless of the local access mode.
    pub(crate) fn readable_draft_id(&self, cx: &App) -> Option<Uuid> {
        match self.active_thread_id {
            Some(id) => self
                .thread_store
                .read(cx)
                .thread(id, cx)
                .map(|thread| thread.read(cx).draft().id),
            None => Some(self.new_thread_draft.id),
        }
    }

    /// Whether the local actor may edit this destination, not the active thread.
    pub(crate) fn draft_can_edit(&self, draft_id: Uuid, cx: &App) -> bool {
        if self.new_thread_draft.id == draft_id {
            return true;
        }
        self.thread_store.read(cx).threads.iter().any(|thread| {
            let thread = thread.read(cx);
            thread.draft().id == draft_id && thread.can_edit_draft()
        })
    }

    /// The draft the composer currently edits, or `None` for read-only threads.
    pub(crate) fn writable_draft_id(&self, cx: &App) -> Option<Uuid> {
        self.readable_draft_id(cx)
            .filter(|id| self.draft_can_edit(*id, cx))
    }

    /// Reconciles access immediately after a policy update or draft reset.
    /// Canonical text and IME are reconciled by `prepare_draft` during render,
    /// which has the window required by the text input APIs.
    pub(crate) fn reconcile_peer_access(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let (draft_id, can_edit, editors) = {
            let thread = thread.read(cx);
            (
                thread.draft().id,
                thread.can_edit_draft(),
                thread
                    .draft()
                    .all_editors()
                    .map(|(_, editor)| editor.clone())
                    .collect::<Vec<_>>(),
            )
        };
        // Downgrades take effect immediately. Grants wait for the window-aware
        // canonical text/IME reconciliation in prepare_draft.
        if !can_edit {
            for editor in editors {
                editor.update(cx, |editor, cx| editor.set_readonly(true, cx));
            }
            self.cancel_draft_attachment_reads(draft_id, cx);
        }
        if self.typing_in.as_ref().is_some_and(|(id, editor, _)| {
            *id == draft_id
                && (!can_edit
                    || self
                        .read_draft(draft_id, cx, |draft| draft.slot_of(*editor))
                        .flatten()
                        .is_none())
        }) {
            self.typing_in = None;
        }
        self.publish_presence(cx);
        cx.notify();
    }

    /// Maintains UI projection caches without requiring or granting edit access.
    pub(crate) fn update_draft_editors<R>(
        &mut self,
        draft_id: Uuid,
        cx: &mut Context<Self>,
        update: impl FnOnce(&[DraftItem], &mut DraftEditorState) -> R,
    ) -> Option<R> {
        if self.new_thread_draft.id == draft_id {
            return Some(self.new_thread_draft.update_draft_editors(update));
        }
        let thread = self
            .thread_store
            .read(cx)
            .threads
            .iter()
            .find(|thread| thread.read(cx).draft().id == draft_id)
            .cloned()?;
        Some(thread.update(cx, |thread, _| thread.update_draft_editors(update)))
    }

    /// Finds a writable draft wherever it lives, so work started on one thread
    /// still lands there after the user switches to another. Local changes
    /// made by `update` are sent to the thread's other participants.
    pub(crate) fn update_draft<R>(
        &mut self,
        draft_id: Uuid,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut ThreadDraft) -> R,
    ) -> Option<R> {
        if self.new_thread_draft.id == draft_id {
            let result = update(&mut self.new_thread_draft);
            // Nobody else has this draft yet; its pending updates stay local
            // until it acquires a thread.
            return Some(result);
        }
        let thread = self
            .thread_store
            .read(cx)
            .threads
            .iter()
            .find(|thread| {
                let thread = thread.read(cx);
                thread.draft().id == draft_id
            })
            .cloned()?;
        thread.update(cx, |thread, _| {
            let actor = thread.participant_id();
            thread
                .with_authorized::<EditDraft, _>(actor, |auth| auth.edit(update))
                .ok()
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        MainStage, participant::ParticipantId, profile::Profile, thread::PeerMode,
        thread::ThreadStore, usage::ActivityRange,
    };
    use gpui::{IntoElement, Render, ScrollHandle, div};
    use std::{collections::HashMap, sync::Arc};

    struct EditorTestRoot {
        cowork: Entity<Cowork>,
        thread: Entity<Thread>,
    }

    impl Render for EditorTestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A mirror without networking or automatic app renders, so policy, reset,
    /// and queued input can be interleaved before the next prepare_draft.
    pub(crate) struct EditorTestApp<'a> {
        pub(crate) cowork: Entity<Cowork>,
        pub(crate) thread: Entity<Thread>,
        pub(crate) cx: &'a mut gpui::VisualTestContext,
        _runtime: tokio::runtime::Runtime,
    }

    impl<'a> EditorTestApp<'a> {
        pub(crate) fn start(cx: &'a mut gpui::TestAppContext) -> Self {
            cx.update(|cx| {
                gpui_component::init(cx);
                crate::theme::init(cx);
            });
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime");
            let tokio_handle = runtime.handle().clone();
            let (root, cx) = cx.add_window_view(|window, cx| {
                let host_id = ParticipantId::new();
                let actor = ParticipantId::new();
                let mut draft = ThreadDraft::new(host_id);
                draft.create_prompt(host_id.as_uuid(), "canonical prompt");
                draft.create_comment(
                    host_id.as_uuid(),
                    draft::CommentTarget {
                        message_id: Uuid::new_v4(),
                        range: 0..1,
                        quote: "x".into(),
                    },
                    "canonical comment",
                );
                let host = Thread::new_local(
                    "test".into(),
                    Vec::new(),
                    draft,
                    host_id,
                    Arc::default(),
                    None,
                );
                let mut snapshot = host.to_protocol();
                snapshot.participants = vec![host_id.into_bytes(), actor.into_bytes()];
                let welcome = protocol::Welcome {
                    participant_id: actor.into_bytes(),
                    thread: snapshot,
                    draft: host.draft().encode_state(),
                    draft_generation: 0,
                    presence: Vec::new(),
                    stored_attachments: Vec::new(),
                };
                let thread = cx.new(|cx| {
                    Thread::from_prepared_welcome(
                        Thread::prepare_welcome(welcome).expect("valid welcome"),
                        ThreadDraft::new(actor),
                        ThreadSharing::NotShared,
                        cx,
                    )
                });
                let thread_store = cx.new(|_| {
                    let mut store = ThreadStore::default();
                    store.threads.push_front(thread.clone());
                    store
                });
                let thread_id = thread.read(cx).instance_id;
                let cowork = cx.new(|cx| {
                    let (model_picker, model_picker_subscription) =
                        Cowork::new_model_picker(window, cx);
                    Cowork {
                        sidebar_open: true,
                        recents_open: true,
                        new_thread_draft: ThreadDraft::new(actor),
                        attachment_errors: Vec::new(),
                        pending_attachments: Vec::new(),
                        timeline_scroll_handle: ScrollHandle::new(),
                        timeline_focus_handle: cx.focus_handle(),
                        follow_generation: true,
                        thread_store,
                        active_thread_id: Some(thread_id),
                        selection_message_id: None,
                        segment_text_views: HashMap::new(),
                        shown_segments: HashMap::new(),
                        render_generation: 0,
                        copied_endpoint_id: None,
                        join_dialog: None,
                        main_stage: MainStage::Thread,
                        selected_welcome_provider: None,
                        profile: Profile::local(actor),
                        shown_profiles: HashMap::new(),
                        profile_error: None,
                        profile_name_subscription: None,
                        tokio_handle,
                        sandboxes: crate::test_support::unused_sandboxes(),
                        active_generations: HashMap::new(),
                        tokens_used: 0,
                        token_activity: Vec::new(),
                        deleted_chats: 0,
                        longest_deleted_chat: Default::default(),
                        activity_range: ActivityRange::default(),
                        local_participant_id: actor,
                        typing_in: None,
                        published_presence: HashMap::new(),
                        caret_label_refresh: None,
                        working_refresh: None,
                        new_thread_model: None,
                        new_thread_project_folders: Vec::new(),
                        new_thread_project_mode: Default::default(),
                        model_picker,
                        model_picker_hovered: false,
                        models: Arc::default(),
                        picker_rows: None,
                        _model_picker_subscription: model_picker_subscription,
                        _window_activation_subscription: cx
                            .observe_window_activation(window, |_, _, _| {}),
                    }
                });
                EditorTestRoot { cowork, thread }
            });
            let (cowork, thread) =
                root.read_with(cx, |root, _| (root.cowork.clone(), root.thread.clone()));
            cx.update(|window, cx| {
                window.activate_window();
                let draft_id = thread.read(cx).draft().id;
                cowork.update(cx, |cowork, cx| cowork.prepare_draft(draft_id, window, cx));
            });
            Self {
                cowork,
                thread,
                cx,
                _runtime: runtime,
            }
        }

        pub(crate) fn set_mode(&mut self, mode: PeerMode) {
            self.thread.update(self.cx, |thread, cx| {
                thread.apply_for_test(protocol::HostMessage::DefaultPeerModeChanged(mode), cx);
            });
        }
    }

    #[gpui::test]
    fn downgrade_restores_all_editors_and_blocks_programmatic_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        let app = EditorTestApp::start(cx);
        let thread = app.thread.clone();
        let cowork = app.cowork.clone();
        app.cx.update(|window, cx| {
            let draft_id = thread.read(cx).draft().id;
            let canonical = thread.read(cx).draft().items();
            let editors = thread
                .read(cx)
                .draft()
                .all_editors()
                .map(|(slot, editor)| (slot, editor.clone()))
                .collect::<Vec<_>>();
            assert_eq!(editors.len(), 4);
            let editor = &editors[0].1;
            editor.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(None, "uncommitted", None, window, cx);
            });
            assert!(
                editor
                    .update(cx, |editor, cx| editor.marked_text_range(window, cx))
                    .is_some()
            );
            thread.update(cx, |thread, cx| {
                thread.apply_for_test(
                    protocol::HostMessage::DefaultPeerModeChanged(PeerMode::ReadOnly),
                    cx,
                )
            });
            cowork.update(cx, |cowork, cx| {
                cowork.typing_in = Some((draft_id, editor.entity_id(), editor.focus_handle(cx)));
                cowork.reconcile_peer_access(thread.read(cx).instance_id, cx);
                for (_, candidate) in &editors {
                    assert!(!candidate.read(cx).is_editable());
                    // Keep one live composition to exercise unmarking; the
                    // other editors exercise programmatic readonly bypasses.
                    if candidate.entity_id() != editor.entity_id() {
                        candidate.update(cx, |editor, cx| {
                            editor.set_value("programmatic bypass", window, cx)
                        });
                    }
                }
                cowork.draft_editor_event(draft_id, &editors[1].1, &InputEvent::Change, window, cx);
                assert!(cowork.typing_in.is_none());
                assert_eq!(
                    cowork.local_presence(thread.read(cx), cx),
                    protocol::Presence::default()
                );
            });
            assert_eq!(thread.read(cx).draft().items(), canonical);
            for (slot, editor) in editors {
                let expected = slot
                    .item()
                    .and_then(|id| thread.read(cx).draft().body(id))
                    .unwrap_or_default();
                assert_eq!(editor.read(cx).value().as_ref(), expected);
                assert!(!editor.read(cx).is_editable());
                assert!(
                    editor
                        .update(cx, |editor, cx| editor.marked_text_range(window, cx))
                        .is_none()
                );
            }
        });
    }

    #[gpui::test]
    fn grant_reconciles_dirty_readonly_editors_before_accepting_input(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut app = EditorTestApp::start(cx);
        app.set_mode(PeerMode::ReadOnly);
        let thread = app.thread.clone();
        let cowork = app.cowork.clone();
        app.cx.update(|window, cx| {
            let draft_id = thread.read(cx).draft().id;
            let canonical = thread.read(cx).draft().items();
            let position = thread
                .read(cx)
                .draft()
                .editor(EditorSlot::DraftPosition)
                .unwrap();
            cowork.update(cx, |cowork, cx| {
                cowork.reconcile_peer_access(thread.read(cx).instance_id, cx)
            });
            position.update(cx, |editor, cx| {
                editor.set_value("must not create a prompt", window, cx)
            });
            thread.update(cx, |thread, cx| {
                thread.apply_for_test(
                    protocol::HostMessage::DefaultPeerModeChanged(PeerMode::Write),
                    cx,
                )
            });
            cowork.update(cx, |cowork, cx| {
                cowork.reconcile_peer_access(thread.read(cx).instance_id, cx);
                assert!(!position.read(cx).is_editable());
                cowork.draft_editor_event(draft_id, &position, &InputEvent::Change, window, cx);
            });
            assert_eq!(thread.read(cx).draft().items(), canonical);
            assert!(position.read(cx).value().is_empty());
            assert!(position.read(cx).is_editable());
        });
    }

    #[gpui::test]
    fn reset_events_from_old_handles_are_inert_after_grant_before_render(
        cx: &mut gpui::TestAppContext,
    ) {
        let app = EditorTestApp::start(cx);
        let thread = app.thread.clone();
        let cowork = app.cowork.clone();
        app.cx.update(|window, cx| {
            let draft_id = thread.read(cx).draft().id;
            let canonical = thread.read(cx).draft().items();
            let state = thread.read(cx).draft().encode_state();
            let old = thread
                .read(cx)
                .draft()
                .all_editors()
                .map(|(_, editor)| editor.clone())
                .collect::<Vec<_>>();
            for editor in &old {
                editor.update(cx, |editor, cx| editor.set_value("rejected", window, cx));
            }
            cowork.update(cx, |cowork, cx| {
                cowork.typing_in = Some((draft_id, old[0].entity_id(), old[0].focus_handle(cx)));
            });
            thread.update(cx, |thread, cx| {
                thread.apply_for_test(
                    protocol::HostMessage::DefaultPeerModeChanged(PeerMode::ReadOnly),
                    cx,
                );
                thread.apply_for_test(
                    protocol::HostMessage::DraftReset {
                        generation: 1,
                        state,
                    },
                    cx,
                );
                thread.apply_for_test(
                    protocol::HostMessage::DefaultPeerModeChanged(PeerMode::Write),
                    cx,
                );
            });
            cowork.update(cx, |cowork, cx| {
                cowork.reconcile_peer_access(thread.read(cx).instance_id, cx);
                assert!(cowork.typing_in.is_none());
                for editor in &old {
                    cowork.draft_editor_event(draft_id, editor, &InputEvent::Focus, window, cx);
                    cowork.draft_editor_event(draft_id, editor, &InputEvent::Change, window, cx);
                    cowork.draft_editor_event(draft_id, editor, &InputEvent::Blur, window, cx);
                }
                assert!(cowork.typing_in.is_none());
                assert!(thread.read(cx).draft().all_editors().next().is_none());
                cowork.prepare_draft(draft_id, window, cx);
            });
            assert_eq!(thread.read(cx).draft().items(), canonical);
            assert!(old.iter().all(|editor| !editor.read(cx).is_editable()));
            assert!(thread.read(cx).draft().all_editors().all(|(_, editor)| {
                editor.read(cx).is_editable()
                    && !old.iter().any(|old| old.entity_id() == editor.entity_id())
            }));
        });
    }
}
