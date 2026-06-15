//! In-process funct-scripted widgets — script runs on a **worker
//! thread**, so a slow / busy / pathological script can never tank
//! the editor's framerate.
//!
//! # Architecture
//!
//! Each `script_widget` pane owns a `WorkerHandle` whose internals are:
//!   - A worker `JoinHandle` running the funct engine.
//!   - An mpsc channel `HostToWorker` for events sent from main →
//!     worker (Tick, Resize, Click, Drag, Release, Hover, Key,
//!     ClaudeEvent, Toggle, TabSelect, Input{Focus,Change,Submit},
//!     Reload, Shutdown). Each maps 1:1 to an optional script handler;
//!     see the handler table below.
//!
//! # Script handlers
//!
//! The top-level script body runs ONCE per AST load (init state, define
//! handlers). After that the host calls these optional functions:
//!
//! | Handler                          | Fired by                       |
//! |----------------------------------|--------------------------------|
//! | `on_init()`                      | during source eval (state init)|
//! | `on_start()`                     | every start, AFTER state set;  |
//! |                                  |   put SIDE EFFECTS here         |
//! | `render(w, h) -> Element`        | whenever a redraw is needed    |
//! | `on_click(x, y, shift, cmd, id)` | press on a Button / empty area |
//! | `on_toggle(id, checked)`         | `Element::Toggle` flipped      |
//! | `on_tab_select(id, tab)`         | `Element::Tabs` selection      |
//! | `on_input_change(id, value)`     | typing in a focused `Input`    |
//! | `on_input_submit(id, value)`     | Enter in a focused `Input`     |
//! | `on_input_focus(id, focused)`    | `Input` focus / blur           |
//! | `on_drag(x, y)` / `on_release`   | drag gesture                   |
//! | `on_hover(x, y)`                 | cursor move (x=inf on leave)   |
//! | `on_key(key)`                    | nav key, NO input focused      |
//! | `on_resize(w, h)`                | pane resized                   |
//! | `on_frame(dt)`                   | per frame, while animating     |
//! | `on_bus(kind, payload)`          | Claude Code bus event          |
//! | `on_message(topic, payload, snd)`| widget↔widget bus message      |
//!
//! `on_message` is the widget↔widget bus — sibling panes talking to each
//! other. Publish with `emit(topic, payload)` (or `emit_retained` to also
//! keep it as the topic's last value for late-joining panes). Delivery is
//! pushed (no `set_animating` polling) and scoped to the same editor
//! project. `snd` is the sender's id; call `my_id()` to recognise echoes
//! of your own emits. This is SEPARATE from the Claude `on_bus` channel.
//! See `crate::msgbus` and AUTHORING.md.
//!
//! IMPORTANT: `on_bus` is the Claude Code **event bus** (pre_tool_use,
//! stop, …), NOT UI events. UI interaction always arrives through the
//! specific `on_click` / `on_toggle` / `on_tab_select` / `on_input_*`
//! handlers above. (`on_bus` was historically named `on_event`, which
//! misled authors into expecting UI events there; the old name still
//! works as a fallback but is deprecated.)
//!
//! The host owns a focused `Input`'s live edit buffer + caret
//! (`WidgetInputFocus`), so typing echoes instantly without the script
//! round-tripping a frame; the script just reacts to `on_input_change` /
//! `on_input_submit`. This mirrors the subprocess NDJSON `HostEvent`
//! protocol in `protocol.rs` one-to-one.
//!   - A shared `Mutex<Option<Element>>` slot — the latest frame the
//!     worker has produced. Main thread reads it whenever it wants.
//!   - A shared `AtomicU64` `frame_gen` — bumped each time the worker
//!     writes a new frame. Main checks this to avoid relocking the
//!     mutex when nothing has changed.
//!   - A shared `Mutex<Value>` snapshot slot — what the worker last
//!     persisted; main reads from this when the host asks for a
//!     `PaneSnapshot.config`.
//!
//! The main thread never executes funct code. It just shuffles events
//! over a channel and reads frames out of a mutex. Worst case the
//! main thread sees a stale frame for one extra tick — it never
//! blocks waiting on the script.
//!
//! # Hot reload
//!
//! Parse on the main thread (cheap, microseconds for typical
//! scripts), then send the new AST over the channel. The worker
//! swaps it in on its next message dispatch and re-initializes its
//! scope from the last known snapshot. Same pattern as `Shutdown`.
//!
//! # Cleanup
//!
//! The pane's `on_close` callback sends `Shutdown` and despawns all
//! sprite entities the widget has been tracking. `Drop` on
//! `WorkerHandle` also sends `Shutdown` as a safety net so a panic-
//! despawned pane doesn't leak the worker thread.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use bevy::prelude::*;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use claude_bus_bevy::ClaudeBusEvent;
use jim_pane::{
    MARGIN, PaneChrome, PaneContentDragged, PaneContentHovered, PaneContentPressed,
    PaneContentReleased, PaneFont, PaneKindMarker, PaneKindSpec, PaneRect, PaneRegistry, PaneTitle,
    TITLE_H,
};

use crate::WidgetTargets;

use crate::protocol::{CanvasAnchor, CanvasItem, Element, ImageRef};
use crate::{
    WidgetClipDirty, WidgetImageCache, canvas_anchor_to_bevy, load_image_for_ref,
    parse_canvas_color,
};

pub const PANE_KIND: &str = "script_widget";

/// Frame cadence used **only when a widget has opted into animation**
/// via `set_animating(true)`. Idle widgets receive no Tick at all; the
/// main thread checks `WorkerSlots::animating` before sending one. So
/// this isn't a polling cadence, it's a max frame rate for the small
/// subset of widgets that are actively in motion.
const ANIMATION_MIN_FRAME_SECS: f32 = 1.0 / 30.0;

pub fn widgets_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".jim");
    p.push("widgets");
    Some(p)
}

// ============================================================
// Worker protocol
// ============================================================

pub(crate) enum HostToWorker {
    /// Animation frame. Only sent while the worker has set its
    /// `animating` flag — idle widgets get zero ticks. Drives
    /// `on_frame(dt)` in the script.
    Tick { dt_secs: f32 },
    /// Mouse press in the pane's content area. Drives `on_click(x, y,
    /// shift, cmd, id)` in the script.
    ///
    /// `button_id` is `Some(id)` when the click landed inside a
    /// `Button` element rendered by the previous frame; the host hit-
    /// tests against `WidgetTargets` (populated by `render::render`).
    /// Scripts that just want "which button did the user press" can
    /// read the `id` argument directly instead of doing their own
    /// y-range routing.
    Click {
        local_x: f32,
        local_y: f32,
        shift: bool,
        cmd: bool,
        button_id: Option<String>,
    },
    /// Cursor moved while the left button is held after a content
    /// press. Drives `on_drag(x, y)` in the script. Coords may sit
    /// outside the content rect — handlers like chess use that to
    /// know the user has dragged past the board edge.
    Drag { local_x: f32, local_y: f32 },
    /// Left button released after a content press. Drives
    /// `on_release(x, y)` in the script. Drag-and-drop widgets commit
    /// here; click-style widgets typically ignore (they've already
    /// acted on Click at press time).
    Release { local_x: f32, local_y: f32 },
    /// Cursor moved over the pane content area with no button held.
    /// Drives `on_hover(x, y)` in the script. `x = f32::INFINITY`
    /// signals the cursor LEFT the pane — widgets should clear any
    /// hover indicator on receipt.
    Hover { local_x: f32, local_y: f32 },
    /// Pane size changed. Drives `on_resize(w, h)` in the script and
    /// updates `canvas_w` / `canvas_h` in scope so `render` sees the
    /// new size.
    Resize { canvas_w: f32, canvas_h: f32 },
    /// A navigation key press routed to the focused widget. Drives
    /// `on_key(key)` in the script. `key` is a stable name like
    /// "ArrowLeft" / "ArrowRight" / "Home" / "End".
    Key { key: String },
    /// A Claude Code bus event. Drives `on_bus(kind, payload)` (legacy
    /// scripts may still name it `on_event` — see worker dispatch).
    ClaudeEvent { kind: String, payload: Value },
    /// User flipped an `Element::Toggle`. Drives `on_toggle(id, checked)`
    /// where `checked` is the NEW value (already computed host-side).
    Toggle { id: String, checked: bool },
    /// User picked a tab in an `Element::Tabs`. Drives
    /// `on_tab_select(id, tab)` — `id` is the tabs-group id, `tab` the
    /// selected `TabItem.id`.
    TabSelect { id: String, tab: String },
    /// User picked an option in an `Element::RadioGroup`. Drives
    /// `on_radio_select(id, option)`.
    RadioSelect { id: String, option: String },
    /// User stepped an `Element::Stepper`. Drives `on_number_change(id, value)`.
    NumberChange { id: String, value: f32 },
    /// User picked an option in an `Element::Select`. Drives
    /// `on_select_change(id, value)`.
    SelectChange { id: String, value: String },
    /// User dismissed an `Element::Dialog`. Drives `on_dialog_close(id)`.
    DialogClose { id: String },
    /// User dismissed an `Element::Toast`. Drives `on_toast_dismiss(id)`.
    ToastDismiss { id: String },
    /// User dragged an `Element::Slider`. Drives `on_slider_change(id, value)`
    /// with the new clamped/snapped value.
    SliderChange { id: String, value: f32 },
    /// An `Element::Input` gained or lost keyboard focus. Drives
    /// `on_input_focus(id, focused)`.
    InputFocus { id: String, focused: bool },
    /// User edited a focused `Element::Input`. Drives
    /// `on_input_change(id, value)` with the full new string. The host
    /// owns the live edit buffer + caret, so the script does NOT need to
    /// echo `value` back to keep typing responsive.
    InputChange { id: String, value: String },
    /// User submitted a focused `Element::Input` (Enter). Drives
    /// `on_input_submit(id, value)`.
    InputSubmit { id: String, value: String },
    /// A widget↔widget bus message delivered to this widget. Drives
    /// `on_message(topic, payload, sender)`. `sender` is the publishing
    /// widget's id (this widget's own id for an echo of its own emit, or
    /// `"tbmsg"` for the CLI). NOT the Claude bus — that's `ClaudeEvent`.
    Message {
        topic: String,
        payload: Value,
        sender: String,
    },
    /// One stdout line from a child spawned via `proc_spawn`, pushed by
    /// the subprocess reader thread. Drives `on_proc_output(handle, line)`
    /// — event-driven delivery so widgets don't poll `proc_read` from
    /// `on_frame`. `handle` is the `proc_spawn` id.
    ProcOutput { handle: i64, line: String },
    /// A child spawned via `proc_spawn` closed its stdout (exited).
    /// Drives `on_proc_exit(handle, code)` once. `code` is the process
    /// exit code, or -1 if it couldn't be determined (e.g. killed).
    ProcExit { handle: i64, code: i64 },
    /// The global theme (palette) changed. Forces the worker to re-run
    /// `render()` so canvas widgets that bake theme colors into their
    /// frame — e.g. the garden's `theme_get("canvas_bg")` sky — pick up
    /// the new values in real time. Flow widgets re-resolve the palette
    /// on the main thread and don't strictly need this, but it's a cheap
    /// no-op for them (the frame just comes out identical).
    Rerender,
    /// Hot reload — main read the new script source off disk; the worker
    /// parses/compiles it (engine-specific) and swaps it in, re-init'ing
    /// from the last snapshot. Carrying *source* (not a parsed AST) keeps
    /// this message engine-neutral so both the funct and funct workers ride
    /// the same channel.
    Reload { source: String },
    /// Hot-swap an imported *module* (e.g. `df`) that changed on disk into
    /// this widget's VM, then re-render. This is how a shared library edit
    /// (the chart helpers in `df.ft`) reaches widgets that imported it —
    /// `Reload` only re-evals a widget's OWN script and would reuse the
    /// cached module. No-op if this widget doesn't import the module.
    ReloadModule { name: String },
    /// Exit the worker loop. Sent by `on_close` and by `Drop`.
    Shutdown,
}

/// One outbound widget↔widget bus message the script published via
/// `emit` / `emit_retained`. The worker thread pushes these into
/// `WorkerSlots::outbox`; the main thread drains them each frame, tags
/// the sender + project, and fans them out (see `crate::msgbus`).
pub(crate) struct OutMsg {
    pub topic: String,
    pub payload: Value,
    pub retain: bool,
}

/// What main reads from the worker: the latest frame the script
/// produced, plus diagnostic state and the animation flag main checks
/// before deciding whether to send Tick.
#[derive(Clone)]
pub(crate) struct WorkerSlots {
    /// Latest fully-rendered frame. Worker overwrites; main clones.
    pub(crate) latest_frame: Arc<Mutex<Option<Element>>>,
    /// Latest snapshot the script published (for persistence).
    pub(crate) snapshot: Arc<Mutex<Value>>,
    /// Bumped each time `latest_frame` is replaced. Main compares
    /// against its last-applied value to skip redundant diffing.
    pub(crate) frame_gen: Arc<AtomicU64>,
    /// Last runtime error the worker encountered. Cleared on next
    /// successful run.
    pub(crate) last_error: Arc<Mutex<Option<String>>>,
    /// Set by the script via `set_animating(true)`. Main reads this
    /// each frame and only sends `Tick` if true. Idle widgets =
    /// zero script eval and zero CPU.
    pub(crate) animating: Arc<AtomicBool>,
    /// Set by the script via `request_render()`. Worker calls
    /// `render(canvas_w, canvas_h)` and publishes a frame whenever
    /// this is set after a handler completes, then clears it.
    pub(crate) render_dirty: Arc<AtomicBool>,
    /// Widget↔widget bus messages the script published via `emit` /
    /// `emit_retained`, awaiting pickup by the main thread.
    pub(crate) outbox: Arc<Mutex<Vec<OutMsg>>>,
    /// Does the loaded script define any pointer-interaction handler?
    /// Set by the worker after each (re)load — the main thread can't
    /// scan the engine-specific program, so the worker reports it here
    /// and the component mirrors it for pinned-widget click hot-zoning.
    pub(crate) wants_clicks: Arc<AtomicBool>,
    /// Does the loaded script define `on_hover`? Reported like
    /// `wants_clicks` so a pinned canvas widget that only hovers (a
    /// chart with tooltips, no `on_click`) still publishes a content
    /// hot-zone — otherwise pinned-pane hover hit-testing skips it.
    pub(crate) wants_hover: Arc<AtomicBool>,
}

impl WorkerSlots {
    fn new() -> Self {
        Self {
            latest_frame: Arc::new(Mutex::new(None)),
            snapshot: Arc::new(Mutex::new(Value::Null)),
            frame_gen: Arc::new(AtomicU64::new(0)),
            last_error: Arc::new(Mutex::new(None)),
            animating: Arc::new(AtomicBool::new(false)),
            render_dirty: Arc::new(AtomicBool::new(true)),
            outbox: Arc::new(Mutex::new(Vec::new())),
            wants_clicks: Arc::new(AtomicBool::new(false)),
            wants_hover: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Owned by the `ScriptWidget` component. Dropping it sends `Shutdown`
/// to the worker as a backstop in case `on_close` didn't run (e.g.
/// pane despawn took an unusual path).
pub struct WorkerHandle {
    tx: Sender<HostToWorker>,
    slots: WorkerSlots,
    _join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub(crate) fn send(&self, msg: HostToWorker) {
        let _ = self.tx.send(msg);
    }

    /// Take everything the script has published since the last drain.
    fn drain_outbox(&self) -> Vec<OutMsg> {
        self.slots
            .outbox
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(HostToWorker::Shutdown);
    }
}

/// Spawn a worker thread that runs the script engine. `initial_source`
/// is the script text read off disk (None if the file couldn't be read —
/// the worker comes up idle and a later `Reload` installs it).
/// `initial_state` carries the snapshot blob from
/// `PaneSnapshot.config.state` so widget state survives restarts.
///
/// The engine is chosen by `script_name`'s extension: `.ft` runs on the
/// funct VM (`funct_widget::funct_worker_main`), everything else on funct.
/// Both ride the same `HostToWorker` channel + `WorkerSlots`, so the
/// entire main-thread plugin (forwarding, frame application, persistence)
/// is engine-agnostic and untouched by the choice.
fn spawn_worker(
    initial_source: Option<String>,
    initial_state: Value,
    params: Value,
    script_name: String,
    widget_id: String,
) -> WorkerHandle {
    let (tx, rx) = mpsc::channel::<HostToWorker>();
    // The worker gets a clone of its own sender so the subprocess reader
    // threads can post `ProcOutput`/`ProcExit` straight onto the worker's
    // queue (waking it via the channel recv — no main-loop polling).
    let self_tx = tx.clone();
    let slots = WorkerSlots::new();
    let slots_for_thread = slots.clone();
    let join = thread::Builder::new()
        .name(format!("widget:{}", script_name))
        .spawn(move || {
            crate::funct_widget::funct_worker_main(
                rx,
                self_tx,
                slots_for_thread,
                initial_source,
                initial_state,
                params,
                widget_id,
            )
        })
        .expect("spawn widget worker thread");
    WorkerHandle {
        tx,
        slots,
        _join: Some(join),
    }
}

// ============================================================
// Component / per-pane state on the main thread
// ============================================================

#[derive(Component)]
pub struct ScriptWidget {
    pub script: String,
    pub script_path: PathBuf,
    /// Stable id for this widget on the widget↔widget bus. Used as the
    /// `sender` on messages it publishes and to dedupe retained backlog
    /// delivery. Derived from the pane entity at spawn.
    pub widget_id: String,
    pub handle: WorkerHandle,
    /// Per-instance params injected as the funct global `params`. Kept so
    /// the snapshot can round-trip them across restart.
    pub params: Value,
    /// Last frame generation we applied to the scene. Compared against
    /// `handle.slots.frame_gen` to skip diffing when nothing changed.
    pub applied_frame_gen: u64,
    /// Snapshot mirror used to populate `PaneSnapshot.config.state`.
    /// Updated whenever a new frame_gen comes in.
    pub last_state: Value,
    pub last_size: Vec2,
    pub last_tick_at: Option<std::time::Instant>,
    pub reload_gen: u32,
    pub applied_reload_gen: u32,
    /// Sprite id → entity. Lets us diff frames instead of
    /// despawn+respawn.
    pub sprite_entities: HashMap<String, Entity>,
    /// While an input/textarea is focused we re-render to show live
    /// keystrokes + the blinking caret. Re-rendering EVERY frame rebuilds
    /// the whole flow tree (expensive with a table), so we only re-render
    /// when this focus signature `(value, caret, caret_visible)` changes
    /// — i.e. on a keystroke or a blink toggle, not 60×/sec.
    pub last_focus_sig: Option<(String, usize, bool)>,
    /// True when the script defines an `on_click` handler — i.e. it's
    /// an interactive widget rather than ambient decoration. Used to
    /// treat a canvas widget's whole content as a hot-zone so its
    /// clicks route while pinned (canvas widgets self-route and publish
    /// no per-element `WidgetTargets`, so they'd otherwise be
    /// click-through when pinned). Recomputed on reload.
    pub wants_clicks: bool,
    /// True when the script defines `on_hover`. Same role as
    /// `wants_clicks` but for hover: lets a pinned chart (hover tooltips,
    /// no `on_click`) publish a content hot-zone so pinned-pane hover
    /// hit-testing reaches it. Recomputed on reload.
    pub wants_hover: bool,
    /// Set by `anim::tick_widget_anims` while a state transition is in
    /// flight, so the next `apply_latest_frames` pass re-renders with the
    /// advanced eased values even though the frame itself didn't change.
    pub force_render: bool,
}

impl ScriptWidget {
    /// Forward a slider value change to the worker (drives `on_slider_change`).
    pub fn send_slider_change(&self, id: String, value: f32) {
        self.handle.send(HostToWorker::SliderChange { id, value });
    }

    /// Forward a select change to the worker (drives `on_select_change`).
    pub fn send_select_change(&self, id: String, value: String) {
        self.handle.send(HostToWorker::SelectChange { id, value });
    }

    /// Forward an arbitrary routed `HostEvent` (used by the overlay/dialog
    /// router, where a body button can fire any click event). Maps the routable
    /// variants to their `HostToWorker` equivalents.
    pub fn send_host_event(&self, evt: &crate::protocol::HostEvent) {
        use crate::protocol::HostEvent as H;
        let msg = match evt {
            H::Click { id } => HostToWorker::Click {
                local_x: 0.0,
                local_y: 0.0,
                shift: false,
                cmd: false,
                button_id: Some(id.clone()),
            },
            H::DialogClose { id } => HostToWorker::DialogClose { id: id.clone() },
            H::ToastDismiss { id } => HostToWorker::ToastDismiss { id: id.clone() },
            H::Toggle { id, checked } => HostToWorker::Toggle {
                id: id.clone(),
                checked: *checked,
            },
            H::TabSelect { id, tab } => HostToWorker::TabSelect {
                id: id.clone(),
                tab: tab.clone(),
            },
            H::RadioSelect { id, option } => HostToWorker::RadioSelect {
                id: id.clone(),
                option: option.clone(),
            },
            H::NumberChange { id, value } => HostToWorker::NumberChange {
                id: id.clone(),
                value: *value,
            },
            H::SelectChange { id, value } => HostToWorker::SelectChange {
                id: id.clone(),
                value: value.clone(),
            },
            _ => return,
        };
        self.handle.send(msg);
    }

    /// True while the script has opted into per-frame animation via
    /// `set_animating(true)`. The host uses this to decide whether the
    /// app must stay in winit `Continuous` update mode — otherwise the
    /// reactive loop only wakes ~every 5s and `on_frame` (proc-polling,
    /// animation) lags badly while the window is idle.
    pub fn is_animating(&self) -> bool {
        self.handle
            .slots
            .animating
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Latest frame the worker produced, cloned out of the shared slot.
    /// Used host-side to seed an input's edit buffer on focus.
    pub fn latest_frame(&self) -> Option<Element> {
        self.handle
            .slots
            .latest_frame
            .lock()
            .ok()
            .and_then(|s| s.clone())
    }

    /// Forward a live input edit to the worker (`on_input_change`).
    pub fn send_input_change(&self, id: String, value: String) {
        self.handle.send(HostToWorker::InputChange { id, value });
    }

    /// Forward an input submit (Enter) to the worker (`on_input_submit`).
    pub fn send_input_submit(&self, id: String, value: String) {
        self.handle.send(HostToWorker::InputSubmit { id, value });
    }

    /// Forward an input focus/blur change to the worker
    /// (`on_input_focus`).
    pub fn send_input_focus(&self, id: String, focused: bool) {
        self.handle.send(HostToWorker::InputFocus { id, focused });
    }

    /// Take the widget↔widget bus messages this script published since
    /// the last drain. Called by the central bus pump (`crate::msgbus`).
    pub(crate) fn drain_bus_outbox(&self) -> Vec<OutMsg> {
        self.handle.drain_outbox()
    }

    /// Deliver a widget↔widget bus message to this worker
    /// (`on_message(topic, payload, sender)`).
    pub(crate) fn deliver_bus_message(&self, topic: String, payload: Value, sender: String) {
        self.handle.send(HostToWorker::Message {
            topic,
            payload,
            sender,
        });
    }
}

#[derive(Resource)]
struct ScriptWatcher {
    rx: Mutex<Receiver<PathBuf>>,
    _watcher: RecommendedWatcher,
}

pub struct ScriptWidgetPlugin;

impl Plugin for ScriptWidgetPlugin {
    fn build(&self, app: &mut App) {
        // Idempotent: WidgetPlugin also inits + ticks the store; this keeps
        // funct-only hosts working.
        app.init_resource::<crate::anim::WidgetAnim>()
            .add_systems(Startup, (register_kind, setup_watcher))
            .add_systems(
                Update,
                (
                    poll_watcher,
                    apply_reloads,
                    forward_clicks_to_workers,
                    forward_drags_to_workers,
                    forward_releases_to_workers,
                    forward_hovers_to_workers,
                    forward_keys_to_workers,
                    forward_inputs_to_workers,
                    apply_latest_frames,
                )
                    .chain(),
            );
        if std::env::var_os("WIDGET_LAYER_DEBUG").is_some() {
            // Observe layer state exactly where Bevy decides which camera
            // draws each entity — i.e. right before CheckVisibility, after
            // pane-layer propagation. WRONG_LAYER here == a real leak.
            app.add_systems(
                bevy::app::PostUpdate,
                debug_widget_layers
                    .after(jim_pane::camera::propagate_render_layers)
                    .before(bevy::camera::visibility::VisibilitySystems::CheckVisibility),
            );
        }
    }
}

/// Regression detector (env `WIDGET_LAYER_DEBUG`): for each funct widget
/// pane, walk its content_root subtree and report descendants whose
/// `RenderLayers` is missing or not equal to the pane's own layer. It is
/// scheduled `.after(propagate_render_layers).before(CheckVisibility)`,
/// so it observes exactly the layer state Bevy uses to pick a camera — a
/// nonzero `WRONG_LAYER` here means content is on the default layer 0 and
/// will be drawn by the main window camera, escaping the pane (over the
/// sidebar / across the cube). Should always be 0; if it isn't, the
/// `propagate_render_layers` ordering in `jim_pane` regressed.
/// Throttled to changes only.
fn debug_widget_layers(
    panes: Query<(Entity, &PaneKindMarker, &PaneChrome, &jim_pane::PaneLayer)>,
    children_q: Query<&Children>,
    layers_q: Query<&bevy::camera::visibility::RenderLayers>,
    mut last: Local<HashMap<Entity, (usize, usize)>>,
) {
    use bevy::camera::visibility::RenderLayers;
    for (pane, kind, chrome, pane_layer) in &panes {
        if kind.0 != PANE_KIND {
            continue;
        }
        let want = RenderLayers::layer(pane_layer.0);
        let mut total = 0usize;
        let mut bad = 0usize;
        let mut stack = vec![chrome.content_root];
        while let Some(e) = stack.pop() {
            total += 1;
            match layers_q.get(e) {
                Ok(rl) if *rl == want => {}
                _ => bad += 1,
            }
            if let Ok(ch) = children_q.get(e) {
                stack.extend(ch.iter());
            }
        }
        let cur = (total, bad);
        if last.get(&pane) != Some(&cur) {
            last.insert(pane, cur);
            eprintln!(
                "[layerdbg] pane {:?} layer={} content_descendants={} WRONG_LAYER={}",
                pane, pane_layer.0, total, bad
            );
        }
    }
}

fn register_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Funct Widget",
        radial_icon: None,
        default_size: Vec2::new(720.0, 360.0),
        spawn: script_widget_spawn,
        snapshot: script_widget_snapshot,
        on_close: Some(script_widget_close),
    });
}

fn setup_watcher(world: &mut World) {
    let Some(dir) = widgets_dir() else {
        warn!("script_widget: HOME not set, no script hot reload");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            "script_widget: couldn't create {}: {} — no hot reload",
            dir.display(),
            e
        );
        return;
    }
    // Bootstrap the default funct (`.ft`) widgets if missing. Each is
    // embedded from the repo's `widgets/` dir; they're starter widgets, so
    // we only write when absent (never clobber the user's edits).
    for (name, body) in [
        ("garden.ft", include_str!("../widgets/garden.ft")),
        ("style_picker.ft", include_str!("../widgets/style_picker.ft")),
        ("theme_editor.ft", include_str!("../widgets/theme_editor.ft")),
        ("chess.ft", include_str!("../widgets/chess.ft")),
    ] {
        let p = dir.join(name);
        if !p.exists() {
            let _ = std::fs::write(&p, body);
        }
    }
    // The funct widget host interface. Always (re)written so `import "host"`
    // in any `.ft` widget resolves and stays in sync with the natives the
    // funct worker registers. Owned by the app, not user-edited.
    let host_ft_path = dir.join("host.ft");
    if std::fs::read_to_string(&host_ft_path).ok().as_deref()
        != Some(crate::funct_widget::HOST_FT)
    {
        let _ = std::fs::write(&host_ft_path, crate::funct_widget::HOST_FT);
    }

    let (tx, rx) = mpsc::channel::<PathBuf>();
    let watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if !matches!(
            ev.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
        ) {
            return;
        }
        for path in ev.paths {
            let _ = tx.send(path);
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            warn!("script_widget: file watcher failed to start: {}", e);
            return;
        }
    };
    let mut watcher = watcher;
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        warn!("script_widget: failed to watch {}: {}", dir.display(), e);
        return;
    }
    world.insert_resource(ScriptWatcher {
        rx: Mutex::new(rx),
        _watcher: watcher,
    });
}

#[derive(Serialize, Deserialize)]
struct ScriptWidgetConfig {
    script: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    state: Value,
    /// Per-instance parameters injected as the funct global `params`
    /// (read via `extern let params` in host.ft). This is what makes a
    /// widget a reusable *primitive*: one `http.ft` pointed at any URL,
    /// one `bar.ft` told which columns to plot — set at spawn, not baked
    /// into a per-endpoint file. Distinct from `state` (runtime VM
    /// snapshot); `params` is the instance's configuration.
    #[serde(default)]
    params: Value,
}

fn script_widget_spawn(world: &mut World, entity: Entity, _content_root: Entity, config: &Value) {
    let cfg: ScriptWidgetConfig =
        serde_json::from_value(config.clone()).unwrap_or_else(|_| ScriptWidgetConfig {
            script: "garden.ft".to_string(),
            title: None,
            state: Value::Null,
            params: Value::Null,
        });
    if let Some(title) = cfg.title.clone() {
        if let Some(mut t) = world.get_mut::<PaneTitle>(entity) {
            t.0 = title;
        }
    } else if let Some(mut t) = world.get_mut::<PaneTitle>(entity) {
        t.0 = cfg.script.trim_end_matches(".ft").to_string();
    }

    let script_path = widgets_dir()
        .map(|d| d.join(&cfg.script))
        .unwrap_or_else(|| PathBuf::from(&cfg.script));

    // Read the script text and hand it to the worker, which parses it on
    // its own thread (engine-specific: funct compiles, funct evals). A read
    // failure means the worker comes up idle until the watcher fires a
    // Reload. Parsing no longer happens on the main thread, so this is
    // engine-agnostic.
    let initial_source = match std::fs::read_to_string(&script_path) {
        Ok(body) => Some(body),
        Err(e) => {
            eprintln!("[widget] failed to read {}: {}", script_path.display(), e);
            None
        }
    };

    // Stable per-pane bus id. `to_bits` is unique among live entities, so
    // two widgets never share an id (and a despawn+respawn gets a fresh
    // one, which is what we want for retained-backlog dedup).
    let widget_id = format!("rw{:x}", entity.to_bits());
    let handle = spawn_worker(
        initial_source,
        cfg.state.clone(),
        cfg.params.clone(),
        cfg.script.clone(),
        widget_id.clone(),
    );

    world.entity_mut(entity).insert((
        ScriptWidget {
            script: cfg.script.clone(),
            script_path,
            widget_id,
            handle,
            params: cfg.params,
            applied_frame_gen: 0,
            last_state: cfg.state,
            last_size: Vec2::ZERO,
            last_tick_at: None,
            reload_gen: 0,
            applied_reload_gen: 0,
            sprite_entities: HashMap::new(),
            last_focus_sig: None,
            // Starts false; mirrored from the worker's `wants_clicks` slot
            // each frame once the engine has loaded the script.
            wants_clicks: false,
            wants_hover: false,
            force_render: false,
        },
        WidgetTargets::default(),
        crate::WidgetScroll::default(),
        crate::WidgetHover::default(),
    ));
}

fn script_widget_snapshot(world: &World, entity: Entity) -> Value {
    let Some(w) = world.get::<ScriptWidget>(entity) else {
        return Value::Null;
    };
    // Prefer the live worker-published snapshot; fall back to the
    // last value the host already mirrored from it.
    let state = w
        .handle
        .slots
        .snapshot
        .lock()
        .ok()
        .map(|s| s.clone())
        .unwrap_or_else(|| w.last_state.clone());
    let title = world.get::<PaneTitle>(entity).map(|t| t.0.clone());
    serde_json::json!({
        "script": w.script,
        "title": title,
        "state": state,
        "params": w.params,
    })
}

/// Pane close: tell the worker to stop, then explicitly clear any
/// sprite entities we created so they don't linger as ghosts on the
/// canvas after the pane disappears.
fn script_widget_close(world: &mut World, entity: Entity) {
    let mut entities_to_despawn: Vec<Entity> = Vec::new();
    if let Some(w) = world.get::<ScriptWidget>(entity) {
        // Tell the worker thread to exit promptly.
        w.handle.send(HostToWorker::Shutdown);
        entities_to_despawn.extend(w.sprite_entities.values().copied());
    }
    // Also despawn the flow-layout content (text / table / input / …)
    // spawned under content_root. These are re-created every render and
    // aren't tracked in `sprite_entities`, so without this they can
    // linger on the canvas after the pane is gone.
    if let Some(chrome) = world.get::<PaneChrome>(entity) {
        let root = chrome.content_root;
        if let Some(children) = world.get::<Children>(root) {
            entities_to_despawn.extend(children.iter());
        }
    }
    for e in entities_to_despawn {
        if world.get_entity(e).is_ok() {
            world.entity_mut(e).despawn();
        }
    }
}

// ============================================================
// File watcher → reload
// ============================================================

fn poll_watcher(watcher: Option<Res<ScriptWatcher>>, mut widgets: Query<&mut ScriptWidget>) {
    let Some(watcher) = watcher else { return };
    let paths: Vec<PathBuf> = {
        let rx = watcher.rx.lock().expect("funct watcher channel poisoned");
        rx.try_iter().collect()
    };
    if paths.is_empty() {
        return;
    }
    let unique: HashSet<PathBuf> = paths.into_iter().collect();
    // A changed file that ISN'T some widget's own script is a shared
    // *library module* (e.g. `df.ft`, imported by every chart). Editing it
    // must hot-swap into every widget that imported it — otherwise charts
    // keep the stale module they compiled at spawn. Collect those module
    // names (file stems) to broadcast as `ReloadModule`.
    let widget_scripts: HashSet<PathBuf> = widgets.iter().map(|w| w.script_path.clone()).collect();
    let changed_modules: Vec<String> = unique
        .iter()
        .filter(|p| !widget_scripts.contains(*p))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect();
    for mut w in &mut widgets {
        if unique.contains(&w.script_path) {
            // The widget's own script changed → full re-eval.
            w.reload_gen = w.reload_gen.wrapping_add(1);
        }
        // Hot-swap any changed imported module into this widget's VM.
        for name in &changed_modules {
            w.handle.send(HostToWorker::ReloadModule { name: name.clone() });
        }
    }
}

fn apply_reloads(mut widgets: Query<&mut ScriptWidget>) {
    for mut w in &mut widgets {
        // Mirror the worker-reported interaction-handler flag every frame
        // (cheap atomic load) so pinned-widget click hot-zoning tracks the
        // currently-loaded script for both engines.
        let wc = w.handle.slots.wants_clicks.load(Ordering::Acquire);
        if w.wants_clicks != wc {
            w.wants_clicks = wc;
        }
        let wh = w.handle.slots.wants_hover.load(Ordering::Acquire);
        if w.wants_hover != wh {
            w.wants_hover = wh;
        }
        if w.applied_reload_gen == w.reload_gen {
            continue;
        }
        w.applied_reload_gen = w.reload_gen;
        let path = w.script_path.clone();
        let Ok(source) = std::fs::read_to_string(&path) else {
            eprintln!("[widget] reload: couldn't read {}", path.display());
            continue;
        };
        // The worker parses/compiles on its own thread (engine-specific)
        // and reports wants_clicks back through the slot.
        w.handle.send(HostToWorker::Reload { source });
        eprintln!("[widget] reloaded {}", path.display());
    }
}

// ============================================================
// Main thread: feed worker, drain claude events, send size + dt
// ============================================================

/// Translate a `PaneContentPressed` into the matching worker handler
/// for funct widgets. The element under the cursor decides which one:
///
///   - `Button` (or empty space)  → `on_click(x, y, shift, cmd, id)`
///   - `Toggle`                   → `on_toggle(id, checked)`
///   - `Tabs`                     → `on_tab_select(id, tab)`
///   - `Input`                    → `on_input_focus(id, true)` + the
///                                  host begins owning the edit buffer
///                                  (see `WidgetInputFocus`).
///
/// Clicking anything that is NOT an Input also blurs a previously
/// focused input.
fn forward_clicks_to_workers(
    mut commands: Commands,
    mut presses: MessageReader<PaneContentPressed>,
    keys: Res<ButtonInput<KeyCode>>,
    widgets: Query<(
        &PaneKindMarker,
        &ScriptWidget,
        Option<&WidgetTargets>,
        Option<&crate::WidgetScroll>,
    )>,
) {
    let cmd = keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight);
    for ev in presses.read() {
        let Ok((kind, w, targets, scroll)) = widgets.get(ev.pane) else {
            continue;
        };
        if kind.0 != PANE_KIND {
            continue;
        }
        // `ev.local_pt` is pane-content coords with scroll=0 baked in.
        // Click rects in `targets` are stored relative to content_root's
        // local frame, which slides up by `scroll.y` when the user
        // scrolls. Add the scroll offset so the hit-test matches the
        // visually-rendered position of each rect.
        let scroll_y = scroll.map(|s| s.y).unwrap_or(0.0);
        let hit_pt = ev.local_pt + Vec2::new(0.0, scroll_y);

        // Find the specific element under the cursor (if any) and route
        // by its kind. Children push their rect BEFORE their clickable
        // parent (e.g. a Button inside a ListItem), so the forward
        // `find` yields the innermost (most specific) target.
        let hit = targets.and_then(|t| {
            t.clicks
                .iter()
                .find(|ct| ct.rect.contains(hit_pt))
                .map(|ct| (ct.id.clone(), ct.kind.clone()))
        });

        match hit {
            Some((id, crate::ClickKind::Toggle { new_checked })) => {
                commands.entity(ev.pane).remove::<crate::WidgetInputFocus>();
                w.handle.send(HostToWorker::Toggle {
                    id,
                    checked: new_checked,
                });
            }
            Some((id, crate::ClickKind::TabSelect { tab })) => {
                commands.entity(ev.pane).remove::<crate::WidgetInputFocus>();
                w.handle.send(HostToWorker::TabSelect { id, tab });
            }
            Some((id, crate::ClickKind::RadioSelect { option })) => {
                commands.entity(ev.pane).remove::<crate::WidgetInputFocus>();
                w.handle.send(HostToWorker::RadioSelect { id, option });
            }
            Some((id, crate::ClickKind::NumberChange { value })) => {
                commands.entity(ev.pane).remove::<crate::WidgetInputFocus>();
                w.handle.send(HostToWorker::NumberChange { id, value });
            }
            Some((id, crate::ClickKind::InputFocus)) => {
                // Seed the host-owned edit buffer from the input's
                // current rendered value so the first keystroke appends
                // rather than clearing.
                let mut focus = crate::WidgetInputFocus::new(id.clone());
                if let Some(frame) = w.latest_frame() {
                    if let Some((value, _, multiline)) = crate::find_input_value(&frame, &id) {
                        focus.caret = value.chars().count();
                        focus.value = value;
                        focus.multiline = multiline;
                    }
                }
                commands.entity(ev.pane).insert(focus);
                w.handle
                    .send(HostToWorker::InputFocus { id, focused: true });
            }
            // Button hit, or empty space (None). Canvas / self-routing
            // widgets rely on the empty-space click reaching `on_click`.
            other => {
                commands.entity(ev.pane).remove::<crate::WidgetInputFocus>();
                let button_id = other.map(|(id, _)| id);
                w.handle.send(HostToWorker::Click {
                    local_x: hit_pt.x,
                    local_y: hit_pt.y,
                    shift: ev.shift,
                    cmd,
                    button_id,
                });
            }
        }
    }
}

fn forward_drags_to_workers(
    mut events: MessageReader<PaneContentDragged>,
    widgets: Query<(&PaneKindMarker, &ScriptWidget, Option<&crate::WidgetScroll>)>,
) {
    for ev in events.read() {
        let Ok((kind, w, scroll)) = widgets.get(ev.pane) else {
            continue;
        };
        if kind.0 != PANE_KIND {
            continue;
        }
        let scroll_y = scroll.map(|s| s.y).unwrap_or(0.0);
        let pt = ev.local_pt + Vec2::new(0.0, scroll_y);
        w.handle.send(HostToWorker::Drag {
            local_x: pt.x,
            local_y: pt.y,
        });
    }
}

fn forward_releases_to_workers(
    mut events: MessageReader<PaneContentReleased>,
    widgets: Query<(&PaneKindMarker, &ScriptWidget, Option<&crate::WidgetScroll>)>,
) {
    for ev in events.read() {
        let Ok((kind, w, scroll)) = widgets.get(ev.pane) else {
            continue;
        };
        if kind.0 != PANE_KIND {
            continue;
        }
        let scroll_y = scroll.map(|s| s.y).unwrap_or(0.0);
        let pt = ev.local_pt + Vec2::new(0.0, scroll_y);
        w.handle.send(HostToWorker::Release {
            local_x: pt.x,
            local_y: pt.y,
        });
    }
}

fn forward_hovers_to_workers(
    mut events: MessageReader<PaneContentHovered>,
    widgets: Query<(&PaneKindMarker, &ScriptWidget, Option<&crate::WidgetScroll>)>,
) {
    for ev in events.read() {
        let Ok((kind, w, scroll)) = widgets.get(ev.pane) else {
            continue;
        };
        if kind.0 != PANE_KIND {
            continue;
        }
        // INFINITY is the "cursor left" sentinel — pass through
        // untouched so the script can detect it.
        let pt = if ev.local_pt.x.is_finite() {
            let scroll_y = scroll.map(|s| s.y).unwrap_or(0.0);
            ev.local_pt + Vec2::new(0.0, scroll_y)
        } else {
            ev.local_pt
        };
        w.handle.send(HostToWorker::Hover {
            local_x: pt.x,
            local_y: pt.y,
        });
    }
}

/// Route navigation keys (arrows / Home / End) to the focused funct
/// widget as `on_key`. Terminals consume these themselves when focused,
/// so there's no conflict; we only fire when a funct widget holds focus
/// and isn't in text-edit mode (which owns the keyboard).
fn forward_keys_to_workers(
    keys: Res<ButtonInput<KeyCode>>,
    focused: Res<jim_pane::FocusedPane>,
    widgets: Query<(
        &PaneKindMarker,
        &ScriptWidget,
        Option<&crate::WidgetInputFocus>,
    )>,
) {
    let Some(pane) = focused.0 else { return };
    let Ok((kind, w, input_focus)) = widgets.get(pane) else {
        return;
    };
    // A focused Element::Input owns the keyboard (arrows move the caret,
    // handled by `handle_widget_input_typing`); don't also fire on_key.
    if kind.0 != PANE_KIND || input_focus.is_some() {
        return;
    }
    for (code, name) in [
        (KeyCode::ArrowLeft, "ArrowLeft"),
        (KeyCode::ArrowRight, "ArrowRight"),
        (KeyCode::ArrowUp, "ArrowUp"),
        (KeyCode::ArrowDown, "ArrowDown"),
        (KeyCode::Home, "Home"),
        (KeyCode::End, "End"),
    ] {
        if keys.just_pressed(code) {
            w.handle.send(HostToWorker::Key {
                key: name.to_string(),
            });
        }
    }
}

fn forward_inputs_to_workers(
    time: Res<Time>,
    pane_zoom: Res<jim_pane::PaneZoom>,
    theme: Res<jim_style::Theme>,
    mut theme_events: MessageReader<jim_style::ThemeChanged>,
    mut events: MessageReader<ClaudeBusEvent>,
    mut widgets: Query<(&PaneKindMarker, &PaneRect, &mut ScriptWidget)>,
) {
    // A palette edit only updates the shared theme snapshot; canvas
    // widgets bake theme colors into their frame, so without a nudge
    // they keep the stale color until some unrelated event re-renders
    // them. Push a one-shot re-render whenever the theme changes.
    // Trigger on the `ThemeChanged` MESSAGE (the canonical signal the
    // chrome + snapshot publisher use). `Res<Theme>::is_changed()` does
    // NOT fire for this query on the style-picker / `set_active_style`
    // path, so canvas charts kept stale baked colors while chrome
    // recolored.
    let theme_changed = theme_events.read().last().is_some() || theme.is_changed();
    if theme_changed {
        // The worker reads `theme_get` off a shared snapshot on its own
        // thread. The event-driven publisher (`publish_snapshot_on_change`)
        // is unordered w.r.t. this nudge, so a worker can process the
        // Rerender below before the snapshot reflects the new project's
        // theme — re-rendering the garden's sky from the *stale*
        // canvas_bg, with nothing to re-trigger it. Publish synchronously
        // here, on the main thread, before any Rerender is queued, so the
        // worker can only ever read the fresh palette.
        jim_style::theme_bridge::refresh_snapshot(&theme);
    }
    let new_events: Vec<(String, Value)> = events
        .read()
        .map(|ev| {
            let payload: Value = serde_json::from_str(&ev.payload_json).unwrap_or(Value::Null);
            (ev.kind.clone(), payload)
        })
        .collect();

    let now = std::time::Instant::now();
    for (kind, rect, mut w) in &mut widgets {
        if kind.0 != PANE_KIND {
            continue;
        }
        // PaneRect is canvas-units now; pane Transform handles zoom.
        let content_size = Vec2::new(
            (rect.size.x - 2.0 * MARGIN).max(0.0),
            (rect.size.y - TITLE_H - 2.0 * MARGIN).max(0.0),
        );
        // Send Resize whenever content_size changes, including the
        // very first non-zero size after spawn. The previous guard
        // (`w.last_size != Vec2::ZERO`) suppressed exactly that case,
        // so the worker stayed at canvas_w=canvas_h=0 until the user
        // manually dragged a corner — visible as garden plants
        // rendering at the top of the pane (y = canvas_h - inset).
        if w.last_size != content_size && content_size != Vec2::ZERO {
            w.handle.send(HostToWorker::Resize {
                canvas_w: content_size.x,
                canvas_h: content_size.y,
            });
        }
        w.last_size = content_size;

        if theme_changed {
            w.handle.send(HostToWorker::Rerender);
        }

        for (k, p) in &new_events {
            w.handle.send(HostToWorker::ClaudeEvent {
                kind: k.clone(),
                payload: p.clone(),
            });
        }

        // Tick only fires while the widget has opted into animation
        // (`set_animating(true)` in the script). Most widgets stay
        // event-driven and never receive a Tick — that's the whole
        // point of the event-driven worker contract.
        if !w.handle.slots.animating.load(Ordering::Acquire) {
            w.last_tick_at = None;
            continue;
        }
        let dt = match w.last_tick_at {
            Some(prev) => (now - prev).as_secs_f32(),
            None => 0.0,
        };
        if dt > 0.0 && dt < ANIMATION_MIN_FRAME_SECS {
            // Cap animation frame rate; drop sub-frame ticks.
            continue;
        }
        w.last_tick_at = Some(now);
        w.handle.send(HostToWorker::Tick { dt_secs: dt });
        let _ = time; // suppress warning; Time is here for future use
    }
}

// ============================================================
// Main thread: read latest frame, diff entities
// ============================================================

fn apply_latest_frames(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut image_cache: ResMut<WidgetImageCache>,
    mut clip_dirty: ResMut<WidgetClipDirty>,
    mut anim_store: ResMut<crate::anim::WidgetAnim>,
    pane_font: Res<PaneFont>,
    pane_metrics: Res<jim_pane::PaneFontMetrics>,
    theme: Res<jim_style::Theme>,
    fonts: Res<jim_style::FontRegistry>,
    pane_zoom: Res<jim_pane::PaneZoom>,
    time: Res<Time>,
    mut q: Query<(
        Entity,
        &PaneKindMarker,
        &PaneChrome,
        &PaneRect,
        &mut ScriptWidget,
        &mut WidgetTargets,
        &mut crate::WidgetScroll,
        Option<&crate::WidgetHover>,
        Option<&crate::WidgetInputFocus>,
    )>,
    children_q: Query<&Children>,
) {
    let theme_changed = theme.is_changed();
    let zoom = pane_zoom.0.max(0.0001);
    // Caret blink: visible during the first half of each 1s cycle.
    let caret_visible = time.elapsed_secs().rem_euclid(1.0) < 0.5;
    for (entity, kind, chrome, rect, mut w, mut targets, mut scroll, hover, input_focus) in &mut q {
        if kind.0 != PANE_KIND {
            continue;
        }
        let current_gen = w.handle.slots.frame_gen.load(Ordering::Acquire);
        // A focused input re-renders to show live keystrokes + a blinking
        // caret, but only when its signature changes (keystroke or blink
        // toggle) — re-rendering every frame would rebuild the whole flow
        // tree 60×/sec and stall typing on heavier widgets.
        let focus_sig = input_focus.map(|f| (f.value.clone(), f.caret, caret_visible));
        let focus_changed = focus_sig != w.last_focus_sig;
        // Theme changes also re-emit so widgets pick up new palette colors.
        let forced = w.force_render;
        if current_gen == w.applied_frame_gen && !theme_changed && !focus_changed && !forced {
            continue;
        }
        let _prof = jim_pane::prof::pane_span(entity.to_bits(), "widget");
        w.applied_frame_gen = current_gen;
        w.last_focus_sig = focus_sig;
        w.force_render = false;

        // Grab the frame the worker last produced.
        let frame = w
            .handle
            .slots
            .latest_frame
            .lock()
            .ok()
            .and_then(|s| s.clone());
        // Also mirror snapshot for persistence. Done in two steps so
        // we don't hold the snapshot lock across a borrow of `w`.
        let new_state = w.handle.slots.snapshot.lock().ok().map(|s| s.clone());
        if let Some(s) = new_state {
            w.last_state = s;
        }

        let Some(frame) = frame else { continue };

        match frame {
            // Absolute-positioned sprite tree: garden + similar
            // visualizers. Diffs against sprite_entities for cheap
            // per-frame mutation.
            Element::Canvas { children } => {
                // Canvas widgets draw absolutely, so the host can't infer
                // their height for scrolling. Derive it from the items'
                // lowest extent and set the scroll bound, so a tall canvas
                // (e.g. a heatmap with many rows) becomes wheel-scrollable.
                let mut extent = 0.0_f32;
                for it in &children {
                    let bottom = match it {
                        CanvasItem::Rect { y, h, anchor, .. } => canvas_item_bottom(*y, *h, *anchor),
                        CanvasItem::Sprite { y, h, anchor, .. } => canvas_item_bottom(*y, *h, *anchor),
                        CanvasItem::Text { y, size, anchor, .. } => {
                            canvas_item_bottom(*y, size.unwrap_or(14.0), *anchor)
                        }
                    };
                    if bottom > extent {
                        extent = bottom;
                    }
                }
                let content_h = (rect.size.y - TITLE_H - 2.0 * MARGIN).max(0.0);
                let new_max = (extent + MARGIN - content_h).max(0.0);
                if (scroll.max_y - new_max).abs() > 0.5 {
                    scroll.max_y = new_max;
                }
                if scroll.y > scroll.max_y {
                    scroll.y = scroll.max_y;
                }
                diff_render(
                    &mut commands,
                    &mut images,
                    &mut image_cache,
                    chrome.content_root,
                    &children,
                    &mut w.sprite_entities,
                    &pane_font.0,
                    &fonts,
                );
            }
            // Flow layout (vstack / hstack / text / button / divider /
            // bar / spacer / etc.). Rebuild from scratch each frame —
            // tree is small enough that diffing isn't worth the code.
            // `render` populates `targets` (the click-target Vec) with a
            // `ClickTarget { id, kind, rect }` per interactive element;
            // `forward_clicks_to_workers` hit-tests against it to route
            // Button / Toggle / Tabs / Input presses to the right
            // handler.
            other => {
                // Clear previously-rendered flow children but keep
                // sprite entities tracked separately (in case a widget
                // ever mixes both, which the protocol doesn't currently
                // allow but might in the future).
                if let Ok(children) = children_q.get(chrome.content_root) {
                    for c in children.iter() {
                        if !w.sprite_entities.values().any(|e| *e == c) {
                            // `try_despawn`: a concurrent pane teardown
                            // (recursive despawn) may have already removed
                            // this child before our buffer applies. See the
                            // stale-entity note on the diff_render despawn.
                            commands.entity(c).try_despawn();
                        }
                    }
                }
                let content_size = Vec2::new(
                    (rect.size.x - 2.0 * MARGIN).max(0.0),
                    (rect.size.y - TITLE_H - 2.0 * MARGIN).max(0.0),
                );
                let ctx = crate::render::LayoutCtx {
                    font: pane_font.0.clone(),
                    metrics: *pane_metrics,
                    owner_pane: entity,
                    content_root: chrome.content_root,
                    content_size,
                    palette: crate::render::WidgetPalette::from_theme(&theme),
                    theme: theme.clone(),
                    fonts: fonts.clone(),
                    focused_input: input_focus.cloned(),
                    caret_visible,
                    hovered_click_id: hover.and_then(|h| h.click_id.clone()),
                    anim: anim_store.snapshot_for(entity),
                };
                // Wipe ALL of last frame's element-derived targets so
                // stale entries from before a re-render don't keep
                // matching clicks — or, for toasts/dialogs, keep
                // rendering forever (toasts used to accumulate one copy
                // per re-render because they were missing here).
                targets.clicks.clear();
                targets.links.clear();
                targets.spans.clear();
                targets.sliders.clear();
                targets.selects.clear();
                targets.tooltips.clear();
                targets.dialogs.clear();
                targets.popovers.clear();
                targets.toasts.clear();
                targets.anims.clear();
                let consumed = crate::render::render(
                    &mut commands,
                    &ctx,
                    &mut targets,
                    &other,
                    Vec2::ZERO,
                    content_size.x,
                    0.0,
                );
                anim_store.apply_requests(entity, &targets.anims);
                // Update scroll bounds based on what the render
                // actually consumed. Clamp current scroll to new max
                // so resizing the pane shorter doesn't strand the
                // user past the new bottom.
                let new_max = (consumed.y - content_size.y).max(0.0);
                if (scroll.max_y - new_max).abs() > 0.1 {
                    scroll.max_y = new_max;
                }
                if scroll.y > new_max {
                    scroll.y = new_max;
                }
            }
        }
        clip_dirty.0 = true;
    }
}

/// Lowest canvas-y a canvas item reaches, accounting for its anchor.
/// The item's `(y)` is the anchor point, so where the item extends to
/// depends on which corner/edge that anchor pins:
///   - top-anchored    → item spans `[y, y + h]`, bottom = `y + h`
///   - center-anchored  → item spans `[y - h/2, y + h/2]`, bottom = `y + h/2`
///   - bottom-anchored  → item spans `[y - h, y]`, bottom = `y`
///
/// Used for the scroll-extent calc: a vertical bar chart anchors its
/// bars `bottom-left` on the baseline and grows *upward*, so a naive
/// `y + h` would report a phantom region as tall as the tallest bar
/// below the baseline — inflating the scroll bound and clipping the
/// bars' tops off (they rendered as uniform short stubs).
fn canvas_item_bottom(y: f32, h: f32, anchor: CanvasAnchor) -> f32 {
    match anchor {
        CanvasAnchor::TopLeft | CanvasAnchor::TopCenter => y + h,
        CanvasAnchor::Center => y + h * 0.5,
        CanvasAnchor::BottomLeft | CanvasAnchor::BottomCenter => y,
    }
}

/// Reconcile `items` against `sprite_entities`: reuse entities whose
/// id appears in both old + new, spawn new entities for ids only in
/// new, despawn entities for ids only in old.
///
/// Compared to despawn-everything-then-respawn this saves the ECS
/// from churning hundreds of entities every frame in a busy garden.
fn diff_render(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    image_cache: &mut WidgetImageCache,
    content_root: Entity,
    items: &[CanvasItem],
    sprite_entities: &mut HashMap<String, Entity>,
    default_font: &Handle<Font>,
    fonts: &jim_style::FontRegistry,
) {
    let mut seen: HashSet<String> = HashSet::with_capacity(items.len());
    for item in items {
        let id = match item {
            CanvasItem::Sprite { id, .. } => id.clone(),
            CanvasItem::Rect { id, .. } => id.clone(),
            CanvasItem::Text { id, .. } => id.clone(),
        };
        seen.insert(id.clone());
        let existing = sprite_entities.get(&id).copied();
        match item {
            CanvasItem::Sprite {
                x,
                y,
                w,
                h,
                image,
                hue_shift,
                anchor,
                z,
                ..
            } => {
                let Some(handle) = load_image_for_ref(images, image_cache, image, *hue_shift)
                else {
                    continue;
                };
                let sprite = Sprite {
                    image: handle,
                    custom_size: Some(Vec2::new(*w, *h)),
                    ..default()
                };
                let transform = Transform::from_xyz(*x, -*y, *z);
                let anchor_cmp = canvas_anchor_to_bevy(*anchor);
                match existing {
                    Some(e) => {
                        // Reuse — overwrite the components we own.
                        commands
                            .entity(e)
                            .try_insert((sprite, anchor_cmp, transform));
                    }
                    None => {
                        let e = commands
                            .spawn((
                                ChildOf(content_root),
                                sprite,
                                anchor_cmp,
                                transform,
                                Visibility::Inherited,
                            ))
                            .id();
                        sprite_entities.insert(id, e);
                    }
                }
            }
            CanvasItem::Rect {
                x,
                y,
                w,
                h,
                color,
                anchor,
                z,
                rotation,
                ..
            } => {
                let bevy_color = parse_canvas_color(color).unwrap_or(Color::srgb(0.20, 0.22, 0.26));
                let sprite = Sprite {
                    color: bevy_color,
                    custom_size: Some(Vec2::new(*w, *h)),
                    ..default()
                };
                // Canvas is y-down but the world is y-up (we render at
                // -y), so a clockwise canvas rotation is a negative
                // world rotation about z.
                let mut transform = Transform::from_xyz(*x, -*y, *z);
                if *rotation != 0.0 {
                    transform.rotation = Quat::from_rotation_z(-rotation.to_radians());
                }
                let anchor_cmp = canvas_anchor_to_bevy(*anchor);
                match existing {
                    Some(e) => {
                        commands
                            .entity(e)
                            .try_insert((sprite, anchor_cmp, transform));
                    }
                    None => {
                        let e = commands
                            .spawn((
                                ChildOf(content_root),
                                sprite,
                                anchor_cmp,
                                transform,
                                Visibility::Inherited,
                            ))
                            .id();
                        sprite_entities.insert(id, e);
                    }
                }
            }
            CanvasItem::Text {
                x,
                y,
                value,
                color,
                size,
                family,
                anchor,
                z,
                ..
            } => {
                let font_size = size.unwrap_or(14.0).max(1.0);
                let col = color
                    .as_deref()
                    .and_then(parse_canvas_color)
                    .unwrap_or(Color::WHITE);
                let base_font = match family.as_deref() {
                    Some(f) => fonts.resolve(f),
                    None => default_font.clone(),
                };
                let anchor_cmp = canvas_anchor_to_bevy(*anchor);
                let transform = Transform::from_xyz(*x, -*y, *z);
                // No-wrap: short labels (button text, status lines) must
                // never break mid-word. Without this, "New game" wraps
                // to "New\ngame" inside a narrow canvas because Bevy's
                // default TextLayout still inserts soft breaks.
                let layout = bevy::text::TextLayout::new_with_no_wrap();

                // Per-glyph font fallback. Bevy draws a Text2d in ONE font
                // and silently drops codepoints it lacks, so geometric
                // shapes / arrows / math vanished from canvas labels. The
                // global PostUpdate splitter skips canvas text (it fights
                // this in-place diff — see `CanvasManagedText`), so we do
                // the split HERE and OWN the child spans: the root holds
                // run 0, each later run becomes a `TextSpan` child tracked
                // in `sprite_entities` under a composite key so the
                // stale-cleanup below despawns spans when the run count
                // shrinks (or the symbol goes away). Fully-covered strings
                // are a single run → no children, zero overhead.
                let runs = fonts.split_runs(&base_font, value);
                let (root_str, root_font) = match runs.first() {
                    Some((s, f)) => (s.clone(), f.clone()),
                    None => (String::new(), base_font),
                };
                let text_entity = match existing {
                    Some(e) => {
                        commands.entity(e).try_insert((
                            Text2d::new(root_str),
                            TextFont {
                                font: root_font,
                                font_size,
                                ..default()
                            },
                            TextColor(col),
                            anchor_cmp,
                            transform,
                            layout,
                            crate::text_fallback::CanvasManagedText,
                        ));
                        e
                    }
                    None => {
                        let e = commands
                            .spawn((
                                ChildOf(content_root),
                                Text2d::new(root_str),
                                TextFont {
                                    font: root_font,
                                    font_size,
                                    ..default()
                                },
                                TextColor(col),
                                anchor_cmp,
                                transform,
                                layout,
                                Visibility::Inherited,
                                crate::text_fallback::CanvasManagedText,
                            ))
                            .id();
                        sprite_entities.insert(id.clone(), e);
                        e
                    }
                };
                // Reconcile fallback spans (runs[1..]) as children of the
                // text root. Composite key keeps them in `sprite_entities`
                // and `seen` so they reuse across frames and get reaped
                // when no longer produced. `\u{1}` can't appear in an
                // author-chosen canvas id, so keys never collide.
                for (n, (s, f)) in runs.iter().enumerate().skip(1) {
                    let span_key = format!("{id}\u{1}fb{n}");
                    seen.insert(span_key.clone());
                    match sprite_entities.get(&span_key).copied() {
                        Some(se) => {
                            commands.entity(se).try_insert((
                                bevy::text::TextSpan::new(s.clone()),
                                TextFont {
                                    font: f.clone(),
                                    font_size,
                                    ..default()
                                },
                                TextColor(col),
                            ));
                        }
                        None => {
                            let se = commands
                                .spawn((
                                    ChildOf(text_entity),
                                    bevy::text::TextSpan::new(s.clone()),
                                    TextFont {
                                        font: f.clone(),
                                        font_size,
                                        ..default()
                                    },
                                    TextColor(col),
                                ))
                                .id();
                            sprite_entities.insert(span_key, se);
                        }
                    }
                }
            }
        }
    }

    // Despawn entities whose id wasn't seen this frame.
    let stale: Vec<String> = sprite_entities
        .keys()
        .filter(|id| !seen.contains(id.as_str()))
        .cloned()
        .collect();
    for id in stale {
        if let Some(e) = sprite_entities.remove(&id) {
            // `try_despawn`, not `despawn`: this system's command buffer
            // can be applied AFTER a pane close (an exclusive system in a
            // different plugin) has already recursively despawned this
            // pane's content. A plain `despawn` on the now-stale entity
            // panics the whole app ("Entity ... is invalid"). The render
            // path must tolerate its content being torn down out from
            // under it — pane teardown is the external authority.
            commands.entity(e).try_despawn();
        }
    }
    let _ = CanvasAnchor::TopLeft; // suppress unused-import warning
    let _: ImageRef = ImageRef::Path {
        path: String::new(),
    };
}

