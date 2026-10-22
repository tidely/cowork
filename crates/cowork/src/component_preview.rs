//! A window showing the tool call card for each kind of call the agent can
//! make, in each project mode, as the host sees it: a row per call and a
//! column per mode. Combinations that run without asking show that instead
//! of a card. `cargo run -p cowork --example component_preview` opens it.

use agent::model::ToolCall;
use gpui::{
    AnyElement, App, Context, FontWeight, IntoElement, Pixels, QuitMode, Render, SharedString,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_component::{ActiveTheme as _, Root};
use sandbox::{Access, RunCommand};
use serde_json::json;

use crate::{project_mode::ProjectMode, tool_call_card::ToolCallCard};

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
        let preview = cx.new(|_| ComponentPreview { last_click: None });
        cx.new(|cx| Root::new(preview, window, cx))
    })?;
    Ok(())
}

struct ComponentPreview {
    /// What the last button pressed would have done, to show it is wired.
    last_click: Option<SharedString>,
}

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
        ToolCallCard::new(format!("preview-{key}-{}", mode.key()), call.clone())
            .project_access(mode.access())
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
/// access and no folders runs at once, and every other call asks.
fn asks(call: &ToolCall, project: Access) -> bool {
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
