//! A window showing the tool call card for each kind of call the agent can
//! make (about permissions, and file writes' and edits' diffs), in each
//! project mode,
//! as the host sees it: a row per call and a
//! column per mode. Combinations that run without asking show that instead
//! of a card. `cargo run -p cowork --example component_preview` opens it.

use agent::model::ToolCall;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, Pixels, QuitMode, Render,
    SharedString, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_component::{ActiveTheme as _, Root};
use sandbox::{Access, RunCommand};
use serde_json::json;

use crate::{
    file_change_view::FileChangeView, project_mode::ProjectMode, tool_call_card::ToolCallCard,
};

/// Runs the preview until its window closes, as the app's own run does.
pub fn run() -> anyhow::Result<()> {
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(|cx: &mut App| {
            crate::init_app(cx);
            if let Err(error) = open(cx) {
                eprintln!("failed to open the component preview: {error}");
                cx.quit();
                return;
            }
            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });
    Ok(())
}

/// The columns, least access first.
const MODES: [ProjectMode; 3] = [ProjectMode::Read, ProjectMode::Write, ProjectMode::Network];
/// Gaps between cells and around the grid.
const SPACING: Pixels = px(24.);

fn open(cx: &mut App) -> anyhow::Result<()> {
    let options = WindowOptions {
        // Fullscreen, so that the grid shows as many states at once as fit.
        window_bounds: Some(WindowBounds::Fullscreen(gpui::Bounds::centered(
            None,
            size(px(1440.), px(900.)),
            cx,
        ))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Cowork component preview".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        let preview = cx.new(|cx| ComponentPreview {
            last_click: None,
            file_views: combinations()
                .into_iter()
                .filter_map(|(key, _, call)| {
                    let view = FileChangeView::of(format!("preview-{key}-change"), &call, cx)?;
                    Some((key, view))
                })
                .collect(),
        });
        cx.new(|cx| Root::new(preview, window, cx))
    })?;
    Ok(())
}

struct ComponentPreview {
    /// What the last button pressed would have done, to show it is wired.
    last_click: Option<SharedString>,
    /// The diff views of the file rows' calls, by row.
    file_views: std::collections::HashMap<&'static str, Entity<FileChangeView>>,
}

const GREETING: &str = "pub struct Greeting {\n    name: String,\n    excited: bool,\n}\n\nimpl Greeting {\n    pub fn new(name: &str) -> Self {\n        Self { name: name.to_owned(), excited: false }\n    }\n}\n";

impl ComponentPreview {
    /// `call`'s card in a project with `mode`, or a note that it runs
    /// without asking there.
    fn cell(
        &self,
        key: &str,
        call: &ToolCall,
        mode: ProjectMode,
        cx: &Context<Self>,
    ) -> AnyElement {
        if !asks(call, mode.access()) {
            return div()
                .px_3()
                .py_2p5()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border.opacity(0.5))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Runs without asking")
                .into_any_element();
        }
        let label = format!("{} in {}", call.name, mode.label());
        let allowed = SharedString::from(format!("Allowed {label}"));
        let denied = SharedString::from(format!("Denied {label}"));
        let card = ToolCallCard::new(format!("preview-{key}-{}", mode.key()), call.clone());
        let card = match self.file_views.get(key) {
            Some(view) => card.body(crate::file_change_view::card_body(view, cx)),
            None => card,
        };
        card.project_access(mode.access())
            .host_only()
            .decide(
                cx.listener(move |this, _, _, cx| {
                    this.last_click = Some(allowed.clone());
                    cx.notify();
                }),
                cx.listener(move |this, _, _, cx| {
                    this.last_click = Some(denied.clone());
                    cx.notify();
                }),
            )
            .into_any_element()
    }
}

impl Render for ComponentPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cx = &*cx;
        let muted = cx.theme().muted_foreground;
        let grid = || div().grid().grid_cols(3).gap(SPACING).items_start();
        // Above the scrolling rows, so the columns stay named.
        let header = div()
            .flex_none()
            .px(SPACING)
            .pt(SPACING)
            .pb_3()
            .flex()
            .flex_col()
            .gap_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(div().text_lg().child("Tool call cards by project mode"))
                    .child(
                        div().text_sm().text_color(muted).child(
                            self.last_click
                                .clone()
                                .unwrap_or_else(|| "Nothing clicked yet".into()),
                        ),
                    ),
            )
            .child(grid().children(MODES.map(|mode| {
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!("{} project", mode.label()))
            })));
        let rows = combinations().into_iter().map(|(key, title, call)| {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(div().text_xs().text_color(muted).child(title))
                .child(grid().children(MODES.map(|mode| self.cell(key, &call, mode, cx))))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(header)
            .child(
                div()
                    .id("component-preview")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .p(SPACING)
                            .flex()
                            .flex_col()
                            .gap(SPACING)
                            .children(rows),
                    ),
            )
    }
}

/// Whether `call` waits for the host in a project with `project` access,
/// as `tool_approval` decides at the start of a run: a command with read
/// access and no folders runs at once, a file write or edit asks only in a Read
/// project, and every other call asks.
fn asks(call: &ToolCall, project: Access) -> bool {
    if call.name == sandbox::WriteFile::NAME || call.name == sandbox::EditFile::NAME {
        return project == Access::Read;
    }
    if call.name != RunCommand::NAME {
        return true;
    }
    let arguments = crate::timeline::arguments_value(call);
    let access = RunCommand::requested_access(&arguments).unwrap_or(project);
    let (read, write) = RunCommand::named_folders(&arguments);
    access > Access::Read || !read.is_empty() || !write.is_empty()
}

/// The rows: each kind of call the agent can make about permissions.
fn combinations() -> Vec<(&'static str, &'static str, ToolCall)> {
    vec![
        (
            "command",
            "A command using the project's mode",
            tool_call("run_command", json!({"command": "cargo test"})),
        ),
        (
            "command-read",
            "A command asking for Read",
            tool_call(
                "run_command",
                json!({"command": "git log --oneline -5", "access": "read"}),
            ),
        ),
        (
            "command-write",
            "A command asking for Write",
            tool_call(
                "run_command",
                json!({"command": "cargo fmt", "access": "write"}),
            ),
        ),
        (
            "command-network",
            "A command asking for Write + Network",
            tool_call(
                "run_command",
                json!({"command": "cargo fetch", "access": "network"}),
            ),
        ),
        (
            "command-read-folder",
            "A command reading a folder outside the project",
            tool_call(
                "run_command",
                json!({"command": "ls /opt/shared/docs", "read_folders": ["/opt/shared/docs"]}),
            ),
        ),
        (
            "command-write-folder",
            "A command changing a folder outside the project",
            tool_call(
                "run_command",
                json!({
                    "command": "cargo new ~/github/agent",
                    "write_folders": ["~/github"],
                }),
            ),
        ),
        (
            "command-network-folder",
            "A command asking for Write + Network and a folder outside the project",
            tool_call(
                "run_command",
                json!({
                    "command": "cargo fetch",
                    "access": "network",
                    "write_folders": ["~/.cargo"],
                }),
            ),
        ),
        (
            "write-file",
            "Writing a file in the project",
            tool_call(
                sandbox::WriteFile::NAME,
                json!({"path": "greeting/src/greeting.rs", "content": GREETING}),
            ),
        ),
        (
            "edit-file",
            "Editing a file in the project",
            tool_call(
                sandbox::EditFile::NAME,
                json!({
                    "path": "greeting/src/greeting.rs",
                    "old_string": "    name: String,\n}",
                    "new_string": "    name: String,\n    excited: bool,\n}",
                }),
            ),
        ),
        (
            "edit-file-all",
            "Editing every occurrence in a file",
            tool_call(
                sandbox::EditFile::NAME,
                json!({
                    "path": "greeting/src/greeting.rs",
                    "old_string": "to_owned()",
                    "new_string": "to_string()",
                    "replace_all": true,
                }),
            ),
        ),
        (
            "permissions-write",
            "Permissions for the turn: Write",
            tool_call(
                "request_permissions",
                json!({"reason": "Format the code and fix the lints.", "access": "write"}),
            ),
        ),
        (
            "permissions-network",
            "Permissions for the turn: Write + Network and folders",
            tool_call(
                "request_permissions",
                json!({
                    "reason": "Download the project's dependencies and build it.",
                    "access": "network",
                    "write_folders": ["~/.cargo", "~/.rustup"],
                    "read_folders": ["/opt/shared/docs"],
                }),
            ),
        ),
        (
            "permissions-read-folder",
            "Permissions for the turn: reading a folder outside the project",
            tool_call(
                "request_permissions",
                json!({
                    "reason": "Check the shared docs for the API's conventions.",
                    "read_folders": ["/opt/shared/docs"],
                }),
            ),
        ),
    ]
}

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    crate::timeline::tool_call(&format!("preview_{name}"), name, arguments)
}

/// Opens a focused, interactive example of compaction settings and the
/// composer's context ring and inline thread indicators. No provider or running
/// agent is needed.
pub fn run_compaction() -> anyhow::Result<()> {
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(|cx: &mut App| {
            crate::init_app(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
                    None,
                    size(px(820.), px(900.)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Cowork context preview".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(error) = cx.open_window(options, |window, cx| {
                let preview = cx.new(|_| CompactionPreview {
                    settings: Default::default(),
                });
                cx.new(|cx| Root::new(preview, window, cx))
            }) {
                eprintln!("failed to open compaction preview: {error}");
                cx.quit();
            }
            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });
    Ok(())
}

struct CompactionPreview {
    settings: crate::context_settings::ContextSettings,
}

impl Render for CompactionPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::context_settings::{context_details, context_ring, controls};
        let settings = self.settings;
        let settings_controls = controls(settings, window, cx, |this, settings, cx| {
            this.settings = settings;
            cx.notify();
        });
        let message = |author: &'static str, text: &'static str| {
            div()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .w(px(52.))
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(author),
                )
                .child(div().flex_1().min_w_0().text_sm().child(text))
        };
        let example = |title: &'static str| {
            div()
                .p_5()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().border)
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(title),
                )
        };
        div().size_full().bg(cx.theme().background).text_color(cx.theme().foreground)
            .id("compaction-preview").overflow_y_scroll()
            .child(div().p_8().flex().flex_col().gap_4()
                .child(div().text_lg().child("Compaction in a conversation"))
                .child(example("Compaction completed between messages")
                    .child(message("You", "Check how thread sharing works."))
                    .child(message("Cowork", "The host forwards updates to collaborators and keeps their drafts in sync."))
                    .child(div().ml(px(64.)).child(crate::Cowork::render_compaction_marker(0, false, cx)))
                    .child(message("You", "Now add an inline indicator when context is compacted.")))
                .child(example("Compaction happening during a reply")
                    .child(message("You", "Review the thread UI and add the indicators."))
                    .child(message("Cowork", "Working · Read thread.rs · Read timeline_view.rs"))
                    .child(div().ml(px(64.)).child(crate::Cowork::render_compaction_marker(1, true, cx))))
                .child(div().mt_4().text_lg().child("Context settings"))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child("Adjust the window and compaction percentage to see the indicators change."))
                .child(settings_controls)
                .children([
                    ("Room to keep working", 40, false),
                    ("Near the compaction trigger", settings.threshold_percent.saturating_sub(3), false),
                    ("Compacting conversation", settings.threshold_percent.saturating_add(1), true),
                ].map(|(label, percent, compacting)| {
                    let usage = settings.example_usage(percent);
                    let hover = settings;
                    div().flex().items_center().justify_between().gap_6().py_3()
                        .child(div().flex_1().flex().flex_col().gap_2()
                            .child(div().text_sm().child(label))
                            .child(div().id(SharedString::from(format!("preview-ring-{label}")))
                                .size(px(28.)).flex().items_center().justify_center()
                                .child(context_ring(usage, compacting, cx))
                                .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::element(move |_, cx| context_details(usage, Some(hover), compacting, cx)).py_2().px_3().build(window, cx))))
                        .child(context_details(usage, Some(settings), compacting, cx))
                })))
    }
}

/// Opens the conversation navigation rail in its states side by side: at
/// rest, hovered with its neighbors widened, current apart from hovered with
/// a long card, a card without a response, an overflowing rail, and a live
/// rail to hover and click. No provider or thread is needed.
pub fn run_turn_navigation() -> anyhow::Result<()> {
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(|cx: &mut App| {
            crate::init_app(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
                    None,
                    size(px(1280.), px(900.)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Cowork turn navigation preview".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(error) = cx.open_window(options, |window, cx| {
                let preview = cx.new(|_| TurnNavigationPreview::new());
                cx.new(|cx| Root::new(preview, window, cx))
            }) {
                eprintln!("failed to open the turn navigation preview: {error}");
                cx.quit();
            }
            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });
    Ok(())
}

/// One stage of the preview: its markers and their own scroll offset.
struct PreviewRail {
    markers: Vec<uuid::Uuid>,
    scroll: gpui::ScrollHandle,
}

impl PreviewRail {
    fn new(count: usize) -> Self {
        Self {
            markers: (0..count).map(|_| uuid::Uuid::new_v4()).collect(),
            scroll: gpui::ScrollHandle::new(),
        }
    }
}

struct TurnNavigationPreview {
    rails: Vec<PreviewRail>,
    /// The live rail's hovered and current markers.
    live_hovered: Option<usize>,
    live_current: usize,
}

const STAGE: gpui::Size<Pixels> = gpui::Size {
    width: px(560.),
    height: px(360.),
};

impl TurnNavigationPreview {
    fn new() -> Self {
        Self {
            rails: [9, 9, 9, 9, 60, 12].map(PreviewRail::new).into(),
            live_hovered: None,
            live_current: 11,
        }
    }

    fn card(&self, rich: bool) -> crate::turn_navigation::TurnPreview {
        use crate::{
            models::{ModelProvider, ModelRef},
            participant::ParticipantId,
            turn_navigation::{TurnPreview, TurnPrompt, TurnResponse},
        };
        let now = std::time::SystemTime::now();
        if rich {
            TurnPreview {
                prompt: TurnPrompt::Text("“Is the retry loop safe if the host disconnects mid-transfer?”\nInvestigate the crash when a collaborator joins while an attachment is still uploading. Keep the protocol unchanged if you can, and add a regression test that reproduces the late join with a large file.\nAlso check the attached log.".into()),
                comment_count: 1,
                response: TurnResponse::Text("The crash comes from the attachment table being read before the host has stored every chunk. A late joiner receives the record from the snapshot but the bytes are still in flight, so the transfer resumes from an index that doesn't exist yet.\n\nI changed the host to send only stored records in the Welcome snapshot and queue the rest, and added a regression test that joins during a 4 MB upload. The protocol is unchanged.".into()),
                authors: vec![ParticipantId::new(), ParticipantId::new(), ParticipantId::new()],
                scheduled: false,
                model: Some(ModelRef { provider: ModelProvider::Anthropic, id: "claude-sonnet".into() }),
                model_name: Some("Claude Sonnet".into()),
                started_at: now - std::time::Duration::from_secs(42 * 60),
            }
        } else {
            TurnPreview {
                prompt: TurnPrompt::Text("crash.log (text), screenshot.png (PNG image)".into()),
                comment_count: 0,
                response: TurnResponse::Status("Stopped"),
                authors: vec![ParticipantId::new()],
                scheduled: true,
                model: None,
                model_name: None,
                started_at: now - std::time::Duration::from_secs(3 * 86_400),
            }
        }
    }

    /// A stage with fake conversation lines, the rail at its right edge
    /// and, when given, a card beside the marker at `card_at`.
    #[allow(clippy::too_many_arguments)]
    fn stage(
        &self,
        title: &'static str,
        rail: usize,
        hovered: Option<usize>,
        current: Option<usize>,
        card: Option<(usize, bool)>,
        live: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::turn_navigation::{
            CARD_GAP, CARD_WIDTH, RAIL_WIDTH, render_rail, render_turn_card,
        };
        let rail_state = &self.rails[rail];
        let entity = cx.entity().downgrade();
        let markers = rail_state.markers.clone();
        let on_hover: crate::turn_navigation::HoverMarker = if live {
            let markers = markers.clone();
            Rc::new(move |id, hovered, _, cx| {
                let index = id.map_or(Some(markers.len()), |id| {
                    markers.iter().position(|marker| *marker == id)
                });
                _ = entity.update(cx, |this, cx| {
                    if hovered {
                        this.live_hovered = index;
                    } else if this.live_hovered == index {
                        this.live_hovered = None;
                    }
                    cx.notify();
                });
            })
        } else {
            Rc::new(|_, _, _, _| {})
        };
        let entity = cx.entity().downgrade();
        let on_click: crate::turn_navigation::ClickMarker = if live {
            Rc::new(move |id, _, cx| {
                let index = id.and_then(|id| markers.iter().position(|marker| *marker == id));
                _ = entity.update(cx, |this, cx| {
                    this.live_current = index.unwrap_or(this.live_current);
                    cx.notify();
                });
            })
        } else {
            Rc::new(|_, _, _| {})
        };
        let rail_element = render_rail(
            &rail_state.markers,
            hovered,
            current,
            &rail_state.scroll,
            on_hover,
            on_click,
            window,
            cx,
        );
        let muted = cx.theme().muted_foreground;
        let card = card.map(|(at, rich)| {
            render_turn_card(
                &self.card(rich),
                &Default::default(),
                std::time::SystemTime::now(),
                cx,
            )
            .absolute()
            .right(RAIL_WIDTH + CARD_GAP)
            .w(CARD_WIDTH.min(STAGE.width - RAIL_WIDTH - CARD_GAP - px(8.)))
            .top(px(24. + 22. * at as f32).min(px(60.)))
        });
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(div().text_xs().text_color(muted).child(title))
            .child(
                div()
                    .relative()
                    .w(STAGE.width)
                    .h(STAGE.height)
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children((0..8).map(|line| {
                        div()
                            .h(px(8.))
                            .w(px(120. + (line * 53 % 300) as f32))
                            .rounded_full()
                            .bg(cx.theme().secondary)
                    }))
                    .child(rail_element)
                    .children(card),
            )
            .into_any_element()
    }
}

impl Render for TurnNavigationPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let live_card = self.live_hovered.map(|index| (index, index % 2 == 0));
        let stages = vec![
            self.stage(
                "At rest, current turn 3",
                0,
                None,
                Some(2),
                None,
                false,
                window,
                cx,
            ),
            self.stage(
                "Hovered turn 5: two neighbors widen on each side",
                1,
                Some(4),
                Some(4),
                None,
                false,
                window,
                cx,
            ),
            self.stage(
                "Hovered turn 7 apart from current turn 2, with a long card",
                2,
                Some(6),
                Some(1),
                Some((6, true)),
                false,
                window,
                cx,
            ),
            self.stage(
                "A stopped, scheduled run with no response or recorded model",
                3,
                Some(2),
                Some(8),
                Some((2, false)),
                false,
                window,
                cx,
            ),
            self.stage(
                "60 turns: the rail scrolls on its own",
                4,
                Some(30),
                Some(59),
                None,
                false,
                window,
                cx,
            ),
            self.stage(
                "Live: hover and click the markers",
                5,
                self.live_hovered,
                Some(self.live_current),
                live_card,
                true,
                window,
                cx,
            ),
        ];
        div()
            .id("turn-navigation-preview")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .p(SPACING)
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(div().text_lg().child("Conversation navigation rail"))
                    .child(div().grid().grid_cols(2).gap(SPACING).children(stages)),
            )
    }
}
