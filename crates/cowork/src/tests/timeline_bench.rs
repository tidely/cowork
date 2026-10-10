//! Opt-in, finite CPU-only timeline benchmark. See `docs/timeline-performance.md`
//! for results, limitations, and bounded profiling commands. Run optimized:
//! `cargo test --release -p cowork timeline_bench -- --ignored --nocapture --test-threads=1`.
//!
//! This uses native platform text shaping, bundled assets, and the real main
//! editor, but no renderer/GPU, app startup, network, generation, or persistence.
//! The platform constructor is used only to obtain its production text system.
//! Keep the platform alive across all cases; it owns the text-system workers.
//!
//! Environment (invalid values fail before measuring):
//! - `COWORK_TIMELINE_BENCH_SIZES`: comma-separated subset of 10,100,1000,5000;
//!   default 10,100,1000. Counts are messages, alternating prompts and replies.
//! - `COWORK_TIMELINE_BENCH_VARIANTS`: subset of short,markdown-long,plain-long,file;
//!   default short,markdown-long,plain-long (also file when a file is supplied).
//!   plain-long is one prose paragraph with the same byte length as markdown-long.
//! - `COWORK_TIMELINE_BENCH_ITERATIONS`: 1..=1000, default 30.
//! - `COWORK_TIMELINE_BENCH_WARMUP`: 1..=100, default 5.
//! - `COWORK_TIMELINE_BENCH_CONTROL`: 0 or 1, default 0; enables placeholder
//!   and matched reply-only selectable/unselectable controls.
//! - `COWORK_TIMELINE_BENCH_RESPONSE_FILE`: optional UTF-8 regular file, at most
//!   64 KiB. Selected cases may contain at most 16 MiB of response source text.
//!
//! CSV durations are microseconds, median and nearest-rank p95. `stage_frame`
//! includes CPU layout, prepaint, paint/scene building, and arena cleanup, not
//! GPU presentation. `stage_draw` and `stage_arena_clear` split that frame.
//! A forwarding element around the editor records `editor_request_layout_nodes`,
//! `editor_prepaint`, and `editor_paint`, inclusive of their descendant calls.
//! request_layout includes deferred Markdown element construction and layout-node
//! building, NOT Taffy's layout computation (which happens later in stage_draw).
//! These phases are not an exhaustive partition of the frame; timers add overhead.
//! `editor_construct` times only render_main_editor and its
//! returned element construction, not layout/paint or destruction; it runs
//! inside Render so the element arena is active, and clears it after each draw.
//! `timeline_clone` excludes dropping the clone. `placeholder_frame` is a crude
//! ablation with one plain fixed-height row per message, not a matched-height
//! substitute or a cost that can meaningfully be subtracted from stage_frame.
//! `reply_only_selectable_frame` and `reply_only_unselectable_frame` are NOT the
//! production stage: they omit user prompts, avatars, and the composer. They use
//! the same persistent AgentMessage text_view entities with fit-content TextViews,
//! identical default Markdown styling, a fixed 14 px font, and production-equivalent
//! wrap width. Only selectable differs, isolating selection overhead with matched
//! views. No Stage phase metrics are reported for controls. CSV message counts are
//! total source messages; each reply-only control renders count/2 replies.
//! To compare selection independently at size 1000 with short replies:
//! `COWORK_TIMELINE_BENCH_CONTROL=1 COWORK_TIMELINE_BENCH_SIZES=1000 COWORK_TIMELINE_BENCH_VARIANTS=short cargo test --release -p cowork timeline_bench -- --ignored --nocapture --test-threads=1`.
//!
//! These are WARM measurements: fixture creation, initial rendering, Markdown
//! parsing/highlighting background tasks, and warmup draws are outside samples.
//! run_until_parked drains background work before measurement and between warmup
//! draws. They do not measure cold startup/parsing, scrolling, or streaming.
//! The viewport starts at the top; the production editor still constructs every
//! message row. Synthetic completed replies have no transcript/tool history.

use std::{
    cell::Cell,
    env,
    hint::black_box,
    io::Read as _,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use gpui::{
    App, AppContext, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
    HeadlessAppContext, InspectorElementId, IntoElement, LayoutId, Pixels, Render, TextRun, Window,
    div, prelude::*, px, size,
};
use gpui_base::{TextView, TextViewState};
use gpui_component::{ActiveTheme as _, Root};
use uuid::Uuid;

use super::{test_cowork, test_thread};
use crate::{
    Cowork,
    assets::Assets,
    participant::ParticipantId,
    protocol::{AgentRun, RunOutcome},
    thread::ThreadStore,
    thread_draft::ThreadDraft,
    timeline::{
        AgentMessage, AgentOutput, AgentStep, OutputMark, PromptBlock, TimelineMessage,
        UserMessageGroup,
    },
};

pub(super) const SHORT: &str =
    "A short completed response with enough text to shape a normal line.";
pub(super) const LONG: &str = "## Completed implementation\n\n\
    This **completed response** explains the change, its *tradeoffs*, and the validation. \
    Inline `identifiers` and Unicode café — 日本語 exercise production text shaping.\n\n\
    - Keep the host authoritative.\n\
    - Validate updates before applying them.\n\
    - Test both ordinary input and boundary cases.\n\n\
    > Warm rendering is different from parsing a newly streamed response.\n\n\
    ```rust\nfn summarize(values: &[usize]) -> usize {\n    values.iter().sum()\n}\n```\n\n\
    | Stage | Result |\n| --- | --- |\n| Validation | Complete |\n| Review | Ready |\n\n\
    The remaining paragraphs describe the implementation without invoking tools or \
    contacting a provider. Each reply owns its own Markdown view, just as real \
    completed messages do. Repeated content is intentional: this measures warm \
    timeline scaling, not a corpus of unique documents.\n\n";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_CASE_BYTES: usize = 16 * 1024 * 1024;

struct Config {
    sizes: Vec<usize>,
    responses: Vec<(&'static str, String)>,
    iterations: usize,
    warmup: usize,
    control: bool,
}

fn setting(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(error) => panic!("{name}: {error}"),
    }
}

fn bounded_setting(name: &str, default: usize, max: usize) -> usize {
    let value = setting(name).map_or(default, |value| {
        value
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{name} must be an integer"))
    });
    assert!((1..=max).contains(&value), "{name} must be in 1..={max}");
    value
}

impl Config {
    fn from_env() -> Self {
        let mut sizes = Vec::new();
        for value in setting("COWORK_TIMELINE_BENCH_SIZES")
            .unwrap_or_else(|| "10,100,1000".into())
            .split(',')
        {
            let count = value
                .trim()
                .parse::<usize>()
                .expect("benchmark size must be an integer");
            assert!(
                [10, 100, 1000, 5000].contains(&count),
                "unsupported benchmark size: {count}"
            );
            assert!(!sizes.contains(&count), "duplicate benchmark size: {count}");
            sizes.push(count);
        }
        let file = setting("COWORK_TIMELINE_BENCH_RESPONSE_FILE").map(|path| {
            let file = std::fs::File::open(&path).expect("open benchmark response file");
            let metadata = file.metadata().expect("stat benchmark response file");
            assert!(metadata.is_file(), "response must be a regular file");
            assert!(
                metadata.len() <= MAX_RESPONSE_BYTES as u64,
                "response exceeds 64 KiB"
            );
            let mut bytes = Vec::new();
            file.take((MAX_RESPONSE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .expect("read benchmark response file");
            assert!(bytes.len() <= MAX_RESPONSE_BYTES, "response exceeds 64 KiB");
            let text = String::from_utf8(bytes).expect("response file must be UTF-8");
            assert!(!text.trim().is_empty(), "response file must not be empty");
            text
        });
        let default_variants = if file.is_some() {
            "short,markdown-long,plain-long,file"
        } else {
            "short,markdown-long,plain-long"
        };
        let mut responses = Vec::new();
        for variant in setting("COWORK_TIMELINE_BENCH_VARIANTS")
            .unwrap_or_else(|| default_variants.into())
            .split(',')
        {
            let (name, text) = match variant.trim() {
                "short" => ("short", SHORT.to_owned()),
                "markdown-long" => ("markdown-long", LONG.repeat(3)),
                "plain-long" => {
                    // ASCII allows truncation to the exact Markdown byte count.
                    let prose = "This completed response explains the implementation and its validation without special block structure. ";
                    let bytes = LONG.len() * 3;
                    let mut text = prose.repeat(bytes.div_ceil(prose.len()));
                    text.truncate(bytes);
                    ("plain-long", text)
                }
                "file" => (
                    "file",
                    file.clone()
                        .expect("file variant requires COWORK_TIMELINE_BENCH_RESPONSE_FILE"),
                ),
                other => panic!("unknown benchmark variant: {other}"),
            };
            assert!(
                !responses.iter().any(|(existing, _)| *existing == name),
                "duplicate variant: {name}"
            );
            for &count in &sizes {
                assert!(
                    text.len() * (count / 2) <= MAX_CASE_BYTES,
                    "{name}/{count} exceeds 16 MiB of response text"
                );
            }
            responses.push((name, text));
        }
        let control = match setting("COWORK_TIMELINE_BENCH_CONTROL").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            _ => panic!("COWORK_TIMELINE_BENCH_CONTROL must be 0 or 1"),
        };
        Self {
            sizes,
            responses,
            iterations: bounded_setting("COWORK_TIMELINE_BENCH_ITERATIONS", 30, 1000),
            warmup: bounded_setting("COWORK_TIMELINE_BENCH_WARMUP", 5, 100),
            control,
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Stage,
    Construct,
    Placeholder,
    ReplyOnlySelectable,
    ReplyOnlyUnselectable,
}

#[derive(Clone, Copy, Default)]
pub(super) struct EditorPhases {
    pub(super) durations: [Duration; 3],
    pub(super) calls: [usize; 3],
}

struct TimedEditor<E> {
    inner: E,
    phases: Rc<Cell<EditorPhases>>,
    profile_totals: Option<Rc<Cell<EditorPhases>>>,
}

impl<E> TimedEditor<E> {
    fn record(&self, phase: usize, elapsed: Duration) {
        let mut records = self.phases.get();
        records.durations[phase] += elapsed;
        records.calls[phase] += 1;
        self.phases.set(records);
        if let Some(totals) = &self.profile_totals {
            let mut records = totals.get();
            records.durations[phase] += elapsed;
            records.calls[phase] += 1;
            totals.set(records);
        }
    }
}

impl<E: Element> IntoElement for TimedEditor<E> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl<E: Element> Element for TimedEditor<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inner.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.inner.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let start = Instant::now();
        let result = self.inner.request_layout(id, inspector_id, window, cx);
        self.record(0, start.elapsed());
        result
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let start = Instant::now();
        let result = self
            .inner
            .prepaint(id, inspector_id, bounds, request_layout, window, cx);
        self.record(1, start.elapsed());
        result
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let start = Instant::now();
        self.inner.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
        self.record(2, start.elapsed());
    }
}

pub(super) struct BenchRoot {
    cowork: Entity<Cowork>,
    reply_views: Vec<Entity<TextViewState>>,
    count: usize,
    mode: Mode,
    construction: Option<Duration>,
    phases: Rc<Cell<EditorPhases>>,
    profile_totals: Option<Rc<Cell<EditorPhases>>>,
    draws: usize,
}

impl BenchRoot {
    /// The same production stage and phase instrumentation for live profiling.
    pub(super) fn stage(cowork: Entity<Cowork>) -> Self {
        Self {
            cowork,
            reply_views: Vec::new(),
            count: 0,
            mode: Mode::Stage,
            construction: None,
            phases: Rc::new(Cell::new(EditorPhases::default())),
            profile_totals: Some(Rc::new(Cell::new(EditorPhases::default()))),
            draws: 0,
        }
    }

    pub(super) fn draws(&self) -> usize {
        self.draws
    }

    /// Inclusive phase totals across scheduled draws, even when an element is cached.
    pub(super) fn profile_phases(&self) -> EditorPhases {
        self.profile_totals.as_ref().expect("profile stage").get()
    }
}

impl Render for BenchRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.draws += 1;
        self.construction = None;
        self.phases.set(EditorPhases::default());
        let outer = div().size_full().flex().flex_col().min_h_0().min_w_0();
        match self.mode {
            Mode::Stage => outer.child(self.cowork.update(cx, |cowork, cx| {
                TimedEditor {
                    inner: cowork
                        .render_main_editor(Rc::new(Cell::new(None)), window, cx)
                        .into_element(),
                    phases: self.phases.clone(),
                    profile_totals: self.profile_totals.clone(),
                }
                .into_any_element()
            })),
            Mode::Construct => {
                self.construction = Some(self.cowork.update(cx, |cowork, cx| {
                    let bounds = Rc::new(Cell::new(None));
                    let start = Instant::now();
                    let element = cowork.render_main_editor(bounds, window, cx);
                    black_box(&element);
                    let elapsed = start.elapsed();
                    // The concrete element is dropped while the draw's arena is
                    // still active. Arena-owned children are reclaimed by clear.
                    drop(element);
                    elapsed
                }));
                outer
            }
            Mode::ReplyOnlySelectable | Mode::ReplyOnlyUnselectable => {
                let selectable = matches!(self.mode, Mode::ReplyOnlySelectable);
                // Match the sidebar-free production editor's text column width.
                let run = TextRun {
                    len: 1,
                    font: window.text_style().font(),
                    color: cx.theme().foreground,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let text_width = window
                    .text_system()
                    .shape_line("0".into(), px(14.), &[run], None)
                    .width()
                    * 120.;
                let wrap_width = (window.viewport_size().width - px(82.))
                    .max(px(120.))
                    .min(text_width);
                outer.child(
                    div()
                        .id("bench-reply-only-scroll")
                        .size_full()
                        .overflow_y_scroll()
                        .child(
                            div()
                                .w(wrap_width)
                                .mx_auto()
                                .flex()
                                .flex_col()
                                .text_size(px(14.))
                                .text_color(cx.theme().foreground)
                                .children(self.reply_views.iter().map(|view| {
                                    TextView::new(view)
                                        .selectable(selectable)
                                        .w_full()
                                        .flex_shrink_0()
                                })),
                        ),
                )
            }
            Mode::Placeholder => outer.child(
                div()
                    .id("bench-placeholder-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .children((0..self.count).map(|index| {
                                div()
                                    .h(px(40.))
                                    .flex_shrink_0()
                                    .child(format!("Message {index}: placeholder"))
                            })),
                    ),
            ),
        }
    }
}

pub(super) fn timeline(count: usize, response: &str, cx: &mut gpui::App) -> Vec<TimelineMessage> {
    let author = ParticipantId::new();
    (0..count)
        .map(|index| {
            if index % 2 == 0 {
                TimelineMessage::User(UserMessageGroup {
                    id: Uuid::new_v4(),
                    blocks: vec![PromptBlock {
                        id: Uuid::new_v4(),
                        author,
                        text: "Explain the implementation and its validation.".into(),
                        attachments: Vec::new(),
                    }],
                    comments: Vec::new(),
                    comments_folded: false,
                })
            } else {
                let mut message = AgentMessage::new(
                    Uuid::new_v4(),
                    None,
                    SystemTime::UNIX_EPOCH,
                    0,
                    AgentRun::Ended {
                        outcome: RunOutcome::Completed,
                        duration: Duration::from_secs(1),
                    },
                    Vec::new(),
                    cx,
                );
                message.show_output(
                    AgentOutput {
                        steps: vec![AgentStep::Text(response.into())],
                        thinking_complete: true,
                        text: response.into(),
                    },
                    cx,
                );
                message.committed = OutputMark {
                    steps: 1,
                    tail: response.len(),
                    answered: 0,
                };
                TimelineMessage::Agent(message)
            }
        })
        .collect()
}

fn report(variant: &str, count: usize, metric: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    let middle = samples.len() / 2;
    let median = if samples.len().is_multiple_of(2) {
        (samples[middle - 1].as_secs_f64() + samples[middle].as_secs_f64()) / 2.
    } else {
        samples[middle].as_secs_f64()
    };
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1].as_secs_f64();
    println!(
        "{variant},{count},{metric},{},{:.3},{:.3}",
        samples.len(),
        median * 1e6,
        p95 * 1e6
    );
}

#[test]
#[ignore = "opt-in finite CPU timeline benchmark; run with --ignored --nocapture"]
fn timeline_bench() {
    let config = Config::from_env();
    // Construct once, not per sample/case: Linux platforms own worker threads.
    let platform = gpui_platform::current_platform(true);
    let text_system = platform.text_system();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("fixture runtime");
    println!("variant,messages,metric,samples,median_us,p95_us");
    for (variant, response) in &config.responses {
        for &count in &config.sizes {
            let mut cx =
                HeadlessAppContext::with_asset_source(text_system.clone(), Arc::new(Assets));
            cx.update(|cx| {
                gpui_component::init(cx);
                crate::theme::init(cx);
            });
            let mut fixture = None;
            let handle = cx
                .open_window(size(px(1200.), px(760.)), |window, cx| {
                    let thread_id = Uuid::new_v4();
                    let messages = timeline(count, response, cx);
                    let reply_views: Vec<_> = messages
                        .iter()
                        .filter_map(|message| match message {
                            TimelineMessage::Agent(message) => Some(message.text_view.clone()),
                            TimelineMessage::User(_) => None,
                        })
                        .collect();
                    assert_eq!(reply_views.len(), count / 2);
                    let thread = cx.new(|_| {
                        test_thread(thread_id, messages, ThreadDraft::new(ParticipantId::new()))
                    });
                    let store = cx.new(|_| ThreadStore {
                        threads: std::collections::VecDeque::from([thread.clone()]),
                        ..Default::default()
                    });
                    let cowork = cx.new(|cx| {
                        test_cowork(store, Some(thread_id), runtime.handle().clone(), window, cx)
                    });
                    // Only the stage is shown; do not reserve space for an absent sidebar.
                    cowork.update(cx, |cowork, _| cowork.sidebar_open = false);
                    let bench = cx.new(|_| BenchRoot {
                        cowork: cowork.clone(),
                        reply_views,
                        count,
                        mode: Mode::Stage,
                        construction: None,
                        phases: Rc::new(Cell::new(EditorPhases::default())),
                        profile_totals: None,
                        draws: 0,
                    });
                    let root = cx.new(|cx| Root::new(bench.clone(), window, cx));
                    fixture = Some((root.clone(), bench, cowork, thread));
                    root
                })
                .expect("headless benchmark window");
            let (root, bench, cowork, thread) = fixture.expect("benchmark fixture");
            cx.run_until_parked();

            let mut modes = vec![
                (Mode::Stage, "stage_frame"),
                (Mode::Construct, "editor_construct"),
            ];
            if config.control {
                modes.push((Mode::Placeholder, "placeholder_frame"));
                modes.push((Mode::ReplyOnlySelectable, "reply_only_selectable_frame"));
                modes.push((Mode::ReplyOnlyUnselectable, "reply_only_unselectable_frame"));
            }
            for (mode, metric) in modes {
                bench.update(&mut cx, |bench, cx| {
                    bench.mode = mode;
                    cx.notify();
                });
                for _ in 0..config.warmup {
                    cx.update_window(handle.into(), |_, window, cx| {
                        cowork.update(cx, |_, cx| cx.notify());
                        bench.update(cx, |_, cx| cx.notify());
                        root.update(cx, |_, cx| cx.notify());
                        window.draw(cx).clear(cx);
                    })
                    .expect("warmup draw");
                    cx.run_until_parked();
                }
                let mut samples = Vec::with_capacity(config.iterations);
                let mut phase_samples: [Vec<Duration>; 5] =
                    std::array::from_fn(|_| Vec::with_capacity(config.iterations));
                for _ in 0..config.iterations {
                    let (elapsed, draw, clear, phases) = cx
                        .update_window(handle.into(), |_, window, cx| {
                            let before = cowork.read(cx).render_generation;
                            let draws = bench.read(cx).draws;
                            cowork.update(cx, |_, cx| cx.notify());
                            bench.update(cx, |bench, cx| {
                                bench.phases.set(EditorPhases::default());
                                cx.notify();
                            });
                            root.update(cx, |_, cx| cx.notify());
                            let start = Instant::now();
                            let arena = window.draw(cx);
                            let draw_end = Instant::now();
                            arena.clear(cx);
                            let clear_end = Instant::now();
                            let draw = draw_end.duration_since(start);
                            let clear = clear_end.duration_since(draw_end);
                            let frame = clear_end.duration_since(start);
                            let phases = bench.read(cx).phases.get();
                            if matches!(mode, Mode::Stage) {
                                assert!(
                                    phases.calls.iter().all(|&calls| calls > 0),
                                    "editor timing wrapper was skipped: {:?}",
                                    phases.calls
                                );
                            }
                            assert_eq!(bench.read(cx).draws, draws + 1, "cached benchmark root");
                            if matches!(mode, Mode::Stage | Mode::Construct) {
                                assert_eq!(
                                    cowork.read(cx).render_generation,
                                    before.wrapping_add(1),
                                    "cached main editor"
                                );
                            }
                            let elapsed = if matches!(mode, Mode::Construct) {
                                bench.read(cx).construction.expect("construction sample")
                            } else {
                                frame
                            };
                            (elapsed, draw, clear, phases)
                        })
                        .expect("measured draw");
                    samples.push(elapsed);
                    if matches!(mode, Mode::Stage) {
                        for (samples, duration) in phase_samples.iter_mut().zip([
                            draw,
                            clear,
                            phases.durations[0],
                            phases.durations[1],
                            phases.durations[2],
                        ]) {
                            samples.push(duration);
                        }
                    }
                }
                report(variant, count, metric, &mut samples);
                if matches!(mode, Mode::Stage) {
                    for (metric, samples) in [
                        "stage_draw",
                        "stage_arena_clear",
                        "editor_request_layout_nodes",
                        "editor_prepaint",
                        "editor_paint",
                    ]
                    .into_iter()
                    .zip(&mut phase_samples)
                    {
                        report(variant, count, metric, samples);
                    }
                }
            }
            let mut clones = Vec::with_capacity(config.iterations);
            cx.update(|cx| {
                for _ in 0..config.iterations {
                    let source = &thread.read(cx).timeline;
                    let start = Instant::now();
                    let cloned = black_box(source).clone();
                    black_box(&cloned);
                    clones.push(start.elapsed());
                    drop(cloned);
                }
            });
            report(variant, count, "timeline_clone", &mut clones);
            // Release fixture handles before HeadlessAppContext shuts down its app.
            drop((root, bench, cowork, thread));
        }
    }
}
