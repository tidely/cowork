//! Host-owned access policy and borrowed, operation-specific views of a thread.
//!
//! An authorization is checked against current membership and policy on every
//! operation. Views cannot be constructed by callers or detached from the
//! closure that borrows the thread; they expose neither `Thread` nor its fields.

use std::{collections::BTreeMap, fmt, marker::PhantomData};

use gpui::AppContext;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Thread, ThreadOwnership, draft::ThreadDraft};
use crate::{models::ModelRef, participant::ParticipantId, protocol};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PeerMode {
    ReadOnly,
    Write,
    /// Preserves the behavior of threads created before access controls.
    #[default]
    Admin,
}

impl PeerMode {
    pub(crate) const fn can_edit_draft(self) -> bool {
        matches!(self, Self::Write | Self::Admin)
    }

    pub(crate) const fn can_control_generation(self) -> bool {
        matches!(self, Self::Admin)
    }

    pub(crate) const fn can_change_model(self) -> bool {
        matches!(self, Self::Admin)
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::Write => "Write",
            Self::Admin => "Admin",
        }
    }
}

/// Missing overrides inherit the *current* default, not the default at join.
/// An explicit override equal to today's default remains explicit: it must
/// survive a later default change. UUID byte keys keep wire ordering stable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerPermissions {
    default_mode: PeerMode,
    overrides: BTreeMap<uuid::Bytes, PeerMode>,
}

impl PeerPermissions {
    pub(crate) fn default_mode(&self) -> PeerMode {
        self.default_mode
    }

    pub(crate) fn override_for(&self, participant: ParticipantId) -> Option<PeerMode> {
        self.overrides.get(&participant.into_bytes()).copied()
    }

    pub(crate) fn mode_for(&self, participant: ParticipantId) -> PeerMode {
        self.override_for(participant).unwrap_or(self.default_mode)
    }

    pub(super) fn set_default_mode(&mut self, mode: PeerMode) {
        self.default_mode = mode;
    }

    pub(super) fn set_override(&mut self, participant: ParticipantId, mode: Option<PeerMode>) {
        match mode {
            Some(mode) => self.overrides.insert(participant.into_bytes(), mode),
            None => self.overrides.remove(&participant.into_bytes()),
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PermissionOperation {
    EditDraft,
    ControlGeneration,
    ChangeModel,
    ManageAccess,
    UploadAttachment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DenialReason {
    NotParticipant,
    InsufficientMode,
    HostOnly,
    /// Encoded bytes belong to a replica the host has already rejected.
    StaleDraftGeneration,
}

/// A runtime command rejection, distinct from the stable join rejection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PermissionDenied {
    pub(crate) participant: uuid::Bytes,
    pub(crate) operation: PermissionOperation,
    pub(crate) reason: DenialReason,
}

impl fmt::Display for PermissionDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "permission denied for {:?}: {:?}",
            self.operation, self.reason
        )
    }
}

impl std::error::Error for PermissionDenied {}

mod sealed {
    pub trait Sealed {}
}

pub(crate) trait Operation: sealed::Sealed {
    const OPERATION: PermissionOperation;
}

pub(crate) struct EditDraft;
pub(crate) struct ControlGeneration;
pub(crate) struct ChangeModel;
pub(crate) struct ManageAccess;

macro_rules! operation {
    ($type:ident) => {
        impl sealed::Sealed for $type {}
        impl Operation for $type {
            const OPERATION: PermissionOperation = PermissionOperation::$type;
        }
    };
}
operation!(EditDraft);
operation!(ControlGeneration);
operation!(ChangeModel);
operation!(ManageAccess);

pub(crate) struct Authorized<'a, Op: Operation> {
    thread: &'a mut Thread,
    actor: ParticipantId,
    operation: PhantomData<Op>,
}

impl Thread {
    pub(crate) fn peer_permissions(&self) -> &PeerPermissions {
        &self.peer_permissions
    }

    pub(crate) fn is_host(&self) -> bool {
        self.ownership == ThreadOwnership::Local
    }

    pub(super) fn host_id(&self) -> Option<ParticipantId> {
        if self.is_host() {
            Some(self.participant_id)
        } else {
            self.participants.first().copied()
        }
    }

    pub(crate) fn local_mode(&self) -> PeerMode {
        if self.host_id() == Some(self.participant_id) {
            PeerMode::Admin
        } else if self.participants.contains(&self.participant_id) {
            self.peer_permissions.mode_for(self.participant_id)
        } else {
            PeerMode::ReadOnly
        }
    }

    pub(crate) fn can_edit_draft(&self) -> bool {
        self.local_mode().can_edit_draft()
    }
    pub(crate) fn can_control_generation(&self) -> bool {
        self.local_mode().can_control_generation()
    }
    pub(crate) fn can_change_model(&self) -> bool {
        self.local_mode().can_change_model()
    }

    pub(super) fn check_permission(
        &self,
        actor: ParticipantId,
        operation: PermissionOperation,
    ) -> Result<(), PermissionDenied> {
        let reason = if !self.is_host() && actor != self.participant_id {
            Some(DenialReason::NotParticipant)
        } else if self.host_id() == Some(actor) {
            // A peer may know the host's identity but may not manage its mirror.
            if operation == PermissionOperation::ManageAccess && !self.is_host() {
                Some(DenialReason::HostOnly)
            } else {
                None
            }
        } else if !self.participants.contains(&actor) {
            Some(DenialReason::NotParticipant)
        } else if operation == PermissionOperation::ManageAccess {
            Some(DenialReason::HostOnly)
        } else {
            let mode = self.peer_permissions.mode_for(actor);
            let allowed = match operation {
                PermissionOperation::EditDraft | PermissionOperation::UploadAttachment => {
                    mode.can_edit_draft()
                }
                PermissionOperation::ControlGeneration => mode.can_control_generation(),
                PermissionOperation::ChangeModel => mode.can_change_model(),
                PermissionOperation::ManageAccess => false,
            };
            (!allowed).then_some(DenialReason::InsufficientMode)
        };
        match reason {
            Some(reason) => Err(PermissionDenied {
                participant: actor.into_bytes(),
                operation,
                reason,
            }),
            None => Ok(()),
        }
    }

    pub(crate) fn with_authorized<Op: Operation, R>(
        &mut self,
        actor: ParticipantId,
        f: impl for<'a> FnOnce(Authorized<'a, Op>) -> R,
    ) -> Result<R, PermissionDenied> {
        self.check_permission(actor, Op::OPERATION)?;
        Ok(f(Authorized {
            thread: self,
            actor,
            operation: PhantomData,
        }))
    }
}

impl Authorized<'_, EditDraft> {
    pub(crate) fn edit<R>(self, f: impl FnOnce(&mut ThreadDraft) -> R) -> R {
        let result = f(&mut self.thread.draft);
        self.thread.flush_draft();
        result
    }

    pub(crate) fn apply_collaborator_update(
        self,
        generation: u64,
        update: Vec<u8>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.thread.is_host(),
            "only the host accepts collaborator updates"
        );
        if generation != self.thread.draft_generation_for(self.actor) {
            return Err(PermissionDenied {
                participant: self.actor.into_bytes(),
                operation: PermissionOperation::EditDraft,
                reason: DenialReason::StaleDraftGeneration,
            }
            .into());
        }
        self.thread
            .apply_collaborator_update_authorized(self.actor, update)
    }
}

impl Authorized<'_, ControlGeneration> {
    /// Consuming a draft is a host operation, but requires Admin rather than
    /// Write. This deliberately does not depend on the local user's mode.
    pub(crate) fn take_submission<R>(
        self,
        f: impl FnOnce(&mut ThreadDraft) -> R,
    ) -> Result<R, PermissionDenied> {
        if !self.thread.is_host() {
            return Err(PermissionDenied {
                participant: self.actor.into_bytes(),
                operation: PermissionOperation::ControlGeneration,
                reason: DenialReason::HostOnly,
            });
        }
        let result = f(&mut self.thread.draft);
        self.thread.flush_draft();
        Ok(result)
    }

    pub(crate) fn request_submit(self) -> bool {
        self.thread.flush_draft();
        self.thread.request(protocol::CollaboratorMessage::Submit {
            sequence: self.thread.submission_count(),
        })
    }

    pub(crate) fn request_stop(self, message_id: Uuid) -> bool {
        self.thread.request(protocol::CollaboratorMessage::Stop {
            message_id: message_id.into_bytes(),
        })
    }
}

impl Authorized<'_, ChangeModel> {
    pub(crate) fn select_model(self, model: ModelRef, cx: &mut impl AppContext) {
        self.thread.select_model_authorized(model, cx);
    }
}

impl Authorized<'_, ManageAccess> {
    pub(crate) fn set_default_mode(&mut self, mode: PeerMode, cx: &mut impl AppContext) {
        if self.thread.peer_permissions.default_mode() != mode {
            self.thread
                .emit(protocol::HostMessage::DefaultPeerModeChanged(mode), cx);
        }
    }

    pub(crate) fn set_override(
        &mut self,
        participant: ParticipantId,
        mode: Option<PeerMode>,
        cx: &mut impl AppContext,
    ) {
        if participant != self.thread.participant_id
            && self.thread.participants.contains(&participant)
            && self.thread.peer_permissions.override_for(participant) != mode
        {
            self.thread.emit(
                protocol::HostMessage::PeerModeOverrideChanged {
                    participant: participant.into_bytes(),
                    mode,
                },
                cx,
            );
        }
    }
}
