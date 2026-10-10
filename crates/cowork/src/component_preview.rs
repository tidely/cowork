//! A window showing components in their states, to review them before
//! anything in the app puts them in those states. `COWORK_PREVIEW=1 cargo run
//! -p cowork` opens it instead of the app.

use gpui::{
    Context, IntoElement, Render, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use gpui_component::{ActiveTheme as _, Root};
use rig::message::{CallId, ToolCall, ToolFunction, ToolName};
use serde_json::json;

use std::path::PathBuf;

use crate::{
    project_folders::{ProjectFolder, ProjectFolders},
    tool_call_card::ToolCallCard,
};

/// Whether to open the preview instead of the app.
pub(crate) fn requested() -> bool {
    std::env::var_os("COWORK_PREVIEW").is_some()
}

pub(crate) fn open(cx: &mut gpui::App) -> anyhow::Result<()> {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            size(px(720.), px(900.)),
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
    fn section(
        title: &'static str,
        card: impl IntoElement,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(title),
            )
            .child(card)
    }

    /// The host's project, whose button and menus report clicks in the
    /// header.
    fn project(&self, id: &'static str, folders: &[&str], cx: &Context<Self>) -> ProjectFolders {
        let folders = folders
            .iter()
            .map(|path| ProjectFolder::local(PathBuf::from(path)))
            .collect();
        ProjectFolders::new(id, folders).editable(
            cx.listener(|this, _, _, cx| {
                this.last_click = Some("Add a folder".into());
                cx.notify();
            }),
            cx.listener(|this, path: &std::path::Path, _, cx| {
                this.last_click = Some(format!("Open {}", path.display()).into());
                cx.notify();
            }),
            cx.listener(|this, path: &std::path::Path, _, cx| {
                this.last_click = Some(format!("Remove {}", path.display()).into());
                cx.notify();
            }),
        )
    }

    /// A card offering a decision that reports clicks in the header.
    fn deciding(&self, id: &'static str, call: ToolCall, cx: &Context<Self>) -> ToolCallCard {
        let name = call.function.name.to_string();
        let allowed = SharedString::from(format!("Allowed {name}"));
        let denied = SharedString::from(format!("Denied {name}"));
        ToolCallCard::new(id, call).decide(
            cx.listener(move |this, _, _, cx| {
                this.last_click = Some(allowed.clone());
                cx.notify();
            }),
            cx.listener(move |this, _, _, cx| {
                this.last_click = Some(denied.clone());
                cx.notify();
            }),
        )
    }
}

impl Render for ComponentPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("component-preview")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .max_w(px(560.))
                    .mx_auto()
                    .py_6()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(div().text_lg().child("Tool call card"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        self.last_click
                                            .clone()
                                            .unwrap_or_else(|| "Nothing clicked yet".into()),
                                    ),
                            ),
                    )
                    .child(Self::section(
                        "An admin, who may decide",
                        self.deciding("preview-calculate", calculate(), cx),
                        cx,
                    ))
                    .child(Self::section(
                        "Anyone else, who waits",
                        ToolCallCard::new("preview-calculate-waiting", calculate()),
                        cx,
                    ))
                    .child(Self::section(
                        "A sandbox command, which only the host may allow",
                        self.deciding("preview-command", run_command(), cx)
                            .host_only(),
                        cx,
                    ))
                    .child(Self::section(
                        "A sandbox command waiting for the host",
                        ToolCallCard::new("preview-command-waiting", run_command()).host_only(),
                        cx,
                    ))
                    .child(Self::section(
                        "A long command with a visible scrolling hint",
                        self.deciding("preview-long-command", long_command(), cx)
                            .host_only(),
                        cx,
                    ))
                    .child(Self::section(
                        "Long and nested arguments",
                        self.deciding("preview-long", long_arguments(), cx),
                        cx,
                    ))
                    .child(Self::section(
                        "No arguments",
                        self.deciding("preview-empty", no_arguments(), cx),
                        cx,
                    ))
                    .child(Self::section(
                        "A custom body, as a tool's own card would pass",
                        self.deciding("preview-custom", respond_to_comment(), cx)
                            .body(custom_body(cx)),
                        cx,
                    ))
                    .child(div().text_lg().child("Project folders"))
                    .child(Self::section(
                        "No folders yet",
                        self.project("preview-project-empty", &[], cx),
                        cx,
                    ))
                    .child(Self::section(
                        "One folder",
                        self.project("preview-project-one", &["/home/me/src/zed"], cx),
                        cx,
                    ))
                    .child(Self::section(
                        "Several folders, named by the first",
                        self.project(
                            "preview-project-several",
                            &[
                                "/home/me/src/zed",
                                "/home/me/src/cowork",
                                "/home/me/src/rig",
                            ],
                            cx,
                        ),
                        cx,
                    ))
                    .child(Self::section(
                        "A long folder name",
                        self.project(
                            "preview-project-long",
                            &["/home/me/src/a-folder-whose-name-is-far-too-long-to-show-in-full"],
                            cx,
                        ),
                        cx,
                    ))
                    .child(Self::section(
                        "A collaborator's view, with names only",
                        ProjectFolders::new(
                            "preview-project-mirrored",
                            ProjectFolder::mirrored(vec!["zed".into(), "cowork".into()]),
                        ),
                        cx,
                    )),
            )
    }
}

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall::new(
        CallId::from_wire(format!("preview_{name}")),
        ToolFunction::new(ToolName::new(name).expect("a valid name"), arguments),
    )
}

fn calculate() -> ToolCall {
    tool_call(
        "calculate",
        json!({"a": 12, "b": 30, "operation": "multiply"}),
    )
}

fn run_command() -> ToolCall {
    tool_call(
        "run_command",
        json!({"command": "# Inspect Rust sources\nfor file in src/*.rs; do\n    printf '%s\\n' \"$file\"\n    grep -n 'TODO' \"$file\" | sort -n\ndone"}),
    )
}

fn long_command() -> ToolCall {
    let command = (1..=30)
        .map(|line| format!("printf '%s\\n' 'Step {line}'"))
        .collect::<Vec<_>>()
        .join("\n");
    tool_call("run_command", json!({"command": command}))
}

fn respond_to_comment() -> ToolCall {
    tool_call(
        "respond_to_comment",
        json!({
            "comment_id": "comment_2",
            "response": "Good catch: the retry loop never resets its backoff, so after one slow \
                response every later request waits the full maximum. Resetting it on success fixes \
                that.",
        }),
    )
}

fn long_arguments() -> ToolCall {
    tool_call(
        "search_files",
        json!({
            "query": "PROTOCOL_VERSION",
            "include": ["crates/**/*.rs", "docs/**/*.md"],
            "options": {"case_sensitive": true, "max_results": 50},
            "explanation": "Every place the protocol version is read or bumped, to check that the \
                handshake test still covers the variants this change touches.",
        }),
    )
}

fn no_arguments() -> ToolCall {
    tool_call("list_models", json!({}))
}

/// What a `respond_to_comment` card might show instead of raw arguments.
fn custom_body(cx: &Context<ComponentPreview>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .text_sm()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Reply to comment 2"),
        )
        .child(
            div()
                .pl_3()
                .border_l_2()
                .border_color(cx.theme().border)
                .child(
                    "Good catch: the retry loop never resets its backoff, so after one slow \
                     response every later request waits the full maximum. Resetting it on success \
                     fixes that.",
                ),
        )
}
