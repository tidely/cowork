//! Sharing threads over iroh: hosting, serving collaborators, joining,
//! and handling collaborators' requests.

use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use anyhow::Context as _;
use draft::AttachmentId;
use gpui::{
    App, AppContext, AsyncApp, ClipboardItem, Context, Entity, Focusable, FutureExt, Subscription,
    WeakEntity, Window, div, prelude::*, px, rgb,
};
use gpui_base::input::{Input, InputEditorStyle, InputEvent, InputState};
use gpui_component::{
    Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogDescription, DialogFooter, DialogHeader, DialogTitle},
};
use iroh::{
    Endpoint, EndpointId,
    endpoint::{Accepting, presets},
};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    Cowork, MainStage,
    participant::ParticipantId,
    profile::validate_profile,
    protocol,
    thread::{
        HostPeer, PeerLink, SharingStatus, THREAD_EVENT_CAPACITY, Thread, ThreadHost, ThreadSharing,
    },
    thread_draft::ThreadDraft,
};

const COWORK_ALPN: &[u8] = b"cowork/0";

const ENDPOINT_ID_TEXT_LENGTH: usize = EndpointId::LENGTH * 2;

/// How long any single step of the collaboration handshake may take.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) fn endpoint_id_input_is_complete(input: &str) -> bool {
    input.trim().len() == ENDPOINT_ID_TEXT_LENGTH
}

pub(crate) enum JoinStatus {
    Idle,
    Joining,
    Failed(String),
}

pub(crate) struct JoinDialog {
    endpoint_token: Entity<InputState>,
    pub(crate) status: JoinStatus,
    _input_subscription: Option<Subscription>,
}

impl Cowork {
    fn prepare_thread_for_sharing(&mut self, cx: &mut Context<Self>) -> Entity<Thread> {
        if let Some(thread) = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
        {
            return thread;
        }

        let draft = std::mem::replace(
            &mut self.new_thread_draft,
            ThreadDraft::new(self.local_participant_id),
        );
        let thread = Self::new_empty_local_thread(
            draft,
            self.local_participant_id,
            self.models.clone(),
            self.new_thread_model.clone(),
            cx,
        );
        self.active_thread_id = Some(thread.read(cx).instance_id);
        self.thread_store.update(cx, |store, _| {
            store.threads.push_front(thread.clone());
        });
        thread
    }

    fn start_sharing(&mut self, thread: Entity<Thread>, cx: &mut Context<Self>) {
        let profile = self.profile.clone();
        thread.update(cx, |thread, _| {
            thread.sharing = ThreadSharing::Sharing;
            thread.profiles.insert(thread.participant_id, profile);
        });
        cx.notify();

        let (peers, accepted_peers) = async_channel::bounded(protocol::PEER_CHANNEL_CAPACITY);
        let endpoint_task = Self::bind_shared_endpoint(&self.tokio_handle, peers);

        cx.spawn(async move |this, cx| {
            let endpoint = endpoint_task
                .await
                .context("Endpoint setup task failed.")
                .and_then(|result| result);
            let endpoint = match endpoint {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    eprintln!("failed to share thread: {error:#}");
                    thread.update(cx, |thread, _| thread.sharing = ThreadSharing::Failed);
                    _ = this.update(cx, |_, cx| cx.notify());
                    return;
                }
            };
            thread.update(cx, |thread, _| {
                thread.sharing = ThreadSharing::Shared {
                    endpoint,
                    events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
                };
                thread.participants = vec![thread.participant_id];
            });
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                return;
            }

            // Peers are only served once the thread is hosting, so that every
            // one of them can subscribe to its events. Connections accepted
            // before that wait in the channel.
            let thread = thread.downgrade();
            while let Ok(peer) = accepted_peers.recv().await {
                let this = this.clone();
                let thread = thread.clone();
                cx.spawn(async move |cx| {
                    if let Err(error) = Self::serve_peer(this, thread, peer, cx).await {
                        eprintln!("stopped serving collaborator: {error:#}");
                    }
                })
                .detach();
            }
        })
        .detach();
    }

    /// Binds the endpoint collaborators dial into, forwarding every accepted
    /// connection to `peers` as a ready to use protocol channel.
    fn bind_shared_endpoint(
        tokio_handle: &tokio::runtime::Handle,
        peers: async_channel::Sender<HostPeer>,
    ) -> tokio::task::JoinHandle<anyhow::Result<Endpoint>> {
        tokio_handle.spawn(async move {
            let endpoint = Endpoint::builder(presets::N0)
                .alpns(vec![COWORK_ALPN.to_vec()])
                .bind()
                .await?;

            tokio::spawn({
                let endpoint = endpoint.clone();
                async move {
                    while let Some(incoming) = endpoint.accept().await {
                        let accepting = match incoming.accept() {
                            Ok(accepting) => accepting,
                            Err(error) => {
                                eprintln!("failed to accept connection: {error}");
                                continue;
                            }
                        };
                        let peers = peers.clone();
                        tokio::spawn(async move {
                            if let Err(error) = Self::accept_peer(accepting, peers).await {
                                eprintln!("failed to accept collaborator: {error:#}");
                            }
                        });
                    }
                }
            });

            Ok(endpoint)
        })
    }

    /// Finishes one incoming connection's handshake and hands its protocol
    /// channel over to the foreground.
    async fn accept_peer(
        accepting: Accepting,
        peers: async_channel::Sender<HostPeer>,
    ) -> anyhow::Result<()> {
        let connection = accepting.await?;
        let (send, recv) = tokio::time::timeout(PEER_TIMEOUT, connection.accept_bi())
            .await
            .context("Timed out waiting for a peer protocol stream.")??;
        // The streams keep the connection alive, so `connection` itself can go.
        peers
            .send(protocol::spawn_peer(tokio::io::join(recv, send)))
            .await
            .context("Shared thread is no longer available.")
    }

    /// Serves one collaborator for as long as it stays connected, keeping it
    /// listed as a participant for exactly that long.
    pub(crate) async fn serve_peer(
        cowork: WeakEntity<Self>,
        thread: WeakEntity<Thread>,
        peer: HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        let join = peer
            .receive()
            .with_timeout(PEER_TIMEOUT, cx.background_executor())
            .await?
            .context("Peer closed before joining.")?;
        let protocol::CollaboratorMessage::Join { protocol_version } = join else {
            anyhow::bail!("Expected a join message, got {join:?}.");
        };
        if protocol_version != protocol::PROTOCOL_VERSION {
            let reason = format!(
                "The host uses collaboration protocol version {}, but this app uses version \
                 {protocol_version}. Both participants need the same version of Cowork.",
                protocol::PROTOCOL_VERSION,
            );
            _ = peer.send(protocol::HostMessage::Rejected(reason)).await;
            // Dropping the peer would tear down the connection right away,
            // which may discard the rejection before it is delivered. The
            // collaborator hangs up once it has read it.
            while peer
                .receive()
                .with_timeout(PEER_TIMEOUT, cx.background_executor())
                .await
                .is_ok_and(|request| request.is_some())
            {}
            anyhow::bail!("Rejected a peer using protocol version {protocol_version}.");
        }
        let profile = peer
            .receive()
            .with_timeout(PEER_TIMEOUT, cx.background_executor())
            .await?
            .context("Peer closed before sending its profile.")?;
        let protocol::CollaboratorMessage::Profile(profile) = profile else {
            anyhow::bail!("Expected a profile message, got {profile:?}.");
        };
        validate_profile(&profile).context("Invalid profile from a joining peer.")?;

        let participant_id = ParticipantId::new();
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::ParticipantJoined {
                    participant: participant_id.into_bytes(),
                    profile,
                },
                cx,
            );
        })?;
        _ = cowork.update(cx, |_, cx| cx.notify());

        let result = Self::serve_participant(&cowork, &thread, participant_id, &peer, cx).await;

        _ = thread.update(cx, |thread, cx| thread.participant_left(participant_id, cx));
        _ = cowork.update(cx, |_, cx| cx.notify());
        result
    }

    /// Re-bases a joined collaborator onto a snapshot, then feeds it the
    /// thread's events verbatim while handling its requests, until either
    /// side goes away.
    async fn serve_participant(
        cowork: &WeakEntity<Self>,
        thread: &WeakEntity<Thread>,
        participant_id: ParticipantId,
        peer: &HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        let (mut events, files) = Self::send_snapshot(thread, participant_id, peer, cx).await?;
        // Files to send, each with how far it got, one chunk at a time on the
        // bulk queue so they never hold up anything else.
        let mut sends = files.iter().map(|id| (*id, 0)).collect::<VecDeque<_>>();
        let mut queued = files.into_iter().collect::<HashSet<_>>();
        loop {
            let chunk = match sends.front() {
                Some(&(id, offset)) => {
                    thread.update(cx, |thread, _| thread.draft.chunk(id, offset))?
                }
                None => None,
            };
            if sends.front().is_some() && chunk.is_none() {
                // Removed since.
                sends.pop_front();
                continue;
            }
            let bulk = peer.bulk.clone();
            let send_chunk = async move {
                match chunk {
                    Some(chunk) => {
                        let next = chunk.offset + chunk.bytes.len() as u64;
                        let done = next >= chunk.total;
                        bulk.send(protocol::HostMessage::AttachmentData(chunk))
                            .await
                            .map(|()| (next, done))
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(event) => {
                        if let protocol::HostMessage::AttachmentStored { id, uploader } = &event
                            && *uploader != participant_id.into_bytes()
                        {
                            let id = AttachmentId::from_uuid(Uuid::from_bytes(*id));
                            if queued.insert(id) {
                                sends.push_back((id, 0));
                            }
                        }
                        peer.send(event)
                            .await
                            .context("Peer stopped receiving thread events.")?;
                    }
                    // A peer that fell further behind than the event buffer
                    // has missed changes, so re-base it rather than applying
                    // deltas to a timeline that no longer matches the host's.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let files;
                        (events, files) =
                            Self::send_snapshot(thread, participant_id, peer, cx).await?;
                        for id in files {
                            if queued.insert(id) {
                                sends.push_back((id, 0));
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                sent = send_chunk => {
                    let (next, done) = sent.context("Peer stopped receiving attachments.")?;
                    if done {
                        sends.pop_front();
                    } else if let Some(front) = sends.front_mut() {
                        front.1 = next;
                    }
                }
                // Also how a disconnect is noticed while the thread is quiet.
                request = peer.receive() => {
                    let Some(request) = request else {
                        return Ok(());
                    };
                    let Some(thread) = thread.upgrade() else {
                        return Ok(());
                    };
                    cowork.update(cx, |cowork, cx| {
                        cowork.collaborator_request(&thread, participant_id, request, cx)
                    })??;
                }
            }
        }
    }

    /// Sends a peer a full snapshot and returns a subscription that resumes
    /// exactly where the snapshot left off, with the files the peer needs the
    /// bytes of.
    async fn send_snapshot(
        thread: &WeakEntity<Thread>,
        participant_id: ParticipantId,
        peer: &HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<(
        broadcast::Receiver<protocol::HostMessage>,
        Vec<AttachmentId>,
    )> {
        let (snapshot, draft, presence, stored_attachments, files, events) = thread
            .update(cx, |thread, _| {
                let presence = thread
                    .draft
                    .presence
                    .iter()
                    .map(|(participant, (presence, _))| {
                        (participant.into_bytes(), presence.clone())
                    })
                    .collect();
                let stored = thread
                    .draft
                    .stored
                    .iter()
                    .map(|id| id.as_uuid().into_bytes())
                    .collect();
                Some((
                    thread.to_protocol(),
                    thread.draft.doc.encode_state(),
                    presence,
                    stored,
                    thread.files_for(participant_id),
                    thread.subscribe()?,
                ))
            })?
            .context("Thread is no longer shared.")?;
        peer.send(protocol::HostMessage::Welcome(Box::new(
            protocol::Welcome {
                participant_id: participant_id.into_bytes(),
                thread: snapshot,
                draft,
                presence,
                stored_attachments,
            },
        )))
        .await
        .context("Peer disconnected before receiving the thread snapshot.")?;
        Ok((events, files))
    }

    /// Applies a request a collaborator sent to a thread this app hosts.
    ///
    /// Fails when the collaborator broke the protocol and must be
    /// disconnected.
    pub(crate) fn collaborator_request(
        &mut self,
        thread: &Entity<Thread>,
        participant: ParticipantId,
        request: protocol::CollaboratorMessage,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match request {
            // Only valid as the first message, which `serve_peer` consumes.
            protocol::CollaboratorMessage::Join { .. } => {}
            protocol::CollaboratorMessage::Profile(profile) => {
                validate_profile(&profile)
                    .with_context(|| format!("Invalid profile from {participant:?}."))?;
                thread.update(cx, |thread, cx| {
                    thread.emit(
                        protocol::HostMessage::ProfileChanged {
                            participant: participant.into_bytes(),
                            profile,
                        },
                        cx,
                    );
                });
                cx.notify();
            }
            protocol::CollaboratorMessage::DraftUpdate(update) => {
                thread
                    .update(cx, |thread, _| {
                        thread.apply_collaborator_update(participant, update)
                    })
                    .with_context(|| format!("Invalid draft update from {participant:?}."))?;
                cx.notify();
            }
            protocol::CollaboratorMessage::Submit { sequence } => {
                let (draft_id, stale) = {
                    let thread = thread.read(cx);
                    (
                        thread.draft.id,
                        thread.runnable_model().is_none()
                            || thread.generating
                            || thread.submission_count() != sequence,
                    )
                };
                // A stale sequence means another submission won the race;
                // everyone sees that one.
                // TODO: tell the submitter why a submission was not accepted.
                if !stale && !self.draft_is_loading_attachments(draft_id, cx) {
                    self.accept_submission(draft_id, Some(thread.clone()), false, cx);
                    let thread_id = thread.read(cx).instance_id;
                    self.thread_updated(thread_id, cx);
                }
            }
            protocol::CollaboratorMessage::AttachmentCancelled(id) => {
                let id = AttachmentId::from_uuid(Uuid::from_bytes(id));
                thread.update(cx, |thread, _| {
                    let draft = &mut thread.draft;
                    let theirs = draft
                        .incoming
                        .get(&id)
                        .is_some_and(|file| file.uploader == Some(participant));
                    if theirs {
                        draft.incoming.remove(&id);
                        draft.discarded.insert(id);
                    }
                });
            }
            protocol::CollaboratorMessage::AttachmentData(chunk) => {
                thread
                    .update(cx, |thread, _| thread.receive_upload(participant, chunk))
                    .with_context(|| format!("Invalid attachment data from {participant:?}."))?;
                cx.notify();
            }
            protocol::CollaboratorMessage::Presence(presence) => {
                thread.update(cx, |thread, cx| {
                    thread.host_presence(participant, presence, cx);
                });
                cx.notify();
            }
            protocol::CollaboratorMessage::SelectModel(model) => {
                thread.update(cx, |thread, cx| thread.host_select_model(model, cx));
                cx.notify();
            }
            protocol::CollaboratorMessage::Stop { message_id } => {
                let thread_id = thread.read(cx).instance_id;
                self.cancel_generation(thread_id, Some(Uuid::from_bytes(message_id)), cx);
            }
        }
        Ok(())
    }

    pub(crate) fn copy_endpoint_id(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let endpoint_id = {
            let thread = thread.read(cx);
            let ThreadSharing::Shared { endpoint, .. } = &thread.sharing else {
                return;
            };
            endpoint.id().to_string()
        };

        cx.write_to_clipboard(ClipboardItem::new_string(endpoint_id));
        self.copied_endpoint_id = Some(thread_id);
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1_500))
                .await;
            _ = this.update(cx, |this, cx| {
                if this.copied_endpoint_id == Some(thread_id) {
                    this.copied_endpoint_id = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn open_join_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let endpoint_token = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Paste endpoint token");
            input.set_editor_style(InputEditorStyle {
                caret: rgb(0xffffff).into(),
                ..Default::default()
            });
            input
        });
        let join_dialog = cx.new(|_| JoinDialog {
            endpoint_token: endpoint_token.clone(),
            status: JoinStatus::Idle,
            _input_subscription: None,
        });
        let input_subscription =
            cx.subscribe(&endpoint_token, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(dialog) = &this.join_dialog {
                        dialog.update(cx, |dialog, _| {
                            if matches!(dialog.status, JoinStatus::Failed(_)) {
                                dialog.status = JoinStatus::Idle;
                            }
                        });
                    }
                    cx.notify();
                }
            });
        join_dialog.update(cx, |dialog, _| {
            dialog._input_subscription = Some(input_subscription);
        });
        self.join_dialog = Some(join_dialog.clone());

        let cowork = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let join_state = join_dialog.read(cx);
            let joining = matches!(join_state.status, JoinStatus::Joining);
            let has_endpoint_id_length =
                endpoint_id_input_is_complete(join_state.endpoint_token.read(cx).value().as_ref());
            let error = match &join_state.status {
                JoinStatus::Failed(error) => Some(error.clone()),
                _ => None,
            };

            let cancel_cowork = cowork.clone();
            let cancel_dialog = join_dialog.clone();
            let join_cowork = cowork.clone();
            let dismiss_cowork = cowork.clone();
            let dismiss_dialog = join_dialog.clone();
            let endpoint_token = join_state.endpoint_token.clone();
            dialog
                .w(px(440.))
                .bg(rgb(0x1c1c1f))
                .keyboard(!joining)
                .overlay_closable(!joining)
                .close_button(!joining)
                .on_cancel(move |_, _, cx| {
                    Self::dismiss_join_dialog(&dismiss_cowork, &dismiss_dialog, cx)
                })
                .content(move |content, _, _| {
                    content
                        .child(
                            DialogHeader::new()
                                .child(DialogTitle::new().child("Join shared thread"))
                                .child(
                                    DialogDescription::new()
                                        .child("Paste the endpoint token shared with you."),
                                ),
                        )
                        .child(
                            div()
                                .id("endpoint-token-input")
                                .h(px(38.))
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x52525b))
                                .bg(rgb(0x18181b))
                                .child(Input::new(&endpoint_token)),
                        )
                        .children(
                            error
                                .as_ref()
                                .map(|error| div().text_color(rgb(0xf87171)).child(error.clone())),
                        )
                        .child(
                            DialogFooter::new()
                                .child(
                                    Button::new("cancel-join")
                                        .outline()
                                        .label("Cancel")
                                        .disabled(joining)
                                        .on_click({
                                            let cancel_cowork = cancel_cowork.clone();
                                            let cancel_dialog = cancel_dialog.clone();
                                            move |_, window, cx| {
                                                if Self::dismiss_join_dialog(
                                                    &cancel_cowork,
                                                    &cancel_dialog,
                                                    cx,
                                                ) {
                                                    window.close_dialog(cx);
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("confirm-join")
                                        .primary()
                                        .label(if joining { "Joining…" } else { "Join thread" })
                                        .loading(joining)
                                        .disabled(!has_endpoint_id_length)
                                        .on_click({
                                            let join_cowork = join_cowork.clone();
                                            move |_, window, cx| {
                                                _ = join_cowork.update(cx, |cowork, cx| {
                                                    cowork.join_shared_thread(window, cx);
                                                });
                                            }
                                        }),
                                ),
                        )
                })
        });
        endpoint_token.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn dismiss_join_dialog(
        cowork: &WeakEntity<Self>,
        dialog: &Entity<JoinDialog>,
        cx: &mut App,
    ) -> bool {
        if matches!(dialog.read(cx).status, JoinStatus::Joining) {
            return false;
        }
        cowork
            .update(cx, |cowork, cx| {
                cowork.join_dialog = None;
                cx.notify();
            })
            .is_ok()
    }

    fn join_shared_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.join_dialog.clone() else {
            return;
        };
        if matches!(dialog.read(cx).status, JoinStatus::Joining) {
            return;
        }

        let token = dialog
            .read(cx)
            .endpoint_token
            .read(cx)
            .value()
            .trim()
            .to_string();
        let endpoint_id = match token.parse::<EndpointId>() {
            Ok(endpoint_id) => endpoint_id,
            Err(_) => {
                dialog.update(cx, |dialog, _| {
                    dialog.status = JoinStatus::Failed("Enter a valid endpoint token.".into());
                });
                cx.notify();
                return;
            }
        };
        dialog.update(cx, |dialog, _| {
            dialog.status = JoinStatus::Joining;
        });
        let window_handle = window.window_handle();
        cx.notify();

        let profile = self.profile.to_protocol();
        let join_task = self.tokio_handle.spawn(async move {
            let endpoint = Endpoint::builder(presets::N0).bind().await?;
            let connection =
                tokio::time::timeout(PEER_TIMEOUT, endpoint.connect(endpoint_id, COWORK_ALPN))
                    .await
                    .context("Connection timed out.")??;
            let (send, recv) = tokio::time::timeout(PEER_TIMEOUT, connection.open_bi())
                .await
                .context("Timed out opening the peer protocol stream.")??;
            let host: ThreadHost = protocol::spawn_peer(tokio::io::join(recv, send));
            host.send(protocol::CollaboratorMessage::Join {
                protocol_version: protocol::PROTOCOL_VERSION,
            })
            .await
            .context("Peer connection is no longer available.")?;
            host.send(protocol::CollaboratorMessage::Profile(profile))
                .await
                .context("Peer connection is no longer available.")?;
            let welcome = tokio::time::timeout(PEER_TIMEOUT, host.receive())
                .await
                .context("Timed out waiting for the thread snapshot.")?
                .context("Host closed the protocol stream before sending the thread snapshot.")?;
            let welcome = match welcome {
                protocol::HostMessage::Welcome(welcome) => *welcome,
                protocol::HostMessage::Rejected(reason) => anyhow::bail!(reason),
                _ => anyhow::bail!("Host sent a thread event before the thread snapshot."),
            };
            Ok::<_, anyhow::Error>((endpoint, connection, host, welcome))
        });

        cx.spawn(async move |this, cx| {
            let result = join_task
                .await
                .context("Join task failed.")
                .and_then(|result| result);
            let (endpoint, connection, host, welcome) = match result {
                Ok(joined) => joined,
                Err(error) => {
                    eprintln!("failed to join shared thread: {error:#}");
                    dialog.update(cx, |dialog, _| {
                        dialog.status = JoinStatus::Failed(error.to_string());
                    });
                    _ = this.update(cx, |_, cx| cx.notify());
                    return;
                }
            };

            let link = PeerLink {
                endpoint,
                _connection: connection,
            };
            let mirrored = this.update(cx, move |this, cx| {
                let thread_id = this.mirror_thread(welcome, host, Some(link), cx)?;
                this.join_dialog = None;
                Ok::<_, anyhow::Error>(thread_id)
            });
            match mirrored {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    eprintln!("failed to open shared thread: {error:#}");
                    dialog.update(cx, |dialog, _| {
                        dialog.status = JoinStatus::Failed(error.to_string());
                    });
                    _ = this.update(cx, |_, cx| cx.notify());
                    return;
                }
                Err(_) => return,
            }
            _ = cx.update_window(window_handle, |_, window, cx| {
                window.close_dialog(cx);
            });
        })
        .detach();
    }

    /// Opens a joined thread: builds its mirror from the host's welcome,
    /// replays the host's events onto it, and forwards its requests to the
    /// host in order. Returns the new thread's id.
    pub(crate) fn mirror_thread(
        &mut self,
        welcome: protocol::Welcome,
        host: ThreadHost,
        link: Option<PeerLink>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Uuid> {
        Thread::validate_welcome(&welcome)?;
        let (host_requests, uploads, events) = host.split();
        let (requests, queued_requests) = async_channel::unbounded();
        cx.background_spawn(async move {
            while let Ok(request) = queued_requests.recv().await {
                if host_requests.send(request).await.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Re-authored with the id the host assigned by `from_welcome`.
        let draft = ThreadDraft::new(self.local_participant_id);
        let thread = cx.new(|cx| {
            Thread::from_welcome(
                welcome,
                draft,
                ThreadSharing::Connected {
                    host: requests,
                    uploads,
                    link,
                },
                cx,
            )
        });
        let thread_id = thread.read(cx).instance_id;
        self.thread_store.update(cx, |store, _| {
            store.threads.push_front(thread.clone());
        });
        self.active_thread_id = Some(thread_id);
        self.selection_message_id = None;
        self.main_stage = MainStage::Thread;
        self.follow_generation = true;
        self.timeline_scroll_handle.scroll_to_bottom();
        cx.notify();

        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                // Carets move constantly; they only need a redraw, not the
                // timeline following new output.
                let presence_only = matches!(
                    event,
                    protocol::HostMessage::Presence { .. }
                        | protocol::HostMessage::AttachmentData(_)
                );
                if let Err(error) = thread.update(cx, |thread, cx| thread.try_apply(event, cx)) {
                    eprintln!("closed shared thread after invalid event from host: {error:#}");
                    break;
                }
                if this
                    .update(cx, |this, cx| {
                        if presence_only {
                            cx.notify();
                        } else {
                            this.thread_updated(thread_id, cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
            // The host stopped sharing, went away, or dropped us.
            _ = this.update(cx, |this, cx| this.remove_mirrored_thread(thread_id, cx));
        })
        .detach();
        Ok(thread_id)
    }

    /// Closes a joined thread, which has nothing left to show once it is no
    /// longer connected to its host.
    fn remove_mirrored_thread(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        if !thread.read(cx).ownership.remove_on_disconnect() {
            return;
        }
        self.thread_store.update(cx, |store, cx| {
            store
                .threads
                .retain(|thread| thread.read(cx).instance_id != thread_id);
        });
        if self.active_thread_id == Some(thread_id) {
            self.active_thread_id = None;
            self.selection_message_id = None;
        }
        cx.notify();
    }

    pub(crate) fn toggle_sharing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread = self.prepare_thread_for_sharing(cx);
        let thread_id = thread.read(cx).instance_id;

        match thread.read(cx).sharing.status() {
            SharingStatus::NotShared | SharingStatus::Failed => self.start_sharing(thread, cx),
            SharingStatus::Sharing => {}
            SharingStatus::Shared => {
                // Replacing the state drops the event channel, which ends every
                // peer's subscription and unwinds the tasks serving them.
                let endpoint = thread.update(cx, |thread, _| {
                    let ThreadSharing::Shared { endpoint, .. } =
                        std::mem::replace(&mut thread.sharing, ThreadSharing::NotShared)
                    else {
                        return None;
                    };
                    thread.participants.clear();
                    thread.draft.presence.clear();
                    Some(endpoint)
                });
                if let Some(endpoint) = endpoint {
                    self.tokio_handle.spawn(async move {
                        endpoint.close().await;
                    });
                }
                cx.notify();
            }
            SharingStatus::Connected => {
                // Replacing the state drops the channel to the host, closing
                // the protocol stream that kept the connection alive.
                let endpoint = thread.update(cx, |thread, _| {
                    let ThreadSharing::Connected { link, .. } =
                        std::mem::replace(&mut thread.sharing, ThreadSharing::NotShared)
                    else {
                        return None;
                    };
                    link.map(|link| link.endpoint)
                });
                if thread.read(cx).ownership.remove_on_disconnect() {
                    self.remove_mirrored_thread(thread_id, cx);
                    self.new_thread_draft = ThreadDraft::new(self.local_participant_id);
                    self.focus_composer(window, cx);
                }
                if let Some(endpoint) = endpoint {
                    self.tokio_handle.spawn(async move {
                        endpoint.close().await;
                    });
                }
                cx.notify();
            }
        }
    }
}
