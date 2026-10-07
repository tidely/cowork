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

use crate::tool_call_card::ToolCallCard;

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
