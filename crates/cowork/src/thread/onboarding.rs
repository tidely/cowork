//! The first chat is a normal, saved thread. Seed its opening through the
//! usual host events so replay, comments, and sharing need no tutorial state.
//! The greeting streams in as previews of the seeded run, as a reply would.
//!
//! Whether to onboard is decided here, from the user's threads alone; which
//! model it runs with is the model selection's (the welcome screen's steps,
//! or the saved model). Onboarding only waits for one.

use std::time::{Duration, SystemTime};

use agent::{DefinitionRevision, Entry, InputId, Origin, model::Message};
use gpui::{AppContext, Context, Task, Window};
use uuid::Uuid;

use super::{Thread, draft::ThreadDraft};
use crate::{
    Cowork, MainStage,
    protocol::{HostMessage, Preview, PreviewDelta, RunEvent, RunOutcome},
    transcript::{RunSource, shared_entry},
};

const INSTRUCTIONS: &str = include_str!("../../prompts/onboarding.md");

/// How often the greeting grows while it streams in.
const STREAM_FRAME: Duration = Duration::from_millis(16);
/// At most this many frames, so the whole greeting shows in well under a
/// second and nobody feels kept waiting.
const STREAM_FRAMES: usize = 40;

/// The greeting still streaming into the onboarding thread's seeded run.
pub(crate) struct OnboardingIntro {
    thread_id: Uuid,
    message_id: Uuid,
}

/// `text` in up to [`STREAM_FRAMES`] pieces of whole words, each ending
/// after its whitespace.
fn stream_chunks(text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_inclusive(char::is_whitespace).collect();
    words
        .chunks(words.len().div_ceil(STREAM_FRAMES).max(1))
        .map(|chunk| chunk.concat())
        .collect()
}

const WELCOME_MESSAGE: &str = concat!(
    "Welcome to Cowork! I can help with almost any kind of work: writing, research, \
planning, budgets, organizing files, or building software.

![Collaborate](icon:collaborate) **Collaborate** on chats and work together, live.\\
![Projects](icon:folder) **Projects** let me work with the files in your folders.\\
![Schedules](icon:schedule) **Schedules** run tasks on their own, daily or weekly.\\
![Comments](icon:comment) **Comments** on my replies point at exactly what to change.

Type **hi** and press **",
    cfg_select!(
        target_os = "macos" => "Cmd-Enter",
        _ => "Ctrl-Enter",
    ),
    "** for a quick tour."
);

impl Cowork {
    /// Whether the user is new to Cowork: no threads of their own, open or
    /// archived. Deleting them all makes them new again.
    pub(crate) fn needs_onboarding(&self, cx: &gpui::App) -> bool {
        let store = self.thread_store.read(cx);
        store.threads.is_empty() && store.archived.is_empty()
    }

    /// Shows the welcome screen, which leads to the onboarding chat. With a
    /// model already selected, the chat starts on its own once `discovered`
    /// (the provider's models being discovered) finishes; see
    /// [`Cowork::onboard_if_welcome`]. Otherwise the user chooses one first.
    pub(crate) fn offer_onboarding(
        &mut self,
        discovered: Task<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.main_stage = MainStage::Welcome;
        if self.new_thread_model.is_some() {
            cx.spawn_in(window, async move |this, cx| {
                discovered.await;
                _ = this.update_in(cx, |this, window, cx| {
                    this.onboard_if_welcome(window, cx);
                });
            })
            .detach();
        } else {
            discovered.detach();
        }
        cx.notify();
    }

    /// Starts the onboarding chat, as continuing from the welcome screen
    /// would, unless the user has moved on from it or the selected model
    /// isn't offered; then the welcome screen stays, with it preselected.
    pub(crate) fn onboard_if_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_stage == MainStage::Welcome {
            self.start_onboarding(window, cx);
        }
    }

    /// No session or model request is created until the user replies. A fresh
    /// draft keeps any work prepared before setup out of the tutorial. The
    /// thread is saved once the greeting has finished streaming in.
    pub(crate) fn start_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active_model_is_runnable(cx) {
            return;
        }
        let thread = Self::new_local_thread(
            "Welcome to Cowork".into(),
            Vec::new(),
            ThreadDraft::new(self.local_participant_id),
            self.local_participant_id,
            self.active_catalog(cx),
            self.active_model(cx),
            cx,
        );
        let message_id = thread.update(cx, |thread, cx| thread.start_onboarding_run(cx));
        let thread_id = thread.read(cx).instance_id;
        self.thread_store.update(cx, |store, _| {
            store.threads.push_front(thread.clone());
        });
        self.profile_statistics = None;
        self.open_thread(thread_id, window, cx);

        self.onboarding_intro = Some(OnboardingIntro {
            thread_id,
            message_id,
        });
        let chunks = stream_chunks(WELCOME_MESSAGE);
        cx.spawn(async move |this, cx| {
            for chunk in chunks {
                cx.background_executor().timer(STREAM_FRAME).await;
                let streaming = this.update(cx, |this, cx| {
                    this.stream_onboarding_intro(message_id, chunk, cx)
                });
                if !matches!(streaming, Ok(true)) {
                    return;
                }
            }
            _ = this.update(cx, |this, cx| {
                if this.streaming_intro(message_id) {
                    this.finish_onboarding_intro(cx);
                }
            });
        })
        .detach();
    }

    /// Whether `message_id` is the greeting still streaming in. The
    /// timeline shows it without a "Working for" line, so that the line
    /// doesn't vanish and shift the greeting once it ends.
    pub(crate) fn streaming_intro(&self, message_id: Uuid) -> bool {
        self.onboarding_intro
            .as_ref()
            .is_some_and(|intro| intro.message_id == message_id)
    }

    /// Shows `chunk` more of the greeting, returning whether it is still
    /// streaming.
    fn stream_onboarding_intro(
        &mut self,
        message_id: Uuid,
        chunk: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(intro) = self
            .onboarding_intro
            .as_ref()
            .filter(|intro| intro.message_id == message_id)
        else {
            return false;
        };
        let thread_id = intro.thread_id;
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return false;
        };
        thread.update(cx, |thread, cx| {
            thread.preview_onboarding(message_id, chunk, cx)
        });
        self.thread_updated(thread_id, cx);
        true
    }

    /// Shows the whole greeting and ends the seeded run: a turn boundary, so
    /// the thread is saved and a reply sent meanwhile starts.
    fn finish_onboarding_intro(&mut self, cx: &mut Context<Self>) {
        let Some(OnboardingIntro {
            thread_id,
            message_id,
        }) = self.onboarding_intro.take()
        else {
            return;
        };
        let store = self.thread_store.read(cx);
        let Some(thread) = store
            .thread(thread_id, cx)
            .or_else(|| store.archived_thread(thread_id, cx))
        else {
            return;
        };
        thread.update(cx, |thread, cx| {
            thread.finish_onboarding_run(message_id, cx)
        });
        self.save_thread(&thread, cx);
        self.thread_updated(thread_id, cx);
        self.start_next_pending(&thread, cx);
    }
}

impl Thread {
    /// A host-authored prompt and canned assistant reply use the same run
    /// representation as generated output. The prompt is context, not a user
    /// submission; only the reply appears in the timeline. This starts the
    /// run with the prompt, returning the message the reply streams into.
    fn start_onboarding_run(&mut self, cx: &mut impl AppContext) -> Uuid {
        let id = Uuid::new_v4();
        self.emit(
            HostMessage::AgentStarted {
                id: id.into_bytes(),
                comment_group_id: None,
                started_at: SystemTime::now(),
                // The greeting is canned; no model writes it.
                model: None,
            },
            cx,
        );
        self.commit_onboarding(
            id,
            Entry {
                message: Message::user(INSTRUCTIONS),
                origin: Origin::Input {
                    id: InputId::new(),
                    source: RunSource::Host,
                },
            },
            cx,
        );
        id
    }

    fn preview_onboarding(&mut self, id: Uuid, text: String, cx: &mut impl AppContext) {
        self.emit(
            HostMessage::AgentEvent {
                id: id.into_bytes(),
                event: RunEvent::Preview(Preview {
                    index: 0,
                    delta: PreviewDelta::Text(text),
                }),
            },
            cx,
        );
    }

    /// Commits the whole reply, however much of it was previewed, and ends
    /// the run. No generated usage or duration is attributed to it.
    fn finish_onboarding_run(&mut self, id: Uuid, cx: &mut impl AppContext) {
        self.commit_onboarding(
            id,
            Entry {
                message: Message::assistant(WELCOME_MESSAGE),
                origin: Origin::Model {
                    revision: DefinitionRevision::default(),
                },
            },
            cx,
        );
        self.emit(
            HostMessage::AgentEnded {
                id: id.into_bytes(),
                outcome: RunOutcome::Completed,
                duration: Duration::ZERO,
            },
            cx,
        );
    }

    fn commit_onboarding(&mut self, id: Uuid, entry: Entry<RunSource>, cx: &mut impl AppContext) {
        self.emit(
            HostMessage::AgentEvent {
                id: id.into_bytes(),
                event: RunEvent::Committed {
                    index: self.transcript.len() as u64,
                    entry: shared_entry(&entry),
                },
            },
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_greeting_streams_in_whole_words_within_its_frames() {
        let text = WELCOME_MESSAGE;
        let chunks = stream_chunks(&text);
        assert!(chunks.len() <= STREAM_FRAMES);
        assert_eq!(chunks.concat(), text);
    }
}
