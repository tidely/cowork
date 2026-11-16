# Timeline virtualization

The center stage draws a chat's timeline through `VirtualTimeline`
(`crates/cowork/src/timeline_virtualization.rs`, read its module doc first), so a
frame costs the rows near the view, not the length of the history. This matters
because everything else scales with the rows that are built. Before
virtualization a chat of 1000 structured-Markdown messages took about 2.6 s per
frame on a desktop CPU, and a click, a width change or a selection drag each cost
whole frames; incoming agent events queued behind those renders for up to a
second during generation. Profiles at the time attributed much of that to GPUI Base's per-row bookkeeping
(selection registration and retained text layouts), which grows faster than
linearly with the number of rows built. Do not reintroduce any path that builds
or lays out every row.

## How it works

`Cowork::render_main_editor` hands the timeline's rows to `VirtualTimeline`, a
custom element inside the `timeline-scroll` div and its `ScrollHandle`. It reports
the rows' total height to the scroll container and builds, lays out and paints only
the rows within 128 px of the view, plus pinned rows. Pending submissions and the
composer follow the rows. Every row is virtualized, live and interactive ones
included, with no message-count threshold.

- **Heights are hints.** A row's height is measured when it is built, at the width it
  is drawn at, and kept in `TimelineLayout`. A row never drawn counts as the average
  measured row. Every drawn row is measured again each frame; when a measurement
  differs, rows are placed around an anchor (the first visible row, or the end of the
  history while it is in view) and the scroll offset moves by the same amount, so
  what is on screen stays put while the extent converges.
- **Nothing invalidates an offscreen height.** A changed row is measured when next
  drawn, so width and typography changes cost only the rows drawn.
- **Rendering never copies the timeline.** `Cowork::plan_timeline_rows` scans it by
  reference once per frame, and a row clones its message (`TimelineRow::capture`)
  only when built. Rows are built during the element's prepaint, so the
  segment-view caches keep what the previous frame used rather than only what this
  render touched; eviction keyed on a render generation must not destroy offscreen
  state.
- **The turn navigation rail uses the same placement.** A turn navigated to becomes
  the anchor, found by identity at its estimated position, so rows on the way are
  never built.

### Selection and copy

GPUI Base's window selection knows only text painted this frame, so
`timeline_selection.rs` keeps the selection working across culled rows:

- The row a press lands in stays built while the button is held, and the rows
  holding a selection's ends (tracked from the pointer during the drag) stay built
  while the selection lasts, at their true positions.
- Copy of a selection spanning rows is built by the app, in timeline order: the
  selected text of the two end rows, and the whole text of every row between them,
  from the views and prompts `Cowork::copy_pieces` lists for each row, painted or
  not. `copy_pieces` must match what `render_timeline_message` draws. Prompts are
  drawn with `PromptText`, which records its selected range.
- Selections within one row, and any not made by a drag in the timeline, are copied
  by GPUI Base itself, from painted rows only.

## Accepted tradeoffs

- The scroll extent is an estimate until rows have been drawn, and the timeline has
  no scrollbar. Scrolling through the history makes it exact.
- An offscreen row that changes (late parsing, folding) keeps its old height until
  drawn, when anchoring absorbs the difference.
- Tool calls and comments are not selectable text and are not copied.

`crates/cowork/src/tests/timeline_virtualization.rs` covers the behaviours a change
must keep: building only nearby rows, anchoring while rows above are measured,
folding above the view, extent convergence, width changes, bottom-follow while rows
grow, clicks and steers, turn navigation to never-drawn rows, and a cross-row drag
that stays culled and copies every row in order.

## Streaming

Agent events arrive through `Cowork::thread_updated`, which requests bottom-follow
and a root notification for every event of the active chat; GPUI coalesces the
notifications. `scroll_to_bottom` only sets a flag, and parsing later changes
heights, so bottom anchoring has to be checked after parser commits, not at event
time. Streaming costs that are not dominated by history size but grow with a single
long reply: `ReplyPreview::partial` clones the partial reply, `AgentMessageUi::advance`
reconstructs its changing output and response string, and Markdown reparses the last
block on each append (a huge paragraph, open fence, list or table makes that
expensive). None of these has been measured as a bottleneck.

## Measuring

Both tools are opt-in `#[ignore]` tests, and production code has no timing
instrumentation. Use the release profile for any conclusion.

- **`crates/cowork/src/tests/timeline_bench.rs`** renders completed chats of
  10 to 5000 messages through the real `Cowork::render_main_editor` in a headless
  window and reports median and p95 CPU frame phases as CSV. Its module doc has the
  command and the `COWORK_TIMELINE_BENCH_*` variables. It is a warm,
  explicit-draw microbenchmark: no GPU, scrolling or streaming. Its notes about
  rendering every row predate virtualization and now describe the cost of the
  rows the editor builds, which are only the ones near the view. It is not a model
  of production frame scheduling, because in GPUI's test mode the effect flush
  draws dirty windows and so charges renders to event handling.
- **`scripts/profile-generation.py`** runs the Linux-only
  `tests/generation_profile.rs`: a real window with production scheduling, fed by
  an Ollama server the script starts itself on port 11435, under `perf`. It
  requires Linux, `perf`, and the model `qwen3.8:27b` already installed, and never
  downloads one. Run it with
  `uv run --no-project python scripts/profile-generation.py --history 1000`
  (`--variant markdown-long`, `--history 0|100|1000`). Reports go to
  `target/generation-profiles/`. It samples the UI process only, not Ollama's
  inference, and its warmup and settle timers are heuristics.

The useful signals for a regression are the worst delay between an event arriving
and being applied and the number of queued events during generation, not
aggregate render time, because responsive animation produces more renders.
