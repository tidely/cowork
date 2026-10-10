//! A thread: its timeline, draft, model, and how it is shared, kept in
//! step with the host by applying the same events on every participant.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use ::draft::{AttachmentId, DraftItem, ItemId};
use agent::TurnFold;
use anyhow::Context as _;

#[path = "thread_draft.rs"]
pub(crate) mod draft;
mod permissions;
#[path = "sharing.rs"]
pub(crate) mod sharing;
#[path = "submission.rs"]
pub(crate) mod submission;
pub(crate) use permissions::{
    ApproveTools, ChangeModel, ControlGeneration, DenialReason, EditDraft, ManageAccess, PeerMode,
    PeerPermissions, PermissionDenied, PermissionOperation,
};

use self::draft::{DraftEditorState, ThreadDraft};
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
    project_folders::{self, ProjectFolder},
    protocol,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadOwnership {
    Local,
    Remote,
}

impl ThreadOwnership {
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
    participant_id: ParticipantId,
    /// Connected participants in join order, starting with the host. Empty
    /// while the thread is not shared.
    participants: Vec<ParticipantId>,
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
    model: Option<ModelRef>,
    /// The models this thread can run: the catalog of whoever runs its agent,
    /// which is this app's own unless the thread is mirrored.
    models: Arc<ModelCatalog>,
    /// How much of the context window the thread fills, as the provider
    /// reported at the end of the last agent request that reported usage.
    /// `None` until one has; see [`Thread::live_context_tokens`].
    pub(crate) context_tokens: Option<u64>,
    /// Bytes of agent output streamed since `context_tokens` was measured.
    /// Every participant counts them from the same events, so their
    /// estimates agree.
    pub(crate) streamed_bytes: u64,
    pub(crate) timeline: Vec<TimelineMessage>,
    draft: ThreadDraft,
    peer_permissions: PeerPermissions,
    /// A peer's epoch advances only when its current replica is rejected.
    /// Keeping this independent of policy prevents queued updates from becoming
    /// valid merely because Write was regranted.
    peer_draft_generations: HashMap<ParticipantId, u64>,
    /// The local mirror's epoch, installed only with authoritative draft state.
    draft_generation: u64,
    pub(crate) generating: bool,
    pub(crate) sharing: ThreadSharing,
    ownership: ThreadOwnership,
    /// The folders the thread works in, in the order the host added them.
    /// The host knows their paths; a mirror only their names, since they
    /// are on the host's machine. Not yet given to the agent.
    project_folders: Vec<ProjectFolder>,
}

/// A host snapshot whose transcript and agent runs have been checked before
/// creating an entity or changing existing state. Keep the parsed transcript
/// alongside it so installation does not decode the same messages again.
pub(crate) struct PreparedWelcome {
    welcome: protocol::Welcome,
    transcript: Vec<RigMessage>,
}

impl Thread {
    pub(crate) fn new_local(
        title: String,
        timeline: Vec<TimelineMessage>,
        mut draft: ThreadDraft,
        participant_id: ParticipantId,
        models: Arc<ModelCatalog>,
        model: Option<ModelRef>,
    ) -> Self {
        // A draft prepared before the thread existed is already in its initial
        // snapshot; do not later publish its old edits as new local updates.
        draft.take_local_update();
        let id = Uuid::new_v4();
        Self {
            instance_id: id,
            summary: ThreadSummary { id, title },
            participant_id,
            participants: Vec::new(),
            profiles: HashMap::new(),
            transcript: Vec::new(),
            agent_turn: TurnFold::default(),
            prompt_names: HashMap::new(),
            tokens_used: 0,
            model,
            models,
            context_tokens: None,
            streamed_bytes: 0,
            timeline,
            draft,
            peer_permissions: PeerPermissions::default(),
            peer_draft_generations: HashMap::new(),
            draft_generation: 0,
            generating: false,
            sharing: ThreadSharing::NotShared,
            ownership: ThreadOwnership::Local,
            project_folders: Vec::new(),
        }
    }

    pub(crate) fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }
    pub(crate) fn participants(&self) -> &[ParticipantId] {
        &self.participants
    }
    pub(crate) fn ownership(&self) -> ThreadOwnership {
        self.ownership
    }
    pub(crate) fn models(&self) -> &Arc<ModelCatalog> {
        &self.models
    }
    pub(crate) fn draft_generation(&self) -> u64 {
        self.draft_generation
    }

    /// The epoch to include in this peer's Welcome, captured together with its
    /// draft snapshot and event subscription on the foreground thread.
    pub(crate) fn draft_generation_for(&self, actor: ParticipantId) -> u64 {
        self.peer_draft_generations
            .get(&actor)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn draft(&self) -> &ThreadDraft {
        &self.draft
    }
    pub(crate) fn model(&self) -> Option<&ModelRef> {
        self.model.as_ref()
    }

    pub(crate) fn update_draft_editors<R>(
        &mut self,
        f: impl FnOnce(&[DraftItem], &mut DraftEditorState) -> R,
    ) -> R {
        self.draft.update_draft_editors(f)
    }

    /// Full authoritative state and the already assigned epoch for one peer.
    /// Reading another reset for stale queued bytes never advances the epoch.
    pub(crate) fn draft_reset_for(&self, actor: ParticipantId) -> protocol::HostMessage {
        protocol::HostMessage::DraftReset {
            generation: self.draft_generation_for(actor),
            state: self.draft.encode_state(),
        }
    }

    /// Registers hosting without exposing mutable membership or deriving the
    /// authority role from transport state. Mirrors cannot become hosts here.
    pub(crate) fn start_hosting(&mut self, endpoint: Endpoint) -> bool {
        if !self.is_host()
            || matches!(
                self.sharing,
                ThreadSharing::Shared { .. } | ThreadSharing::Connected { .. }
            )
        {
            return false;
        }
        let (events, _) = broadcast::channel(THREAD_EVENT_CAPACITY);
        self.participants.clear();
        self.participants.push(self.participant_id);
        self.peer_draft_generations.clear();
        self.draft.presence.clear();
        self.sharing = ThreadSharing::Shared { endpoint, events };
        true
    }

    /// Cleans departing peers while membership and the broadcast channel still
    /// exist. Later serving-task teardown can safely repeat participant_left.
    pub(crate) fn stop_hosting(&mut self, cx: &mut impl AppContext) -> Option<Endpoint> {
        if !self.is_host() || !matches!(self.sharing, ThreadSharing::Shared { .. }) {
            return None;
        }
        self.cleanup_hosted_participants(cx);
        let ThreadSharing::Shared { endpoint, .. } =
            std::mem::replace(&mut self.sharing, ThreadSharing::NotShared)
        else {
            unreachable!()
        };
        Some(endpoint)
    }

    fn cleanup_hosted_participants(&mut self, cx: &mut impl AppContext) {
        let peers = self
            .participants
            .iter()
            .copied()
            .filter(|actor| *actor != self.participant_id)
            .collect::<Vec<_>>();
        for actor in peers {
            self.participant_left(actor, cx);
        }
        self.participants.clear();
        self.peer_draft_generations.clear();
        self.draft.presence.clear();
    }

    pub(crate) fn project_folders(&self) -> &[ProjectFolder] {
        &self.project_folders
    }

    /// Adds folders on this machine to the project, skipping ones already
    /// in it. Only the host can, as the folders are on its machine.
    pub(crate) fn add_project_folders(&mut self, paths: Vec<PathBuf>) -> bool {
        if !self.is_host() {
            return false;
        }
        if project_folders::add_folders(&mut self.project_folders, paths) {
            self.publish_project_folders();
        }
        true
    }

    pub(crate) fn remove_project_folder(&mut self, path: &Path) -> bool {
        if !self.is_host() {
            return false;
        }
        if project_folders::remove_folder(&mut self.project_folders, path) {
            self.publish_project_folders();
        }
        true
    }

    fn project_folder_names(&self) -> Vec<String> {
        self.project_folders
            .iter()
            .map(|folder| folder.name.to_string())
            .collect()
    }

    /// Published rather than emitted: the host keeps paths the event leaves
    /// out.
    fn publish_project_folders(&self) {
        self.publish(protocol::HostMessage::ProjectFoldersChanged(
            self.project_folder_names(),
        ));
    }

    /// Source catalogs are host maintenance, not an Admin peer model command.
    pub(crate) fn set_model_catalog(
        &mut self,
        catalog: ModelCatalog,
        cx: &mut impl AppContext,
    ) -> bool {
        if !self.is_host() {
            return false;
        }
        if *self.models != catalog {
            self.emit(protocol::HostMessage::ModelCatalogChanged(catalog), cx);
        }
        true
    }

    /// Cosmetic profile changes are allowed for every connected member.
    pub(crate) fn host_profile(
        &mut self,
        actor: ParticipantId,
        profile: protocol::Profile,
        cx: &mut impl AppContext,
    ) -> bool {
        if !self.is_host() || (actor != self.participant_id && !self.participants.contains(&actor))
        {
            return false;
        }
        self.emit(
            protocol::HostMessage::ProfileChanged {
                participant: actor.into_bytes(),
                profile,
            },
            cx,
        );
        true
    }

    pub(crate) fn update_local_profile(
        &mut self,
        profile: &Profile,
        cx: &mut impl AppContext,
    ) -> bool {
        if self.is_host() {
            return self.host_profile(self.participant_id, profile.to_protocol(), cx);
        }
        if !self.participants.contains(&self.participant_id) {
            return false;
        }
        self.request(protocol::CollaboratorMessage::Profile(
            profile.to_protocol(),
        ))
    }

    /// Marks only a matching locally authored file the host already has in full.
    /// Remote uploads use receive_upload's accepted-transfer path instead.
    pub(crate) fn mark_local_attachment_stored(
        &mut self,
        id: AttachmentId,
        cx: &mut impl AppContext,
    ) -> bool {
        if !self.is_host() {
            return false;
        }
        let Some(file) = self.draft.files.get(&id) else {
            return false;
        };
        let accepted = self.draft.attachment_records().iter().any(|record| {
            record.id == id
                && record.creator == self.participant_id.as_uuid()
                && record.name == file.name
                && record.kind == file.kind()
                && record.size == file.len()
        });
        if !accepted {
            return false;
        }
        self.emit(
            protocol::HostMessage::AttachmentStored {
                id: id.as_uuid().into_bytes(),
                uploader: self.participant_id.into_bytes(),
            },
            cx,
        );
        true
    }

    /// Builds the local mirror of a thread hosted by someone else.
    pub(crate) fn from_prepared_welcome(
        prepared: PreparedWelcome,
        draft: ThreadDraft,
        sharing: ThreadSharing,
        cx: &mut impl AppContext,
    ) -> Self {
        let welcome = &prepared.welcome;
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
            peer_permissions: PeerPermissions::default(),
            peer_draft_generations: HashMap::new(),
            draft_generation: 0,
            generating: false,
            sharing,
            ownership: ThreadOwnership::Remote,
            project_folders: Vec::new(),
        };
        thread
            .apply_welcome(prepared, cx)
            .expect("a prepared welcome installs checked draft state");
        thread
    }

    /// Validates a host's draft snapshot and nested Rig JSON, including that
    /// agent messages fit the transcript, before creating a mirror or installing
    /// a replacement epoch into an existing one.
    pub(crate) fn prepare_welcome(welcome: protocol::Welcome) -> anyhow::Result<PreparedWelcome> {
        // Epoch changes install a fresh document, so check its state before
        // constructing a mirror or changing any existing thread state.
        let snapshot = ::draft::Draft::new();
        snapshot
            .apply_update(&welcome.draft)
            .context("invalid host draft snapshot")?;
        snapshot.validate().context("invalid host draft snapshot")?;
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
        validate_agent_runs(&transcript, &agent_messages).context("invalid host snapshot")?;
        Ok(PreparedWelcome {
            welcome,
            transcript,
        })
    }

    /// Replaces authoritative state, retaining unsent edits only when the
    /// host's peer epoch still matches the local replica's epoch.
    fn try_rebase(
        &mut self,
        welcome: protocol::Welcome,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        let prepared = Self::prepare_welcome(welcome)?;
        self.apply_welcome(prepared, cx)
    }

    /// Installs a checked snapshot with epoch-aware draft replacement.
    fn apply_welcome(
        &mut self,
        prepared: PreparedWelcome,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        let PreparedWelcome {
            welcome,
            transcript,
        } = prepared;
        let protocol::Welcome {
            participant_id,
            mut thread,
            draft,
            draft_generation,
            presence,
            stored_attachments,
        } = welcome;
        self.install_draft_state(draft_generation, &draft)?;
        self.draft.stored = stored_attachments
            .into_iter()
            .map(|id| AttachmentId::from_uuid(Uuid::from_bytes(id)))
            .collect();
        let stored = &self.draft.stored;
        self.draft.uploads.retain(|id, _| !stored.contains(id));
        self.draft.keeps_removed_files = true;
        self.participant_id = ParticipantId::from_bytes(participant_id);
        self.draft.author = self.participant_id;

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
        self.peer_permissions = std::mem::take(&mut thread.peer_permissions);
        self.clear_read_only_presence(cx);
        self.models = Arc::new(std::mem::take(&mut thread.models));
        self.model = thread.model.take();
        self.context_tokens = thread.context_tokens;
        self.streamed_bytes = thread.streamed_bytes;
        self.transcript = transcript;
        self.prompt_names = std::mem::take(&mut thread.prompt_names)
            .into_iter()
            .map(|(participant, name)| (ParticipantId::from_bytes(participant), name.into()))
            .collect();
        self.project_folders = ProjectFolder::mirrored(std::mem::take(&mut thread.project_folders));
        let (summary, timeline) = thread.into_native(cx);
        self.summary = summary;
        self.set_timeline(timeline);
        self.restore_agent_output(cx);
        Ok(())
    }

    /// A repeated reset for stale queued bytes must not erase edits made from
    /// the already fresh replica. Matching epochs merge; only a newer epoch
    /// replaces the document and invalidates editor handles.
    fn install_draft_state(&mut self, generation: u64, state: &[u8]) -> anyhow::Result<()> {
        if generation < self.draft_generation {
            return Ok(());
        }
        if generation == self.draft_generation {
            self.draft.doc.apply_update(state)?;
        } else {
            self.draft.reset(state)?;
            self.draft_generation = generation;
        }
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
            peer_permissions: self.peer_permissions.clone(),
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
            project_folders: self.project_folder_names(),
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
        let Some(update) = self.draft.take_local_update() else {
            return;
        };
        match &self.sharing {
            ThreadSharing::Shared { .. } => {
                self.publish(protocol::HostMessage::DraftUpdate(update));
            }
            ThreadSharing::Connected { .. } => {
                self.request(protocol::CollaboratorMessage::DraftUpdate {
                    generation: self.draft_generation,
                    update,
                });
            }
            // Whoever joins later receives the whole draft with their welcome.
            ThreadSharing::NotShared | ThreadSharing::Sharing | ThreadSharing::Failed => {}
        }
    }

    /// Applies a collaborator's draft update and forwards it to everyone.
    ///
    /// Membership, epoch, and EditDraft permission are checked before decoding,
    /// applying, or broadcasting bytes. Rejecting the current epoch for revoked
    /// permissions advances only this peer's epoch; stale queued bytes never
    /// advance it again. The caller sends the denial and draft_reset_for(author)
    /// to that peer only, including after Write has already been regranted.
    ///
    /// Structural validation remains separate: an authorized update that
    /// breaks invariants has already been applied, and requires disconnecting
    /// the collaborator under the existing protocol validation policy.
    fn apply_collaborator_update(
        &mut self,
        author: ParticipantId,
        generation: u64,
        update: Vec<u8>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.is_host(), "only the host accepts collaborator updates");
        if author != self.participant_id && !self.participants.contains(&author) {
            return Err(PermissionDenied {
                participant: author.into_bytes(),
                operation: PermissionOperation::EditDraft,
                reason: DenialReason::NotParticipant,
            }
            .into());
        }
        let expected = self.draft_generation_for(author);
        if generation != expected {
            return Err(PermissionDenied {
                participant: author.into_bytes(),
                operation: PermissionOperation::EditDraft,
                reason: DenialReason::StaleDraftGeneration,
            }
            .into());
        }
        match self.with_authorized::<EditDraft, _>(author, |auth| {
            auth.apply_collaborator_update(generation, update)
        }) {
            Ok(result) => result,
            Err(denied) => {
                let next = expected
                    .checked_add(1)
                    .context("draft generation exhausted")?;
                self.peer_draft_generations.insert(author, next);
                Err(denied.into())
            }
        }
    }

    fn apply_collaborator_update_authorized(
        &mut self,
        author: ParticipantId,
        update: Vec<u8>,
    ) -> anyhow::Result<()> {
        let before = self.draft.doc.items();
        let attachments_before = self.draft.attachment_records();
        self.draft.doc.apply_update(&update)?;
        self.publish(protocol::HostMessage::DraftUpdate(update));
        self.draft.doc.validate()?;
        ::draft::verify_change(&before, &self.draft.doc.items(), author.as_uuid())?;

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
        anyhow::ensure!(self.is_host(), "only the host accepts uploads");
        let id = AttachmentId::from_uuid(Uuid::from_bytes(chunk.id));
        let member = uploader == self.participant_id || self.participants.contains(&uploader);
        if !member {
            return Err(PermissionDenied {
                participant: uploader.into_bytes(),
                operation: PermissionOperation::UploadAttachment,
                reason: DenialReason::NotParticipant,
            }
            .into());
        }
        if self
            .draft
            .incoming
            .get(&id)
            .is_some_and(|file| file.uploader != Some(uploader))
        {
            return Err(PermissionDenied {
                participant: uploader.into_bytes(),
                operation: PermissionOperation::UploadAttachment,
                reason: DenialReason::InsufficientMode,
            }
            .into());
        }
        // A record accepted while the uploader could write remains a bounded
        // transfer grant after downgrade. Pre-record bytes are never such a grant.
        if self
            .check_permission(uploader, PermissionOperation::UploadAttachment)
            .is_err()
        {
            let accepted = self.accepted_upload(uploader, &chunk);
            if !accepted {
                return Err(PermissionDenied {
                    participant: uploader.into_bytes(),
                    operation: PermissionOperation::UploadAttachment,
                    reason: DenialReason::InsufficientMode,
                }
                .into());
            }
        }
        if let Some(id) = self.draft.receive_chunk(chunk, Some(uploader))? {
            self.store_upload(uploader, id)?;
        }
        Ok(())
    }

    fn accepted_upload(&self, uploader: ParticipantId, chunk: &protocol::AttachmentChunk) -> bool {
        let id = AttachmentId::from_uuid(Uuid::from_bytes(chunk.id));
        self.draft.attachment_records().iter().any(|record| {
            record.id == id
                && record.creator == uploader.as_uuid()
                && record.name == chunk.name
                && record.size == chunk.total
                && record.kind == crate::attachments::kind_from_protocol(chunk.kind)
        })
    }

    /// Advances the local sender's bookkeeping without granting draft-edit
    /// access. An already announced local file may finish after downgrade.
    pub(crate) fn mark_upload_progress(&mut self, id: AttachmentId, offset: u64) -> bool {
        if (!self.is_host() && !self.participants.contains(&self.participant_id))
            || self.draft.stored.contains(&id)
            || !self
                .draft
                .attachment_records()
                .iter()
                .any(|record| record.id == id && record.creator == self.participant_id.as_uuid())
            || !self
                .draft
                .files
                .get(&id)
                .is_some_and(|file| offset <= file.bytes().len() as u64)
        {
            return false;
        }
        self.draft.uploads.insert(id, offset);
        true
    }

    /// Cancelling an accepted transfer remains allowed after downgrade, but
    /// cannot discard another participant's bytes or remove a canonical record.
    pub(crate) fn cancel_upload(
        &mut self,
        uploader: ParticipantId,
        id: AttachmentId,
    ) -> Result<(), PermissionDenied> {
        if !self.is_host()
            || (uploader != self.participant_id && !self.participants.contains(&uploader))
        {
            return Err(PermissionDenied {
                participant: uploader.into_bytes(),
                operation: PermissionOperation::UploadAttachment,
                reason: DenialReason::NotParticipant,
            });
        }
        if self
            .draft
            .incoming
            .get(&id)
            .is_some_and(|file| file.uploader == Some(uploader))
        {
            self.draft.incoming.remove(&id);
            self.draft.discarded.insert(id);
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
        let eligible = |record: &::draft::AttachmentRecord| {
            record.creator != participant.as_uuid()
                && self.draft.stored.contains(&record.id)
                && self.draft.files.contains_key(&record.id)
        };
        let timeline = self
            .timeline
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => Some(group),
                TimelineMessage::Agent(_) => None,
            })
            .flat_map(|group| &group.blocks)
            .flat_map(|block| &block.attachments)
            .filter(|record| eligible(record))
            .map(|record| record.id);
        let draft = self
            .draft
            .attachment_records()
            .into_iter()
            .filter(eligible)
            .map(|record| record.id);
        timeline.chain(draft).collect()
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
        if !self.is_host()
            || (participant != self.participant_id && !self.participants.contains(&participant))
        {
            return;
        }
        if self
            .check_permission(participant, PermissionOperation::EditDraft)
            .is_err()
        {
            // Readers may neither announce work that blocks submission nor
            // nominate an empty canonical item for deletion by leaving it.
            self.emit(
                protocol::HostMessage::Presence {
                    participant: participant.into_bytes(),
                    presence: protocol::Presence::default(),
                },
                cx,
            );
            return;
        }
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
        if !self.is_host()
            || participant == self.participant_id
            || !self.participants.contains(&participant)
        {
            return;
        }
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
        let operation = match &request {
            protocol::CollaboratorMessage::DraftUpdate { generation, .. } => {
                if *generation != self.draft_generation {
                    return false;
                }
                Some(PermissionOperation::EditDraft)
            }
            protocol::CollaboratorMessage::Submit { .. }
            | protocol::CollaboratorMessage::Stop { .. } => {
                Some(PermissionOperation::ControlGeneration)
            }
            protocol::CollaboratorMessage::SelectModel(_) => Some(PermissionOperation::ChangeModel),
            protocol::CollaboratorMessage::DecideToolCall { .. } => {
                Some(PermissionOperation::ApproveTools)
            }
            protocol::CollaboratorMessage::AttachmentData(_) => {
                Some(PermissionOperation::UploadAttachment)
            }
            _ => None,
        };
        if operation.is_some_and(|operation| {
            self.check_permission(self.participant_id, operation)
                .is_err()
                && !matches!(&request, protocol::CollaboratorMessage::AttachmentData(chunk)
                    if self.participants.contains(&self.participant_id)
                        && self.accepted_upload(self.participant_id, chunk))
        }) {
            return false;
        }
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
    pub(crate) fn select_model(
        &mut self,
        model: ModelRef,
        cx: &mut impl AppContext,
    ) -> Result<(), PermissionDenied> {
        self.with_authorized::<ChangeModel, _>(self.participant_id, |authorized| {
            authorized.select_model(model, cx)
        })
    }

    fn select_model_authorized(&mut self, model: ModelRef, cx: &mut impl AppContext) {
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
    fn host_select_model(&mut self, model: ModelRef, cx: &mut impl AppContext) {
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
    fn publish(&self, event: protocol::HostMessage) {
        if !self.is_host() {
            return;
        }
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
    fn emit(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        // This is the authoritative host event path, never a way for a mirror
        // to optimistically change its model or delegate access to itself.
        if !self.is_host() {
            eprintln!("only the host can emit thread events");
            return;
        }
        // Checked up front so that an unshared thread, which is the common
        // case, never pays to clone a streamed chunk.
        if matches!(self.sharing, ThreadSharing::Shared { .. }) {
            self.publish(event.clone());
        }
        self.apply(event, cx);
    }

    /// Deliberate trusted host setup for tests outside the thread subtree.
    #[cfg(test)]
    pub(crate) fn emit_for_test(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        self.emit(event, cx);
    }

    #[cfg(test)]
    pub(crate) fn publish_for_test(&self, event: protocol::HostMessage) {
        self.publish(event);
    }

    #[cfg(test)]
    pub(crate) fn apply_for_test(
        &mut self,
        event: protocol::HostMessage,
        cx: &mut impl AppContext,
    ) {
        self.apply(event, cx);
    }

    #[cfg(test)]
    pub(crate) fn try_apply_for_test(
        &mut self,
        event: protocol::HostMessage,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        self.try_apply(event, cx)
    }

    /// Keeps epoch and permission checks in external regression harnesses.
    #[cfg(test)]
    pub(crate) fn apply_collaborator_update_for_test(
        &mut self,
        actor: ParticipantId,
        generation: u64,
        update: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.apply_collaborator_update(actor, generation, update)
    }

    /// Folds a thread event into the timeline.
    fn apply(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        self.try_apply(event, cx).expect("a valid thread event");
    }

    /// Applies an event received from an untrusted host.
    fn try_apply(
        &mut self,
        event: protocol::HostMessage,
        cx: &mut impl AppContext,
    ) -> anyhow::Result<()> {
        match event {
            protocol::HostMessage::Welcome(welcome) => self.try_rebase(*welcome, cx)?,
            // Only ever sent in place of the first `Welcome`, which the join
            // handshake consumes.
            protocol::HostMessage::Rejected(_) => {}
            protocol::HostMessage::PermissionDenied(denied) => {
                eprintln!("{denied}");
            }
            protocol::HostMessage::DraftReset { generation, state } => {
                self.install_draft_state(generation, &state)?
            }
            protocol::HostMessage::DefaultPeerModeChanged(mode) => {
                self.peer_permissions.set_default_mode(mode);
                self.prune_unaccepted_uploads();
                self.clear_read_only_presence(cx);
            }
            protocol::HostMessage::PeerModeOverrideChanged { participant, mode } => {
                self.peer_permissions
                    .set_override(ParticipantId::from_bytes(participant), mode);
                self.prune_unaccepted_uploads();
                self.clear_read_only_presence(cx);
            }
            protocol::HostMessage::ParticipantJoined {
                participant,
                profile,
            } => {
                let participant = ParticipantId::from_bytes(participant);
                if !self.participants.contains(&participant) {
                    self.participants.push(participant);
                }
                if self.is_host() && participant != self.participant_id {
                    self.peer_draft_generations.entry(participant).or_insert(0);
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
                self.peer_permissions.set_override(participant, None);
                self.peer_draft_generations.remove(&participant);
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
            protocol::HostMessage::ProjectFoldersChanged(names) => {
                self.project_folders = ProjectFolder::mirrored(names);
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
                self.transcript.push(prompt.to_rig()?);
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
            protocol::HostMessage::ToolApprovalRequested { id, call } => {
                self.await_tool_approval(Uuid::from_bytes(id), call.to_call_id()?)?;
            }
            protocol::HostMessage::ToolApprovalResolved { id, call } => {
                self.resolve_tool_approval(Uuid::from_bytes(id), &call.to_call_id()?);
            }
        }
        Ok(())
    }

    /// Revocation clears attendance and pending work without routing the
    /// cleanup through host_presence: no reader-controlled empty-item deletion.
    /// The host rebroadcasts this cleanup, while mirrors also clear immediately
    /// when applying policy so submission gating never waits for peer cooperation.
    fn clear_read_only_presence(&mut self, cx: &mut impl AppContext) {
        let host = self.host_id();
        let cleared = self
            .draft
            .presence
            .iter()
            .filter_map(|(actor, (presence, _))| {
                (Some(*actor) != host
                    && !self.peer_permissions.mode_for(*actor).can_edit_draft()
                    && *presence != protocol::Presence::default())
                .then_some(*actor)
            })
            .sorted_by_key(|actor| actor.into_bytes())
            .collect::<Vec<_>>();
        for actor in cleared {
            let presence = protocol::Presence::default();
            if self.is_host() {
                self.emit(
                    protocol::HostMessage::Presence {
                        participant: actor.into_bytes(),
                        presence,
                    },
                    cx,
                );
            } else {
                self.draft.set_presence(actor, presence);
            }
        }
    }

    /// Discard unannounced buffers as soon as their uploader loses Write.
    /// Announced records remain transfer grants until removed or disconnected.
    fn prune_unaccepted_uploads(&mut self) {
        if !self.is_host() {
            return;
        }
        let records = self.draft.attachment_records();
        let permissions = &self.peer_permissions;
        let host = self.participant_id;
        self.draft.incoming.retain(|id, file| {
            file.uploader.is_none_or(|uploader| {
                uploader == host
                    || permissions.mode_for(uploader).can_edit_draft()
                    || records
                        .iter()
                        .any(|record| record.id == *id && record.creator == uploader.as_uuid())
            })
        });
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
                        .map(|response| CommentResponseShown {
                            id: response.id,
                            comment_id: response.comment_id,
                            response: response.response.clone(),
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
            prompt_names: snapshot
                .prompt_names
                .into_iter()
                .map(|(participant, name)| PromptName { participant, name })
                .collect(),
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
    pub(crate) prompt_names: Vec<PromptName>,
    /// The bytes of each file in the timeline, `None` while missing.
    pub(crate) files: Vec<(uuid::Bytes, Option<Vec<u8>>)>,
}

/// The name a participant is known by in the agent's prompts.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct PromptName {
    pub(crate) participant: uuid::Bytes,
    pub(crate) name: String,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct AgentShown {
    pub(crate) output: crate::timeline::AgentOutput,
    pub(crate) comment_responses: Vec<CommentResponseShown>,
}

/// A reply to a comment, without its participant-local view state.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct CommentResponseShown {
    pub(crate) id: Uuid,
    pub(crate) comment_id: Uuid,
    pub(crate) response: String,
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

#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod tests;

impl ThreadStore {
    pub(crate) fn thread(&self, thread_id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        self.threads
            .iter()
            .find(|thread| thread.read(cx).instance_id == thread_id)
            .cloned()
    }
}
