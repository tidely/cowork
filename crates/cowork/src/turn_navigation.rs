//! The conversation navigation rail: a marker per turn at the right edge of
//! the center stage, a card previewing the turn under the pointer, and
//! jumping to a turn. See `docs/conversation-navigation.md`.
//!
//! A **turn** is a user group that started an agent run: the group appended
//! right before its run's `AgentStarted`, as both `UserMessage` and
//! `PendingDelivered` do (`docs/collaboration.md`, "Pending delivery"). A
//! steer is a group too, but its run's output names it with a
//! [`AgentStep::Steering`] step, the rule live folding and replay share, so
//! it is never a turn; nor is a pending group whose run hasn't started, nor
//! the onboarding greeting, which has no group. A turn is identified by its
//! group's id, which is also its row in the timeline.
//!
//! Everything here is local: which marker is hovered, the rail's scroll
//! offset, and the turn navigated to. The card is built from the shared
//! records of the hovered turn alone: its group, and its run's response,
//! model, and start time. The card's second section is that response, the
//! agent's visible answer (what was called the "agent prompt"), never its
//! reasoning, tool calls, or the system prompt.

use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Write as _},
    rc::Rc,
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Local};
use draft::AttachmentKind;
use gpui::{
    AnyElement, App, Bounds, Context, Element, ElementId, Entity, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, MouseButton, Pixels, Position, ScrollHandle,
    SharedString, Style, Window, div, point, prelude::*, px,
};
use gpui_component::{ActiveTheme as _, Icon};
use gpui_kit_assets::IconName;
use uuid::Uuid;

use crate::{
    Cowork,
    models::ModelRef,
    participant::ParticipantId,
    profile::{Profile, participant_name},
    protocol::{AgentRun, RunOutcome},
    thread::Thread,
    timeline::{AgentStep, RunActivity, TimelineMessage},
    transcript::RunSource,
};

/// The width the rail takes at the right edge of the stage, every marker's
/// hit area. Message rows and the composer keep a gap this wide on their
/// right (see `Cowork::render_message_row`), so the rail never covers text.
pub(crate) const RAIL_WIDTH: Pixels = px(40.);
/// From the stage's right edge to every pill's right end.
const RAIL_INSET: Pixels = px(10.);
/// A marker's height, and the distance between markers.
const MARKER_PITCH: Pixels = px(14.);
const PILL_HEIGHT: Pixels = px(3.);
/// The hovered pill's width, then its neighbors' by distance.
const HOVER_WIDTHS: [Pixels; 3] = [px(28.), px(22.), px(16.)];
const REST_WIDTH: Pixels = px(12.);
const PILL_MOTION: Duration = Duration::from_millis(140);

pub(crate) const CARD_WIDTH: Pixels = px(360.);
/// The least room the card keeps from the stage's edges.
const CARD_INSET: Pixels = px(8.);
/// Between the card and the rail.
pub(crate) const CARD_GAP: Pixels = px(6.);
/// Below this the stage is too narrow for a card worth reading.
const MIN_CARD_WIDTH: Pixels = px(160.);
const PROMPT_LINES: usize = 4;
const RESPONSE_LINES: usize = 6;
/// How much of a prompt or response the card lays out at most: more than
/// its clamped lines can show, without shaping a whole long reply.
const PREVIEW_BYTES: usize = 800;

/// Measure the card before centering it beside its marker. Clamping against
/// the stage (rather than the window) keeps it out of the top and bottom bars.
struct PositionedTurnCard {
    card: AnyElement,
    stage: Bounds<Pixels>,
    marker_y: Pixels,
}

impl IntoElement for PositionedTurnCard {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for PositionedTurnCard {
    type RequestLayoutState = LayoutId;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, LayoutId) {
        let child = self.card.request_layout(window, cx);
        let layout = window.request_layout(
            Style {
                position: Position::Absolute,
                ..Style::default()
            },
            [child],
            cx,
        );
        (layout, child)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut LayoutId,
        window: &mut Window,
        cx: &mut App,
    ) {
        let size = window.layout_bounds(*child).size;
        let top = (self.marker_y - size.height / 2.)
            .max(self.stage.top() + CARD_INSET)
            .min(self.stage.bottom() - CARD_INSET - size.height);
        let left = self.stage.right() - RAIL_WIDTH - CARD_GAP - size.width;
        let offset = point(left, top) - bounds.origin;
        window.with_element_offset(offset, |window| self.card.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut LayoutId,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.card.paint(window, cx);
    }
}

/// A turn of the conversation, as it is in this frame's timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Turn {
    /// The user group that started the run: the turn's identity.
    pub(crate) group: Uuid,
    pub(crate) group_index: usize,
    pub(crate) run_index: usize,
}

/// The user groups that joined a run as steers.
pub(crate) fn steered_groups(timeline: &[TimelineMessage]) -> HashSet<Uuid> {
    timeline
        .iter()
        .filter_map(|entry| match entry {
            TimelineMessage::Agent(message) => Some(message.ui.output.steps()),
            TimelineMessage::User(_) => None,
        })
        .flatten()
        .filter_map(|step| match step {
            AgentStep::Steering(id) => Some(*id),
            _ => None,
        })
        .collect()
}

/// Whether the entry at `index` is a user group that started the run right
/// after it, rather than one of `steered`.
pub(crate) fn initiates_run(
    timeline: &[TimelineMessage],
    index: usize,
    steered: &HashSet<Uuid>,
) -> bool {
    matches!(
        (timeline.get(index), timeline.get(index + 1)),
        (Some(TimelineMessage::User(group)), Some(TimelineMessage::Agent(_)))
            if !steered.contains(&group.record.id)
    )
}

/// The turns of `timeline`, oldest first.
pub(crate) fn conversation_turns(timeline: &[TimelineMessage]) -> Vec<Turn> {
    let steered = steered_groups(timeline);
    (0..timeline.len())
        .filter(|&index| initiates_run(timeline, index, &steered))
        .filter_map(|index| match &timeline[index] {
            TimelineMessage::User(group) => Some(Turn {
                group: group.record.id,
                group_index: index,
                run_index: index + 1,
            }),
            TimelineMessage::Agent(_) => None,
        })
        .collect()
}

/// A pill's width with the marker at `hovered` under the pointer: wider the
/// closer it is, by chronological distance.
fn pill_width(index: usize, hovered: Option<usize>) -> Pixels {
    hovered
        .and_then(|hovered| HOVER_WIDTHS.get(index.abs_diff(hovered)).copied())
        .unwrap_or(REST_WIDTH)
}

/// The rail's local state for the active thread.
#[derive(Default)]
pub(crate) struct TurnRail {
    /// The thread the rest is about; any other thread starts afresh.
    thread: Option<Uuid>,
    /// The turn whose card shows, and whose neighbors widen.
    pub(crate) hovered: Option<Uuid>,
    /// The final action marker widens without opening a turn card.
    hovered_end: bool,
    pub(crate) scroll: ScrollHandle,
    /// The current turn the rail last scrolled into view.
    shown_current: Option<Uuid>,
}

impl TurnRail {
    fn show_thread(&mut self, thread: Uuid) {
        if self.thread != Some(thread) {
            *self = Self {
                thread: Some(thread),
                ..Self::default()
            };
        }
    }
}

/// What the card says of a turn's response.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnResponse {
    /// The run's response, as far as it has come.
    Text(String),
    /// Why there is none (yet).
    Status(&'static str),
}

/// What the card shows of a turn, from its shared records.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TurnPreview {
    /// The comments' and prompt blocks' text, in the order the timeline
    /// shows them, or attachments' names where a block has no text.
    pub(crate) prompt: String,
    pub(crate) response: TurnResponse,
    /// Everyone who wrote the group's comments or prompt blocks, in the
    /// order they first did; not who sent it, nor who steered the run.
    pub(crate) authors: Vec<ParticipantId>,
    /// The run was started by a scheduled task, as its committed input
    /// records.
    pub(crate) scheduled: bool,
    /// The model the run used, as it was started; `None` when unrecorded.
    pub(crate) model: Option<ModelRef>,
    /// The model's name in the thread's catalog, when it is still there.
    pub(crate) model_name: Option<String>,
    /// When the host started the run.
    pub(crate) started_at: SystemTime,
}

impl TurnPreview {
    pub(crate) fn of(thread: &Thread, turn: &Turn) -> Option<Self> {
        let timeline = &thread.timeline;
        let (TimelineMessage::User(group), TimelineMessage::Agent(run)) = (
            timeline.get(turn.group_index)?,
            timeline.get(turn.run_index)?,
        ) else {
            return None;
        };
        if group.record.id != turn.group {
            return None;
        }
        let group = &group.record;
        let mut prompt = PreviewText::default();
        'prompt: {
            for comment in &group.comments {
                let body = comment.body.trim();
                if !body.is_empty()
                    && prompt
                        .section()
                        .and_then(|()| write!(prompt, "“{body}”"))
                        .is_err()
                {
                    break 'prompt;
                }
            }
            for block in &group.blocks {
                let text = block.text.trim();
                if !text.is_empty() {
                    if prompt
                        .section()
                        .and_then(|()| prompt.write_str(text))
                        .is_err()
                    {
                        break 'prompt;
                    }
                } else {
                    for (index, record) in block.attachments.iter().enumerate() {
                        let separator = if index == 0 {
                            prompt.section()
                        } else {
                            prompt.write_str(", ")
                        };
                        if separator
                            .and_then(|()| {
                                write!(prompt, "{} ({})", record.name, kind_label(record.kind))
                            })
                            .is_err()
                        {
                            break 'prompt;
                        }
                    }
                }
            }
        }
        let mut authors = Vec::new();
        for author in group
            .comments
            .iter()
            .map(|comment| comment.author)
            .chain(group.blocks.iter().map(|block| block.author))
        {
            if !authors.contains(&author) {
                authors.push(author);
            }
        }
        let record = &run.record;
        let text = run.ui.output.text.trim();
        let response = if !text.is_empty() {
            let mut response = PreviewText::default();
            _ = response.write_str(text);
            TurnResponse::Text(response.finish())
        } else {
            TurnResponse::Status(match &record.run {
                AgentRun::Generating => match thread.run_activity(record.id) {
                    Some(RunActivity::AwaitingApproval) => "Waiting for approval",
                    Some(RunActivity::Compacting) => "Compacting context…",
                    _ => "Working…",
                },
                AgentRun::Ended { outcome, .. } => match outcome {
                    RunOutcome::Completed => "No response",
                    RunOutcome::Stopped => "Stopped",
                    RunOutcome::Failed(_) => "Failed",
                },
            })
        };
        let scheduled = thread
            .transcript
            .entries()
            .get(record.start.0 as usize)
            .is_some_and(|entry| {
                matches!(
                    &entry.origin,
                    agent::Origin::Input {
                        source: RunSource::Schedule,
                        ..
                    }
                )
            });
        let model_name = record
            .model
            .as_ref()
            .and_then(|model| thread.models().get(model))
            .map(|info| info.name.clone());
        Some(Self {
            prompt: prompt.finish(),
            response,
            authors,
            scheduled,
            model: record.model.clone(),
            model_name,
            started_at: record.started_at,
        })
    }
}

fn kind_label(kind: AttachmentKind) -> &'static str {
    match kind {
        AttachmentKind::Text => "text",
        AttachmentKind::Png => "PNG image",
        AttachmentKind::Jpeg => "JPEG image",
    }
}

/// Accumulates only a preview's byte budget, including separators and its
/// truncation indicator. A formatting error means the budget is exhausted:
/// callers stop visiting further input, and `write!` stops formatting parts.
#[derive(Default)]
struct PreviewText {
    text: String,
    truncated: bool,
}

impl PreviewText {
    fn section(&mut self) -> fmt::Result {
        if self.text.is_empty() {
            Ok(())
        } else {
            self.write_str("\n")
        }
    }

    fn finish(mut self) -> String {
        if self.truncated {
            const ELLIPSIS: &str = "…";
            let end = char_boundary(&self.text, PREVIEW_BYTES - ELLIPSIS.len());
            self.text.truncate(end);
            self.text.push_str(ELLIPSIS);
        }
        self.text
    }
}

impl fmt::Write for PreviewText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.truncated {
            return Err(fmt::Error);
        }
        let remaining = PREVIEW_BYTES - self.text.len();
        let end = char_boundary(text, remaining);
        self.text.push_str(&text[..end]);
        if end < text.len() {
            self.truncated = true;
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

fn char_boundary(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// How long before `now` a run started: "just now", "5 min ago", "3 h
/// ago", "2 d ago". `None` for a time after `now`, as another computer's
/// clock can give.
fn time_ago(at: SystemTime, now: SystemTime) -> Option<String> {
    let seconds = now.duration_since(at).ok()?.as_secs();
    Some(match seconds {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    })
}

/// When a run started, relatively and in local time: "5 min ago · Today
/// 09:00", the date as the scheduled tasks show theirs.
fn started_label(at: SystemTime, now: SystemTime) -> String {
    let local = |time: SystemTime| DateTime::<Local>::from(time).naive_local();
    let absolute = crate::schedules::format_relative(local(at), local(now));
    match time_ago(at, now) {
        Some(ago) => format!("{ago} · {absolute}"),
        None => absolute,
    }
}

// `None` identifies the final Jump to End action, not a submitted turn.
pub(crate) type HoverMarker = Rc<dyn Fn(Option<Uuid>, bool, &mut Window, &mut App)>;
pub(crate) type ClickMarker = Rc<dyn Fn(Option<Uuid>, &mut Window, &mut App)>;

/// The rail of `markers`, oldest at the top, absolutely placed at the right
/// edge of its parent: centered when it fits, and scrolling on its own when
/// it doesn't. Wheel events over it never reach the conversation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_rail(
    markers: &[Uuid],
    hovered: Option<usize>,
    current: Option<usize>,
    scroll: &ScrollHandle,
    on_hover: HoverMarker,
    on_click: ClickMarker,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme();
    let (rest, hover, strong) = (
        theme.muted_foreground.opacity(0.45),
        theme.muted_foreground,
        theme.foreground,
    );
    let markers = markers
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None))
        .enumerate()
        .map(|(index, id)| {
            let element_id = id.map_or_else(|| ElementId::from("jump-to-end"), ElementId::Uuid);
            let width = gpui_base::transition(
                (element_id.clone(), "turn-pill"),
                pill_width(index, hovered),
                gpui_base::Transition::new(PILL_MOTION),
                window,
                cx,
            );
            let color: Hsla = if current == Some(index) {
                strong
            } else if hovered == Some(index) {
                hover
            } else {
                rest
            };
            let (on_hover, on_click) = (on_hover.clone(), on_click.clone());
            div()
                .id(element_id)
                .debug_selector(move || {
                    id.map_or_else(
                        || "jump-to-end".to_owned(),
                        |id| format!("turn-marker-{id}"),
                    )
                })
                .h(MARKER_PITCH)
                .w_full()
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .pr(RAIL_INSET)
                .cursor_pointer()
                .on_hover(move |hovered, window, cx| on_hover(id, *hovered, window, cx))
                // Focus stays where it was, as on the timeline's blank
                // space. Propagation must go on, to make the click.
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                .on_click(move |_, window, cx| on_click(id, window, cx))
                .when(id.is_none(), |this| {
                    this.tooltip_show_delay(Duration::ZERO)
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Jump to End").build(window, cx)
                        })
                })
                .child(
                    div()
                        .h(PILL_HEIGHT)
                        .w(width)
                        .flex_none()
                        .rounded_full()
                        .bg(color),
                )
        })
        .collect::<Vec<_>>();
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(RAIL_WIDTH)
        .flex()
        .flex_col()
        .justify_center()
        .child(
            div()
                .id("turn-rail")
                .debug_selector(|| "turn-rail".to_owned())
                .w_full()
                .min_h_0()
                .max_h_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(scroll)
                // What is behind the markers takes no pointer events, the
                // conversation's scrolling included.
                .occlude()
                .children(markers),
        )
        .into_any_element()
}

/// The card previewing a turn, sized by its caller.
pub(crate) fn render_turn_card(
    preview: &TurnPreview,
    profiles: &HashMap<ParticipantId, Profile>,
    now: SystemTime,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let divider = || div().h(px(1.)).w_full().flex_none().bg(theme.border);
    let response = match &preview.response {
        TurnResponse::Text(text) => div()
            .text_color(theme.muted_foreground)
            .line_clamp(RESPONSE_LINES)
            .text_ellipsis()
            .child(SharedString::new(&text)),
        TurnResponse::Status(status) => div()
            .italic()
            .text_color(theme.muted_foreground)
            .child(*status),
    };
    let authors = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x_3()
        .gap_y_1()
        .children(preview.authors.iter().map(|&author| {
            let profile = profiles.get(&author);
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .min_w_0()
                .child(Cowork::render_avatar_for(author, profile, px(16.)))
                .child(div().truncate().child(participant_name(author, profile)))
        }))
        .when(preview.scheduled, |this| {
            this.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Scheduled task"),
            )
        });
    let model = div()
        .flex()
        .items_center()
        .gap_1p5()
        .min_w_0()
        .map(|this| match &preview.model {
            Some(model) => {
                this.child(model.provider.icon(px(14.), cx))
                    .child(div().truncate().child(format!(
                        "{} · {}",
                        model.provider.label(),
                        preview.model_name.as_deref().unwrap_or(&model.id)
                    )))
            }
            None => this.child("Unknown model"),
        });
    div()
        .id("turn-card")
        .debug_selector(|| "turn-card".to_owned())
        .flex()
        .flex_col()
        .gap_2p5()
        .p_3()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .text_sm()
        .text_color(theme.popover_foreground)
        .child(
            div()
                .line_clamp(PROMPT_LINES)
                .text_ellipsis()
                .child(SharedString::from(preview.prompt.clone())),
        )
        .child(divider())
        .child(response)
        .child(divider())
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(authors.text_color(theme.popover_foreground))
                .child(model)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(Icon::new(IconName::Clock).size(px(14.)).flex_none())
                        .child(started_label(preview.started_at, now)),
                ),
        )
        // A preview, not part of the conversation: clicks on it neither
        // focus nor select anything.
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
}

impl Cowork {
    /// The rail of the active thread's `turns` and, while a marker is
    /// hovered, its card, to place over the stage. Nothing without turns.
    pub(crate) fn render_turn_navigation(
        &mut self,
        thread_id: Uuid,
        thread: &Entity<Thread>,
        turns: &[Turn],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.turn_rail.show_thread(thread_id);
        let index_of =
            |id: Option<Uuid>| id.and_then(|id| turns.iter().position(|turn| turn.group == id));
        let hovered_turn = index_of(self.turn_rail.hovered);
        let hovered = if self.turn_rail.hovered_end {
            Some(turns.len())
        } else {
            hovered_turn
        };
        if hovered_turn.is_none() {
            self.turn_rail.hovered = None;
        }
        if thread.read(cx).timeline.is_empty() {
            return Vec::new();
        }
        // Until the timeline has placed its rows, the view is at the end.
        let current = index_of(self.timeline_layout.borrow().current_turn(thread_id))
            .or_else(|| turns.len().checked_sub(1));
        // Keep the current turn's marker in view as it changes, and only
        // then, so as not to fight someone browsing the rail.
        if let Some(current) = current
            && self.turn_rail.shown_current != Some(turns[current].group)
        {
            self.turn_rail.shown_current = Some(turns[current].group);
            self.turn_rail.scroll.scroll_to_item(current);
        }

        let owner = cx.entity().downgrade();
        let on_hover: HoverMarker = Rc::new(move |id, hovered, _, cx| {
            _ = owner.update(cx, |this, cx| {
                if let Some(id) = id {
                    if hovered {
                        this.turn_rail.hovered_end = false;
                    }
                    this.hover_turn_marker(id, hovered, cx);
                } else {
                    if hovered {
                        this.dismiss_turn_card(cx);
                    }
                    this.turn_rail.hovered_end = hovered;
                    cx.notify();
                }
            });
        });
        let owner = cx.entity().downgrade();
        let on_click: ClickMarker = Rc::new(move |id, _, cx| {
            _ = owner.update(cx, |this, cx| {
                if let Some(id) = id {
                    this.navigate_to_turn(id, cx);
                } else {
                    this.timeline_layout.borrow_mut().cancel_navigation();
                    this.timeline_scroll_handle.scroll_to_bottom();
                    this.follow_generation = true;
                    this.dismiss_turn_card(cx);
                    cx.notify();
                }
            });
        });
        let markers = turns.iter().map(|turn| turn.group).collect::<Vec<_>>();
        let mut elements = vec![render_rail(
            &markers,
            hovered,
            current,
            &self.turn_rail.scroll,
            on_hover,
            on_click,
            window,
            cx,
        )];
        elements.extend(
            hovered_turn
                .and_then(|index| self.render_hovered_turn_card(thread, &turns[index], index, cx)),
        );
        elements
    }

    /// The hovered turn's card, beside its marker and inside the stage, as
    /// both were placed in the last frame.
    fn render_hovered_turn_card(
        &self,
        thread: &Entity<Thread>,
        turn: &Turn,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let stage = self.timeline_scroll_handle.bounds();
        let marker = self.turn_rail.scroll.bounds_for_item(index)?;
        let width = (stage.size.width - RAIL_WIDTH - CARD_GAP - CARD_INSET).min(CARD_WIDTH);
        if width < MIN_CARD_WIDTH {
            return None;
        }
        let preview = TurnPreview::of(thread.read(cx), turn)?;
        let marker_y = marker.center().y + self.turn_rail.scroll.offset().y;
        let height = stage.size.height;
        let card = render_turn_card(&preview, &self.shown_profiles, SystemTime::now(), cx)
            .w(width)
            .max_h(height - CARD_INSET * 2.);
        Some(
            PositionedTurnCard {
                card: card.into_any_element(),
                stage,
                marker_y,
            }
            .into_any_element(),
        )
    }

    fn hover_turn_marker(&mut self, id: Uuid, hovered: bool, cx: &mut Context<Self>) {
        let rail = &mut self.turn_rail;
        if hovered {
            rail.hovered = Some(id);
        } else if rail.hovered == Some(id) {
            rail.hovered = None;
        }
        cx.notify();
    }

    /// Closes the card, if one shows; true when it did.
    pub(crate) fn dismiss_turn_card(&mut self, cx: &mut Context<Self>) -> bool {
        let rail = &mut self.turn_rail;
        let shown = rail.hovered.take().is_some();
        if shown {
            cx.notify();
        }
        shown
    }

    /// Scrolls the active thread to turn `group`, and stops following a
    /// run's output there, as scrolling up by hand does.
    pub(crate) fn navigate_to_turn(&mut self, group: Uuid, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        self.timeline_layout.borrow_mut().navigate(thread_id, group);
        self.follow_generation = false;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent::{Entry, InputId, Origin, model::Message};
    use draft::{AttachmentId, AttachmentRecord};

    use super::*;
    use crate::{
        models::ModelProvider,
        protocol::{self, HostMessage, Preview, PreviewDelta, RunEvent},
        thread_draft::ThreadDraft,
        timeline::PromptBlock,
        transcript::shared_entry,
    };

    #[test]
    fn preview_accumulation_bounds_unicode_and_stops_formatting() {
        let mut complete = PreviewText::default();
        complete.write_str("A comment").unwrap();
        complete.section().unwrap();
        complete.write_str("An attachment (text)").unwrap();
        assert_eq!(complete.finish(), "A comment\nAn attachment (text)");

        let mut exact = PreviewText::default();
        let full = "x".repeat(PREVIEW_BYTES);
        exact.write_str(&full).unwrap();
        assert_eq!(exact.finish(), full, "an exact fit needs no ellipsis");

        struct UnreachablePart;
        impl fmt::Display for UnreachablePart {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("formatting must stop after the budget is exhausted");
            }
        }
        let mut truncated = PreviewText::default();
        truncated.write_str(&"a".repeat(PREVIEW_BYTES - 4)).unwrap();
        assert!(write!(truncated, "🙂🙂{}", UnreachablePart).is_err());
        assert!(truncated.write_str("ignored").is_err());
        let text = truncated.finish();
        assert!(text.len() <= PREVIEW_BYTES);
        assert!(text.ends_with('…'));
        assert_eq!(text.trim_end_matches('…'), "a".repeat(PREVIEW_BYTES - 4));
    }

    /// A thread folding host messages, as the host's own and every
    /// collaborator's copy do, numbering transcript entries as it goes.
    struct Script {
        thread: Thread,
        entries: u64,
    }

    impl Script {
        fn new(host: ParticipantId) -> Self {
            Self {
                thread: Thread::new_local(
                    "Turns".into(),
                    Vec::new(),
                    ThreadDraft::new(host),
                    host,
                    Arc::default(),
                    None,
                ),
                entries: 0,
            }
        }

        fn apply(&mut self, message: HostMessage, cx: &mut App) {
            self.thread.apply_for_test(message, cx);
        }

        /// A run starting with `model` at `started_at`.
        fn start(
            &mut self,
            run: Uuid,
            model: Option<ModelRef>,
            started_at: SystemTime,
            cx: &mut App,
        ) {
            self.apply(
                HostMessage::AgentStarted {
                    id: run.into_bytes(),
                    comment_group_id: None,
                    started_at,
                    model,
                },
                cx,
            );
        }

        /// Input `id` joining run `run`, from `source`.
        fn input(&mut self, run: Uuid, id: InputId, source: RunSource, cx: &mut App) {
            let index = self.entries;
            self.entries += 1;
            let entry = Entry {
                message: Message::user("input"),
                origin: Origin::Input { id, source },
            };
            self.apply(
                HostMessage::AgentEvent {
                    id: run.into_bytes(),
                    event: RunEvent::Committed {
                        index,
                        entry: shared_entry(&entry),
                    },
                },
                cx,
            );
        }

        fn preview(&mut self, run: Uuid, text: &str, cx: &mut App) {
            self.apply(
                HostMessage::AgentEvent {
                    id: run.into_bytes(),
                    event: RunEvent::Preview(Preview {
                        index: 0,
                        delta: PreviewDelta::Text(text.into()),
                    }),
                },
                cx,
            );
        }

        fn end(&mut self, run: Uuid, outcome: RunOutcome, cx: &mut App) {
            self.apply(
                HostMessage::AgentEnded {
                    id: run.into_bytes(),
                    outcome,
                    duration: Duration::from_secs(1),
                },
                cx,
            );
        }

        fn turns(&self) -> Vec<Turn> {
            conversation_turns(&self.thread.timeline)
        }

        fn preview_of(&self, group: Uuid) -> TurnPreview {
            let turn = self
                .turns()
                .into_iter()
                .find(|turn| turn.group == group)
                .expect("a turn");
            TurnPreview::of(&self.thread, &turn).expect("its preview")
        }
    }

    fn block(author: ParticipantId, text: &str) -> PromptBlock {
        PromptBlock {
            id: Uuid::new_v4(),
            author,
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    fn comment(author: ParticipantId, body: &str) -> protocol::UserComment {
        protocol::UserComment {
            id: Uuid::new_v4().into_bytes(),
            author: author.into_bytes(),
            reference: protocol::CommentReference {
                message_id: Uuid::new_v4().into_bytes(),
                range: 0..4,
                quote: "text".into(),
            },
            body: body.into(),
        }
    }

    fn group(
        id: Uuid,
        comments: Vec<protocol::UserComment>,
        blocks: &[PromptBlock],
    ) -> protocol::UserMessage {
        protocol::UserMessage {
            id: id.into_bytes(),
            comments,
            blocks: blocks.iter().map(PromptBlock::to_protocol).collect(),
        }
    }

    /// A collaborative submission is one turn with all its authors; a steer
    /// joining its run adds neither a turn nor an author; a queued group is
    /// a turn only once its own run starts.
    #[gpui::test]
    fn turns_are_the_groups_that_start_runs(cx: &mut gpui::TestAppContext) {
        let (host, alice, bob, carol) = (
            ParticipantId::new(),
            ParticipantId::new(),
            ParticipantId::new(),
            ParticipantId::new(),
        );
        cx.update(|cx| {
            let mut script = Script::new(host);
            let (first, run) = (Uuid::new_v4(), Uuid::new_v4());
            script.apply(
                HostMessage::UserMessage(group(
                    first,
                    vec![comment(bob, "Why this way?")],
                    &[
                        block(alice, "Fix the crash."),
                        block(bob, "Add a test."),
                        block(alice, "Thanks."),
                    ],
                )),
                cx,
            );
            assert!(script.turns().is_empty(), "no run has started yet");
            script.start(run, None, SystemTime::now(), cx);
            script.input(
                run,
                InputId::new(),
                RunSource::Participant(host.into_bytes()),
                cx,
            );
            assert_eq!(script.turns().len(), 1);

            // Carol steers the run: her group is delivered, then joins it.
            let steer = Uuid::new_v4();
            script.apply(
                HostMessage::PendingDelivered {
                    id: steer.into_bytes(),
                    group: group(steer, Vec::new(), &[block(carol, "Also check Windows.")]),
                },
                cx,
            );
            script.input(
                run,
                steer.into(),
                RunSource::Participant(carol.into_bytes()),
                cx,
            );
            let turns = script.turns();
            assert_eq!(
                turns.iter().map(|turn| turn.group).collect::<Vec<_>>(),
                [first]
            );
            assert_eq!(script.preview_of(first).authors, [bob, alice]);
            script.end(run, RunOutcome::Completed, cx);

            // A queued group is delivered as its run starts, not before.
            let (queued, next) = (Uuid::new_v4(), Uuid::new_v4());
            script.apply(
                HostMessage::PendingDelivered {
                    id: queued.into_bytes(),
                    group: group(queued, Vec::new(), &[block(carol, "Then refactor.")]),
                },
                cx,
            );
            assert_eq!(script.turns().len(), 1);
            script.start(next, None, SystemTime::now(), cx);
            let turns = script.turns();
            assert_eq!(
                turns.iter().map(|turn| turn.group).collect::<Vec<_>>(),
                [first, queued]
            );
            assert_eq!(script.preview_of(queued).authors, [carol]);
        });
    }

    /// The card shows each run's own response, model and start time, even
    /// once the thread has moved to another model, and says honestly when
    /// there is no response or no recorded model.
    #[gpui::test]
    fn previews_use_each_runs_own_records(cx: &mut gpui::TestAppContext) {
        let host = ParticipantId::new();
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let first_model = ModelRef {
            provider: ModelProvider::Ollama,
            id: "qwen3:8b".into(),
        };
        let later_model = ModelRef {
            provider: ModelProvider::Anthropic,
            id: "claude".into(),
        };
        cx.update(|cx| {
            let mut script = Script::new(host);

            // Comments only.
            let (commented, run) = (Uuid::new_v4(), Uuid::new_v4());
            script.apply(
                HostMessage::UserMessage(group(
                    commented,
                    vec![comment(host, "Is this safe?")],
                    &[],
                )),
                cx,
            );
            script.start(run, Some(first_model.clone()), earlier, cx);
            script.input(
                run,
                InputId::new(),
                RunSource::Participant(host.into_bytes()),
                cx,
            );
            script.preview(run, "It is, because…", cx);
            script.end(run, RunOutcome::Stopped, cx);

            // A scheduled run whose attachment-only prompt got no answer.
            script.apply(HostMessage::ModelSelected(later_model.clone()), cx);
            let (attached, scheduled) = (Uuid::new_v4(), Uuid::new_v4());
            let mut files = block(host, "");
            files.attachments.push(AttachmentRecord {
                id: AttachmentId::new(),
                name: "crash.log".into(),
                kind: AttachmentKind::Text,
                size: 3,
                creator: host.as_uuid(),
            });
            script.apply(
                HostMessage::UserMessage(group(attached, Vec::new(), &[files])),
                cx,
            );
            script.start(scheduled, None, SystemTime::now(), cx);
            script.input(scheduled, InputId::new(), RunSource::Schedule, cx);
            assert_eq!(
                script.preview_of(attached).response,
                TurnResponse::Status("Working…")
            );
            script.end(scheduled, RunOutcome::Completed, cx);

            let first = script.preview_of(commented);
            assert_eq!(first.prompt, "“Is this safe?”");
            assert_eq!(first.response, TurnResponse::Text("It is, because…".into()));
            assert_eq!(first.model, Some(first_model));
            assert_eq!(first.started_at, earlier);
            assert!(!first.scheduled);

            let second = script.preview_of(attached);
            assert_eq!(second.prompt, "crash.log (text)");
            assert_eq!(second.response, TurnResponse::Status("No response"));
            assert_eq!(second.model, None, "never the thread's current model");
            assert!(second.scheduled);
            assert_eq!(second.authors, [host]);
        });
    }

    #[test]
    fn hovering_widens_two_neighbors_on_each_side() {
        let widths = (0..7)
            .map(|index| pill_width(index, Some(3)))
            .collect::<Vec<_>>();
        assert!(widths[3] > widths[2] && widths[2] > widths[1] && widths[1] > widths[0]);
        assert_eq!(widths[2], widths[4]);
        assert_eq!(widths[1], widths[5]);
        assert_eq!(widths[0], widths[6]);
        assert!((0..7).all(|index| pill_width(index, None) == widths[0]));
    }

    #[test]
    fn start_times_read_relatively_and_locally() {
        let now = SystemTime::now();
        let ago = |seconds| time_ago(now - Duration::from_secs(seconds), now);
        assert_eq!(ago(5).as_deref(), Some("just now"));
        assert_eq!(ago(5 * 60).as_deref(), Some("5 min ago"));
        assert_eq!(ago(3 * 3_600).as_deref(), Some("3 h ago"));
        assert_eq!(ago(2 * 86_400).as_deref(), Some("2 d ago"));
        // Another computer's clock can be ahead: only the date is shown.
        let ahead = now + Duration::from_secs(120);
        assert_eq!(time_ago(ahead, now), None);
        assert!(!started_label(ahead, now).contains("ago"));
    }
}
