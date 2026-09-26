//! A thread: its timeline, draft, model, and how it is shared, kept in
//! step with the host by applying the same events on every participant.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use agent::TurnFold;
use anyhow::Context as _;
use draft::{AttachmentId, ItemId};
use gpui::{App, AppContext, Entity, SharedString};
use iroh::{Endpoint, endpoint::Connection};
use itertools::Itertools;
use rig::completion::Message as RigMessage;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    models::{ModelCatalog, ModelInfo, ModelRef},
    participant::ParticipantId,
    profile::Profile,
    protocol,
    thread_draft::ThreadDraft,
    timeline::{AgentMessage, TimelineMessage},
    transcript::validate_agent_runs,
};

/// How many thread events a collaborator may fall behind before the host
/// re-bases it on a fresh snapshot instead of a delta.
pub(crate) const THREAD_EVENT_CAPACITY: usize = 1024;

/// A rough average for estimating the tokens in streamed text before the
/// provider reports the exact count.
const BYTES_PER_TOKEN: u64 = 4;

#[derive(Clone)]
pub(crate) struct ThreadSummary {
    pub(crate) id: Uuid,
    pub(crate) title: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharingStatus {
    NotShared,
    Sharing,
    Shared,
    Connected,
    Failed,
}

/// The host's end of a collaborator connection.
pub(crate) type HostPeer = protocol::Peer<protocol::HostMessage, protocol::CollaboratorMessage>;

/// A collaborator's end of its connection to a thread's host.
pub(crate) type ThreadHost = protocol::Peer<protocol::CollaboratorMessage, protocol::HostMessage>;

pub(crate) enum ThreadSharing {
    NotShared,
    Sharing,
    /// Hosting the thread. `events` fans every local change out to all
    /// collaborators; dropping it tears their connections down.
    Shared {
        endpoint: Endpoint,
        events: broadcast::Sender<protocol::HostMessage>,
    },
    /// Mirroring someone else's thread.
    ///
    /// Requests to the host, such as draft updates, are sent on `host`. It is
    /// unbounded so that no request is ever dropped: a lost draft update
    /// would leave every later one waiting on it forever. Replacing this state
    /// closes `host` and drops `link`, which is what disconnects.
    Connected {
        host: async_channel::Sender<protocol::CollaboratorMessage>,
        /// For attachment bytes; see [`protocol::Peer::bulk`].
        uploads: async_channel::Sender<protocol::CollaboratorMessage>,
        /// `None` when the peer is not reached over the network, as in tests.
        link: Option<PeerLink>,
    },
    Failed,
}

/// Keeps a collaborator's QUIC connection to the host open.
pub(crate) struct PeerLink {
    pub(crate) endpoint: Endpoint,
    pub(crate) _connection: Connection,
}

impl ThreadSharing {
    pub(crate) fn status(&self) -> SharingStatus {
        match self {
            Self::NotShared => SharingStatus::NotShared,
            Self::Sharing => SharingStatus::Sharing,
            Self::Shared { .. } => SharingStatus::Shared,
            Self::Connected { .. } => SharingStatus::Connected,
            Self::Failed => SharingStatus::Failed,
        }
    }

    pub(crate) fn is_collaborating(&self) -> bool {
        matches!(
            self,
            Self::Sharing | Self::Shared { .. } | Self::Connected { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadOwnership {
    Local,
    Remote,
}

impl ThreadOwnership {
    /// Every participant edits the draft. Read-only viewers will come with
    /// sharing permissions, which the read-only rendering is kept for.
    pub(crate) fn can_write(self) -> bool {
        true
    }

    pub(crate) fn remove_on_disconnect(self) -> bool {
        matches!(self, Self::Remote)
    }
}

pub(crate) struct Thread {
    /// Identifies this local view. Multiple views may mirror the same shared
    /// thread, so this must remain distinct from `summary.id`.
    pub(crate) instance_id: Uuid,
    pub(crate) summary: ThreadSummary,
    /// Who the local user is in this thread: the app's own id for local and
    /// hosted threads, the id the host assigned for mirrored ones.
    pub(crate) participant_id: ParticipantId,
    /// Connected participants in join order, starting with the host. Empty
    /// while the thread is not shared.
    pub(crate) participants: Vec<ParticipantId>,
    /// The profile of everyone who has joined while shared, kept after they
    /// leave so their messages still name them. Includes the local user's,
    /// although [`Cowork::profile`] is what shows for them.
    pub(crate) profiles: HashMap<ParticipantId, Profile>,
    /// Everything the agent has been sent and has replied, exactly as sent,
    /// so each request extends the previous one and the provider's prompt
    /// cache stays valid. The host records it and everyone mirrors it; see
    /// `transcript.rs`.
    pub(crate) transcript: Vec<RigMessage>,
    /// Folds the running agent's events into the transcript's next message.
    /// The events it has folded so far are the running message's pending
    /// events.
    pub(crate) agent_turn: TurnFold,
    /// The name each author is given in prompts, fixed when their first
    /// item is submitted so that renaming never changes the transcript and
    /// the agent knows everyone by one name. Mirrored like the transcript.
    pub(crate) prompt_names: HashMap<ParticipantId, SharedString>,
    /// Tokens the agent has used in this thread, counted at the end of each
    /// turn; see [`Cowork::record_turn_usage`]. Only the host, which runs
    /// the agent, fills it.
    pub(crate) tokens_used: u64,
    /// The selected model. It may be missing from `models`, in which case it
    /// cannot run until another is picked; see [`Thread::runnable_model`].
    pub(crate) model: Option<ModelRef>,
    /// The models this thread can run: the catalog of whoever runs its agent,
    /// which is this app's own unless the thread is mirrored.
    pub(crate) models: Arc<ModelCatalog>,
    /// How much of the context window the thread fills, as the provider
    /// reported at the end of the last agent request that reported usage.
    /// `None` until one has; see [`Thread::live_context_tokens`].
    pub(crate) context_tokens: Option<u64>,
    /// Bytes of agent output streamed since `context_tokens` was measured.
    /// Every participant counts them from the same events, so their
    /// estimates agree.
    pub(crate) streamed_bytes: u64,
    pub(crate) timeline: Vec<TimelineMessage>,
    pub(crate) draft: ThreadDraft,
    pub(crate) generating: bool,
    pub(crate) sharing: ThreadSharing,
    pub(crate) ownership: ThreadOwnership,
}

impl Thread {
    /// Builds the local mirror of a thread hosted by someone else.
    pub(crate) fn from_welcome(
        welcome: protocol::Welcome,
        draft: ThreadDraft,
        sharing: ThreadSharing,
        cx: &mut impl AppContext,
    ) -> Self {
        let mut thread = Self {
            instance_id: Uuid::new_v4(),
            summary: ThreadSummary {
                id: Uuid::from_bytes(welcome.thread.id),
                title: String::new(),
            },
            participant_id: ParticipantId::from_bytes(welcome.participant_id),
            participants: Vec::new(),
            profiles: HashMap::new(),
            transcript: Vec::new(),
            agent_turn: TurnFold::default(),
            prompt_names: HashMap::new(),
            tokens_used: 0,
            model: None,
            models: Arc::default(),
            context_tokens: None,
            streamed_bytes: 0,
            timeline: Vec::new(),
            draft,
            generating: false,
            sharing,
            ownership: ThreadOwnership::Remote,
        };
        thread.rebase(welcome, cx);
        thread
    }

    /// Validates the nested Rig JSON in an untrusted host snapshot, and that
    /// its agent messages fit its transcript, before an entity is created or
    /// existing thread state is changed.
    pub(crate) fn validate_welcome(welcome: &protocol::Welcome) -> anyhow::Result<()> {
        let transcript = parse_transcript(&welcome.thread.transcript)?;
        let agent_messages = welcome
            .thread
            .messages
            .iter()
            .filter_map(|message| match message {
                protocol::TimelineMessage::Agent(message) => Some(message),
                protocol::TimelineMessage::User(_) => None,
            })
            .collect::<Vec<_>>();
        validate_agent_runs(&transcript, &agent_messages).context("invalid host snapshot")
    }

    /// Replaces everything the host is authoritative for with its snapshot.
    ///
    /// The draft is merged rather than replaced: local edits the host has
    /// not received yet are still on their way to it and must survive.
    pub(crate) fn rebase(&mut self, welcome: protocol::Welcome, cx: &mut impl AppContext) {
        self.try_rebase(welcome, cx)
            .expect("a valid thread snapshot");
    }

    /// Applies a snapshot received from the host.
    pub(crate) fn try_rebase(
        &mut self,
        welcome: protocol::Welcome,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        Self::validate_welcome(&welcome)?;
        let protocol::Welcome {
            participant_id,
            mut thread,
            draft,
            presence,
            stored_attachments,
        } = welcome;
        let transcript = parse_transcript(&thread.transcript)?;
        self.draft.stored = stored_attachments
            .into_iter()
            .map(|id| AttachmentId::from_uuid(Uuid::from_bytes(id)))
            .collect();
        let stored = &self.draft.stored;
        self.draft.uploads.retain(|id, _| !stored.contains(id));
        self.draft.keeps_removed_files = true;
        self.participant_id = ParticipantId::from_bytes(participant_id);
        self.draft.author = self.participant_id;
        if let Err(error) = self.draft.doc.apply_update(&draft) {
            eprintln!("failed to merge the host's draft: {error:#}");
        }
        self.draft.presence.clear();
        for (participant, presence) in presence {
            self.draft
                .set_presence(ParticipantId::from_bytes(participant), presence);
        }
        self.participants = thread
            .participants
            .iter()
            .copied()
            .map(ParticipantId::from_bytes)
            .collect();
        self.profiles = thread
            .profiles
            .iter()
            .map(|(participant, profile)| {
                (
                    ParticipantId::from_bytes(*participant),
                    Profile::from_protocol(profile.clone()),
                )
            })
            .collect();
        self.models = Arc::new(std::mem::take(&mut thread.models));
        self.model = thread.model.take();
        self.context_tokens = thread.context_tokens;
        self.streamed_bytes = thread.streamed_bytes;
        self.transcript = transcript;
        self.prompt_names = std::mem::take(&mut thread.prompt_names)
            .into_iter()
            .map(|(participant, name)| (ParticipantId::from_bytes(participant), name.into()))
            .collect();
        let (summary, timeline) = thread.into_native(cx);
        self.summary = summary;
        self.set_timeline(timeline);
        self.restore_agent_output(cx);
        Ok(())
    }

    pub(crate) fn to_protocol(&self) -> protocol::ThreadSnapshot {
        protocol::ThreadSnapshot {
            id: self.summary.id.into_bytes(),
            title: self.summary.title.clone(),
            participants: self
                .participants
                .iter()
                .map(|participant| participant.into_bytes())
                .collect(),
            // Sorted, so the same thread always makes the same snapshot.
            profiles: self
                .profiles
                .iter()
                .map(|(participant, profile)| (participant.into_bytes(), profile.to_protocol()))
                .sorted_by_key(|(participant, _)| *participant)
                .collect(),
            models: (*self.models).clone(),
            model: self.model.clone(),
            context_tokens: self.context_tokens,
            streamed_bytes: self.streamed_bytes,
            messages: self
                .timeline
                .iter()
                .map(TimelineMessage::to_protocol)
                .collect(),
            transcript: self
                .transcript
                .iter()
                .map(protocol::Json::from_rig)
                .collect(),
            prompt_names: self
                .prompt_names
                .iter()
                .map(|(participant, name)| (participant.into_bytes(), name.to_string()))
                .sorted()
                .collect(),
        }
    }

    /// Names `names`' participants in prompts, for those who have no name
    /// there yet; see [`Thread::prompt_names`].
    pub(crate) fn name_in_prompts(
        &mut self,
        names: HashMap<ParticipantId, SharedString>,
        cx: &mut impl AppContext,
    ) {
        // In a stable order, so everyone applies the same events.
        for (participant, name) in names
            .into_iter()
            .sorted_by_key(|(participant, _)| participant.into_bytes())
        {
            if !self.prompt_names.contains_key(&participant) {
                self.emit(
                    protocol::HostMessage::PromptNamed {
                        participant: participant.into_bytes(),
                        name: name.to_string(),
                    },
                    cx,
                );
            }
        }
    }

    /// Sends the draft's local changes to whoever else has a copy: every
    /// collaborator when hosting, the host when mirroring.
    pub(crate) fn flush_draft(&mut self) {
        let Some(update) = self.draft.doc.take_local_update() else {
            return;
        };
        match &self.sharing {
            ThreadSharing::Shared { .. } => {
                self.publish(protocol::HostMessage::DraftUpdate(update));
            }
            ThreadSharing::Connected { .. } => {
                self.request(protocol::CollaboratorMessage::DraftUpdate(update));
            }
            // Whoever joins later receives the whole draft with their welcome.
            ThreadSharing::NotShared | ThreadSharing::Sharing | ThreadSharing::Failed => {}
        }
    }

    /// Applies a collaborator's draft update and forwards it to everyone.
    ///
    /// Fails when the update breaks the draft's invariants or changes what
    /// `author` may not change, such as attributing an item to someone else.
    /// It has been applied by then; failing only tells the caller to
    /// disconnect the collaborator.
    pub(crate) fn apply_collaborator_update(
        &mut self,
        author: ParticipantId,
        update: Vec<u8>,
    ) -> anyhow::Result<()> {
        let before = self.draft.doc.items();
        let attachments_before = self.draft.attachment_records();
        self.draft.doc.apply_update(&update)?;
        self.publish(protocol::HostMessage::DraftUpdate(update));
        self.draft.doc.validate()?;
        draft::verify_change(&before, &self.draft.doc.items(), author.as_uuid())?;

        // The host keeps only the bytes of files still attached; submitted
        // ones leave the draft through the host's own submission instead.
        let attachments = self.draft.attachment_records();
        for removed in attachments_before
            .iter()
            .filter(|record| !attachments.iter().any(|kept| kept.id == record.id))
        {
            self.draft.drop_file(removed.id);
        }
        // Bytes can arrive before the record that announces them.
        let complete = self
            .draft
            .incoming
            .iter()
            .filter(|(_, file)| {
                file.uploader == Some(author) && file.bytes.len() as u64 == file.total
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in complete {
            self.store_upload(author, id)?;
        }
        Ok(())
    }

    /// Takes a piece of a file a collaborator is uploading.
    pub(crate) fn receive_upload(
        &mut self,
        uploader: ParticipantId,
        chunk: protocol::AttachmentChunk,
    ) -> anyhow::Result<()> {
        if let Some(id) = self.draft.receive_chunk(chunk, Some(uploader))? {
            self.store_upload(uploader, id)?;
        }
        Ok(())
    }

    /// Stores a completely uploaded file once its record is in the draft,
    /// and announces it. Until the record arrives it waits in `incoming`.
    fn store_upload(&mut self, uploader: ParticipantId, id: AttachmentId) -> anyhow::Result<()> {
        let Some(record) = self
            .draft
            .attachment_records()
            .into_iter()
            .find(|record| record.id == id)
        else {
            return Ok(());
        };
        let Some(file) = self.draft.incoming.get(&id) else {
            return Ok(());
        };
        anyhow::ensure!(
            record.creator == uploader.as_uuid()
                && record.size == file.total
                && record.kind == file.kind,
            "{} does not match its attachment record",
            file.name
        );
        self.draft.complete_file(id)?;
        self.publish_stored(id, uploader);
        Ok(())
    }

    /// The stored files `participant` needs the bytes of: all but the ones
    /// they attached themselves. Timeline files come first, in order.
    pub(crate) fn files_for(&self, participant: ParticipantId) -> Vec<AttachmentId> {
        let timeline = self.timeline.iter().flat_map(|message| match message {
            TimelineMessage::User(group) => group
                .blocks
                .iter()
                .flat_map(|block| block.attachments.clone())
                .collect(),
            TimelineMessage::Agent(_) => Vec::new(),
        });
        timeline
            .chain(self.draft.attachment_records())
            .filter(|record| {
                record.creator != participant.as_uuid()
                    && self.draft.stored.contains(&record.id)
                    && self.draft.files.contains_key(&record.id)
            })
            .map(|record| record.id)
            .collect()
    }

    /// Marks a file as held by the host and tells everyone.
    fn publish_stored(&mut self, id: AttachmentId, uploader: ParticipantId) {
        self.draft.stored.insert(id);
        self.publish(protocol::HostMessage::AttachmentStored {
            id: id.as_uuid().into_bytes(),
            uploader: uploader.into_bytes(),
        });
    }

    /// Tells the other participants where the local user is in the draft.
    pub(crate) fn publish_presence(
        &mut self,
        presence: protocol::Presence,
        cx: &mut impl AppContext,
    ) {
        match &self.sharing {
            ThreadSharing::Shared { .. } => {
                self.host_presence(self.participant_id, presence, cx);
            }
            ThreadSharing::Connected { .. } => {
                self.request(protocol::CollaboratorMessage::Presence(presence));
            }
            ThreadSharing::NotShared | ThreadSharing::Sharing | ThreadSharing::Failed => {}
        }
    }

    /// Records and broadcasts a participant's presence in a hosted thread.
    ///
    /// The host also removes an empty item the participant has just left if
    /// nobody is in it anymore. Whoever leaves an item last removes it
    /// themselves, but two people leaving at once each still see the other
    /// there, so the host settles it.
    pub(crate) fn host_presence(
        &mut self,
        participant: ParticipantId,
        presence: protocol::Presence,
        cx: &mut impl AppContext,
    ) {
        let left = self
            .draft
            .presence
            .get(&participant)
            .and_then(|(previous, _)| previous.focus)
            .filter(|previous| Some(*previous) != presence.focus);
        self.emit(
            protocol::HostMessage::Presence {
                participant: participant.into_bytes(),
                presence,
            },
            cx,
        );
        self.remove_if_left_empty(left);
    }

    /// Removes the item behind `focus` if it is empty, nobody is in it, and
    /// no files are on their way into it.
    fn remove_if_left_empty(&mut self, focus: Option<protocol::PresenceFocus>) {
        if let Some(protocol::PresenceFocus::Item(id)) = focus {
            let id = ItemId::from_uuid(Uuid::from_bytes(id));
            if !self.draft.is_attended(id, None)
                && !self.draft.has_announced_reads_into(id)
                && self.draft.remove_if_empty(id)
            {
                self.flush_draft();
            }
        }
    }

    /// Lets everyone know a collaborator has left, first removing the empty
    /// item they were in if nobody else is in it either.
    pub(crate) fn participant_left(
        &mut self,
        participant: ParticipantId,
        cx: &mut impl AppContext,
    ) {
        // Files they had not finished uploading can never be submitted.
        let unfinished = self
            .draft
            .attachment_records()
            .into_iter()
            .filter(|record| {
                record.creator == participant.as_uuid() && !self.draft.stored.contains(&record.id)
            })
            .map(|record| record.id)
            .collect::<Vec<_>>();
        for id in &unfinished {
            self.draft.doc.remove_attachment(*id);
        }
        self.draft
            .incoming
            .retain(|_, file| file.uploader != Some(participant));
        for id in unfinished {
            self.draft.drop_file(id);
        }
        self.flush_draft();

        let focus = self
            .draft
            .presence
            .remove(&participant)
            .and_then(|(presence, _)| presence.focus);
        self.remove_if_left_empty(focus);
        self.emit(
            protocol::HostMessage::ParticipantLeft(participant.into_bytes()),
            cx,
        );
    }

    /// How many submissions this thread has accepted.
    pub(crate) fn submission_count(&self) -> u64 {
        self.timeline
            .iter()
            .filter(|message| matches!(message, TimelineMessage::User(_)))
            .count() as u64
    }

    /// Sends a request to the host of a mirrored thread. Returns `false` when
    /// this thread is not mirrored or its connection has closed.
    pub(crate) fn request(&self, request: protocol::CollaboratorMessage) -> bool {
        let ThreadSharing::Connected { host, .. } = &self.sharing else {
            return false;
        };
        match host.try_send(request) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("failed to send request to host: {error}");
                false
            }
        }
    }

    /// Selects the thread's model on behalf of the local user.
    ///
    /// A mirrored thread asks the host to apply the choice; only the host's
    /// broadcast confirms it, since its catalog may have changed meanwhile.
    pub(crate) fn select_model(&mut self, model: ModelRef, cx: &mut impl AppContext) {
        if self.model.as_ref() == Some(&model) {
            return;
        }
        if matches!(self.sharing, ThreadSharing::Connected { .. }) {
            self.request(protocol::CollaboratorMessage::SelectModel(model));
        } else {
            self.host_select_model(model, cx);
        }
    }

    /// Selects the thread's model as the host, for the local user and
    /// collaborators alike. Only a model the thread's catalog offers can be
    /// selected.
    pub(crate) fn host_select_model(&mut self, model: ModelRef, cx: &mut impl AppContext) {
        if self.models.contains(&model) {
            self.emit(protocol::HostMessage::ModelSelected(model), cx);
        }
    }

    /// The selected model and what the catalog knows about it, or `None` when
    /// no model is selected or the catalog no longer offers it.
    pub(crate) fn runnable_model(&self) -> Option<(&ModelRef, &ModelInfo)> {
        let model = self.model.as_ref()?;
        Some((model, self.models.get(model)?))
    }

    /// The size of the selected model's context window, or 0 when it cannot
    /// run.
    pub(crate) fn max_tokens(&self) -> u64 {
        self.runnable_model().map_or(0, |(_, info)| info.max_tokens)
    }

    /// The agent message currently being generated, if any.
    pub(crate) fn running_agent_message_id(&self) -> Option<Uuid> {
        self.timeline.iter().rev().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if message.is_generating() => Some(message.id),
            _ => None,
        })
    }

    /// Subscribes to this thread's events, returning `None` when it is not
    /// being hosted.
    ///
    /// Callers that also need a snapshot must take both in the same
    /// `Entity::update`: thread state only changes on the foreground thread, so
    /// pairing them there guarantees the subscription starts exactly where the
    /// snapshot ends, with no event missed or replayed.
    pub(crate) fn subscribe(&self) -> Option<broadcast::Receiver<protocol::HostMessage>> {
        match &self.sharing {
            ThreadSharing::Shared { events, .. } => Some(events.subscribe()),
            _ => None,
        }
    }

    /// Broadcasts an event to every collaborator without applying it locally.
    ///
    /// Needed for the changes whose local representation carries more than the
    /// wire form does, such as a user message that also remembers the prompt
    /// the agent was given and whether its comments are folded.
    pub(crate) fn publish(&self, event: protocol::HostMessage) {
        if let ThreadSharing::Shared { events, .. } = &self.sharing {
            // An error here only means nobody has joined yet.
            _ = events.send(event);
        }
    }

    /// Applies a thread event locally and broadcasts it verbatim.
    ///
    /// Host and collaborators then run the same [`Thread::apply`] over the same
    /// events, so their timelines stay identical by construction. Only use this
    /// for events that fully describe the change they make.
    pub(crate) fn emit(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        // Checked up front so that an unshared thread, which is the common
        // case, never pays to clone a streamed chunk.
        if matches!(self.sharing, ThreadSharing::Shared { .. }) {
            self.publish(event.clone());
        }
        self.apply(event, cx);
    }

    /// Folds a thread event into the timeline.
    pub(crate) fn apply(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        self.try_apply(event, cx).expect("a valid thread event");
    }

    /// Applies an event received from an untrusted host.
    pub(crate) fn try_apply(
        &mut self,
        event: protocol::HostMessage,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        match event {
            protocol::HostMessage::Welcome(welcome) => self.try_rebase(*welcome, cx)?,
            // Only ever sent in place of the first `Welcome`, which the join
            // handshake consumes.
            protocol::HostMessage::Rejected(_) => {}
            protocol::HostMessage::ParticipantJoined {
                participant,
                profile,
            } => {
                let participant = ParticipantId::from_bytes(participant);
                if !self.participants.contains(&participant) {
                    self.participants.push(participant);
                }
                self.profiles
                    .insert(participant, Profile::from_protocol(profile));
            }
            protocol::HostMessage::ProfileChanged {
                participant,
                profile,
            } => {
                self.profiles.insert(
                    ParticipantId::from_bytes(participant),
                    Profile::from_protocol(profile),
                );
            }
            protocol::HostMessage::ParticipantLeft(participant) => {
                let participant = ParticipantId::from_bytes(participant);
                self.participants
                    .retain(|existing| *existing != participant);
                self.draft.presence.remove(&participant);
            }
            protocol::HostMessage::Presence {
                participant,
                presence,
            } => {
                self.draft
                    .set_presence(ParticipantId::from_bytes(participant), presence);
            }
            protocol::HostMessage::AttachmentStored { id, .. } => {
                let id = AttachmentId::from_uuid(Uuid::from_bytes(id));
                self.draft.stored.insert(id);
                self.draft.uploads.remove(&id);
            }
            // Only ever relayed by the host, which holds every file.
            protocol::HostMessage::AttachmentData(chunk) => {
                match self.draft.receive_chunk(chunk, None) {
                    Ok(Some(id)) => {
                        if let Err(error) = self.draft.complete_file(id) {
                            eprintln!("failed to read a received attachment: {error:#}");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("ignored attachment data: {error:#}"),
                }
            }
            protocol::HostMessage::ModelCatalogChanged(models) => {
                self.models = Arc::new(models);
            }
            protocol::HostMessage::ModelSelected(model) => {
                self.model = Some(model);
            }
            protocol::HostMessage::DraftUpdate(update) => {
                if let Err(error) = self.draft.doc.apply_update(&update) {
                    eprintln!("failed to apply a draft update: {error:#}");
                }
            }
            protocol::HostMessage::ThreadTitled(title) => self.summary.title = title,
            protocol::HostMessage::UserMessage(message) => self
                .timeline
                .push(TimelineMessage::User(message.into_native())),
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id,
                started_at,
                prompt,
            } => {
                self.push_transcript(&prompt)?;
                self.timeline.push(TimelineMessage::Agent(AgentMessage::new(
                    Uuid::from_bytes(id),
                    comment_group_id.map(Uuid::from_bytes),
                    started_at,
                    self.transcript.len() - 1,
                    protocol::AgentRun::Generating,
                    Vec::new(),
                    cx,
                )));
                self.generating = true;
                self.agent_turn = TurnFold::default();
            }
            protocol::HostMessage::AgentEvent { id, event } => {
                self.apply_agent_event(Uuid::from_bytes(id), event, cx)?;
            }
            protocol::HostMessage::PromptNamed { participant, name } => {
                self.prompt_names
                    .insert(ParticipantId::from_bytes(participant), name.into());
            }
            protocol::HostMessage::AgentEnded {
                id,
                outcome,
                duration,
            } => self.end_agent_run(id, outcome, duration, cx),
        }
        Ok(())
    }

    /// How long the agent has spent generating in this thread.
    pub(crate) fn generation_time(&self) -> Duration {
        self.timeline
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::Agent(message) => message.run.duration(),
                TimelineMessage::User(_) => None,
            })
            .sum()
    }

    /// How much of the context window the thread fills right now: the last
    /// measured count, plus an estimate for the output streamed since. `None`
    /// until there is either.
    pub(crate) fn live_context_tokens(&self) -> Option<u64> {
        if self.context_tokens.is_none() && self.streamed_bytes == 0 {
            return None;
        }
        Some(self.context_tokens.unwrap_or(0) + self.streamed_bytes.div_ceil(BYTES_PER_TOKEN))
    }

    /// What every copy of this thread must have identically; see
    /// [`Conversation`].
    #[cfg(test)]
    pub(crate) fn conversation(&self) -> Conversation {
        let snapshot = self.to_protocol();
        let files = self
            .timeline
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => Some(&group.blocks),
                TimelineMessage::Agent(_) => None,
            })
            .flatten()
            .flat_map(|block| &block.attachments)
            .map(|record| {
                (
                    record.id.as_uuid().into_bytes(),
                    self.draft
                        .files
                        .get(&record.id)
                        .map(|file| file.bytes().to_vec()),
                )
            })
            .collect();
        let agent_output = self
            .timeline
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::Agent(message) => Some(AgentShown {
                    output: message.output.clone(),
                    comment_responses: message
                        .comment_responses
                        .iter()
                        .map(|response| {
                            (response.id, response.comment_id, response.response.clone())
                        })
                        .collect(),
                }),
                TimelineMessage::User(_) => None,
            })
            .collect();
        Conversation {
            timeline: snapshot.messages,
            agent_output,
            transcript: self.transcript.clone(),
            prompt_names: snapshot.prompt_names,
            files,
        }
    }

    fn set_timeline(&mut self, timeline: Vec<TimelineMessage>) {
        self.generating = timeline.iter().any(
            |message| matches!(message, TimelineMessage::Agent(message) if message.is_generating()),
        );
        self.timeline = timeline;
    }

    pub(crate) fn agent_message_mut(&mut self, id: uuid::Bytes) -> Option<&mut AgentMessage> {
        let id = Uuid::from_bytes(id);
        self.timeline.iter_mut().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if message.id == id => Some(message),
            _ => None,
        })
    }
}

/// A thread's conversation: what was said in it, and what the agent was sent.
/// The host and every collaborator have the same.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct Conversation {
    pub(crate) timeline: Vec<protocol::TimelineMessage>,
    /// What each agent message shows, which is derived rather than sent.
    pub(crate) agent_output: Vec<AgentShown>,
    pub(crate) transcript: Vec<RigMessage>,
    pub(crate) prompt_names: Vec<(uuid::Bytes, String)>,
    /// The bytes of each file in the timeline, `None` while missing.
    pub(crate) files: Vec<(uuid::Bytes, Option<Vec<u8>>)>,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct AgentShown {
    pub(crate) output: crate::timeline::AgentOutput,
    /// Each reply's id, the comment it answers, and its text.
    pub(crate) comment_responses: Vec<(Uuid, Uuid, String)>,
}

fn parse_transcript(transcript: &[protocol::Json<RigMessage>]) -> anyhow::Result<Vec<RigMessage>> {
    transcript
        .iter()
        .enumerate()
        .map(|(index, message)| {
            message
                .to_rig()
                .with_context(|| format!("invalid transcript message {index} from host"))
        })
        .collect()
}

impl protocol::ThreadSnapshot {
    pub(crate) fn into_native(
        self,
        cx: &mut impl AppContext,
    ) -> (ThreadSummary, Vec<TimelineMessage>) {
        let summary = ThreadSummary {
            id: Uuid::from_bytes(self.id),
            title: self.title,
        };
        let timeline = self
            .messages
            .into_iter()
            .map(|message| message.into_native(cx))
            .collect();
        (summary, timeline)
    }
}

#[derive(Default)]
pub(crate) struct ThreadStore {
    pub(crate) threads: VecDeque<Entity<Thread>>,
}

impl ThreadStore {
    pub(crate) fn thread(&self, thread_id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        self.threads
            .iter()
            .find(|thread| thread.read(cx).instance_id == thread_id)
            .cloned()
    }
}
