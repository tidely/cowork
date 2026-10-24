# Large-thread and live-generation performance investigation

## Scope and infrastructure

The opt-in benchmark is `crates/cowork/src/tests/timeline_bench.rs`. It uses
GPUI's existing `HeadlessAppContext`, production platform text shaping, bundled
assets, and the real `Cowork::render_main_editor` in a 1200 × 760 window.
No new dependencies, Cargo features, application flags, or production-code
instrumentation are required. It is ignored during ordinary tests.

The installed GPUI version is `gpui-pre 0.3.8`, with `gpui-base 0.7.1`.
GPUI also supplies a Criterion-based `#[gpui::bench]` harness behind
`bench-support`, plus a Linux offscreen GPU renderer. The existing test-support
headless API was sufficient here: finite draws and explicit phase timers isolate
CPU work without adding Criterion or requiring a GPU/display renderer.

Fixtures alternate prompts and completed replies. Reply variants are:

- `short`: one short paragraph.
- `markdown-long`: about 3 KiB, including headings, inline code, lists, quotes,
  fenced code, and tables.
- `plain-long`: a single prose paragraph of the same byte length as
  `markdown-long` (not identical content).
- `file`: optional UTF-8 response loaded from a regular file, e.g. an Ollama
  response captured separately. Generation is deliberately outside timing.

The benchmark performs warmup draws and drains background parsing work. It
forces root/editor notifications and asserts that measured frames actually
rendered. It reports median/p95 CSV timings for full CPU frames, draw and arena
cleanup, editor layout-node requests/prepaint/paint, editor construction, and
isolated timeline cloning. Optional controls compare placeholder rows and
otherwise matched reply-only TextViews with selection enabled/disabled.

## Reproduce

Run from the workspace root. Always use an optimized profile for performance
conclusions; the debug profile is substantially slower.

```sh
# Short, reasonably quick scaling run; outer timeout also bounds build time.
timeout 180s env CARGO_PROFILE_RELEASE_DEBUG=1 \
  COWORK_TIMELINE_BENCH_VARIANTS=short \
  COWORK_TIMELINE_BENCH_CONTROL=1 \
  cargo test --release -p cowork timeline_bench -- \
  --ignored --nocapture --test-threads=1

# Full default matrix: 10/100/1000 messages, three variants, 30 samples each.
timeout 300s env CARGO_PROFILE_RELEASE_DEBUG=1 \
  cargo test --release -p cowork timeline_bench -- \
  --ignored --nocapture --test-threads=1
```

All sizes/sample counts are bounded and validated. The full environment-variable
reference is at the top of the benchmark module. A single-case fresh process is
recommended for comparisons: GPUI Base's retained layout table is thread-local
and survives individual headless app contexts. Sweeps remove dead entries but
retain allocated table capacity, so matrix ordering can affect later cases.
The 5000-message size is opt-in; it is not needed to reproduce either hotspot.

For Linux profiling, first build with `CARGO_PROFILE_RELEASE_DEBUG=1`. Locate the
printed `target/release/deps/cowork-…` executable from:

```sh
timeout 300s env CARGO_PROFILE_RELEASE_DEBUG=1 \
  cargo test --release -p cowork timeline_bench --no-run
```

Then run `perf record` directly around that executable with the test arguments
`timeline_bench --ignored --nocapture --test-threads=1`. The investigation used
`-e cpu-clock:u -F 199 --call-graph dwarf,16384`, a single 1000-message variant,
8 samples and 2 warmup draws. Bound the command with `timeout 120s`. Write the
capture under ignored `target/`, not into the source tree.

The installed tool was `/usr/lib/linux-tools/6.8.0-137-generic/perf` (not on
`PATH`); user-space software sampling worked without changing system settings.
Use `perf report --stdio --no-inline --no-children --call-graph none --sort symbol`
for a fast flat self-time report. The initial inline-resolution report timed out
after 120 seconds with addr2line errors; disabling inline resolution succeeded.
Perf's Rust v0 symbols were mangled; the names below are interpreted symbols.

## Live-generation profiling

The live investigation uses **Linux perf sampling of an actual GPUI window**, not
headless draw timings. `crates/cowork/src/tests/generation_profile.rs` is an
ignored, Linux-only harness that creates the real center stage with production
GPUI scheduling. The agent session's Ollama events are received asynchronously and follow
`Thread::emit` and `Cowork::thread_updated`; Markdown parsing and rendering run
normally. It neither manually draws nor imposes a frame cadence.

### Reproduce with an owned Ollama server

```sh
uv run --no-project python scripts/profile-generation.py --history 1000
uv run --no-project python scripts/profile-generation.py --history 1000 --variant markdown-long
```

The supervisor builds the optimized test executable, starts **its own Ollama on
127.0.0.1:11435**, verifies that `qwen3.8:27b` is already installed, and runs perf.
It refuses an occupied port rather than reusing/stopping someone else's server.
It disables cloud access and model-cache pruning, never downloads a model, and
terminates only the process groups it creates. The real window closes itself.
Limits are 60 seconds for generation, a GPUI 90-second watchdog, a 120-second
supervisor capture timeout, and bounded startup/build/report commands. Model
output is capped at 128 predicted tokens by default, at most 256.

Captures, server/run logs, self-time and inclusive reports are saved to a fresh
ignored directory under `target/generation-profiles/`. No Python packages are
required; use the repository's `uv` workflow. The supervisor discovers installed
perf, including `/usr/lib/linux-tools/*/perf` when it is absent from PATH. It uses
`c++filt -s rust` when available to make Rust v0 symbols readable.

Perf starts with events disabled. The harness enables sampling through a FIFO
after fixture warmup and disables it after generation and a short settle period.
Thus setup, correctness checks, and shutdown are excluded. Sampling includes the
UI process's parser/rendering/runtime threads but **not the separate Ollama
server's inference CPU/GPU work**. Model/network waits are wall time, not CPU
samples. The warmup/settle timers are heuristics, not parse-completion fences.

**Important correction:** an initial headless live attempt was discarded. GPUI's
`GpuiMode::Test` eagerly draws dirty windows during effect flushing, charging
hidden renders to event updates and parser drains. The real-window reruns use
`Application`'s Production mode; do not interpret the discarded headless timings
as live event-processing cost. The completed-thread benchmark remains a useful
explicit-draw microbenchmark, but not a production scheduling model.

### Profile findings from isolated-server reruns

Each profile ran against our own server on port 11435, with real production
scheduling. The history is synthetic UI history; Ollama receives a short prompt,
not the entire synthetic history. The Markdown history uses one `LONG` fixture
per reply (about 1 KiB), rather than the three repetitions in the static matrix.

| UI history | Agent events | Stage renders | Event application total | Max queued events | Worst arrival-to-consumption delay |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1000 short messages | 112 | 15 | 2.33 ms | 57 | 529 ms |
| 1000 Markdown messages | 107 | 7 | 5.20 ms | 100 | 1101 ms |
| No history, text response | 125 | 819 | 1.41 ms | 5 | 0.54 ms |

Event application totals include emit/folding/text-update/notify work, **not
asynchronous Markdown parsing or scheduled frames**. Serialization was separately
about 0.04 ms in each large-history run. Counts are stage render calls, not
GPU-presented frames. The no-history run used the tested supervisor command
`--history 0`; its samples are too sparse for precise percentage comparisons.

Scoped perf samples identify the main costs:

- **Short history:** `AnyWeakEntity::upgrade` 31.55% self time and
  `AnyEntity::drop` 23.86%. Selection registration accounts for about **54%
  inclusive**, with `publish_snapshots` about 37% and whole-window copy queries
  about 20%. These inclusive percentages overlap; do not add them.
- **Markdown history:** `Inline::retain_styled_text` **33.05% self time**;
  entity upgrade/drop another 13.89%/10.95% self time. Selection registration
  about 27% inclusive. This is the same repeated retained-layout sweep and
  selection bookkeeping found in completed-thread profiles, now exercised by
  actual generation.
- Neither JSON serialization nor per-event folding was a dominant sampled
  hotspot in these short-response, large-history runs. This is not a finding
  about arbitrarily long replies.

The short-history capture had 859 samples at 199 Hz; the Markdown capture had
2548 samples at 499 Hz. No lost samples were reported. DWARF stacks were bounded
at 16 KiB, so some deeply nested rendering ancestry is incomplete. Local
selection/cache stacks and flat self-time are more trustworthy than attributing
all work to a single outer `Window::draw` frame in an inclusive report.

**Interpretation:** the UI thread spends its time repeatedly revisiting the
history, so incoming events wait behind expensive renders. GPUI already coalesces
notifications—112 events did not produce 112 root renders—but coalescing does
not make a 250–700 ms render responsive. The empty-history profile also shows
many more stage renders than events: parsing notifications and live animation
can request work independently of token arrival.

A reasoning-enabled empty-history capture also completed (253 events, 372 stage
renders, about 2.94 ms event application). A later repeat exhausted its 256-token
budget entirely in reasoning and correctly failed with a no-answer/length
error. `--reasoning` is therefore useful for exploratory profiles but is not a
reliable smoke test at the deliberately small output cap. Failed-run captures
are retained and the supervisor still attempts to generate reports.

Raw successful captures from this investigation remain under ignored `target/`:
`generation-profile-real-short.data`, `generation-profile-real-markdown.data`,
and `generation-profile-real-reasoning-empty.data`. Their corresponding
`-flat.txt` and `-inclusive.txt` files describe the sampled stacks. The validated
supervisor's empty-history capture is in
`target/generation-profiles/short-h0-gz71ln72/`.

## Implemented fix: full timeline virtualization

`lib.rs::render_main_editor` hands the timeline's rows to `VirtualTimeline`
(`timeline_virtualization.rs`), a custom GPUI element inside the existing
`timeline-scroll` div and `ScrollHandle`. It reports the rows' total height to
the scroll container and builds, lays out and paints only the rows within
128 px of the view, plus pinned rows (a held press's row and a selection's end
rows). Pending submissions and the composer follow the rows, laid out
natively. Every row is virtualized, live and interactive ones included; there
is no message-count threshold. There are **no vendored crates, dependency
patches, or dependency changes**.

Heights are measured when a row is built, at the width it is drawn at, and
kept in `TimelineLayout`; a row never drawn counts as the average measured
row. They are hints: every drawn row is measured again each frame, and when a
measurement differs the rows are placed around an anchor (the first visible
row, or the end of the history while it is in view) and the scroll offset
moves by the same amount, clamped to what the next frame's container allows.
What is on screen stays put while the extent converges. Nothing invalidates an
offscreen height; a changed row is measured when next drawn. Width and
typography changes therefore cost only the rows drawn.

Rendering reads the thread's timeline in place and never copies it whole:
`Cowork::plan_timeline_rows` scans it by reference once per frame, and a row
clones its message (`TimelineRow::capture`) only when it is built. Rows are
built during the element's prepaint, so the segment-view caches keep what the
previous frame used rather than only what this render touched.

Selection no longer lays out the history (`timeline_selection.rs`). The row a
press lands in stays built while the button is held, and the rows holding a
selection's ends (tracked from the pointer during the drag) stay built while
the selection lasts, at their true positions, so GPUI Base keeps the
selection. Rows paint in timeline order. Copy of a selection spanning rows is
built by the app, in timeline order: the selected text of the two end rows
(Markdown views' own selection; prompts through `PromptText`, which records
its projected range) and the whole text of every row between them, from the
views and prompts `Cowork::copy_pieces` lists for each row, painted or not.

### Interaction benchmark

Measured with a temporary benchmark (since removed) that dispatched real
input through the whole `Cowork` root with production text shaping, scrolled
to the bottom. In GPUI's Test mode the effect flush draws dirty windows, so a
sample is event handling plus the frames it caused. Medians in ms, release:

| 1000 turns | Before | Virtualized |
| --- | ---: | ---: |
| short: redraw / click | 9.6 / 10.1 | 1.07 / 1.15 |
| markdown: redraw / click | 12.6 / 13.6 | 1.28 / 1.57 |
| short: drag frame | 1283 | 1.14 |
| markdown: drag frame | 10293 | 1.36 |
| short: width change (sidebar toggle) | 95 | 1.19 |
| markdown: width change | 943 | 1.39 |
| markdown: scroll up 240 px | 20.3 | 1.41 |
| markdown: copy the whole history | n/a | 1.7 (worst 18) |

Before virtualization a click cost a whole frame like any redraw: mouse down
and up each caused one. A width change laid out every row natively.

### Live-generation verification (of the earlier culling)

These captures verified the earlier `DeferredTimelineRow` culling, which kept
every outer row in layout. The same owned-Ollama supervisor and real production scheduling were used after
the fix, with `qwen3.8:27b` and 1000 messages:

| History | Worst event lag, before → after | Max queued events, before → after | Events after | Stage renders after |
| --- | ---: | ---: | ---: | ---: |
| Short | 529 → 6.812 ms | 57 → 5 | 125 | 601 |
| Markdown | 1101 → 8.828 ms | 100 → 5 | 125 | 447 |

Event application after the fix totaled 8.796 ms / 8.726 ms respectively. Model
wall times were 4.325 s / 4.132 s; scoped capture regions were 4.526 s / 4.342 s.
These are individual real-generation captures, **not identical-event replays**:
responses vary, and responsive live animations now produce many more renders.
Consequently, aggregate layout/prepaint/paint time is not a like-for-like
per-frame comparison. Event lag and queue depth demonstrate the responsiveness
improvement in these captures, not a guaranteed bound for every workload.

Captures and reports:

- `target/generation-profiles/short-h1000-k85x6uzv/`
- `target/generation-profiles/markdown-long-h1000-cy0f8t6k/`

### Correctness and tradeoffs

- The scroll extent is an estimate until rows have been drawn; the timeline
  has no scrollbar. Scrolling through the history makes it exact.
- An offscreen row that changes (late parsing, folding) keeps its old height
  until drawn, when anchoring absorbs the difference.
- Copy of a selection spanning rows depends on `Cowork::copy_pieces` matching
  what `render_timeline_message` draws; tool calls and comments are not
  selectable text and are not copied. Selections spanning rows that a drag
  didn't make (e.g. shift-click extension) are not tracked; GPUI Base copies
  those itself, painted rows only.
- GPUI Base orders selection participants by `document_order`: prompts set
  `timeline index × 1000 + block`, while agent Markdown views take a per-frame
  paint counter. The two schemes don't interleave, so GPUI Base's own copy
  put every reply after the first prompt (fixed for spanning selections by
  building Copy in the app), and highlighting can still include prompts above
  an anchor in agent text. A proper fix needs GPUI Base to let the app set a
  `TextView`'s order.
- After any frame that lays out every Markdown row (none remain in the app's
  own paths), the retained-layout sweep below makes later frames proportional
  to history again.
- Regression tests (`tests/timeline_virtualization.rs`) cover building only
  nearby rows, anchoring while rows above are measured, folding above the
  view, extent convergence, width changes, bottom-follow while rows grow,
  clicks, steers, and a cross-row drag staying culled and copying every row
  in order with partial end rows.

## Completed-thread measurements

Host: AMD Ryzen 9 9950X, Linux x86_64, Rust/Cargo 1.99.0. Optimized release test
build with debug level 1. These are wall-clock CPU scene-building measurements,
not real-window frame latency or GPU execution time. No GPUI entity leak-detection
cfg was enabled in the dependency build.

First optimized matrix, 30 measured warm frames per case, medians in milliseconds:

| Total messages | Short replies | Structured Markdown replies | Same-byte-length prose |
| ---: | ---: | ---: | ---: |
| 10 | 0.423 | 2.795 | 0.781 |
| 100 | 7.023 | 45.843 | 6.990 |
| 1000 | 288.022 | 2596.178 | 302.309 |

At 1000 structured-Markdown messages:

| Measurement | Median ms |
| --- | ---: |
| Full CPU frame | 2596.178 |
| Editor prepaint | 839.862 |
| Editor paint | 1351.977 |
| Editor layout-node requests | 93.040 |
| Editor construction only | 4.690 |
| Timeline clone only | 0.242 |
| Arena cleanup | 18.662 |

These medians are not an additive phase partition. Layout-node requests include
*deferred* TextView/Markdown element construction, but not subsequent Taffy layout
computation. Full frames include that computation and other root/window work.
Clone timing excludes destruction; full frames do not.

The selection control run, 30 samples, renders only persistent agent TextViews
without prompts, avatars, or composer:

| Replies (half the source messages) | Selection enabled, ms | Selection disabled, ms |
| ---: | ---: | ---: |
| 5 | 0.092 | 0.082 |
| 50 | 1.106 | 0.658 |
| 500 | 45.835 | 5.786 |

This supports selection bookkeeping as a major short-reply scaling cost. It is
an ablation, not an equivalent replacement for the production stage. A separate
5-sample 500-reply structured-Markdown control remained slow with selection off
(about 2.78 seconds), consistent with the independent layout-cache hotspot.

## Findings

### 1. The outer timeline is not virtualized

`crates/cowork/src/lib.rs`, `Cowork::render_main_editor`, clones the timeline and
constructs every message inside one scrolling flex column. Clipping does not
prevent those offscreen rows from taking part in layout/prepaint/CPU paint.
Unchanged Markdown is parsed already, but its block element tree is reconstructed
during layout requests. The clone is measurable but much smaller than the
observed frame cost; eliminating it alone would not resolve this slowdown.
(Rendering no longer clones the timeline; see "Implemented fix".)

### 2. Selection bookkeeping repeats whole-window work per participant

In installed `gpui-base-0.7.1/src/text_selection.rs`:

- `WindowSelectionState::register_participant` prunes all participants, inserts
  one registration, and calls `publish_snapshots`.
- `publish_snapshots` prunes the whole map again, then upgrades/updates every
  participant, even when its selection snapshot is unchanged.
- `copy_items` scans all participants even if nothing is selected.

`SelectableText::prepaint` registers each user prompt. `TextView::paint` registers
each selectable agent reply. `SelectableText::paint` also calls the whole-window
`TextSelection::selected_text` query twice per prompt paint.

Since the participant map persists across frames, registering N participants
per warm frame incurs approximately O(N²) participant visits. Weak upgrades,
strong-handle drops, and entity updates repeat for each visit. The standalone
1000-short-message perf capture reported approximately **30.42% self time in
`AnyWeakEntity::upgrade` and 26.92% in `AnyEntity::drop`**, with selection prune,
snapshot-publication, and copy-query functions also appearing in the flat profile.

### 3. Retained Markdown layouts have a repeated full-map sweep

In installed `gpui-base-0.7.1/src/text/inline.rs`, `retain_layout` does:

```rust
if layouts.len() >= RETAINED_SWEEP_AT { // 4096
    layouts.retain(|_, retained| retained.state.strong_count() > 0);
}
layouts.insert(state_key(state), retained);
```

Once there are over 4096 *live* retained layouts, cleanup cannot bring the map
below the trigger. Each insertion scans the table again. Unchanged Inline
layouts are removed/reinserted during normal request-layout/prepaint/paint, so
warm cache hits still incur this maintenance cost. With N live layouts this
becomes roughly O(N²), and HashMap capacity can further affect scan cost.

A structured reply contains many paragraph/code/table-cell states. Inline-code
paragraphs also create persistent InlineFlow fragment states. At 500 such replies
the fixture comfortably exceeds 4096 entries without relying on previous cases.
The standalone 1000-Markdown-message perf capture reported **67.51% self time in
`Inline::retain_styled_text`**, consistent with the cleanup scan inlined there.
This is source-backed attribution, not an instruction-level counter of sweeps.
The profile covers setup/warmup and the finite test, not measured frames alone.

## Fix brainstorm and recommended order

The app-level culling fix above is implemented. The table records the original
brainstorm; dependency-internal fixes remain upstream opportunities, not local
patches or vendored/forked dependencies.

| Priority | Change | Why / tradeoff |
| --- | --- | --- |
| 1 | Amortize retained-layout dead-entry cleanup in GPUI Base | Directly sampled hotspot. Sweep once per frame, after a bounded number of operations, or on an amortized growth watermark—not every insertion above 4096. Preserve weak-state/address-reuse checks and bound dead-entry retention. An upstream fix only; no vendoring, forks, or registry-source edits. |
| 1 | Batch selection housekeeping/publication once per frame | Directly sampled hotspot. Register geometry incrementally, prune/publish once, and avoid whole-window copy queries when selection is unchanged. Selection hit testing/copy must still observe current geometry; blindly delaying all state changes can make interaction incorrect. |
| Implemented | Cull stable variable-height timeline bodies at app level | Stops revisiting the entire history during generation. Preserve cross-row selection/copy, UTF-8 byte-offset comment anchors, focused editors, expanded work, and stable bottom-follow. Render-generation-based segment-cache eviction must not destroy offscreen state. Larger app-level change, but addresses ordinary linear work as well as the quadratic hotspots. |
| 2 | Isolate historical rows and live animation into cached views | An alternative/intermediate step: unchanged historical rows should not reconstruct Markdown trees when the active reply or shimmer changes. Cache invalidation must include width/theme/selection/comments; caching is not automatically equivalent to virtualization. Measure animation-only stacks before deciding how much to suppress it. |
| 3 | Separate authoritative event ingestion from presentation updates | Apply every ordered event, but update preview views/notify/follow at a bounded presentation cadence. Flush approvals, errors, stop, part/turn/run ends promptly. Notifications already coalesce, and event ingestion is currently small; throttling notify alone will not cure expensive individual frames. |
| 3 | Avoid redundant streamed-response parsing and growing-prefix copies | Production updates both a text step's Markdown state and the separate response state, although that step view is unused for the response. Create hidden work views lazily or share appropriate state. Incremental preview updates must still defer to the committed reply's content. Most relevant to long replies; not established as dominant in these captures. |
| 4 | Bound queues and improve fairness/backpressure | Queues visibly build behind long frames. Do not drop arbitrary agent events; a bounded queue requires adapting the synchronous emit callback or another lossless strategy, with cancellation/approval behavior designed explicitly. Bounding the queue alone does not fix the redraw bottleneck. |

### Source-backed streaming costs to investigate after the measured hot paths

- There is **no replay of committed conversation history per token**.
  `ReplyPreview::apply` appends only the new preview. However,
  `ReplyPreview::partial` clones the current partial reply, and
  `AgentMessageUi::advance` rewinds/reconstructs its changing output and
  response string. Long growing replies can incur substantial cumulative
  prefix-copy work.
- Committed entries are serialized to `protocol::Json` and decoded on the local
  authoritative path too. `pending_events` accumulates a reply's previews until
  it is committed. Rendering no longer copies the timeline, so these only
  reach rows that are built.
- GPUI Base appends coalesce queued parser updates (up to 64 per parse) and
  normally reparse the **last Markdown block**, not the whole document. A huge
  paragraph/open fence/list/table can still make this expensive; document source
  concatenation also copies growing text. Background parsing is not eliminated
  by throttling redraw notifications.
- `Cowork::thread_updated` requests bottom-follow and root notification on every
  active-thread event. `scroll_to_bottom` sets a flag; it does not synchronously
  perform layout per token. Parsing later changes height, so bottom anchoring
  should be checked after parser commits when designing batching/virtualization.

Relevant source: `generation.rs::agent_event/thread_updated`,
`transcript.rs::apply_agent_event/ReplyPreview::partial`,
`timeline.rs::AgentMessageUi::advance/show_tail`, and installed GPUI Base's
`text/state.rs::parse_content`, `text/inline.rs::retain_layout`, and
`text_selection.rs::register_participant/publish_snapshots/copy_items`.

### Limits and cleanup

The static benchmark measures warm completed threads at the top of the viewport;
its times exclude live generation, cold parsing and GPU rendering. The live
profile uses real GPU submission, parser tasks and bottom-follow, but only the
center-stage fixture, not normal app startup/sidebar/presence scheduling. It
excludes peers, attachment transfer, comment/tool histories, real user scrolling,
and long model responses. Synthetic history is not sent to Ollama. Repeated text
favors shared shaping caches. Neither approach measures GPU completion or
compositor latency. There are no timing assertions.

The profiling tools remain opt-in ignored tests and a standalone finite-lifetime
supervisor; production code has no timing instrumentation. The app-level culling
fix and ordinary regression tests are also included. Obsolete headless live
captures were removed; relevant live perf data/logs remain ignored under
`target/` for further inspection. No dependency code, user statistics, schedules,
or saved threads were modified. Every owned server and profile window was
stopped.
