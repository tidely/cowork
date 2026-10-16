//! The card asking whether a tool call may run: what the call would do, and
//! Allow and Deny for someone who may decide, or a note that it waits for
//! someone who may.
//!
//! The card is the shared shell. A tool that wants to show its calls in its
//! own terms passes its own [`ToolCallCard::body`]; `run_command` and
//! `request_permissions` are shown in theirs here, and every other tool gets
//! its arguments listed.
//!
//! It keeps to a title, what the call would do, and the decision. Only a
//! call asking for more than the thread's mode gets a badge beside the
//! title, naming only the additional permissions, and a quiet note beside
//! the decision of how long it lasts.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, FontWeight, IntoElement, Pixels, RenderOnce, SharedString,
    StyleRefinement, StyledText, Window, div, prelude::*, px, relative,
};
use gpui_base::text::CodeBlock;
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    group_box::{GroupBox, GroupBoxVariants as _},
    h_flex,
    shimmer::ShimmerText,
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName;
use rig::{message::ToolCall, tool::Tool as _};
use sandbox::{Access, Folders, Grant, RequestPermissions, RunCommand};

use crate::highlight::highlight_code_block_in_mode;

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// What the card offers under the call.
enum Decision {
    /// Allow and Deny, for someone who may approve tool calls.
    Offered {
        on_allow: ClickHandler,
        on_deny: ClickHandler,
    },
    /// A note that the call waits for someone who may.
    Waiting,
}

/// Who a call that waits is waiting for, as its note names them.
const DECIDERS: &str = "the host or an admin";
const HOST: &str = "the host";

#[derive(IntoElement)]
pub(crate) struct ToolCallCard {
    /// Prefixes the ids and debug selectors of the card's parts, so it must
    /// be unique among the cards on screen.
    id: SharedString,
    call: ToolCall,
    body: Option<AnyElement>,
    decision: Decision,
    waiting_for: &'static str,
    /// The project's mode's access, which a command runs with unless it
    /// asks for its own; `None` when the card isn't shown for a thread.
    project_access: Option<Access>,
    /// Host-only resolved coverage. Collaborators don't know host paths or
    /// live run grants and continue to show a conservative badge.
    approved: Option<Grant>,
}

impl ToolCallCard {
    /// A card for `call` that lists its arguments and waits for a decision,
    /// until [`Self::decide`] offers one.
    pub(crate) fn new(id: impl Into<SharedString>, call: ToolCall) -> Self {
        Self {
            id: id.into(),
            call,
            body: None,
            decision: Decision::Waiting,
            waiting_for: DECIDERS,
            project_access: None,
            approved: None,
        }
    }

    /// Shows what a command would be allowed to do in a project with
    /// `access`, and when it asks for more.
    pub(crate) fn project_access(mut self, access: Access) -> Self {
        self.project_access = Some(access);
        self
    }

    /// Includes already approved project folders and live run grants when
    /// deciding which permissions the pending call would add.
    pub(crate) fn approved(mut self, grant: Grant) -> Self {
        self.approved = Some(grant);
        self
    }

    /// Names only the host in the waiting note, for a tool only the host may
    /// allow.
    pub(crate) fn host_only(mut self) -> Self {
        self.waiting_for = HOST;
        self
    }

    /// Shows `body` instead of the arguments: a tool's own presentation of
    /// what the call would do.
    #[expect(dead_code, reason = "for tools' own cards; none passes one yet")]
    pub(crate) fn body(mut self, body: impl IntoElement) -> Self {
        self.body = Some(body.into_any_element());
        self
    }

    /// Offers Allow and Deny in place of the waiting note. Only for someone
    /// who may approve tool calls; the card does not check.
    pub(crate) fn decide(
        mut self,
        on_allow: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_deny: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.decision = Decision::Offered {
            on_allow: Rc::new(on_allow),
            on_deny: Rc::new(on_deny),
        };
        self
    }
}

/// The badge for a call asking for more than the thread's mode.
struct ExtraAccess {
    access: ExtraRights,
    /// Starts the badge's tooltip, before the access's name.
    tooltip: &'static str,
    /// How long the access lasts, as the note beside the decision says it;
    /// `None` when the title already says.
    scope: Option<&'static str>,
}

/// Writes and network are independent when showing what an approval adds.
/// A network command still runs with write access, but its badge must not
/// ask the user to approve writes their project already permits.
#[derive(Debug, PartialEq, Eq)]
enum ExtraRights {
    Write,
    Network,
    WriteAndNetwork,
}

impl ExtraRights {
    fn label(&self) -> &'static str {
        match self {
            Self::Write => "Write",
            Self::Network => "Network",
            Self::WriteAndNetwork => "Write + Network",
        }
    }

    fn icon(&self) -> IconName {
        match self {
            Self::Write => IconName::Pencil,
            Self::Network | Self::WriteAndNetwork => IconName::Globe,
        }
    }
}

impl RenderOnce for ToolCallCard {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let muted = cx.theme().muted_foreground;
        let tool = self.call.function.name.as_str();
        let (icon, title) = title(tool);
        let mut access = None;
        let body =
            self.body.unwrap_or_else(|| {
                let arguments = self.call.function.arguments_value();
                if tool == RunCommand::NAME
                    && let Some(command) = arguments["command"].as_str()
                {
                    access = extra_access(&arguments, self.project_access, self.approved.as_ref())
                        .map(|access| ExtraAccess {
                            access,
                            tooltip: "This command asks for",
                            scope: Some("For this command only"),
                        });
                    command_view(&id, command, &arguments, cx)
                } else if tool == RequestPermissions::NAME
                    && let Some(reason) = arguments["reason"].as_str()
                {
                    // The title already says the access lasts for the turn.
                    access = extra_access(&arguments, self.project_access, self.approved.as_ref())
                        .map(|access| ExtraAccess {
                            access,
                            tooltip: "Later commands in this turn get",
                            scope: None,
                        });
                    permissions_view(&id, reason, &arguments, cx)
                } else {
                    arguments_view(&id, &self.call, cx)
                }
            });

        let header = h_flex()
            .min_w_0()
            .gap_2()
            .justify_between()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(Icon::new(icon).size_3p5().text_color(muted).flex_none())
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    ),
            )
            .children(access.as_ref().map(|shown| access_badge(&id, shown, cx)));

        let decision = match self.decision {
            Decision::Offered { on_allow, on_deny } => {
                let deny = format!("{id}-deny");
                let allow = format!("{id}-allow");
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(deny.clone()))
                            .debug_selector(move || deny.clone())
                            .ghost()
                            .xsmall()
                            .label("Deny")
                            .on_click(move |event, window, cx| on_deny(event, window, cx)),
                    )
                    .child(
                        Button::new(SharedString::from(allow.clone()))
                            .debug_selector(move || allow.clone())
                            .primary()
                            .xsmall()
                            .label("Allow")
                            .on_click(move |event, window, cx| on_allow(event, window, cx)),
                    )
                    .into_any_element()
            }
            Decision::Waiting => {
                let waiting = format!("{id}-waiting");
                div()
                    .flex_none()
                    .debug_selector({
                        let waiting = waiting.clone();
                        move || waiting.clone()
                    })
                    .text_color(muted)
                    // Shimmering like the "Working for" line, as both are
                    // something still under way.
                    .child(
                        ShimmerText::new(format!("Waiting for {}…", self.waiting_for))
                            .id(SharedString::from(waiting)),
                    )
                    .into_any_element()
            }
        };
        let warning = access
            .and_then(|shown| shown.scope)
            .map(|scope| escalation_note(&id, scope, cx));
        let footer = h_flex()
            .min_w_0()
            .gap_2()
            .justify_between()
            .child(div().flex_1().min_w_0().children(warning))
            .child(decision);

        div().debug_selector(move || id.to_string()).child(
            GroupBox::new()
                .outline()
                .content_style(StyleRefinement::default().px_3().py_2p5().gap_2().text_xs())
                .child(header)
                .child(body)
                .child(footer),
        )
    }
}

/// The card's icon and title: what the call does in the user's words rather
/// than the tool's identifier.
fn title(tool: &str) -> (IconName, SharedString) {
    match tool {
        RunCommand::NAME => (IconName::SquareTerminal, "Terminal".into()),
        RequestPermissions::NAME => (IconName::Shield, "Permissions for this turn".into()),
        _ => (IconName::Wrench, humanize(tool).into()),
    }
}

/// `search_files` as "Search files".
fn humanize(tool: &str) -> String {
    let words = tool.replace('_', " ");
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => tool.to_owned(),
    }
}

/// Only the rights an approval adds beyond the project's automatic access.
/// On the host, named write folders already covered by the project or run
/// grants add no rights. Without host coverage they conservatively add writes.
/// Reading outside folders is shown by their lines. Network and writes are independent so an
/// approval in Write mode asks only for Network, unless it adds write folders.
fn extra_access(
    arguments: &serde_json::Value,
    project: Option<Access>,
    approved: Option<&Grant>,
) -> Option<ExtraRights> {
    // Outside a thread there is no mode, and anything above Read is extra.
    let project = project.unwrap_or(Access::Read);
    let requested = RunCommand::requested_access(arguments).unwrap_or(project);
    let (_, write) = RunCommand::named_folders(arguments);
    let baseline = approved.map_or(project, |grant| project.max(grant.access));
    let extra_folders = !write.is_empty()
        && !approved.is_some_and(|grant| {
            Folders::resolve(&[], &write).is_ok_and(|folders| grant.folders.covers(&folders))
        });
    added_rights(requested, baseline, extra_folders)
}

fn added_rights(requested: Access, baseline: Access, extra_folders: bool) -> Option<ExtraRights> {
    let writes = extra_folders || (requested >= Access::Write && baseline < Access::Write);
    let network = requested == Access::Network && baseline < Access::Network;
    match (writes, network) {
        (true, true) => Some(ExtraRights::WriteAndNetwork),
        (true, false) => Some(ExtraRights::Write),
        (false, true) => Some(ExtraRights::Network),
        (false, false) => None,
    }
}

/// The rights the approval adds, in the warning color.
fn access_badge(id: &SharedString, shown: &ExtraAccess, cx: &App) -> AnyElement {
    let selector = format!("{id}-access");
    let tooltip = SharedString::from(format!("{} {} access", shown.tooltip, shown.access.label()));
    h_flex()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .gap_1()
        .text_color(cx.theme().warning)
        .child(Icon::new(shown.access.icon()).size_3())
        .child(shown.access.label())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .into_any_element()
}

/// The quiet note beside the decision of how long the badge's access lasts.
fn escalation_note(id: &SharedString, scope: &'static str, cx: &App) -> AnyElement {
    let selector = format!("{id}-escalation");
    div()
        .debug_selector(move || selector.clone())
        .truncate()
        .text_color(cx.theme().muted_foreground)
        .child(scope)
        .into_any_element()
}

/// How tall the arguments may grow before they scroll: about seven lines, so
/// a call with long arguments stays a card rather than a page.
const MAX_ARGUMENTS_HEIGHT: Pixels = px(112.);

/// The call's arguments as pretty-printed JSON, in a block of their own.
fn arguments_view(id: &SharedString, call: &ToolCall, cx: &App) -> AnyElement {
    // A `Value` always encodes.
    let json = serde_json::to_string_pretty(&call.function.arguments_value()).unwrap_or_default();
    code_block(format!("{id}-arguments"), json, cx)
}

/// The folders outside the project a call names, as written, one per line:
/// those it would change in the warning color, as nothing else outside the
/// project and scratch folder may be changed, and those it would read. The
/// icons are the modes' own, leaving the one warning sign to the footer.
fn folders_view(id: &SharedString, arguments: &serde_json::Value, cx: &App) -> Option<AnyElement> {
    let (read, write) = RunCommand::named_folders(arguments);
    if read.is_empty() && write.is_empty() {
        return None;
    }
    let line = |icon: IconName, verb: &str, path: String, color| {
        h_flex()
            .gap_1()
            .min_w_0()
            .text_color(color)
            .child(Icon::new(icon).size_3())
            .child(div().flex_none().child(verb.to_owned()))
            .child(
                div()
                    .truncate()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(path),
            )
    };
    let selector = format!("{id}-folders");
    Some(
        div()
            .debug_selector(move || selector.clone())
            .flex()
            .flex_col()
            .gap_0p5()
            .children(
                write
                    .into_iter()
                    .map(|path| line(IconName::Pencil, "can change", path, cx.theme().warning)),
            )
            .children(
                read.into_iter()
                    .map(|path| line(IconName::Eye, "can read", path, cx.theme().muted_foreground)),
            )
            .into_any_element(),
    )
}

/// Highlights the exact command, without adding a prompt that could change
/// how the shell syntax is parsed or what someone copies from the card.
fn command_view(
    id: &SharedString,
    command: &str,
    arguments: &serde_json::Value,
    cx: &App,
) -> AnyElement {
    let block = CodeBlock::from_code(command, Some("sh"));
    let highlights = highlight_code_block_in_mode(&block, cx.theme().is_dark());
    let line_count = command.lines().count();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(folders_view(id, arguments, cx))
        .child(code_block(
            format!("{id}-command"),
            StyledText::new(command.to_owned()).with_highlights(highlights),
            cx,
        ))
        .when(line_count > 6, |view| {
            let selector = format!("{id}-command-continuation");
            view.child(
                div()
                    .debug_selector(move || selector.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{line_count} lines, scroll for more")),
            )
        })
        .into_any_element()
}

/// A `request_permissions` call: why the agent asks and the folders it asks
/// for; the access is beside the title.
fn permissions_view(
    id: &SharedString,
    reason: &str,
    arguments: &serde_json::Value,
    cx: &App,
) -> AnyElement {
    let reason_selector = format!("{id}-reason");
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .debug_selector(move || reason_selector.clone())
                .text_sm()
                .child(reason.to_owned()),
        )
        .children(folders_view(id, arguments, cx))
        .into_any_element()
}

/// Monospace content that scrolls past a few lines, for a card's body.
fn code_block(id: impl Into<SharedString>, text: impl IntoElement, cx: &App) -> AnyElement {
    let id = id.into();
    div()
        .debug_selector({
            let id = id.clone();
            move || id.to_string()
        })
        .id(id)
        .max_h(MAX_ARGUMENTS_HEIGHT)
        .overflow_y_scroll()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(cx.theme().secondary)
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs()
        .line_height(relative(1.35))
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{Modifiers, Render};
    use rig::message::{CallId, ToolFunction, ToolName};
    use serde_json::json;

    use super::*;

    fn call() -> ToolCall {
        ToolCall::new(
            CallId::from_wire("call_1"),
            ToolFunction::new(
                ToolName::new("calculate").expect("a valid name"),
                json!({"a": 1, "b": 2, "operation": "add"}),
            ),
        )
    }

    struct CardView {
        call: ToolCall,
        offer: bool,
        allowed: Rc<Cell<u32>>,
        denied: Rc<Cell<u32>>,
    }

    impl Render for CardView {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            let card = ToolCallCard::new("card", self.call.clone());
            let card = if self.offer {
                let allowed = self.allowed.clone();
                let denied = self.denied.clone();
                card.decide(
                    move |_, _, _| allowed.set(allowed.get() + 1),
                    move |_, _, _| denied.set(denied.get() + 1),
                )
            } else {
                card
            };
            div().w(px(480.)).child(card)
        }
    }

    /// A command's card in a project with `project` access.
    struct AccessView {
        call: ToolCall,
        project: Access,
    }

    impl Render for AccessView {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .w(px(640.))
                .child(ToolCallCard::new("card", self.call.clone()).project_access(self.project))
        }
    }

    /// The card says what the command could do, and warns only when it
    /// asks for more than the project's mode.
    #[gpui::test]
    fn command_cards_show_their_access(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        for (project, arguments, extra) in [
            (Access::Write, json!({"command": "ls"}), None),
            (
                Access::Write,
                json!({"command": "ls", "access": "read"}),
                None,
            ),
            (
                Access::Network,
                json!({"command": "cargo fetch", "access": "network"}),
                None,
            ),
            (
                Access::Read,
                json!({"command": "cargo fetch", "access": "network"}),
                Some(ExtraRights::WriteAndNetwork),
            ),
            (
                Access::Write,
                json!({"command": "cargo fetch", "access": "network"}),
                Some(ExtraRights::Network),
            ),
            (
                Access::Write,
                json!({"command": "cargo fetch", "access": "network", "write_folders": ["~/.cargo"]}),
                Some(ExtraRights::WriteAndNetwork),
            ),
            (
                Access::Network,
                json!({"command": "cargo fetch", "access": "network", "write_folders": ["~/.cargo"]}),
                Some(ExtraRights::Write),
            ),
            (
                Access::Read,
                json!({"command": "cargo new ~/github/agent", "write_folders": ["~/github"]}),
                Some(ExtraRights::Write),
            ),
            (
                Access::Read,
                json!({"command": "cat /srv/docs/a", "read_folders": ["/srv/docs"]}),
                None,
            ),
        ] {
            assert_eq!(
                extra_access(&arguments, Some(project), None),
                extra,
                "{arguments}"
            );
        }
        // Cards within the thread's mode don't repeat it.
        for (arguments, extra) in [
            (json!({"command": "ls"}), false),
            (json!({"command": "cargo fetch", "access": "network"}), true),
        ] {
            let call = command_call(arguments);
            let (_, cx) = cx.add_window_view(|_, _| AccessView {
                call,
                project: Access::Read,
            });
            cx.run_until_parked();
            assert_eq!(cx.debug_bounds("card-access").is_some(), extra);
            assert_eq!(cx.debug_bounds("card-escalation").is_some(), extra);
        }
    }

    #[test]
    fn permission_badges_account_for_existing_folder_grants() {
        // Resolved as grants are: on Windows that is not `canonicalize`'s
        // verbatim `\\?\` form, which no grant would cover.
        let resolve = |path: &std::path::Path| {
            let named = path.to_string_lossy().into_owned();
            Folders::resolve(&[], &[named]).unwrap().write.remove(0)
        };
        let here = resolve(&std::env::current_dir().unwrap());
        let parent = resolve(here.parent().unwrap());
        let arguments = json!({"command": "fetch", "access": "network", "write_folders": [here]});
        // A run grant for an external folder, or a writable project containing
        // the requested folder, both leave only network permission to approve.
        for covered in [here.clone(), parent.clone()] {
            let approved = Grant {
                access: Access::Write,
                folders: Folders {
                    read: vec![],
                    write: vec![covered],
                },
            };
            assert_eq!(
                extra_access(&arguments, Some(Access::Write), Some(&approved)),
                Some(ExtraRights::Network)
            );
        }
        let approved = Grant {
            access: Access::Write,
            folders: Folders {
                read: vec![],
                write: vec![here],
            },
        };
        let new_folder =
            json!({"command": "fetch", "access": "network", "write_folders": [parent]});
        assert_eq!(
            extra_access(&new_folder, Some(Access::Write), Some(&approved)),
            Some(ExtraRights::WriteAndNetwork)
        );
        let network_grant = Grant {
            access: Access::Network,
            ..approved
        };
        assert_eq!(
            extra_access(&arguments, Some(Access::Read), Some(&network_grant)),
            None
        );
    }

    /// A command shows as a command rather than JSON, lists the folders
    /// outside the project it names, and falls back to its raw arguments
    /// when they aren't a command.
    #[gpui::test]
    fn command_cards_show_the_command_its_folders_or_raw_arguments(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        for (arguments, command, folders) in [
            (
                json!({"command": "printf '%s\\n' \"hello\"\npwd"}),
                true,
                false,
            ),
            (
                json!({"command": "cargo fetch", "write_folders": ["~/.cargo"]}),
                true,
                true,
            ),
            (json!({"command": 42}), false, false),
        ] {
            let call = command_call(arguments);
            let (_, cx) = cx.add_window_view(|_, _| AccessView {
                call,
                project: Access::Read,
            });
            cx.run_until_parked();
            assert_eq!(cx.debug_bounds("card-command").is_some(), command);
            assert_eq!(cx.debug_bounds("card-arguments").is_some(), !command);
            assert_eq!(cx.debug_bounds("card-folders").is_some(), folders);
        }
    }

    /// A request for permissions shows its reason, its folders, and a badge
    /// only for access beyond the thread's mode.
    #[gpui::test]
    fn permission_requests_show_what_they_ask_for(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let request = |arguments| {
            ToolCall::new(
                CallId::from_wire("request_1"),
                ToolFunction::new(ToolName::new(RequestPermissions::NAME).unwrap(), arguments),
            )
        };
        for (project, arguments, access, folders) in [
            (
                Access::Read,
                json!({"reason": "Fetch.", "access": "network", "write_folders": ["~/.cargo"]}),
                true,
                true,
            ),
            (
                Access::Network,
                json!({"reason": "Fetch.", "access": "write"}),
                false,
                false,
            ),
            (
                Access::Write,
                json!({"reason": "Read docs.", "read_folders": ["/srv/docs"]}),
                false,
                true,
            ),
        ] {
            let call = request(arguments);
            let (_, cx) = cx.add_window_view(|_, _| AccessView { call, project });
            cx.run_until_parked();
            assert!(cx.debug_bounds("card-reason").is_some());
            assert!(cx.debug_bounds("card-arguments").is_none());
            assert_eq!(cx.debug_bounds("card-access").is_some(), access);
            // The title says how long the access lasts.
            assert!(cx.debug_bounds("card-escalation").is_none());
            assert_eq!(cx.debug_bounds("card-folders").is_some(), folders);
        }

        let call = request(json!({"access": "write"}));
        let (_, cx) = cx.add_window_view(|_, _| AccessView {
            call,
            project: Access::Read,
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("card-arguments").is_some(),
            "without a reason, the arguments are listed"
        );
    }

    fn render_card(
        offer: bool,
        cx: &mut gpui::TestAppContext,
    ) -> (Rc<Cell<u32>>, Rc<Cell<u32>>, &mut gpui::VisualTestContext) {
        render_call_card(offer, call(), cx)
    }

    fn render_call_card(
        offer: bool,
        call: ToolCall,
        cx: &mut gpui::TestAppContext,
    ) -> (Rc<Cell<u32>>, Rc<Cell<u32>>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let allowed = Rc::new(Cell::new(0));
        let denied = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view(|_, _| CardView {
            call,
            offer,
            allowed: allowed.clone(),
            denied: denied.clone(),
        });
        cx.run_until_parked();
        (allowed, denied, cx)
    }

    #[gpui::test]
    fn only_deciders_get_allow_and_deny(cx: &mut gpui::TestAppContext) {
        let (allowed, denied, window) = render_card(true, cx);
        assert!(window.debug_bounds("card-waiting").is_none());

        let deny = window.debug_bounds("card-deny").expect("Deny is offered");
        window.simulate_click(deny.center(), Modifiers::default());
        assert_eq!((allowed.get(), denied.get()), (0, 1));

        let allow = window.debug_bounds("card-allow").expect("Allow is offered");
        window.simulate_click(allow.center(), Modifiers::default());
        assert_eq!((allowed.get(), denied.get()), (1, 1));

        // Everyone else sees that the call waits.
        let unused = Rc::new(Cell::new(0));
        let (_, window) = cx.add_window_view(|_, _| CardView {
            call: call(),
            offer: false,
            allowed: unused.clone(),
            denied: unused,
        });
        window.run_until_parked();
        assert!(window.debug_bounds("card-waiting").is_some());
        assert!(window.debug_bounds("card-allow").is_none());
        assert!(window.debug_bounds("card-deny").is_none());
    }

    fn command_call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(
            CallId::from_wire("command_1"),
            ToolFunction::new(ToolName::new(RunCommand::NAME).unwrap(), arguments),
        )
    }

    #[gpui::test]
    fn long_commands_show_a_hint_outside_the_scroll_area(cx: &mut gpui::TestAppContext) {
        let command = (0..30)
            .map(|line| format!("echo {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_, _, cx) = render_call_card(true, command_call(json!({"command": command})), cx);
        let command = cx.debug_bounds("card-command").expect("command is visible");
        let hint = cx
            .debug_bounds("card-command-continuation")
            .expect("long commands show a continuation hint");
        assert!(
            hint.top() >= command.bottom(),
            "hint is outside the clipped command"
        );
        let allow = cx.debug_bounds("card-allow").expect("Allow stays visible");
        assert!(allow.top() >= hint.bottom());
    }
}
