//! Submitting a draft: which participant's submission wins, turning the
//! draft into a user message, and the prompt sent for it.

#[path = "generation.rs"]
mod generation;
pub(crate) use generation::ActiveGeneration;
#[cfg(test)]
pub(crate) use generation::RunTotals;

use std::{collections::HashMap, sync::Arc};

use draft::{DraftItem, DraftItemKind};
use gpui::{Context, Entity, SharedString, Window};
use itertools::Itertools;
use rig::completion::Message as RigMessage;
use tools::TurnComments;
use uuid::Uuid;

use crate::{
    Cowork, SubmitComposer,
    attachments::{MAX_MESSAGE_ATTACHMENT_BYTES, format_bytes},
    composer_attachments::AttachmentError,
    models::ModelRef,
    participant::ParticipantId,
    profile::participant_name,
    prompt::{agent_message, prompt_name},
    protocol,
    thread::{ControlGeneration, Thread, ThreadSharing},
    thread_draft::{EditorSlot, ItemPresence, ThreadDraft},
    timeline::{
        CommentReference, PromptBlock, TimelineMessage, UserComment, UserCommentBody,
        UserMessageGroup,
    },
};

/// The owned run payload of an accepted submission, consumed synchronously
/// before spawning any work. This is not a reusable authorization token.
struct GenerationPlan {
    thread_id: Uuid,
    prompt: RigMessage,
    history: Vec<RigMessage>,
    comment_group_id: Option<Uuid>,
    turn_comments: Arc<TurnComments>,
    /// Whether a scheduled task started the run, which nobody may be
    /// watching; see `tool_approval.rs`.
    scheduled: bool,
}

impl Cowork {
    pub(crate) fn composer_button_clicked(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generating = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .is_some_and(|thread| thread.read(cx).generating);
        if generating {
            self.stop_generation(cx);
        } else {
            self.submit_composer(window, cx);
        }
    }

    pub(crate) fn submit_composer_action(
        &mut self,
        _: &SubmitComposer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_composer(window, cx);
    }

    pub(crate) fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_thread = self.active_thread(cx);
        if active_thread
            .as_ref()
            .is_some_and(|thread| !thread.read(cx).can_control_generation())
        {
            return;
        }
        if !self.active_model_is_runnable(cx) {
            return;
        }
        if active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).generating)
        {
            return;
        }
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        if self.draft_is_loading_attachments(draft_id, cx) {
            return;
        }

        // The host of a mirrored thread accepts submissions, so that exactly
        // one happens however many participants press Ctrl-Enter at once.
        if let Some(thread) = active_thread
            .as_ref()
            .filter(|thread| matches!(thread.read(cx).sharing, ThreadSharing::Connected { .. }))
        {
            let requested = thread.update(cx, |thread, _| {
                if thread.draft().items().iter().all(DraftItem::is_empty) {
                    return false;
                }
                thread
                    .with_authorized::<ControlGeneration, _>(thread.participant_id(), |auth| {
                        auth.request_submit()
                    })
                    .unwrap_or(false)
            });
            if requested {
                self.selection_message_id = None;
                self.follow_generation = true;
            }
            return;
        }

        let actor = active_thread
            .as_ref()
            .map_or(self.local_participant_id, |thread| {
                thread.read(cx).participant_id()
            });
        if self.accept_submission(draft_id, active_thread, actor, true, cx) {
            self.selection_message_id = None;
            self.follow_generation = true;
            self.timeline_scroll_handle.scroll_to_bottom();
            // Whoever was typing in a submitted item continues at the draft
            // position; see `prepare_draft`.
            if self.focused_draft_editor(window, cx).is_none() {
                self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
            }
        }
    }

    /// Submits a local or hosted thread's draft: publishes its non-empty
    /// items as one user message and starts the agent on it. Without a
    /// thread, the draft starts a new one. Returns whether anything was
    /// submitted. Rejections are shown when the local user submitted.
    pub(crate) fn accept_submission(
        &mut self,
        draft_id: Uuid,
        active_thread: Option<Entity<Thread>>,
        actor: ParticipantId,
        submitted_locally: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        // An actor is the requester, not the host's local viewer. In particular,
        // Write may edit the draft but cannot consume it on someone else's behalf.
        if active_thread.as_ref().is_some_and(|thread| {
            let thread = thread.read(cx);
            !thread.is_host() || thread.draft().id != draft_id || thread.generating
        }) || (active_thread.is_none()
            && (actor != self.local_participant_id || draft_id != self.new_thread_draft.id))
        {
            return false;
        }
        // Checked again here, as several participants' files add up.
        let attached = self
            .read_draft(draft_id, cx, |draft| {
                draft
                    .items()
                    .into_iter()
                    .filter(|item| !item.is_empty())
                    .flat_map(|item| match item.kind {
                        DraftItemKind::Prompt { attachments } => attachments,
                        DraftItemKind::Comment { .. } => Vec::new(),
                    })
                    .map(|record| record.size)
                    .sum::<u64>()
            })
            .unwrap_or(0);
        if attached > MAX_MESSAGE_ATTACHMENT_BYTES {
            // TODO: tell a collaborator who submitted, too.
            if !submitted_locally {
                return false;
            }
            self.attachment_errors
                .retain(|error| error.draft_id != draft_id);
            self.attachment_errors.push(AttachmentError {
                draft_id,
                message: format!(
                    "Attachments on one message can total at most {}",
                    format_bytes(MAX_MESSAGE_ATTACHMENT_BYTES)
                ),
            });
            cx.notify();
            return false;
        }
        let submission = if let Some(thread) = &active_thread {
            thread.update(cx, |thread, _| {
                thread
                    .with_authorized::<ControlGeneration, _>(actor, |auth| {
                        auth.take_submission(Self::take_submission)
                    })
                    .and_then(|result| result)
                    .ok()
                    .flatten()
            })
        } else {
            Self::take_submission(&mut self.new_thread_draft)
        };
        let Some((comments, blocks, comments_folded)) = submission else {
            return false;
        };
        self.attachment_errors
            .retain(|error| error.draft_id != draft_id);

        let (timeline, files, history, mut prompt_names) = match &active_thread {
            Some(thread) => {
                let thread = thread.read(cx);
                (
                    thread.timeline.clone(),
                    thread.draft().files.clone(),
                    thread.transcript.clone(),
                    thread.prompt_names.clone(),
                )
            }
            None => (
                Vec::new(),
                self.new_thread_draft.files.clone(),
                Vec::new(),
                HashMap::new(),
            ),
        };
        let profiles = self.profiles_for(active_thread.as_ref().map(|thread| thread.read(cx)));
        let authors = comments
            .iter()
            .map(|comment| comment.author)
            .chain(blocks.iter().map(|block| block.author));
        for author in authors {
            prompt_names
                .entry(author)
                .or_insert_with(|| participant_name(author, profiles.get(&author)));
        }
        let turn_comments = Arc::new(TurnComments::new(comments.len()));
        let preface = Self::comments_preface(
            &comments,
            turn_comments.comment_ids(),
            &timeline,
            &prompt_names,
        );
        let prompt = agent_message(preface.as_deref(), &blocks, &files, &prompt_names);
        let has_comments = !comments.is_empty();
        let submitted_group = UserMessageGroup {
            id: Uuid::new_v4(),
            comments,
            blocks,
            comments_folded: has_comments || comments_folded,
        };
        let comment_group_id = has_comments.then_some(submitted_group.id);
        let title = submitted_group.title_text().to_owned();

        let thread_id = if let Some(thread) = active_thread {
            let thread_id = thread.read(cx).instance_id;
            thread.update(cx, |thread, cx| {
                if let Some(title) = Self::title_for_first_message(&thread.timeline, &title) {
                    thread.emit(protocol::HostMessage::ThreadTitled(title), cx);
                }
                thread.publish(protocol::HostMessage::UserMessage(
                    submitted_group.to_protocol(),
                ));
                thread.timeline.push(TimelineMessage::User(submitted_group));
                thread.name_in_prompts(prompt_names, cx);
            });
            thread_id
        } else {
            // The draft moves into the new thread, keeping whatever was not
            // submitted and any attachments still being read.
            let draft = std::mem::replace(
                &mut self.new_thread_draft,
                ThreadDraft::new(self.local_participant_id),
            );
            let thread = Self::new_local_thread(
                Self::thread_title(&title),
                vec![TimelineMessage::User(submitted_group)],
                draft,
                self.local_participant_id,
                self.models.clone(),
                self.new_thread_model.clone(),
                cx,
            );
            self.move_project_into(&thread, cx);
            thread.update(cx, |thread, cx| thread.name_in_prompts(prompt_names, cx));
            let thread_id = thread.read(cx).instance_id;
            self.thread_store.update(cx, |store, _| {
                store.threads.push_front(thread.clone());
            });
            self.active_thread_id = Some(thread_id);
            thread_id
        };

        self.start_generation(
            GenerationPlan {
                thread_id,
                prompt,
                history,
                comment_group_id,
                turn_comments,
                scheduled: false,
            },
            cx,
        );
        true
    }

    /// Submits a scheduled task's prompt to one of the user's own threads as
    /// the host, leaving its draft alone, and starts the agent on it with
    /// `model`. Names a thread that has no messages yet `title`. Returns the
    /// agent message the run produces, or why it couldn't start.
    pub(crate) fn submit_scheduled_prompt(
        &mut self,
        thread: &Entity<Thread>,
        text: &str,
        title: &str,
        model: &ModelRef,
        cx: &mut Context<Self>,
    ) -> Result<Uuid, String> {
        let state = thread.read(cx);
        if !state.is_host() {
            return Err("Only your own threads can be continued.".into());
        }
        if state.generating {
            return Err("The thread was already running.".into());
        }
        if state.model() != Some(model) {
            // Ignored when the catalog lacks the model, which is caught below.
            _ = thread.update(cx, |thread, cx| thread.select_model(model.clone(), cx));
        }
        if thread.read(cx).runnable_model().is_none() {
            return Err(format!("The model {} isn't available.", model.id));
        }

        let state = thread.read(cx);
        let author = state.participant_id();
        let blocks = vec![PromptBlock {
            id: Uuid::new_v4(),
            author,
            text: text.to_owned(),
            attachments: Vec::new(),
        }];
        let mut prompt_names = state.prompt_names.clone();
        let profiles = self.profiles_for(Some(state));
        prompt_names
            .entry(author)
            .or_insert_with(|| participant_name(author, profiles.get(&author)));
        let prompt = agent_message(None, &blocks, &HashMap::new(), &prompt_names);
        let history = state.transcript.clone();
        let thread_id = state.instance_id;
        let group = UserMessageGroup {
            id: Uuid::new_v4(),
            comments: Vec::new(),
            blocks,
            comments_folded: false,
        };
        thread.update(cx, |thread, cx| {
            if thread.timeline.is_empty() {
                thread.emit(
                    protocol::HostMessage::ThreadTitled(Self::thread_title(title)),
                    cx,
                );
            }
            thread.publish(protocol::HostMessage::UserMessage(group.to_protocol()));
            thread.timeline.push(TimelineMessage::User(group));
            thread.name_in_prompts(prompt_names, cx);
        });
        let message_id = self
            .start_generation(
                GenerationPlan {
                    thread_id,
                    prompt,
                    history,
                    comment_group_id: None,
                    turn_comments: Arc::new(TurnComments::new(0)),
                    scheduled: true,
                },
                cx,
            )
            .ok_or("The thread is gone.")?;
        self.thread_updated(thread_id, cx);
        Ok(message_id)
    }

    /// Takes every non-empty item out of the draft, in draft order, as the
    /// comments and prompt blocks of one submission. Empty items stay, such
    /// as a comment nobody has written yet. Returns `None` when there is
    /// nothing to submit.
    pub(crate) fn take_submission(
        draft: &mut ThreadDraft,
    ) -> Option<(Vec<UserComment>, Vec<PromptBlock>, bool)> {
        let items = draft
            .items()
            .into_iter()
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        if items.is_empty() {
            return None;
        }
        let mut comments = Vec::new();
        let mut blocks = Vec::new();
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            ids.push(item.id);
            let author = ParticipantId::from_uuid(item.creator);
            match item.kind {
                DraftItemKind::Comment { target } => comments.push(UserComment {
                    id: item.id.as_uuid(),
                    author,
                    presence: ItemPresence::default(),
                    reference: CommentReference {
                        message_id: target.message_id,
                        range: target.range,
                        quote: target.quote,
                    },
                    body: UserCommentBody::Submitted(item.body.into()),
                }),
                DraftItemKind::Prompt { attachments } => blocks.push(PromptBlock {
                    id: item.id.as_uuid(),
                    author,
                    text: item.body,
                    attachments,
                }),
            }
        }

        // Their files stay: the submitted message shows and sends them.
        draft.take_items(&ids);
        Some((comments, blocks, draft.comments_folded))
    }

    /// A thread's title: the start of its first prompt on one line. Longer
    /// than the sidebar shows, which fades it out rather than marking the
    /// cut, but capped, as it is sent to every collaborator.
    pub(crate) fn thread_title(prompt: &str) -> String {
        const MAX_CHARACTERS: usize = 100;

        Itertools::intersperse(prompt.split_whitespace(), " ")
            .flat_map(str::chars)
            .take(MAX_CHARACTERS)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    pub(crate) fn title_for_first_message(
        timeline: &[TimelineMessage],
        prompt: &str,
    ) -> Option<String> {
        timeline.is_empty().then(|| Self::thread_title(prompt))
    }

    /// The instructions for answering a submission's comments, sent ahead of
    /// its prompt blocks.
    pub(crate) fn comments_preface(
        comments: &[UserComment],
        comment_ids: &[tools::CommentId],
        timeline: &[TimelineMessage],
        names: &HashMap<ParticipantId, SharedString>,
    ) -> Option<String> {
        if comments.is_empty() {
            return None;
        }

        let mut result = String::from(
            "Participants attached the following inline comments to immutable excerpts from the conversation. You MUST call `respond_to_comment` exactly once for every comment_id listed here before finishing your response. Put the direct reply to that comment in the tool's `response` argument; do not repeat these replies in your final prose. Any messages after this list are ordinary messages, not comments: answer those in your normal reply.\n",
        );
        for (index, (comment, comment_id)) in comments.iter().zip(comment_ids.iter()).enumerate() {
            let UserCommentBody::Submitted(body) = &comment.body else {
                continue;
            };
            let message_number = timeline
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        TimelineMessage::Agent(message)
                            if message.id == comment.reference.message_id
                                || message.comment_responses.iter().any(|response| {
                                    response.id == comment.reference.message_id
                                })
                    )
                })
                .map(|index| index + 1)
                .unwrap_or_default();
            result.push_str(&format!(
                "\n{}. {} — {}, on an excerpt from assistant message {}:\n> {}\nComment: {}\n",
                index + 1,
                comment_id,
                prompt_name(names, comment.author),
                message_number,
                comment.reference.quote.replace('\n', "\n> "),
                body.trim(),
            ));
        }
        Some(result)
    }
}
