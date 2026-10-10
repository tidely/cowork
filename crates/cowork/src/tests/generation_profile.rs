//! Linux-only live profile with a real window and GPUI's production scheduler.
//! Run isolated with `timeout 120s cargo test --release -p cowork generation_profile
//! -- --ignored --nocapture --test-threads=1`. The ignored test itself is opt-in.
//! OLLAMA_API_BASE_URL selects the server (e.g. http://localhost:11435); model is
//! qwen3.8:27b. No downloads, tools, normal Cowork startup, statistics or persistence.
//! COWORK_GENERATION_PROFILE_{HISTORY,VARIANT,REASONING,NUM_PREDICT} accept respectively
//! 0/100/1000, short/markdown-long, true/false, 1..=256; defaults 1000,short,false,128.
//! Context 4096; producer timeout 60s, GPUI watchdog 90s, cap 2048 agent events.
//! Optional COWORK_GENERATION_PROFILE_CONTROL names an existing Linux perf FIFO;
//! its reader MUST be running (`-D -1 --control=fifo:target/control`); open waits.
//! Enable follows a GPUI 500ms warmup timer, before AgentStarted/model spawn.
//! Disable follows AgentEnded and a 200ms settle timer, before checks/quit.
//! These timers allow real parser tasks/frames to run, not a guaranteed parse fence.
//! No headless Test mode, manual draw/drain, polling, batching, or forced cadence.
//! Every event follows emit_for_test (direct Thread::emit delegate) + thread_updated.
//! Counts are stage renders, NOT GPU-presented frames. Inclusive phase totals and
//! event-processing wall durations are diagnostics; model latency is separate.
//! Profiling includes the real renderer, async parsing and instrumentation overhead.
//! Window close, watchdog, failure and unwind cancel the producer/disable profiling.

#![cfg(target_os = "linux")]

use super::timeline_bench::{BenchRoot, LONG, SHORT, timeline};
use super::{test_cowork, test_thread};
use crate::{
    Cowork,
    assets::Assets,
    participant::ParticipantId,
    protocol::{self, HostMessage, RunOutcome},
    thread::{Thread, ThreadStore},
    thread_draft::ThreadDraft,
    timeline::{AgentStep, TimelineMessage},
    transcript::validate_agent_runs,
};
use agent::AgentEvent;
use futures::FutureExt as _;
use gpui::{
    App, AppContext, Application, Bounds, Entity, Subscription, WindowBounds, WindowOptions, px,
    size,
};
use gpui_component::Root;
use rig::{
    completion::{AssistantContent, Message},
    providers::ollama::Ollama,
    tool::ToolSet,
};
use std::{
    cell::RefCell,
    env,
    fs::{File, OpenOptions},
    io::{self, Write as _},
    panic::AssertUnwindSafe,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::mpsc;
use uuid::Uuid;

const PROMPT: &str = "Explain presentation batching in a short Markdown paragraph and three bullets. Do not call tools.";
const EVENT_CAP: usize = 2048;
fn setting<T: std::str::FromStr>(key: &str, default: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    env::var(format!("COWORK_GENERATION_PROFILE_{key}"))
        .unwrap_or_else(|_| default.into())
        .parse()
        .expect(key)
}
fn marker(message: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "# profile region {message}")?;
    stdout.flush()
}
struct ProfileRegion {
    control: Option<File>,
    active: bool,
}
impl ProfileRegion {
    fn begin() -> io::Result<Self> {
        let control = env::var_os("COWORK_GENERATION_PROFILE_CONTROL")
            .map(|path| {
                use std::os::unix::fs::FileTypeExt as _;
                if !std::fs::metadata(&path)?.file_type().is_fifo() {
                    return Err(io::Error::other("perf control must be an existing FIFO"));
                }
                OpenOptions::new().write(true).open(path)
            })
            .transpose()?;
        let mut region = Self {
            control,
            active: true,
        };
        if let Some(file) = &mut region.control {
            file.write_all(b"enable\n")?;
        }
        marker("begin")?;
        Ok(region)
    }
    fn finish(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.control {
            file.write_all(b"disable\n")?;
        }
        self.active = false;
        marker("end")
    }
}
impl Drop for ProfileRegion {
    fn drop(&mut self) {
        if self.active {
            if let Some(file) = &mut self.control {
                let _ = file.write_all(b"disable\n");
            }
            let _ = marker("end (early exit/unwind)");
        }
    }
}
struct Control {
    outcome: Rc<RefCell<Result<(), String>>>,
    finished: bool,
    region: Option<ProfileRegion>,
    abort: Option<tokio::task::AbortHandle>,
}
impl Control {
    fn stop(&mut self, mut result: Result<(), String>) {
        if self.finished {
            return;
        }
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        if let Some(mut region) = self.region.take()
            && let Err(error) = region.finish()
        {
            result = Err(format!("{result:?}; perf disable: {error:?}"));
        }
        if let Err(error) = &result {
            eprintln!("generation_profile: {error:?}");
        }
        *self.outcome.borrow_mut() = result;
        self.finished = true;
    }
}
struct RunGuard {
    control: Rc<RefCell<Control>>,
    runtime: Option<tokio::runtime::Runtime>,
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.control
            .borrow_mut()
            .stop(Err("application exited early or test unwound".into()));
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(100));
        }
    }
}
#[derive(Debug, Default)]
struct Stats {
    events: usize,
    host_events: usize,
    json: Duration,
    ingest: Duration,
    lag_max: Duration,
}
struct Fixture {
    bench: Entity<BenchRoot>,
    cowork: Entity<Cowork>,
    thread: Entity<Thread>,
    thread_id: Uuid,
    message_id: Uuid,
    _stage_updates: Subscription,
}
impl Fixture {
    fn open(
        cx: &mut App,
        runtime: tokio::runtime::Handle,
        history: usize,
        response: &str,
    ) -> anyhow::Result<Self> {
        let thread_id = Uuid::new_v4();
        let mut fixture = None;
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1200.), px(760.)),
                cx,
            ))),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let messages = timeline(history, response, cx);
            let thread = cx
                .new(|_| test_thread(thread_id, messages, ThreadDraft::new(ParticipantId::new())));
            let store = cx.new(|_| ThreadStore {
                threads: std::collections::VecDeque::from([thread.clone()]),
                ..Default::default()
            });
            let cowork = cx.new(|cx| test_cowork(store, Some(thread_id), runtime, window, cx));
            cowork.update(cx, |cowork, _| {
                cowork.sidebar_open = false;
                cowork.follow_generation = true;
            });
            let bench = cx.new(|_| BenchRoot::stage(cowork.clone()));
            // Forward Cowork's normal notify to the harness wrapper, not a draw request.
            let subscription =
                bench.update(cx, |_, cx| cx.observe(&cowork, |_, _, cx| cx.notify()));
            let root = cx.new(|cx| Root::new(bench.clone(), window, cx));
            fixture = Some(Self {
                bench,
                cowork,
                thread,
                thread_id,
                message_id: Uuid::new_v4(),
                _stage_updates: subscription,
            });
            root
        })?;
        cx.activate(true);
        Ok(fixture.expect("window fixture"))
    }
    fn emit(&self, event: HostMessage, stats: &mut Stats, cx: &mut App) {
        let start = Instant::now();
        self.thread
            .update(cx, |thread, cx| thread.emit_for_test(event, cx));
        self.cowork
            .update(cx, |cowork, cx| cowork.thread_updated(self.thread_id, cx));
        stats.ingest += start.elapsed();
        stats.host_events += 1;
    }
    fn check(&self, events: &[AgentEvent], expected: &[Message], cx: &App) -> anyhow::Result<()> {
        let mut history = vec![Message::user(PROMPT)];
        let mut fold = agent::TurnFold::default();
        for event in events {
            history.extend(fold.apply(event)?.message);
        }
        anyhow::ensure!(
            fold.partial().is_none() && fold.pending_calls().is_empty(),
            "unfinished fold"
        );
        anyhow::ensure!(
            history == expected,
            "capture differs from native Rig history"
        );
        let thread = self.thread.read(cx);
        anyhow::ensure!(
            thread.transcript == expected,
            "production transcript differs from Rig"
        );
        let Some(TimelineMessage::Agent(message)) = thread.timeline.last() else {
            anyhow::bail!("missing live message")
        };
        anyhow::ensure!(
            !message.is_generating() && message.pending_events.is_empty(),
            "unfinished run"
        );
        // Synthetic history has no transcript: validate only the real run.
        validate_agent_runs(&thread.transcript, &[&message.to_protocol()])?;
        anyhow::ensure!(expected.len() == 2, "expected one tool-free turn");
        let Message::Assistant(reply) = &expected[1] else {
            anyhow::bail!("missing reply")
        };
        let (mut answer, mut reasoning) = (String::new(), String::new());
        for part in &reply.content {
            match part {
                AssistantContent::Text(text) => answer.push_str(&text.text),
                AssistantContent::Reasoning(text) => reasoning.push_str(&text.text),
                AssistantContent::ToolCall(_) => anyhow::bail!("tool call in tool-free profile"),
                _ => {}
            }
        }
        let shown: String = message
            .output
            .steps
            .iter()
            .filter_map(|step| match step {
                AgentStep::Thinking(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        anyhow::ensure!(
            message.output.text == answer && shown == reasoning,
            "final output differs"
        );
        anyhow::ensure!(
            self.cowork.read(cx).follow_generation,
            "bottom follow disabled"
        );
        println!(
            "# scroll_offset={:?} (follow requested, bottom not guaranteed)",
            self.cowork.read(cx).timeline_scroll_handle.offset()
        );
        Ok(())
    }
}
enum Update {
    Event(Instant, Box<AgentEvent>),
    Done(Result<Vec<Message>, String>, Duration),
}

#[test]
#[ignore = "real-window live profile; explicitly run isolated, requires Ollama qwen3.8:27b"]
fn generation_profile() {
    let history: usize = setting("HISTORY", "1000");
    let variant: String = setting("VARIANT", "short");
    let reasoning: bool = setting("REASONING", "false");
    let num_predict: usize = setting("NUM_PREDICT", "128");
    assert!([0, 100, 1000].contains(&history));
    assert!(matches!(variant.as_str(), "short" | "markdown-long"));
    assert!((1..=256).contains(&num_predict));
    println!(
        "# real-window profile model=qwen3.8:27b history={history} variant={variant} reasoning={reasoning} num_predict={num_predict}; production GPUI scheduling"
    );
    let outcome = Rc::new(RefCell::new(Err("profile did not complete".to_owned())));
    let control = Rc::new(RefCell::new(Control {
        outcome: outcome.clone(),
        finished: false,
        region: None,
        abort: None,
    }));
    let guard = RunGuard {
        control: control.clone(),
        runtime: Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("profile runtime"),
        ),
    };
    let runtime = guard.runtime.as_ref().unwrap().handle().clone();
    // Application uses Production mode even with test-support enabled. Do not
    // substitute HeadlessAppContext: its Test effect flush eagerly draws windows.
    Application::with_platform(gpui_platform::current_platform(false)).with_assets(Assets).run(move |cx| {
        gpui_component::init(cx);
        crate::theme::init(cx);
        let closed = control.clone();
        cx.on_window_closed(move |cx, _| {
            closed.borrow_mut().stop(Err("profile window closed".into()));
            cx.quit();
        }).detach();
        let quitting = control.clone();
        cx.on_app_quit(move |_| {
            quitting.borrow_mut().stop(Err("application quit before completion".into()));
            async {}
        }).detach();
        let watchdog = control.clone();
        let deadline = cx.background_executor().timer(Duration::from_secs(90));
        cx.spawn(async move |cx| {
            deadline.await;
            if !watchdog.borrow().finished {
                watchdog.borrow_mut().stop(Err("GPUI watchdog timeout (90s)".into()));
                cx.update(|cx| cx.quit());
            }
        }).detach();
        let response = if variant == "short" { SHORT } else { LONG };
        let fixture = match Fixture::open(cx, runtime.clone(), history, response) {
            Ok(fixture) => fixture,
            Err(error) => { control.borrow_mut().stop(Err(format!("open fixture: {error:#}"))); cx.quit(); return; }
        };
        cx.spawn(async move |cx| {
            let run = AssertUnwindSafe(async {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                if control.borrow().finished { return Ok::<_, anyhow::Error>(()); }
                let (draws_before, phases_before) = cx.update(|cx| {
                    let stage = fixture.bench.read(cx);
                    (stage.draws(), stage.profile_phases())
                });
                control.borrow_mut().region = Some(ProfileRegion::begin()?);
                let region_started = Instant::now();
                let mut stats = Stats::default();
                cx.update(|cx| fixture.emit(HostMessage::AgentStarted {
                    id: fixture.message_id.into_bytes(), comment_group_id: None,
                    started_at: SystemTime::now(), prompt: protocol::Json::from_rig(&Message::user(PROMPT)),
                }, &mut stats, cx));
                let (sender, mut receiver) = mpsc::unbounded_channel();
                let queued = Arc::new(AtomicUsize::new(0));
                let queued_max = Arc::new(AtomicUsize::new(0));
                let producer_queued = queued.clone();
                let producer_max = queued_max.clone();
                let task = runtime.spawn(async move {
                    let started = Instant::now();
                    let mut history = Vec::new();
                    let result = async {
                        let model = Ollama::from_env()?.native_completion("qwen3.8:27b");
                        // rig-core 0.44 native Chat: think top-level, options merged.
                        let agent = agent::Agent::new(model.erase(), ToolSet::default()).additional_params(serde_json::json!({
                            "think": reasoning, "options": {"num_ctx": 4096, "num_predict": num_predict}
                        }));
                        let cap = tokio::sync::Notify::new();
                        let mut count = 0;
                        let run = agent.run(Message::user(PROMPT), &mut history, |event| {
                            if count >= EVENT_CAP { cap.notify_one(); return; }
                            count += 1;
                            let pending = producer_queued.fetch_add(1, Ordering::SeqCst) + 1;
                            producer_max.fetch_max(pending, Ordering::SeqCst);
                            if sender.send(Update::Event(Instant::now(), Box::new(event))).is_err() { cap.notify_one(); }
                        });
                        tokio::time::timeout(Duration::from_secs(60), async {
                            tokio::select! {
                                result = run => result,
                                _ = cap.notified() => Err(anyhow::anyhow!("event cap reached or consumer closed")),
                            }
                        }).await.map_err(|_| anyhow::anyhow!("producer timeout (60s)"))??;
                        Ok::<_, anyhow::Error>(())
                    }.await;
                    let _ = sender.send(Update::Done(result.map(|()| history).map_err(|e| format!("{e:#}")), started.elapsed()));
                });
                control.borrow_mut().abort = Some(task.abort_handle());
                // Same serial recv/update flow as production. No arbitrary poll
                // bursts or presentation batching; parser tasks and frames run normally.
                let mut events = Vec::new();
                let (result, model_wall) = loop {
                    let update = receiver.recv().await.ok_or_else(|| anyhow::anyhow!("producer exited without completion"))?;
                    if control.borrow().finished { return Ok(()); }
                    match update {
                        Update::Event(arrived, event) => {
                            queued.fetch_sub(1, Ordering::SeqCst);
                            stats.lag_max = stats.lag_max.max(arrived.elapsed());
                            anyhow::ensure!(events.len() < EVENT_CAP, "event cap exceeded");
                            events.push((*event).clone());
                            let start = Instant::now();
                            let event = protocol::Json::shared(*event);
                            stats.json += start.elapsed();
                            cx.update(|cx| fixture.emit(HostMessage::AgentEvent { id: fixture.message_id.into_bytes(), event }, &mut stats, cx));
                            stats.events += 1;
                        }
                        Update::Done(result, wall) => break (result, wall),
                    }
                };
                let outcome = match &result { Ok(_) => RunOutcome::Completed, Err(error) => RunOutcome::Failed(error.clone()) };
                cx.update(|cx| fixture.emit(HostMessage::AgentEnded { id: fixture.message_id.into_bytes(), outcome, duration: model_wall }, &mut stats, cx));
                cx.background_executor().timer(Duration::from_millis(200)).await;
                if control.borrow().finished { return Ok(()); }
                // Disable before correctness folding, reporting and runtime shutdown.
                let region_wall = region_started.elapsed();
                if let Some(mut region) = control.borrow_mut().region.take() { region.finish()?; }
                cx.update(|cx| {
                    let stage = fixture.bench.read(cx);
                    let renders = stage.draws() - draws_before;
                    let after = stage.profile_phases();
                    let phases: [Duration; 3] = std::array::from_fn(|i| after.durations[i] - phases_before.durations[i]);
                    let calls: [usize; 3] = std::array::from_fn(|i| after.calls[i] - phases_before.calls[i]);
                    println!("# event processing (wall totals, excludes scheduled frames): {stats:#?}");
                    println!("# stage_renders={renders} editor_layout/prepaint/paint_totals={phases:?} phase_calls={calls:?}");
                    println!("# queued_events_max={} model_wall={model_wall:?} region_wall={:?} (model/network latency is not CPU cost)", queued_max.load(Ordering::SeqCst), region_wall);
                    anyhow::ensure!(renders > 0 && calls.iter().all(|&n| n > 0), "no actual stage renders");
                    anyhow::ensure!(queued.load(Ordering::SeqCst) == 0 && stats.events == events.len() && stats.host_events == events.len() + 2, "event counts differ");
                    let expected = result.map_err(anyhow::Error::msg)?;
                    anyhow::ensure!(!events.is_empty(), "empty generation");
                    fixture.check(&events, &expected, cx)
                })
            }).catch_unwind().await;
            let result = match run {
                Ok(result) => result.map_err(|error| format!("{error:#}")),
                Err(panic) => Err(format!("GPUI profile task panicked: {}", panic.downcast_ref::<String>().map(String::as_str).or_else(|| panic.downcast_ref::<&str>().copied()).unwrap_or("non-string panic"))),
            };
            if !control.borrow().finished {
                control.borrow_mut().stop(result);
                cx.update(|cx| cx.quit());
            }
        }).detach();
    });
    drop(guard);
    let result = outcome.borrow().clone();
    assert!(result.is_ok(), "generation_profile failed: {result:?}");
}
