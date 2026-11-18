---
name: ui-screenshots
description: Render Cowork UI states to PNG files offscreen through GPUI, without driving the desktop. Use to see or show what a screen looks like, to check a UI change visually, or to add a scene for a state that is hard to reach by hand.
---

# UI screenshots

Cowork renders app states to PNGs from an ignored test, `ui_screenshots` in
`crates/cowork/src/tests/screenshots.rs`. It builds the real app view in
GPUI's headless app and draws each frame with GPUI's wgpu renderer: no
window appears and no display is needed, on Windows, macOS or Linux. Use it
instead of launching the app and capturing or clicking on someone's screen.

## Run

```sh
COWORK_SCREENSHOTS=opening COWORK_SCREENSHOTS_DIR="$DELTA_SCRATCH_DIR/ui" \
  cargo test -p cowork ui_screenshots -- --ignored --nocapture --test-threads=1
```

- `COWORK_SCREENSHOTS` picks scenes whose name contains it; omit it to run
  them all.
- PNGs are the app's default window, `DEFAULT_WINDOW_SIZE` (1200×760
  logical pixels), at scale 1. `COWORK_SCREENSHOTS_SIZE=1440x900` changes
  the logical size and `COWORK_SCREENSHOTS_SCALE=2` the scale factor, for
  checking narrow windows or high-DPI detail.
- `COWORK_SCREENSHOTS_DIR` is where PNGs go, one folder per scene; the default
  is `target/screenshots`. Writing under `$DELTA_SCRATCH_DIR` lets you open
  each image with `read_file` as `scratch://ui/<scene>/<shot>.png`. Copy any
  you want to keep or show elsewhere, since scratch is cleared after the turn.
- Each capture prints `captured <path>`. A failed scene fails the test with
  its error.
- On Windows the path in `COWORK_SCREENSHOTS_DIR` must be a Windows path
  (`cygpath -w` in Git Bash).

Look at every image you report on. Name what you checked, such as alignment
with neighbouring elements or text that is cut off, rather than only saying
it rendered.

## Add a scene

1. Write `crates/cowork/src/tests/screenshots/<area>.rs` with a
   `pub(super) fn scene(context: &SceneContext) -> anyhow::Result<()>`.
   `opening.rs` is the model to copy.
2. Register it in `SCENES` in `screenshots.rs`, with `mod <area>;`.
3. In the scene:
   - `context.open(|window, cx| (store, active_id))?` opens the window on a
     `ThreadStore` you build, showing that chat or a new one. Fill it with
     the fixtures in `tests/mod.rs` and `test_support.rs` (`test_thread`,
     `ScriptedHost`, `ManualLoader` to hold or fail loads).
   - `stage.update(|cowork, window, cx| ...)?` changes state through the
     app's own methods (`open_thread`, `open_settings`, ...), as its handlers
     would, then runs the work that follows.
   - `stage.advance(duration)` moves GPUI's simulated clock on, firing the
     timers it passes; use the app's own constants (`PLACEHOLDER_DELAY`).
   - `stage.settle()` runs work a fixture started from outside the app, such
     as `loader.finish(id)`.
   - `stage.capture("2-name")?` saves a frame; number shots so they sort in
     order.

Keep a scene in the repository when its state takes setup to reach, so the
next change can be checked the same way. A scene that only shows the default
window adds nothing.

## Limits

- Text goes through GPUI's cosmic-text system with the platform's UI font,
  not the app's own text system (DirectWrite on Windows, Core Text on
  macOS), so glyphs and wrapping can differ slightly from the running app.
  Don't judge exact line breaks or font rendering from a screenshot.
- wgpu needs a GPU adapter. Without one, `WgpuHeadlessRenderer::new` fails
  and the scene reports it.
- Time is simulated, so animations are drawn at their first frame.
- GPUI's test window is fixed at scale 2, so other scales are resized from
  a 2x frame rather than laid out at that scale. Hairlines and hinting can
  differ slightly from a real 1x display.
- There is no element lookup or pointer input yet. Reach states through
  `Cowork` methods, not clicks.
- Scenes show, they don't assert. Cover behaviour with a `#[gpui::test]`
  as `AGENTS.md` describes, and use scenes to see it.
