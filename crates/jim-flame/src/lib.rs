//! `jim-flame` — flame-graph viewer pane for jim-editor.
//!
//! Wraps the generic [`flame_bevy`] embedding (offscreen wgpu 29 render →
//! CPU pixel readback → `bevy::Image`) as a first-class jim-pane kind. The
//! rendered image is shown in a `Sprite` filling the pane's content area, so
//! it inherits jim-pane's per-pane `RenderLayers` clipping for free.
//!
//! Phase 1: register the `"flame"` pane kind, load a trace file (or a bundled
//! sample) into a `flame_core::Profile`, and paint it.
//!
//! Phase 2 (this file): live resize tracking (offscreen canvas follows the
//! pane), plus input routing through jim-pane's content events — drag to pan,
//! click to select/toggle tabs+tracks, hover to highlight, wheel to
//! zoom/scroll, and keyboard shortcuts when the pane owns the keyboard. The
//! renderer-call mapping mirrors `flame_bevy::forward_input`, but coordinates
//! come from jim-pane (content-local, logical) and are scaled to the physical
//! canvas — `forward_input` itself assumes `scale_factor == 1`, so we don't
//! use it. Theming, live sources, and bus control are later phases.

use std::collections::VecDeque;
use std::sync::Arc;

use bevy::input::gestures::PinchGesture;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::render::render_resource::Extent3d;
use bevy::sprite::Anchor;
use bevy::window::PrimaryWindow;
use serde_json::Value;

use flame_bevy::flame_core::{self, ProfileBuilder, TraceSource};
use flame_bevy::flame_render::{LayoutMode, MainTab, Renderer};
use flame_bevy::{forward_input, FlameGraph, FlameGraphPlugin};
use flame_core::Profile;

use jim_pane::{
    next_pane_z, pt_to_content_local, spawn_pane_from_registry, topmost_pane_at, FocusedPane,
    KeyboardOwner, PaneCapturesPinch, PaneContentDragged, PaneContentHovered, PaneContentPressed,
    PaneContentReleased, PaneKindMarker, PaneKindSpec, PaneRect, PaneRegistry, PaneTag, PaneTitle,
    PaneViewport, MARGIN, TITLE_H,
};
use jim_widget::{BusMessageObserved, PendingMsg, WidgetMsgBus};

/// Pinch-zoom sensitivity. Mirrors the canvas's `pinch_gain` so the flame
/// graph and the surrounding canvas zoom at a comparable rate.
const PINCH_GAIN: f32 = 3.5;

/// Stable kind id stored in snapshots and used as the registry key.
pub const PANE_KIND: &str = "flame";

/// Bundled trace shown when a flame pane is spawned without a `path`, or
/// when the configured path can't be read. Keeps the pane useful out of the
/// box and gives the screenshot path something to render.
const DEFAULT_SAMPLE: &[u8] = include_bytes!("../assets/sample.chrome.json");

/// Fallback panel size if the pane has no `PaneRect` yet at spawn time.
const FALLBACK_SIZE: Vec2 = Vec2::new(900.0, 500.0);

/// Adds the flame-graph pane kind plus the generic flame-bevy render/upload
/// systems. The app shell installs this via `app.add_plugins(FlamePlugin)`.
pub struct FlamePlugin;

impl Plugin for FlamePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FlameGraphPlugin)
            .init_resource::<TraceRecorder>()
            .init_resource::<RecorderSpawnQueue>()
            .add_systems(Startup, register_flame_kind)
            .add_systems(
                Update,
                (
                    // Keep the offscreen canvas in step with the pane size by
                    // resizing the target image (flame-bevy's source of truth)
                    // before its render/upload chain runs.
                    flame_track_resize.before(forward_input),
                    flame_follow_latest,
                    // Recorder: capture → bounded ring; viewer panes sync from it.
                    // The control UI is the `trace_recorder.ft` funct widget; it
                    // drives these over the bus.
                    recorder_capture,
                    flame_recorder_sync,
                    recorder_bus_input,
                    recorder_publish_status,
                    flame_on_hover,
                    flame_on_press,
                    flame_on_drag,
                    flame_on_release,
                    flame_on_wheel,
                    flame_on_pinch,
                    flame_on_keys,
                ),
            )
            // Exclusive: spawns a viewer pane on "Send to Flame".
            .add_systems(Update, recorder_do_spawn);
    }
}

// ---------- Trace recorder (separate, bounded circular buffer) ----------

/// Soft cap on buffered spans. The recorder is a *circular* buffer: once the
/// total exceeds this, the oldest frames are evicted. Keeps memory bounded no
/// matter how long recording runs — the viewer never holds unbounded history.
const RECORDER_SPAN_BUDGET: usize = 120_000;

/// The editor's self-profiler capture buffer. A single global recorder owns the
/// bounded history; flame panes are dumb viewers that pull a `Profile` snapshot
/// from it (see [`FlameRecorderView`]). Toggled + sent to a viewer from a
/// dedicated recorder pane — it is deliberately NOT part of the flame graph.
#[derive(Resource, Default)]
pub struct TraceRecorder {
    /// Whether new frames are being captured into the ring.
    pub recording: bool,
    /// Captured frames, oldest first. Evicted from the front when over budget.
    frames: VecDeque<CapturedFrame>,
    /// Running sum of `frames[*].spans.len()` for cheap budget checks.
    total_spans: usize,
    /// Last source-ring frame id pulled in, so we don't re-capture frames.
    last_captured: u64,
    /// Bumps whenever the buffer or recording state changes; viewers diff it.
    pub version: u64,
}

struct CapturedFrame {
    /// Frame start time (process-epoch ns) — drawn as a frame-boundary marker.
    start_ns: u64,
    spans: Vec<CapSpan>,
}

/// One captured span with an ABSOLUTE start time, so frames lay out
/// sequentially on the viewer's timeline. `display`/`category` are resolved at
/// capture time (the pane entity may be gone by the time we render).
struct CapSpan {
    thread: u64,
    depth: u16,
    start_ns: u64,
    dur_ns: u64,
    category: &'static str,
    display: String,
}

impl TraceRecorder {
    /// Begin capturing. Enables tracing and starts from the current frame (no
    /// backfill of pre-record history).
    pub fn start(&mut self) {
        if self.recording {
            return;
        }
        jim_pane::trace::set_enabled(true);
        self.last_captured = jim_pane::trace::current_frame();
        self.recording = true;
        self.version += 1;
    }

    /// Stop capturing. The buffer is kept so the viewer can be inspected.
    pub fn stop(&mut self) {
        if !self.recording {
            return;
        }
        self.recording = false;
        self.version += 1;
    }

    pub fn toggle(&mut self) {
        if self.recording {
            self.stop();
        } else {
            self.start();
        }
    }

    pub fn span_count(&self) -> usize {
        self.total_spans
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Frame-boundary timestamps (ns) for drawing markers in the viewer.
    fn frame_boundaries(&self) -> Vec<u64> {
        self.frames.iter().map(|f| f.start_ns).collect()
    }

    /// Build a `Profile` snapshot of the whole buffer (absolute timestamps so
    /// frames sit side-by-side in time). One track per thread.
    fn build_profile(&self) -> Profile {
        use flame_bevy::flame_core::{CategoryId, TrackId, TrackKind};
        let mut b = ProfileBuilder::new();
        let process = b.add_process(0, "jim");
        let mut tracks: std::collections::HashMap<u64, TrackId> = std::collections::HashMap::new();
        let mut cats: std::collections::HashMap<&str, CategoryId> = std::collections::HashMap::new();
        for frame in &self.frames {
            for s in &frame.spans {
                let track = match tracks.get(&s.thread) {
                    Some(t) => *t,
                    None => {
                        let label = format!("thread {}", s.thread);
                        let tid = b.add_thread(Some(process), s.thread as i64, &label);
                        let t = b.add_track(TrackKind::Thread(tid), &label, None);
                        tracks.insert(s.thread, t);
                        t
                    }
                };
                let category = match cats.get(s.category) {
                    Some(c) => *c,
                    None => {
                        let c = b.intern_category(s.category);
                        cats.insert(s.category, c);
                        c
                    }
                };
                let name_id = b.intern_string(&s.display);
                b.add_complete_slice(track, s.depth, s.start_ns, s.dur_ns, name_id, category, None);
            }
        }
        b.finish()
    }
}

/// Queue of "Send to Flame" requests (target project), drained by the
/// exclusive `recorder_do_spawn`.
#[derive(Resource, Default)]
struct RecorderSpawnQueue(Vec<Option<u64>>);

/// Per-pane bookkeeping for a flame graph. The heavy `FlameGraph` component
/// (renderer + wgpu state + target image) lives on the same entity; this
/// just tracks the child sprite and the source so we can re-snapshot it.
#[derive(Component)]
pub struct FlamePane {
    /// The `Sprite` child (under `content_root`) that displays the rendered
    /// image. Held so resize/teardown can find it without a query.
    pub sprite: Entity,
    /// Trace source path, if loaded from a file. `None` = bundled sample.
    pub source_path: Option<String>,
    /// Display scale factor the offscreen canvas is rendered at. flame-render
    /// sizes its UI in physical pixels (fonts/rows/tabs baked for ~2× retina),
    /// so we render the image at `logical × scale_factor` and show the sprite
    /// at logical size. Also the cursor→canvas multiplier for input.
    pub scale_factor: f32,
    /// Active timeline-drag anchor: last cursor in content-local logical px.
    /// `Some` only between a press that started a pan and its release.
    drag_last: Option<Vec2>,
    /// When true, the pane watches `~/.jim/traces` and live-reloads the newest
    /// dump as it appears (set by a `"latest"` / `"jim-latest"` source).
    follow: bool,
    /// Modification time of the currently-loaded trace file — used to detect a
    /// newer dump while following.
    loaded_mtime: Option<std::time::SystemTime>,
}

/// Marker on a flame pane that views the [`TraceRecorder`]: it pulls a fresh
/// `Profile` snapshot whenever the recorder's version changes (live-rolling
/// while recording, frozen when stopped). The pane holds only the current
/// snapshot — the bounded history lives in the recorder, not here.
#[derive(Component, Default)]
pub struct FlameRecorderView {
    /// Last recorder version applied, so we only rebuild on change.
    seen_version: u64,
}

/// Map a jim-pane content-local (logical) point to flame-render physical
/// canvas coordinates. Both spaces are top-left origin, y-down.
#[inline]
fn to_canvas(local: Vec2, scale_factor: f32) -> (f32, f32) {
    (local.x * scale_factor, local.y * scale_factor)
}

/// Display scale factor of the primary window (≈2.0 on retina, 1.0 otherwise).
fn primary_scale_factor(world: &mut World) -> f32 {
    world
        .query_filtered::<&Window, With<PrimaryWindow>>()
        .iter(world)
        .next()
        .map(|w| w.resolution.scale_factor())
        .unwrap_or(1.0)
        .max(1.0)
}

fn register_flame_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Flame Graph",
        // Three stacked bars ≈ a flame graph. ASCII-safe in SF Mono (the
        // chrome font), so it won't trip the color-font rasterizer panic.
        radial_icon: Some("≡"),
        default_size: Vec2::new(900.0, 520.0),
        spawn: flame_spawn_from_config,
        snapshot: flame_snapshot,
        on_close: None,
    });
}

fn flame_spawn_from_config(
    world: &mut World,
    entity: Entity,
    content_root: Entity,
    config: &Value,
) {
    let scale_factor = primary_scale_factor(world);
    let size = world
        .get::<PaneRect>(entity)
        .map(|r| r.size)
        .unwrap_or(FALLBACK_SIZE);
    // Logical content area (jim-pane coordinate space the sprite lives in).
    let content_w = (size.x - 2.0 * MARGIN).max(1.0);
    let content_h = (size.y - TITLE_H - 2.0 * MARGIN).max(1.0);
    // Physical canvas: flame-render's UI metrics assume ~2× density, so we
    // render at logical × scale_factor and let the sprite downscale to
    // logical — correct proportions AND crisp on retina.
    let px = (
        (content_w * scale_factor).round().max(1.0) as u32,
        (content_h * scale_factor).round().max(1.0) as u32,
    );

    let (source_path, follow, recorder_view) = resolve_trace_source(config);
    let loaded_mtime = source_path.as_deref().and_then(file_mtime);
    // Recorder-view panes start empty and are filled from the recorder on the
    // next sync; everything else loads its file (or bundled sample) up front.
    let profile = if recorder_view {
        ProfileBuilder::new().finish()
    } else {
        load_profile(source_path.as_deref())
    };

    let image = world
        .resource_mut::<Assets<Image>>()
        .add(FlameGraph::blank_image(px.0, px.1));
    let mut flame = FlameGraph::new(image.clone(), px);
    flame.set_profile(Arc::new(profile));

    let sprite = world
        .spawn((
            ChildOf(content_root),
            Sprite {
                image: image.clone(),
                custom_size: Some(Vec2::new(content_w, content_h)),
                ..default()
            },
            // Content-root origin is the content top-left; match the
            // terminal's convention so the panel fills the area.
            Anchor::TOP_LEFT,
            Transform::from_xyz(0.0, 0.0, 0.0),
            Visibility::Inherited,
        ))
        .id();

    world.entity_mut(entity).insert((
        flame,
        FlamePane {
            sprite,
            source_path,
            scale_factor,
            drag_last: None,
            follow,
            loaded_mtime,
        },
        // Capture trackpad pinch so the flame graph zooms instead of the
        // canvas while the cursor is over this pane.
        PaneCapturesPinch,
    ));
    if recorder_view {
        world.entity_mut(entity).insert(FlameRecorderView::default());
    }
}

fn flame_snapshot(world: &World, entity: Entity) -> Value {
    // Persist the mode, not the resolved file, so recorder/follow panes restore
    // as recorder/follow rather than pinned to whatever file was current.
    if world.get::<FlameRecorderView>(entity).is_some() {
        return serde_json::json!({ "source": "recorder" });
    }
    let pane = world.get::<FlamePane>(entity);
    if pane.map_or(false, |p| p.follow) {
        return serde_json::json!({ "path": "latest" });
    }
    match pane.and_then(|p| p.source_path.clone()) {
        Some(p) => serde_json::json!({ "path": p }),
        None => serde_json::json!({}),
    }
}

/// Resolve the trace source from a spawn config, returning
/// `(path, follow, recorder_view)`:
/// - `{"source": "recorder"}` → a viewer bound to the [`TraceRecorder`];
///   live-rolls from the bounded ring while recording. `path`/`follow` unused.
/// - `{"path": "latest"}` / `{"source": "jim-latest"}` → newest
///   `~/.jim/traces/*.json` dump, live-**follow**ed as new dumps land.
/// - `{"path": "/abs/file.json"}` → a static file.
///
/// The returned path is the resolved absolute file, which is what gets
/// snapshotted for restore.
fn resolve_trace_source(config: &Value) -> (Option<String>, bool, bool) {
    let source = config.get("source").and_then(|v| v.as_str());
    if source == Some("recorder") {
        return (None, false, true);
    }
    if let Some(p) = config.get("path").and_then(|v| v.as_str()) {
        if p == "latest" || p == "jim-latest" {
            return (newest_jim_trace(), true, false);
        }
        return (Some(p.to_string()), false, false);
    }
    if source == Some("jim-latest") {
        return (newest_jim_trace(), true, false);
    }
    (None, false, false)
}

/// Newest `*.json` under `~/.jim/traces`, by modification time. Quiet: it's
/// polled while following, so it must not log on every call.
fn newest_jim_trace() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::Path::new(&home).join(".jim").join("traces");
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if newest.as_ref().map_or(true, |(t, _)| modified > *t) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, p)| p.to_string_lossy().into_owned())
}

/// Modification time of a file, if readable.
fn file_mtime(path: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Read a trace file (or the bundled sample) and parse it into a `Profile`.
fn load_profile(path: Option<&str>) -> Profile {
    let (bytes, name): (Vec<u8>, Option<String>) = match path {
        Some(p) => match std::fs::read(p) {
            Ok(b) => (b, Some(p.to_string())),
            Err(e) => {
                log::warn!("jim-flame: can't read trace {p}: {e}; using bundled sample");
                (DEFAULT_SAMPLE.to_vec(), Some("sample.chrome.json".into()))
            }
        },
        None => (DEFAULT_SAMPLE.to_vec(), Some("sample.chrome.json".into())),
    };
    parse_profile(&bytes, name.as_deref())
}

/// Detect-then-load across the bundled format readers. Returns an empty
/// profile (not a panic) if nothing matches — a blank flame graph is a
/// clearer signal than a crash, and the warning names the file.
fn parse_profile(bytes: &[u8], name: Option<&str>) -> Profile {
    let sources: [&dyn TraceSource; 5] = [
        // jim's own dumps first — its detect is the most specific.
        &flame_format_jimtrace::JimTraceSource,
        &flame_format_chrome::ChromeSource,
        &flame_format_speedscope::SpeedscopeSource,
        &flame_format_firefox::FirefoxSource,
        &flame_format_folded::FoldedSource,
    ];
    let mut builder = ProfileBuilder::new();
    for src in sources {
        if src.detect(bytes, name) {
            match src.load(bytes, &mut builder) {
                Ok(()) => return builder.finish(),
                Err(e) => log::warn!("jim-flame: {} failed to load: {e}", src.name()),
            }
        }
    }
    log::error!(
        "jim-flame: no trace format matched {:?}; rendering empty profile",
        name.unwrap_or("<bytes>")
    );
    builder.finish()
}

// ---------- Resize ----------

/// Keep each flame pane's offscreen canvas matched to its content area. We
/// resize the target `Image` (flame-bevy's source of truth) to the physical
/// size and the display `Sprite` to the logical size; flame-bevy's
/// `resize_image_to_panel` then resizes the renderer to match on the same
/// frame (this runs `.before(forward_input)`), and `render_and_upload`
/// repaints. Driven by `Changed<PaneRect>`, so idle panes cost nothing.
fn flame_track_resize(
    panes: Query<(&PaneRect, &FlamePane, &FlameGraph), Changed<PaneRect>>,
    mut sprites: Query<&mut Sprite>,
    mut images: ResMut<Assets<Image>>,
) {
    for (rect, pane, flame) in &panes {
        let content_w = (rect.size.x - 2.0 * MARGIN).max(1.0);
        let content_h = (rect.size.y - TITLE_H - 2.0 * MARGIN).max(1.0);
        if let Ok(mut s) = sprites.get_mut(pane.sprite) {
            s.custom_size = Some(Vec2::new(content_w, content_h));
        }
        let pw = (content_w * pane.scale_factor).round().max(1.0) as u32;
        let ph = (content_h * pane.scale_factor).round().max(1.0) as u32;
        if let Some(mut img) = images.get_mut(&flame.image()) {
            let cur = img.texture_descriptor.size;
            if cur.width != pw || cur.height != ph {
                img.texture_descriptor.size = Extent3d {
                    width: pw,
                    height: ph,
                    depth_or_array_layers: 1,
                };
                // Keep data consistent with the descriptor so Bevy's image
                // upload never sees a size/length mismatch in the gap before
                // render_and_upload refills it.
                img.data = Some(vec![0; (pw * ph * 4) as usize]);
            }
        }
    }
}

// ---------- Live follow ----------

/// For panes in follow mode, poll `~/.jim/traces` and live-reload the newest
/// dump when a newer one appears. Uses `set_profile_live`, which preserves the
/// viewport (zoom/pan) across updates, so watching doesn't snap the view back.
///
/// Polled on a ~250ms timer rather than every frame; the read_dir is cheap and
/// only triggers a redraw when the newest file actually changes, so an idle
/// editor (no new slow frames) costs nothing. We don't force continuous
/// ticking — new dumps only happen while the app is already busy.
fn flame_follow_latest(
    time: Res<Time>,
    mut accum: Local<f32>,
    mut q: Query<(&mut FlamePane, &mut FlameGraph)>,
) {
    *accum += time.delta_secs();
    if *accum < 0.25 {
        return;
    }
    *accum = 0.0;

    for (mut pane, mut flame) in &mut q {
        if !pane.follow {
            continue;
        }
        let Some(newest) = newest_jim_trace() else {
            continue;
        };
        let mtime = file_mtime(&newest);
        let is_new = pane.source_path.as_deref() != Some(newest.as_str())
            || (mtime.is_some() && mtime != pane.loaded_mtime);
        if !is_new {
            continue;
        }
        let profile = load_profile(Some(&newest));
        let r = flame.renderer_mut();
        // Live variant keeps the current viewport/selection instead of fitting.
        r.set_profile_live(Arc::new(profile));
        r.rebuild_instances();
        flame.mark_dirty();
        log::info!(
            "jim-flame: live-reloaded {}",
            std::path::Path::new(&newest)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&newest)
        );
        pane.source_path = Some(newest);
        pane.loaded_mtime = mtime;
    }
}

// ---------- Recorder capture + viewer sync ----------

/// While recording, pull newly-completed frames from jim-pane's span ring into
/// the bounded recorder buffer (evicting oldest over budget) and bump the
/// version. Throttled to ~10Hz; cheap when idle (returns immediately when not
/// recording). Pane titles are resolved here, at capture time.
fn recorder_capture(
    time: Res<Time>,
    mut accum: Local<f32>,
    mut recorder: ResMut<TraceRecorder>,
    titles: Query<&PaneTitle>,
) {
    if !recorder.recording {
        return;
    }
    *accum += time.delta_secs();
    if *accum < 0.1 {
        return;
    }
    *accum = 0.0;

    // Frames [last_captured+1 ..= current-1] are complete; the current frame is
    // still being recorded. Catch up by id so we never miss frames between ticks.
    let upto = jim_pane::trace::current_frame().saturating_sub(1);
    let mut added = false;
    let mut f = recorder.last_captured + 1;
    while f <= upto {
        let ft = jim_pane::trace::collect_frame(f);
        if !ft.spans.is_empty() {
            let spans: Vec<CapSpan> = ft
                .spans
                .iter()
                .map(|s| CapSpan {
                    thread: s.thread,
                    depth: s.depth,
                    start_ns: s.start_ns,
                    dur_ns: s.dur_ns,
                    category: s.category,
                    display: if s.entity_bits != 0 {
                        titles
                            .get(Entity::from_bits(s.entity_bits))
                            .ok()
                            .map(|t| t.0.clone())
                            .unwrap_or_else(|| s.name.to_string())
                    } else {
                        s.name.to_string()
                    },
                })
                .collect();
            recorder.total_spans += spans.len();
            recorder.frames.push_back(CapturedFrame {
                start_ns: ft.frame_start_ns,
                spans,
            });
            added = true;
        }
        f += 1;
    }
    recorder.last_captured = upto;

    // Circular eviction: drop oldest frames until back under budget.
    while recorder.total_spans > RECORDER_SPAN_BUDGET {
        match recorder.frames.pop_front() {
            Some(old) => recorder.total_spans -= old.spans.len(),
            None => break,
        }
    }
    if added {
        recorder.version += 1;
    }
}

/// Viewer panes pull a fresh `Profile` snapshot from the recorder when its
/// version changes. While recording this rolls (fit each rebuild); once stopped
/// the version stops bumping so the view freezes and the user can zoom in. The
/// pane holds only this snapshot — bounded history stays in the recorder.
fn flame_recorder_sync(
    recorder: Res<TraceRecorder>,
    mut q: Query<(&mut FlameRecorderView, &mut FlameGraph)>,
) {
    for (mut view, mut flame) in &mut q {
        if view.seen_version == recorder.version {
            continue;
        }
        view.seen_version = recorder.version;
        let profile = Arc::new(recorder.build_profile());
        let markers = recorder.frame_boundaries();
        let r = flame.renderer_mut();
        // `set_profile_live` (not `set_profile`) is the rolling-update path: it
        // re-fits the growing time range each tick BUT preserves layout_mode
        // (AGGREGATED), merge_mode, direction, and selection across updates.
        // Plain `set_profile` resets layout_mode → AGGREGATED would flip back
        // every sync while recording.
        r.set_profile_live(profile);
        // Per-frame boundary markers across the recorded timeline.
        r.set_markers(markers);
        r.rebuild_instances();
        flame.mark_dirty();
    }
}

// ---------- Recorder bus bridge ----------
//
// The control UI is the `trace_recorder.ft` funct widget, not a native pane.
// It drives the recorder over the widget bus; these two systems are the native
// half: read button intents in, publish status out.

/// Topic the recorder widget emits on to toggle capture.
const TOPIC_RECORD: &str = "trace.record";
/// Topic the recorder widget emits on to open/link a flame viewer pane.
const TOPIC_SEND: &str = "trace.send";
/// Topic the native side publishes status on (retained, global).
const TOPIC_STATUS: &str = "trace.status";

/// Read recorder-control intents from the bus (emitted by the funct widget):
/// `trace.record` toggles capture; `trace.send` queues a viewer spawn.
fn recorder_bus_input(
    mut ev: MessageReader<BusMessageObserved>,
    mut recorder: ResMut<TraceRecorder>,
    mut queue: ResMut<RecorderSpawnQueue>,
) {
    for m in ev.read() {
        match m.topic.as_str() {
            TOPIC_RECORD => recorder.toggle(),
            TOPIC_SEND => queue.0.push(m.project),
            _ => {}
        }
    }
}

/// Publish recorder status back to the widget(s) as a retained, global bus
/// message so the UI reflects recording state + buffer fill, and a freshly
/// spawned widget picks up the current state on init.
fn recorder_publish_status(recorder: Res<TraceRecorder>, mut bus: ResMut<WidgetMsgBus>) {
    if !recorder.is_changed() {
        return;
    }
    bus.push_external(PendingMsg {
        project: None,
        topic: TOPIC_STATUS.to_string(),
        payload: serde_json::json!({
            "recording": recorder.recording,
            "frames": recorder.frame_count(),
            "spans": recorder.span_count(),
        }),
        sender: "trace-recorder".to_string(),
        retain: true,
    });
}

/// Exclusive: drain "Send to Flame" requests. Reuses an existing recorder-view
/// pane if one is already open; otherwise spawns one in the requesting project.
fn recorder_do_spawn(world: &mut World) {
    let reqs = std::mem::take(&mut world.resource_mut::<RecorderSpawnQueue>().0);
    if reqs.is_empty() {
        return;
    }
    let mut q = world.query_filtered::<Entity, With<FlameRecorderView>>();
    let mut have_viewer = q.iter(world).next().is_some();
    for project in reqs {
        // At most one viewer pane — reuse the existing one (it already live-syncs).
        if have_viewer {
            continue;
        }
        let z = next_pane_z(world);
        let rect = PaneRect {
            pos: Vec2::new(200.0, 160.0),
            size: Vec2::new(1100.0, 600.0),
            z,
        };
        spawn_pane_from_registry(
            world,
            PANE_KIND,
            "Flame Graph",
            rect,
            project,
            &serde_json::json!({ "source": "recorder" }),
        );
        have_viewer = true;
    }
}

// ---------- Input ----------

/// Cursor hover → highlight the slice under it.
fn flame_on_hover(
    mut ev: MessageReader<PaneContentHovered>,
    mut q: Query<(&FlamePane, &mut FlameGraph)>,
) {
    for h in ev.read() {
        let Ok((pane, mut flame)) = q.get_mut(h.pane) else {
            continue;
        };
        // Hover-leave arrives as a sentinel local_pt of (inf, inf); guard
        // FIRST so we never feed a non-finite coord into the hit-test.
        if !h.local_pt.is_finite() {
            let r = flame.renderer_mut();
            if r.hovered.is_some() {
                r.set_hover(None);
                flame.mark_dirty();
            }
            continue;
        }
        let (cx, cy) = to_canvas(h.local_pt, pane.scale_factor);
        let r = flame.renderer_mut();
        let hit = r.hit_test(cx, cy);
        let prev = r.hovered;
        r.set_hover(hit);
        if r.hovered != prev {
            flame.mark_dirty();
        }
    }
}

/// Left press → tab/track/slice hit-tests; arm a pan drag on an empty hit.
fn flame_on_press(
    mut ev: MessageReader<PaneContentPressed>,
    mut q: Query<(&mut FlamePane, &mut FlameGraph)>,
) {
    for p in ev.read() {
        let Ok((mut pane, mut flame)) = q.get_mut(p.pane) else {
            continue;
        };
        let (cx, cy) = to_canvas(p.local_pt, pane.scale_factor);
        let started_drag = apply_press(flame.renderer_mut(), cx, cy);
        pane.drag_last = started_drag.then_some(p.local_pt);
        flame.mark_dirty();
    }
}

/// Drag while panning → translate the viewport by the cursor delta.
fn flame_on_drag(
    mut ev: MessageReader<PaneContentDragged>,
    mut q: Query<(&mut FlamePane, &mut FlameGraph)>,
) {
    for d in ev.read() {
        let Ok((mut pane, mut flame)) = q.get_mut(d.pane) else {
            continue;
        };
        let Some(last) = pane.drag_last else {
            continue;
        };
        let delta = (d.local_pt - last) * pane.scale_factor;
        pane.drag_last = Some(d.local_pt);
        let r = flame.renderer_mut();
        if r.active_tab == MainTab::Sequence {
            r.pan_sequence(-delta.y);
        } else {
            r.viewport.pan_x_px(delta.x);
            r.viewport.pan_y_px(delta.y);
            r.clamp_viewport();
        }
        r.rebuild_instances();
        flame.mark_dirty();
    }
}

/// Release → end any active pan drag.
fn flame_on_release(mut ev: MessageReader<PaneContentReleased>, mut q: Query<&mut FlamePane>) {
    for r in ev.read() {
        if let Ok(mut pane) = q.get_mut(r.pane) {
            pane.drag_last = None;
        }
    }
}

/// Mouse wheel over a flame pane → zoom/scroll. Plain wheel is pane-local;
/// Cmd+wheel is canvas pan (same split the run-button pane uses), so we bail
/// when a Super key is down. Routes to the topmost flame pane under the cursor.
fn flame_on_wheel(
    mut wheel: MessageReader<MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    viewport: Res<PaneViewport>,
    panes_q: Query<(Entity, &PaneRect, Option<&Visibility>, &PaneKindMarker), With<PaneTag>>,
    mut flames: Query<(&FlamePane, &mut FlameGraph)>,
) {
    if keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight) {
        wheel.clear();
        return;
    }
    let mut dx = 0.0f32;
    let mut dy = 0.0f32;
    for ev in wheel.read() {
        let (ex, ey) = match ev.unit {
            MouseScrollUnit::Line => (ev.x * 30.0, ev.y * 30.0),
            MouseScrollUnit::Pixel => (ev.x, ev.y),
        };
        dx += ex;
        dy += ey;
    }
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(pt) = window.cursor_position() else {
        return;
    };
    let canvas_pt = viewport.window_to_canvas(pt);
    let rects: Vec<(Entity, PaneRect)> = panes_q
        .iter()
        .filter(|(_, _, vis, kind)| kind.0 == PANE_KIND && !matches!(vis, Some(Visibility::Hidden)))
        .map(|(e, r, _, _)| (e, *r))
        .collect();
    let Some(target) = topmost_pane_at(canvas_pt, &rects) else {
        return;
    };
    let Some(rect) = rects.iter().find(|(e, _)| *e == target).map(|(_, r)| *r) else {
        return;
    };
    let Ok((pane, mut flame)) = flames.get_mut(target) else {
        return;
    };
    let (cx, cy) = to_canvas(pt_to_content_local(canvas_pt, &rect), pane.scale_factor);
    apply_wheel(flame.renderer_mut(), cx, cy, dx, dy);
    flame.mark_dirty();
}

/// Trackpad pinch over a flame pane → zoom the flame graph around the cursor.
/// The canvas yields the pinch to us via the `PaneCapturesPinch` marker, so
/// only the flame graph zooms. Pinch-out (`total > 0`) zooms in.
fn flame_on_pinch(
    mut pinch: MessageReader<PinchGesture>,
    windows: Query<&Window, With<PrimaryWindow>>,
    viewport: Res<PaneViewport>,
    panes_q: Query<(Entity, &PaneRect, Option<&Visibility>, &PaneKindMarker), With<PaneTag>>,
    mut flames: Query<(&FlamePane, &mut FlameGraph)>,
) {
    let mut total = 0.0f32;
    for ev in pinch.read() {
        total += ev.0;
    }
    if total == 0.0 {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(pt) = window.cursor_position() else {
        return;
    };
    let canvas_pt = viewport.window_to_canvas(pt);
    let rects: Vec<(Entity, PaneRect)> = panes_q
        .iter()
        .filter(|(_, _, vis, kind)| kind.0 == PANE_KIND && !matches!(vis, Some(Visibility::Hidden)))
        .map(|(e, r, _, _)| (e, *r))
        .collect();
    let Some(target) = topmost_pane_at(canvas_pt, &rects) else {
        return;
    };
    let Some(rect) = rects.iter().find(|(e, _)| *e == target).map(|(_, r)| *r) else {
        return;
    };
    let Ok((pane, mut flame)) = flames.get_mut(target) else {
        return;
    };
    let (cx, cy) = to_canvas(pt_to_content_local(canvas_pt, &rect), pane.scale_factor);
    let r = flame.renderer_mut();
    if r.active_tab == MainTab::Sequence {
        // Sequence zoom: factor > 1 zooms in (matches the wheel mapping).
        let factor = (1.0 + total * PINCH_GAIN).clamp(0.25, 4.0);
        r.zoom_sequence(cy, factor);
    } else {
        // Timeline zoom_at: factor < 1 zooms in (ns-per-pixel multiplier).
        let factor = (1.0f64 / (1.0 + total as f64 * PINCH_GAIN as f64)).clamp(0.1, 10.0);
        r.viewport.zoom_at(cx, factor);
        r.clamp_viewport();
    }
    r.rebuild_instances();
    flame.mark_dirty();
}

/// Keyboard shortcuts (tabs 1-5, f flip, m merge, a/0/Home fit-all, +/- zoom,
/// arrows pan) — only when a flame pane owns the keyboard and no modifier
/// chord is held (so app/global chords keep working).
fn flame_on_keys(
    mut keyev: MessageReader<KeyboardInput>,
    focused: Res<FocusedPane>,
    owner: Res<KeyboardOwner>,
    keys: Res<ButtonInput<KeyCode>>,
    mut q: Query<&mut FlameGraph>,
) {
    let Some(pane) = focused.0 else {
        return;
    };
    if !owner.allows_pane(pane) {
        return;
    }
    if keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight)
        || keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight)
        || keys.pressed(KeyCode::AltLeft)
        || keys.pressed(KeyCode::AltRight)
    {
        return;
    }
    let Ok(mut flame) = q.get_mut(pane) else {
        return;
    };
    let mut dirty = false;
    for k in keyev.read() {
        if !matches!(k.state, ButtonState::Pressed) {
            continue;
        }
        if apply_key(flame.renderer_mut(), &k.logical_key) {
            dirty = true;
        }
    }
    if dirty {
        flame.mark_dirty();
    }
}

// ---------- Renderer-call mapping ----------
//
// Mirrors `flame_bevy::forward_input` / its `handle_left_press`, but operates
// on physical canvas coordinates supplied by the jim-pane systems above. Kept
// here (not in flame-bevy) because the click priority and key bindings are a
// host-policy decision; flame-render exposes only the individual hit-tests.

/// Route a left press at physical canvas coords. Returns true if the press
/// landed on empty timeline and a pan drag should begin.
fn apply_press(r: &mut Renderer, cx: f32, cy: f32) -> bool {
    if let Some(tab) = r.hit_test_inspector_tab(cx, cy) {
        r.set_tab(tab);
        r.rebuild_instances();
        false
    } else if r.active_tab == MainTab::CallTree {
        if let Some(node) = r.hit_test_call_tree(cx, cy) {
            r.toggle_tree_node(node);
            r.rebuild_instances();
        }
        false
    } else if let Some(mode) = r.hit_test_layout_button(cx, cy) {
        r.set_layout_mode(mode);
        r.rebuild_instances();
        false
    } else if let Some(tab) = r.hit_test_sidebar_tab(cx, cy) {
        r.set_sidebar_tab(tab);
        r.rebuild_instances();
        false
    } else if let Some(pick) = r.hit_test_group_row(cx, cy) {
        r.set_group_key(pick);
        r.rebuild_instances();
        false
    } else if let Some(track_id) = r.hit_test_track_header(cx, cy) {
        r.toggle_track_collapsed(track_id);
        r.rebuild_instances();
        false
    } else if r.cursor_in_inspector(cx) {
        if let Some(slice_idx) = r.hit_test_inspector(cx, cy) {
            r.select_slice(Some(slice_idx));
            if let Some(p) = &r.profile {
                let s = p.slices.start_ns[slice_idx as usize];
                let d = p.slices.dur_ns[slice_idx as usize];
                let mid = s as f64 + d as f64 * 0.5;
                r.viewport.start_ns =
                    mid - r.viewport.size_px.0 as f64 * r.viewport.ns_per_pixel * 0.5;
            }
            r.clamp_viewport();
            r.rebuild_instances();
        }
        false
    } else {
        let slice_idx = r.hit_test(cx, cy).and_then(|i| r.instance_to_slice(i));
        r.select_slice(slice_idx);
        r.rebuild_instances();
        true
    }
}

/// Route a wheel event (deltas already in px) at physical canvas coords.
fn apply_wheel(r: &mut Renderer, cx: f32, cy: f32, dx: f32, dy: f32) {
    match r.active_tab {
        MainTab::CallTree => {
            if dy != 0.0 {
                r.pan_call_tree(dy);
                r.rebuild_instances();
            }
        }
        MainTab::Sequence => {
            if dy != 0.0 {
                let factor = (1.0 + dy * 0.004).clamp(0.25, 4.0);
                r.zoom_sequence(cy, factor);
                r.rebuild_instances();
            }
        }
        MainTab::Flame if r.cursor_in_inspector(cx) => {
            if dy != 0.0 {
                r.pan_sidebar(-dy);
                r.rebuild_instances();
            }
        }
        _ => {
            if dx != 0.0 {
                r.viewport.pan_x_px(dx);
            }
            if dy != 0.0 {
                r.viewport.pan_y_px(dy);
            }
            r.clamp_viewport();
            r.rebuild_instances();
        }
    }
}

/// Route a single pressed key. Returns true if it was a flame shortcut.
fn apply_key(r: &mut Renderer, key: &Key) -> bool {
    let mut handled = true;
    match key {
        Key::Character(s) => match s.as_str() {
            "1" | "2" | "3" | "4" | "5" => {
                if let Ok(idx) = s.parse::<usize>() {
                    if let Some(&tab) = MainTab::ALL.get(idx.saturating_sub(1)) {
                        r.set_tab(tab);
                        r.rebuild_instances();
                    }
                }
            }
            "f" | "F" => {
                r.flip_direction();
                r.rebuild_instances();
            }
            "m" | "M" => {
                r.toggle_merge_mode();
                r.rebuild_instances();
            }
            // Toggle AGGREGATED (left-heavy) vs time-ordered layout.
            "g" | "G" => {
                let next = match r.layout_mode {
                    LayoutMode::LeftHeavy => LayoutMode::TimeOrdered,
                    _ => LayoutMode::LeftHeavy,
                };
                r.set_layout_mode(next);
                r.rebuild_instances();
            }
            "a" | "A" | "0" => {
                r.fit_all();
                r.rebuild_instances();
            }
            "+" | "=" => {
                if r.active_tab == MainTab::Sequence {
                    r.zoom_sequence(r.viewport.size_px.1 * 0.5, 1.5);
                } else {
                    r.viewport.zoom_at(r.viewport.size_px.0 * 0.5, 0.7);
                    r.clamp_viewport();
                }
                r.rebuild_instances();
            }
            "-" | "_" => {
                if r.active_tab == MainTab::Sequence {
                    r.zoom_sequence(r.viewport.size_px.1 * 0.5, 1.0 / 1.5);
                } else {
                    r.viewport.zoom_at(r.viewport.size_px.0 * 0.5, 1.43);
                    r.clamp_viewport();
                }
                r.rebuild_instances();
            }
            _ => handled = false,
        },
        Key::Home => {
            r.fit_all();
            r.rebuild_instances();
        }
        Key::ArrowLeft => {
            let pan = r.viewport.size_px.0 * 0.10;
            r.viewport.pan_x_px(pan);
            r.clamp_viewport();
            r.rebuild_instances();
        }
        Key::ArrowRight => {
            let pan = r.viewport.size_px.0 * 0.10;
            r.viewport.pan_x_px(-pan);
            r.clamp_viewport();
            r.rebuild_instances();
        }
        Key::ArrowUp => {
            r.viewport.pan_y_px(20.0);
            r.clamp_viewport();
            r.rebuild_instances();
        }
        Key::ArrowDown => {
            r.viewport.pan_y_px(-20.0);
            r.clamp_viewport();
            r.rebuild_instances();
        }
        _ => handled = false,
    }
    handled
}
