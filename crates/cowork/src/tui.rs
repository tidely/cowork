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
    app::{AppState, PendingToolPermission, SubmitResult, ThreadEvent, ThreadId},
    persistence, runtime, ui,
};

pub type RuntimeEventSender = mpsc::Sender<RuntimeEvent>;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Debug)]
pub enum RuntimeEvent {
    Terminal(Event),
    Agent(ThreadId, ThreadEvent),
    ToolPermissionRequest(PendingToolPermission),
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut terminal = TerminalGuard::new()?;
    let (events, mut receiver) = mpsc::channel(512);
    spawn_input_thread(events.clone());

    let session_path = persistence::session_path();
    let (mut app, memory) = match session_path.as_deref().and_then(persistence::load) {
        Some(snapshot) => (
            AppState::restored(
                snapshot.threads,
                snapshot.next_thread_id,
                snapshot.next_agent_id,
                snapshot.always_allowed_tools,
            ),
            ConversationStore::from_conversations(snapshot.conversations),
        ),
        None => (AppState::new(), ConversationStore::new()),
    };
    terminal.draw(|frame| ui::render(frame, &app))?;

    // Snapshots are debounced onto the tick and only written while no run is in
    // flight, so a half-finished turn is never persisted; a settling terminal
    // event clears `Running` and the next tick captures the final state.
    let mut dirty = false;
    let mut ticks = time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = ticks.tick() => {
                if dirty && !app.any_running() {
                    persistence::save(session_path.as_deref(), &app, &memory).await;
                    dirty = false;
                }
            }
            event = receiver.recv() => {
                let Some(event) = event else { break; };
                if handle_runtime_event(event, &mut app, &events, &memory) {
                    dirty = true;
                }
            }
        }

        terminal.draw(|frame| ui::render(frame, &app))?;

        if app.should_quit {
            // A force-quit may interrupt a live run; `restored` settles the
            // in-flight statuses on next launch.
            persistence::save(session_path.as_deref(), &app, &memory).await;
            break;
        }
    }

    Ok(())
}

/// Apply one runtime event. Returns whether it may have changed persistable
/// state, so the caller can mark the session dirty. Key presses are treated as
/// dirty wholesale (covering submit, new threads, and sidebar expansion); the
/// debounced, idle-gated save keeps that coarseness cheap.
fn handle_runtime_event(
    event: RuntimeEvent,
    app: &mut AppState,
    events: &RuntimeEventSender,
    memory: &ConversationStore,
) -> bool {
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
            true
        }
        RuntimeEvent::Terminal(_) => false,
        RuntimeEvent::Agent(thread_id, event) => {
            app.apply_thread_event(thread_id, event);
            true
        }
        RuntimeEvent::ToolPermissionRequest(request) => {
            app.apply_tool_permission_request(request);
            true
        }
    }
}

fn spawn_input_thread(events: RuntimeEventSender) {
    thread::spawn(move || {
        while let Ok(event) = event::read() {
            if events.blocking_send(RuntimeEvent::Terminal(event)).is_err() {
                break;
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
