//! Renders app states to PNGs, for looking at UI without driving a
//! desktop; how to run and extend it is in the `ui-screenshots` skill
//! (`.agents/skills/ui-screenshots/SKILL.md`).
//!
//! Each scene is a short script over a real `Cowork` in its own window:
//! set up the store, change state, advance time, capture. The window is
//! GPUI's headless one: never shown, drawn by GPUI's wgpu renderer into an
//! image, which works on every desktop platform and needs no display. Text
//! goes through GPUI's cosmic-text system on every platform too, since only
//! its glyphs reach the wgpu renderer (DirectWrite's are for DirectX), with
//! the platform's UI font standing in for the system font; wrapping can
//! differ slightly from the app's own text system.
//!
//! Time is GPUI's simulated test clock, so timers fire exactly when a
//! scene advances past them, and a capture never depends on how fast the
//! machine is. Animations are drawn at their first frame.
//!
//! Scenes render at the app's own [`DEFAULT_WINDOW_SIZE`] and scale factor
//! 1, so a PNG is what the app first opens as on a standard display;
//! `COWORK_SCREENSHOTS_SIZE` and `COWORK_SCREENSHOTS_SCALE` change that
//! ([`Resolution::from_env`]). GPUI's test window is fixed at scale 2, so
//! frames are drawn at 2x and resized to the scale asked for.

mod comments;
mod opening;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context as _;
use gpui::{
    App, AppContext as _, Entity, HeadlessAppContext, Pixels, Size, Window, WindowHandle, px, size,
};
use gpui_component::Root;
use uuid::Uuid;

use super::test_cowork;
use crate::{Cowork, DEFAULT_WINDOW_SIZE, MainStage, assets::Assets, thread::ThreadStore};

type Scene = fn(&SceneContext) -> anyhow::Result<()>;

/// Every scene, by the name its PNGs are filed under and
/// `COWORK_SCREENSHOTS` selects it by.
const SCENES: &[(&str, Scene)] = &[("comments", comments::scene), ("opening", opening::scene)];

/// What scenes render at: the window's logical size and the scale factor
/// of the PNG.
#[derive(Clone, Copy, Debug)]
struct Resolution {
    size: Size<Pixels>,
    scale: f32,
}

impl Resolution {
    /// The app's default window at scale 1, unless `COWORK_SCREENSHOTS_SIZE`
    /// (`<width>x<height>` in logical pixels) or `COWORK_SCREENSHOTS_SCALE`
    /// say otherwise.
    fn from_env() -> anyhow::Result<Self> {
        let size = match std::env::var("COWORK_SCREENSHOTS_SIZE") {
            Ok(value) => {
                let (width, height) = value
                    .split_once('x')
                    .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
                    .filter(|&(width, height): &(f32, f32)| width > 0. && height > 0.)
                    .with_context(|| {
                        format!("COWORK_SCREENSHOTS_SIZE={value:?} is not <width>x<height>")
                    })?;
                size(px(width), px(height))
            }
            Err(_) => DEFAULT_WINDOW_SIZE,
        };
        let scale = match std::env::var("COWORK_SCREENSHOTS_SCALE") {
            Ok(value) => value
                .parse()
                .ok()
                .filter(|scale: &f32| *scale > 0.)
                .with_context(|| {
                    format!("COWORK_SCREENSHOTS_SCALE={value:?} is not a positive number")
                })?,
            Err(_) => 1.,
        };
        Ok(Self { size, scale })
    }

    fn pixels(self) -> (u32, u32) {
        let pixels = |length: Pixels| (f32::from(length) * self.scale).round() as u32;
        (pixels(self.size.width), pixels(self.size.height))
    }
}

/// What the app's system font is drawn with: the platform's UI font, or on
/// Linux what GPUI's Linux platform uses.
const SYSTEM_FONT: &str = cfg_select! {
    target_os = "windows" => "Segoe UI",
    target_os = "macos" => "Helvetica Neue",
    _ => "IBM Plex Sans",
};

#[test]
#[ignore = "renders the UI to PNGs; see the ui-screenshots skill"]
fn ui_screenshots() {
    let filter = std::env::var("COWORK_SCREENSHOTS").ok();
    let dir = std::env::var_os("COWORK_SCREENSHOTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots")
        });
    let scenes: Vec<_> = SCENES
        .iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|filter| name.contains(filter)))
        .copied()
        .collect();
    assert!(!scenes.is_empty(), "no scene matches {filter:?}");
    let resolution = Resolution::from_env().expect("a valid resolution");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("scene runtime");
    // Made once: loading the system's fonts takes a while.
    let text_system: Arc<dyn gpui::PlatformTextSystem> =
        Arc::new(gpui_wgpu::CosmicTextSystem::new(SYSTEM_FONT));
    let mut failures = Vec::new();
    for (name, scene) in scenes {
        let context = SceneContext {
            dir: dir.join(name),
            tokio: runtime.handle().clone(),
            text_system: text_system.clone(),
            resolution,
        };
        if let Err(error) = scene(&context) {
            failures.push(format!("{name}: {error:#}"));
        }
    }
    assert!(
        failures.is_empty(),
        "scenes failed:\n{}",
        failures.join("\n")
    );
}

/// What the harness gives a scene: where its PNGs go, the runtime
/// `Cowork` runs agent work on, and the text system to lay text out with.
pub(super) struct SceneContext {
    dir: PathBuf,
    tokio: tokio::runtime::Handle,
    text_system: Arc<dyn gpui::PlatformTextSystem>,
    resolution: Resolution,
}

impl SceneContext {
    /// Opens the app's window, headless, on the store `build` returns,
    /// showing its active chat if it names one, else a new chat.
    pub(super) fn open(
        &self,
        build: impl FnOnce(&mut Window, &mut App) -> (Entity<ThreadStore>, Option<Uuid>),
    ) -> anyhow::Result<Stage> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let mut cx =
            HeadlessAppContext::with_platform(self.text_system.clone(), Arc::new(Assets), || {
                gpui_wgpu::WgpuHeadlessRenderer::new().map(|renderer| {
                    Some(Box::new(renderer) as Box<dyn gpui::PlatformHeadlessRenderer>)
                })
            });
        cx.update(crate::init_app);
        let tokio = self.tokio.clone();
        let mut cowork = None;
        let window = cx.open_window(self.resolution.size, |window, cx| {
            let (store, active) = build(window, cx);
            let entity = cx.new(|cx| {
                let mut cowork = test_cowork(store, active, tokio, window, cx);
                cowork.main_stage = MainStage::Thread;
                cowork
            });
            cowork = Some(entity.clone());
            cx.new(|cx| Root::new(entity, window, cx))
        })?;
        cx.run_until_parked();
        Ok(Stage {
            window,
            cowork: cowork.expect("built with the window"),
            dir: self.dir.clone(),
            resolution: self.resolution,
            cx,
        })
    }
}

/// A scene's window, to change and capture.
pub(super) struct Stage {
    window: WindowHandle<Root>,
    // Released before `cx`, which shuts the app down when dropped.
    cowork: Entity<Cowork>,
    dir: PathBuf,
    resolution: Resolution,
    cx: HeadlessAppContext,
}

impl Stage {
    /// Changes the app as its own handlers would, with its window, and
    /// runs the work that follows.
    pub(super) fn update<R>(
        &mut self,
        f: impl FnOnce(&mut Cowork, &mut Window, &mut gpui::Context<Cowork>) -> R,
    ) -> anyhow::Result<R> {
        let cowork = self.cowork.clone();
        let result = self.cx.update_window(self.window.into(), |_, window, cx| {
            cowork.update(cx, |cowork, cx| f(cowork, window, cx))
        })?;
        self.cx.run_until_parked();
        Ok(result)
    }

    /// Runs work started outside the app, such as a fixture finishing a
    /// load, until nothing is left to do.
    pub(super) fn settle(&mut self) {
        self.cx.run_until_parked();
    }

    /// Moves the simulated clock on, firing the timers it passes.
    pub(super) fn advance(&mut self, duration: Duration) {
        self.cx.advance_clock(duration);
        self.cx.run_until_parked();
    }

    /// Draws the window as it is now and saves it as `<name>.png` in the
    /// scene's folder, returning the path.
    pub(super) fn capture(&mut self, name: &str) -> anyhow::Result<PathBuf> {
        self.cx.run_until_parked();
        self.cx.update_window(self.window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })?;
        let mut image = self.cx.capture_screenshot(self.window.into())?;
        let (width, height) = self.resolution.pixels();
        if image.dimensions() != (width, height) {
            image = image::imageops::resize(
                &image,
                width,
                height,
                image::imageops::FilterType::Lanczos3,
            );
        }
        let path = self.dir.join(format!("{name}.png"));
        image
            .save(&path)
            .with_context(|| format!("saving {}", path.display()))?;
        println!("captured {}", path.display());
        Ok(path)
    }
}
