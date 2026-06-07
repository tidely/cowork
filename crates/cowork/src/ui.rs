use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::app::{AgentDepth, AgentStatus, AppState, Focus, Message, MessageRole, ToolStatus};

pub fn render(frame: &mut Frame<'_>, app: &AppState) {
    let root = frame.area();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(40)])
        .split(root);

    render_sidebar(frame, app, columns[0]);
    render_main(frame, app, columns[1]);
}

fn render_sidebar(frame: &mut Frame<'_>, app: &AppState, area: Rect) {
    let border_style = if app.focus == Focus::Sidebar {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let visible_height = area.height.saturating_sub(2) as usize;
    let content_width = area.width.saturating_sub(2).max(1) as usize;

    let mut lines = Vec::new();
    // Track the visual-line span of the selected item so the viewport can
    // follow it as the selection moves with the arrow keys.
    let mut selected_span: Option<(usize, usize)> = None;
    for item in app.sidebar_items() {
        let indent = "  ".repeat(item.depth);
        let icon = if item.has_children {
            if item.expanded { "▾" } else { "▸" }
        } else {
            status_icon(item.status)
        };
        let style = if item.selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(match item.status {
                Some(AgentStatus::Running) => Color::Yellow,
                Some(AgentStatus::Error) => Color::Red,
                _ => Color::White,
            })
        };

        // Pre-wrap the label with a hanging indent so wrapped continuation
        // lines stay aligned under the first line's text instead of falling
        // back to column 0 (which hid the nesting depth).
        let prefix = format!("{indent}{icon} ");
        let prefix_width = prefix.chars().count();
        let hanging_indent = " ".repeat(prefix_width);
        let label_width = content_width.saturating_sub(prefix_width).max(1);

        let start = lines.len();
        for (i, chunk) in wrap_text(&item.label, label_width).into_iter().enumerate() {
            let text = if i == 0 {
                format!("{prefix}{chunk}")
            } else {
                format!("{hanging_indent}{chunk}")
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        if item.selected {
            selected_span = Some((start, lines.len()));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from("No threads"));
    }

    // Lines are already wrapped to the content width, so the scroll offset is
    // a direct visual-line count. Keep the viewport anchored at the top and
    // only scroll far enough to keep the selected item fully in view.
    let max_scroll = lines.len().saturating_sub(visible_height) as u16;
    let sidebar_scroll = match selected_span {
        Some((start, end)) if visible_height > 0 => {
            let (start, end, vh) = (start as u16, end as u16, visible_height as u16);
            let scroll = if end > vh { end.saturating_sub(vh) } else { 0 };
            scroll.min(max_scroll).min(start)
        }
        _ => 0,
    };

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Threads  Ctrl+n new  Ctrl+[ collapse  Ctrl+] expand ")
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .scroll((sidebar_scroll, 0));
    frame.render_widget(paragraph, area);
}

/// Word-wrap `text` into chunks no wider than `width` columns, hard-breaking
/// any single word that is itself longer than `width`.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;

    let push_chunk = |chunk: &str, lines: &mut Vec<String>| {
        // Hard-break a word longer than the available width.
        let mut chars = chunk.chars().peekable();
        while chars.peek().is_some() {
            let piece: String = chars.by_ref().take(width).collect();
            lines.push(piece);
        }
    };

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current_len == 0 {
            if word_len <= width {
                current = word.to_string();
                current_len = word_len;
            } else {
                push_chunk(word, &mut lines);
            }
        } else if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push(std::mem::take(&mut current));
            current_len = 0;
            if word_len <= width {
                current = word.to_string();
                current_len = word_len;
            } else {
                push_chunk(word, &mut lines);
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_main(frame: &mut Frame<'_>, app: &AppState, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    render_conversation(frame, app, rows[0]);
    render_input(frame, app, rows[1]);
}

fn render_conversation(frame: &mut Frame<'_>, app: &AppState, area: Rect) {
    let agent = app.selected_agent();
    let thread_title = app
        .selected_thread()
        .map(|thread| thread.title.as_str())
        .unwrap_or("Thread");
    let focused = app.focus == Focus::Conversation;
    let title = match agent {
        Some(agent) if focused => {
            format!(
                " {thread_title} / {}  ↑↓ select  Enter toggle ",
                agent.label
            )
        }
        Some(agent) => format!(" {thread_title} / {} ", agent.label),
        None => format!(" {thread_title} "),
    };

    let selected_message = focused
        .then(|| app.selected_collapsible_message_index())
        .flatten();

    let mut lines = Vec::new();
    let mut selected_line_start = None;
    if let Some(agent) = agent {
        lines.push(Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                status_label(agent.status),
                Style::default().fg(status_color(agent.status)),
            ),
            Span::styled("  Depth: ", Style::default().fg(Color::DarkGray)),
            Span::styled(depth_label(agent.depth), Style::default().fg(Color::Gray)),
        ]));
        lines.push(Line::from(""));

        for (index, message) in agent.messages.iter().enumerate() {
            let selected = selected_message == Some(index);
            if selected {
                selected_line_start = Some(lines.len());
            }
            append_message_lines(&mut lines, message, selected);
        }
    } else {
        lines.push(Line::from("No agent selected"));
    }

    let visible_height = area.height.saturating_sub(2) as usize;
    let content_width = area.width.saturating_sub(2).max(1) as usize;
    let visual_height = wrapped_line_count(&lines, content_width);
    let max_scroll = visual_height.saturating_sub(visible_height) as u16;
    let scroll_from_top = match selected_line_start {
        // When navigating collapsibles, keep the selected one in view with a
        // little context above it.
        Some(start) => {
            let selected_row = wrapped_line_count(&lines[..start], content_width) as u16;
            selected_row.saturating_sub(2).min(max_scroll)
        }
        None => max_scroll.saturating_sub(app.conversation_scroll),
    };

    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .scroll((scroll_from_top, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn wrapped_line_count(lines: &[Line<'_>], width: usize) -> usize {
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

fn render_input(frame: &mut Frame<'_>, app: &AppState, area: Rect) {
    let running = app.active_agent_running();
    let title = if running {
        " Prompt — waiting for active agent "
    } else {
        " Prompt — Tab cycles focus "
    };
    let border_style = if app.focus == Focus::Input {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let input_style = if running {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };

    // The input is a single content row, so scroll horizontally to keep the
    // cursor visible instead of letting the text overflow past the edge.
    const PREFIX: &str = "> ";
    let inner_width = area.width.saturating_sub(2) as usize;
    let available = inner_width.saturating_sub(PREFIX.chars().count()).max(1);
    let cursor_chars = app.input.value[..app.input.cursor].chars().count();
    let scroll = cursor_chars.saturating_sub(available - 1);
    let visible: String = app
        .input
        .value
        .chars()
        .skip(scroll)
        .take(available)
        .collect();

    let paragraph = Paragraph::new(format!("{PREFIX}{visible}"))
        .style(input_style)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(border_style),
        );
    frame.render_widget(paragraph, area);

    if app.focus == Focus::Input && !running {
        let cursor_offset = (cursor_chars - scroll) as u16;
        let x = area
            .x
            .saturating_add(1 + PREFIX.chars().count() as u16)
            .saturating_add(cursor_offset);
        frame.set_cursor_position(Position::new(x, area.y.saturating_add(1)));
    }
}

fn append_message_lines(lines: &mut Vec<Line<'static>>, message: &Message, selected: bool) {
    if message.role == MessageRole::ToolCall {
        append_tool_call_lines(lines, message, selected);
        return;
    }

    let style = message_style(message.role);
    let marker = if message.role == MessageRole::Reasoning {
        if message.collapsed { "▸ " } else { "▾ " }
    } else {
        ""
    };
    lines.push(Line::from(Span::styled(
        format!("┌─ {marker}{} ", message.role.label()),
        header_style(style, selected),
    )));

    if message.collapsed {
        lines.push(Line::from(vec![
            Span::styled("│ ", style),
            Span::styled(collapsed_preview(&message.content), style),
        ]));
    } else if message.content.is_empty() {
        lines.push(Line::from("│"));
    } else {
        for content_line in message.content.lines() {
            lines.push(Line::from(vec![
                Span::styled("│ ", style),
                Span::styled(content_line.to_string(), style),
            ]));
        }
    }

    lines.push(Line::from(Span::styled("└", style)));
    lines.push(Line::from(""));
}

fn append_tool_call_lines(lines: &mut Vec<Line<'static>>, message: &Message, selected: bool) {
    let done = message.tool_status.is_done();
    let style = match message.tool_status {
        ToolStatus::Failed => Style::default().fg(Color::Red),
        ToolStatus::Finished => Style::default().fg(Color::Green),
        ToolStatus::Running => message_style(MessageRole::ToolCall),
    };
    let marker = if message.collapsed { "▸" } else { "▾" };
    let status = match message.tool_status {
        ToolStatus::Failed => "✗",
        ToolStatus::Finished => "✓",
        ToolStatus::Running => "◐",
    };
    let tool_name = tool_call_name(&message.content);

    lines.push(Line::from(Span::styled(
        format!("┌─ {marker} Tool call {status} {tool_name}"),
        header_style(style, selected),
    )));

    if message.collapsed {
        // While running show "running"; once done the colored marker carries the
        // status, so preview the result instead of writing "finished".
        let preview = if done {
            message
                .tool_result
                .as_deref()
                .filter(|result| !result.is_empty())
                .map(collapsed_preview)
                .unwrap_or_default()
        } else {
            "running".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled("│ ", style),
            Span::styled(preview, style),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "│ Arguments",
            style.add_modifier(Modifier::BOLD),
        )));
        for content_line in tool_call_arguments(&message.content).lines() {
            lines.push(Line::from(vec![
                Span::styled("│ ", style),
                Span::styled(content_line.to_string(), style),
            ]));
        }

        lines.push(Line::from(Span::styled("│", style)));
        lines.push(Line::from(Span::styled(
            "│ Result",
            style.add_modifier(Modifier::BOLD),
        )));
        match message.tool_result.as_deref() {
            Some(result) if !result.is_empty() => {
                for result_line in result.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", style),
                        Span::styled(result_line.to_string(), style),
                    ]));
                }
            }
            _ => {
                let label = if done { "(no output)" } else { "running" };
                lines.push(Line::from(vec![
                    Span::styled("│ ", style),
                    Span::styled(label.to_string(), style),
                ]));
            }
        }
    }

    lines.push(Line::from(Span::styled("└", style)));
    lines.push(Line::from(""));
}

fn tool_call_name(content: &str) -> &str {
    content.lines().next().unwrap_or("unknown")
}

fn tool_call_arguments(content: &str) -> &str {
    content
        .split_once('\n')
        .map(|(_, arguments)| arguments)
        .unwrap_or("")
}

fn collapsed_preview(content: &str) -> String {
    let preview = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("thinking hidden");
    let preview: String = preview.chars().take(80).collect();
    if content.chars().count() > preview.chars().count() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn header_style(style: Style, selected: bool) -> Style {
    let style = style.add_modifier(Modifier::BOLD);
    if selected {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

fn message_style(role: MessageRole) -> Style {
    match role {
        MessageRole::System => Style::default().fg(Color::Magenta),
        MessageRole::User => Style::default().fg(Color::Green),
        MessageRole::Assistant => Style::default().fg(Color::White),
        MessageRole::Reasoning => Style::default().fg(Color::DarkGray),
        MessageRole::ToolCall => Style::default().fg(Color::Yellow),
        MessageRole::ToolResult => Style::default().fg(Color::Blue),
        MessageRole::Status => Style::default().fg(Color::Gray),
        MessageRole::Error => Style::default().fg(Color::Red),
    }
}

fn status_icon(status: Option<AgentStatus>) -> &'static str {
    match status {
        Some(AgentStatus::Idle) => "○",
        Some(AgentStatus::Running) => "◐",
        Some(AgentStatus::Complete) => "●",
        Some(AgentStatus::Error) => "!",
        None => " ",
    }
}

fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Running => "running",
        AgentStatus::Complete => "complete",
        AgentStatus::Error => "error",
    }
}

fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Idle => Color::DarkGray,
        AgentStatus::Running => Color::Yellow,
        AgentStatus::Complete => Color::Green,
        AgentStatus::Error => Color::Red,
    }
}

fn depth_label(depth: AgentDepth) -> &'static str {
    match depth {
        AgentDepth::Main => "main",
        AgentDepth::Subagent => "subagent",
        AgentDepth::Worker => "worker",
    }
}
