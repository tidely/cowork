//! The card that tells who is behind an avatar: a participant, or the model
//! that wrote a reply. It is informational only, built as a GPUI tooltip, so
//! it opens after the tooltip delay, follows the pointer to the avatar, and
//! closes as soon as the pointer leaves the avatar.
//!
//! The card holds only what it describes and reads everything else from the
//! app each frame, so presence and a running reply stay current while it is
//! open.

use std::time::{Duration, SystemTime};

use gpui::{
    AnyElement, App, Context, IntoElement, Render, SharedString, WeakEntity, Window, div,
    prelude::*, px,
};
use gpui_component::{ActiveTheme as _, Icon};
use gpui_kit_assets::IconName;
use uuid::Uuid;

use crate::{
    Cowork,
    models::{ModelProvider, ModelRef},
    participant::ParticipantId,
    peer_access::mode_icon,
    protocol::PresenceFocus,
    thread::{PeerMode, Thread},
    timeline::TimelineMessage,
    turn_navigation::time_ago,
    usage::{format_stat_duration, format_token_count},
};

const CARD_WIDTH: gpui::Pixels = px(260.);
const AVATAR_SIZE: gpui::Pixels = px(36.);
const ROW_ICON: gpui::Pixels = px(14.);
/// Keeps the card off the pointer's tip, which GPUI places it beside.
const POINTER_GAP: gpui::Pixels = px(8.);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AvatarCardSubject {
    Participant(ParticipantId),
    /// The model behind agent message `id`.
    Reply(Uuid),
}

pub(crate) struct AvatarCard {
    cowork: WeakEntity<Cowork>,
    subject: AvatarCardSubject,
}

impl AvatarCard {
    pub(crate) fn new(cowork: WeakEntity<Cowork>, subject: AvatarCardSubject) -> Self {
        Self { cowork, subject }
    }
}

impl Render for AvatarCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cx: &App = cx;
        let card = self.cowork.upgrade().and_then(|cowork| {
            let cowork = cowork.read(cx);
            let thread = cowork.active_thread(cx);
            let thread = thread.as_ref().map(|thread| thread.read(cx));
            let now = SystemTime::now();
            match self.subject {
                AvatarCardSubject::Participant(participant) => {
                    let you = participant == cowork.local_participant_id
                        || thread.is_some_and(|thread| thread.participant_id() == participant);
                    let details = ParticipantDetails::of(thread, participant, you);
                    Some(render_participant(cowork, participant, &details, now, cx))
                }
                AvatarCardSubject::Reply(id) => {
                    let thread = thread?;
                    let host = thread.host_id().map(|host| {
                        if host == thread.participant_id() {
                            Host::You
                        } else {
                            Host::Named(cowork.name_of(host))
                        }
                    });
                    let details = ReplyDetails::of(thread, id, host, now)?;
                    Some(render_reply(&details, cx))
                }
            }
        });
        div()
            .p(POINTER_GAP)
            .children(card.map(|card| card.debug_selector(|| "avatar-card".to_owned())))
    }
}

/// What the card says about a participant, from the thread it is shown in.
#[derive(Debug, PartialEq, Eq)]
struct ParticipantDetails {
    /// In the shared chat right now.
    here: bool,
    status: Status,
    /// Only while they are here in a shared chat, where it means something.
    access: Option<Access>,
    prompts: usize,
    comments: usize,
    joined_at: Option<SystemTime>,
}

#[derive(Debug, PartialEq, Eq)]
enum Status {
    You,
    Adding(String),
    InDraft,
    Here,
    Left(Option<SystemTime>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Host,
    Peer(PeerMode),
}

impl ParticipantDetails {
    fn of(thread: Option<&Thread>, participant: ParticipantId, you: bool) -> Self {
        let Some(thread) = thread else {
            return Self {
                here: false,
                status: if you { Status::You } else { Status::Left(None) },
                access: None,
                prompts: 0,
                comments: 0,
                joined_at: None,
            };
        };
        let here = thread.participants().contains(&participant);
        let times = thread.membership_times(participant);
        let status = if you {
            Status::You
        } else if here {
            match thread.draft().presence.get(&participant) {
                Some((presence, _)) if !presence.pending_reads.is_empty() => {
                    Status::Adding(presence.pending_reads[0].name.clone())
                }
                Some((presence, _)) if presence.focus.is_some_and(is_in_draft) => Status::InDraft,
                _ => Status::Here,
            }
        } else {
            Status::Left(times.left_at)
        };
        let access = here.then(|| {
            if thread.host_id() == Some(participant) {
                Access::Host
            } else {
                Access::Peer(thread.peer_permissions().mode_for(participant))
            }
        });
        let (mut prompts, mut comments) = (0, 0);
        for message in &thread.timeline {
            if let TimelineMessage::User(message) = message {
                let group = &message.record;
                prompts += group
                    .blocks
                    .iter()
                    .filter(|block| block.author == participant)
                    .count();
                comments += group
                    .comments
                    .iter()
                    .filter(|comment| comment.author == participant)
                    .count();
            }
        }
        Self {
            here,
            status,
            access,
            prompts,
            comments,
            // Someone's own join is not news to them.
            joined_at: if you || !here { None } else { times.joined_at },
        }
    }
}

fn is_in_draft(focus: PresenceFocus) -> bool {
    matches!(focus, PresenceFocus::Item(_) | PresenceFocus::DraftPosition)
}

/// Who runs the thread's agent, as the reply card names them.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Host {
    You,
    Named(SharedString),
}

/// What the card says about the model behind a reply.
#[derive(Debug, PartialEq, Eq)]
struct ReplyDetails {
    model: Option<ModelRef>,
    name: SharedString,
    host: Option<Host>,
    work: Option<Duration>,
    running: bool,
    tokens: u64,
}

impl ReplyDetails {
    fn of(thread: &Thread, id: Uuid, host: Option<Host>, now: SystemTime) -> Option<Self> {
        let message = thread.timeline.iter().find_map(|message| match message {
            TimelineMessage::Agent(message) if message.record.id == id => Some(message),
            _ => None,
        })?;
        let record = &message.record;
        let name = match &record.model {
            Some(model) => thread
                .models()
                .get(model)
                .map_or_else(|| model.id.clone(), |info| info.name.clone())
                .into(),
            None => "Unknown model".into(),
        };
        Some(Self {
            model: record.model.clone(),
            name,
            host,
            work: record.work_duration(now),
            running: record.run.duration().is_none(),
            tokens: record.tokens,
        })
    }

    fn runs_on(&self) -> Option<(IconName, String)> {
        let provider = self.model.as_ref()?.provider;
        let owner = match &self.host {
            Some(Host::You) => "your".to_owned(),
            Some(Host::Named(name)) => format!("{name}'s"),
            None => "the host's".to_owned(),
        };
        Some(match provider {
            // Cowork only reaches Ollama on the host's own machine.
            ModelProvider::Ollama => (IconName::Laptop, format!("Runs on {owner} computer")),
            ModelProvider::Anthropic | ModelProvider::OpenAi => (
                IconName::KeyRound,
                format!("Uses {owner} {} key", provider.label()),
            ),
        })
    }

    fn work(&self) -> Option<String> {
        let tokens =
            (self.tokens > 0).then(|| format!("{} tokens", format_token_count(self.tokens)));
        let work = self.work.map(|work| {
            let work = format_stat_duration(work);
            if self.running {
                format!("Working for {work}")
            } else {
                format!("Worked {work}")
            }
        });
        match (work, tokens) {
            (Some(work), Some(tokens)) => Some(format!("{work} · {tokens}")),
            (work, tokens) => work.or(tokens),
        }
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn render_participant(
    cowork: &Cowork,
    participant: ParticipantId,
    details: &ParticipantDetails,
    now: SystemTime,
    cx: &App,
) -> gpui::Div {
    let theme = cx.theme();
    // Wrapped, since a picture avatar clips what overflows it.
    let avatar = div()
        .relative()
        .flex_none()
        .child(cowork.render_participant_avatar(participant, AVATAR_SIZE))
        .when(details.here, |this| {
            this.child(
                div()
                    .absolute()
                    .right(px(-1.))
                    .bottom(px(-1.))
                    .size(px(11.))
                    .rounded_full()
                    .border_2()
                    .border_color(theme.popover)
                    .bg(theme.success),
            )
        });
    let status = match &details.status {
        Status::You => "You".to_owned(),
        Status::Adding(name) => format!("Adding {name}"),
        Status::InDraft => "In the draft".to_owned(),
        Status::Here => "Here now".to_owned(),
        Status::Left(at) => match at.and_then(|at| time_ago(at, now)) {
            Some(ago) => format!("Left {ago}"),
            None => "Not here".to_owned(),
        },
    };

    let mut rows = Vec::new();
    if let Some(access) = details.access {
        let (icon, text) = match access {
            Access::Host => (IconName::Crown, "Host"),
            Access::Peer(mode) => (mode_icon(mode), mode.ability()),
        };
        rows.push(row(icon, text.to_owned(), cx));
    }
    let messages = match (details.prompts, details.comments) {
        (0, 0) => None,
        (prompts, 0) => Some(plural(prompts, "prompt", "prompts")),
        (0, comments) => Some(plural(comments, "comment", "comments")),
        (prompts, comments) => Some(format!(
            "{}, {}",
            plural(prompts, "prompt", "prompts"),
            plural(comments, "comment", "comments")
        )),
    };
    if let Some(messages) = messages {
        rows.push(row(IconName::MessageSquare, messages, cx));
    }
    if let Some(ago) = details.joined_at.and_then(|at| time_ago(at, now)) {
        rows.push(row(IconName::Clock, format!("Joined {ago}"), cx));
    }

    card(avatar, cowork.name_of(participant), status, rows, cx)
}

fn render_reply(details: &ReplyDetails, cx: &App) -> gpui::Div {
    let theme = cx.theme();
    let logo = div()
        .size(AVATAR_SIZE)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme.secondary)
        .child(match &details.model {
            Some(model) => model.provider.icon(px(18.), cx),
            None => div().child(
                Icon::new(IconName::Bot)
                    .size(px(18.))
                    .text_color(theme.foreground),
            ),
        });
    let provider = details
        .model
        .as_ref()
        .map_or("", |model| model.provider.label());

    let mut rows = Vec::new();
    if let Some((icon, text)) = details.runs_on() {
        rows.push(row(icon, text, cx));
    }
    if let Some(work) = details.work() {
        rows.push(row(IconName::Clock, work, cx));
    }

    card(logo, details.name.clone(), provider.to_owned(), rows, cx)
}

fn card(
    avatar: impl IntoElement,
    name: SharedString,
    subtitle: String,
    rows: Vec<AnyElement>,
    cx: &App,
) -> gpui::Div {
    let theme = cx.theme();
    let header = div().flex().items_center().gap_2p5().child(avatar).child(
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .min_w_0()
            .child(
                div()
                    .truncate()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(name),
            )
            .when(!subtitle.is_empty(), |this| {
                this.child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(subtitle),
                )
            }),
    );
    div()
        .w(CARD_WIDTH)
        .flex()
        .flex_col()
        .gap_3()
        .p(px(14.))
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .text_color(theme.popover_foreground)
        .child(header)
        .when(!rows.is_empty(), |this| {
            this.child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
                .child(div().flex().flex_col().gap_2().children(rows))
        })
}

fn row(icon: IconName, text: String, cx: &App) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(Icon::new(icon).size(ROW_ICON).flex_none())
        .child(div().truncate().child(text))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{protocol, thread::draft::ThreadDraft};

    fn joined(participant: ParticipantId) -> protocol::HostMessage {
        protocol::HostMessage::ParticipantJoined {
            participant: participant.into_bytes(),
            profile: protocol::Profile::default(),
        }
    }

    /// A peer reads as here, in the draft, with their access and join time,
    /// until they leave; the host reads as the host.
    #[gpui::test]
    fn participant_details_follow_presence_and_membership(cx: &mut gpui::TestAppContext) {
        let host = ParticipantId::new();
        let peer = ParticipantId::new();
        let mut thread = Thread::new_local(
            "Test".into(),
            Vec::new(),
            ThreadDraft::new(host),
            host,
            Arc::default(),
            None,
        );
        cx.update(|cx| {
            thread.apply_for_test(joined(host), cx);
            thread.apply_for_test(joined(peer), cx);
            thread.apply_for_test(
                protocol::HostMessage::Presence {
                    participant: peer.into_bytes(),
                    presence: protocol::Presence {
                        focus: Some(PresenceFocus::DraftPosition),
                        selection: None,
                        pending_reads: Vec::new(),
                    },
                },
                cx,
            );
        });

        let here = ParticipantDetails::of(Some(&thread), peer, false);
        assert!(here.here);
        assert_eq!(here.status, Status::InDraft);
        assert_eq!(here.access, Some(Access::Peer(PeerMode::Write)));
        assert!(here.joined_at.is_some());
        assert_eq!(
            ParticipantDetails::of(Some(&thread), host, true).access,
            Some(Access::Host)
        );

        cx.update(|cx| {
            thread.apply_for_test(
                protocol::HostMessage::ParticipantLeft(peer.into_bytes()),
                cx,
            )
        });
        let gone = ParticipantDetails::of(Some(&thread), peer, false);
        assert!(!gone.here);
        assert!(matches!(gone.status, Status::Left(Some(_))));
        assert_eq!(gone.access, None);
        assert_eq!(gone.joined_at, None);
    }
}
