use std::{io, thread, time::Duration};

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use llm::ConversationStore;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{sync::mpsc, time};

use crate::{
    app::{AgentEvent, AppState, PendingToolPermission, SubmitResult, ThreadId},
    runtime, ui,
};

pub type RuntimeEventSender = mpsc::Sender<RuntimeEvent>;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Debug)]
pub enum RuntimeEvent {
    Terminal(Event),
    Agent(ThreadId, AgentEvent),
    ToolPermissionRequest(PendingToolPermission),
    Tick,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut terminal = TerminalGuard::new()?;
    let (events, mut receiver) = mpsc::channel(512);
    spawn_input_thread(events.clone());
    let memory = ConversationStore::new();

    let mut app = AppState::new();
    terminal.draw(|frame| ui::render(frame, &app))?;

    let mut ticks = time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = ticks.tick() => {
                handle_runtime_event(RuntimeEvent::Tick, &mut app, &events, &memory);
            }
            event = receiver.recv() => {
                let Some(event) = event else { break; };
                handle_runtime_event(event, &mut app, &events, &memory);
            }
        }

        terminal.draw(|frame| ui::render(frame, &app))?;

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn handle_runtime_event(
    event: RuntimeEvent,
    app: &mut AppState,
    events: &RuntimeEventSender,
    memory: &ConversationStore,
) {
    match event {
        RuntimeEvent::Terminal(Event::Key(key)) if key.kind == KeyEventKind::Press => {
            if let SubmitResult::Submitted {
                thread_id,
                conversation_id,
                prompt,
                cancel,
            } = app.handle_key(key)
            {
                runtime::spawn_prompt_task(
                    thread_id,
                    prompt,
                    conversation_id,
                    memory.clone(),
                    events.clone(),
                    cancel,
                );
            }
        }
        RuntimeEvent::Terminal(_) => {}
        RuntimeEvent::Agent(thread_id, event) => app.apply_agent_event(thread_id, event),
        RuntimeEvent::ToolPermissionRequest(request) => app.apply_tool_permission_request(request),
        RuntimeEvent::Tick => {}
    }
}

fn spawn_input_thread(events: RuntimeEventSender) {
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(event) => {
                    if events.blocking_send(RuntimeEvent::Terminal(event)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = events.blocking_send(RuntimeEvent::Tick);
                    break;
                }
            }
        }
    });
}

struct TerminalGuard {
    terminal: TuiTerminal,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame<'_>),
    {
        self.terminal.draw(render).map(|_| ())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}
