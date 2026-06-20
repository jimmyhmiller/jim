//! In-process **funct**-scripted widgets — the funct VM counterpart to
//! `script_widget::worker_main`.
//!
//! This module is *only* the worker-thread body. Everything the main
//! thread touches — the `ScriptWidget` component, the `HostToWorker`
//! channel, `WorkerSlots`, the whole Bevy plugin (input forwarding,
//! frame application, persistence) — is shared with the rhai path and
//! lives in `script_widget.rs`. `spawn_worker` picks this body for any
//! script whose filename ends in `.ft`. So adding funct cost zero new
//! Bevy systems: a `.ft` file produces the same `protocol::Element`
//! frames through the same slots.
//!
//! # Why funct (the point of the whole transition)
//!
//! Rhai runs a handler to completion on the host call stack — once
//! `call_fn` starts you cannot interrupt it. funct's VM is *reified*:
//! `start(name, args)` builds an owned `VmState`, and `run(&mut st,
//! StopWhen::Fuel(n))` executes at most `n` instructions and hands back
//! `Paused(FuelExhausted)` with the half-finished state intact. So this
//! worker drives every handler in **fuel-bounded slices**: a slow or
//! runaway handler is paused between two instructions, the worker stays
//! responsive to `Shutdown`/`Resize`, and the partial `VmState` is plain
//! data (snapshottable, resumable). That is the "pause at any time /
//! time-slice at will" capability rhai structurally cannot provide.
//!
//! # Handler model
//!
//! Identical handler names to the rhai contract (`on_init`, `render`,
//! `on_click`, `on_frame`, `on_bus`, `on_message`, the `on_input_*` /
//! `on_*_select` family, `on_proc_output` / `on_proc_exit`). Missing
//! handlers are not errors — funct reports them as an absent global and
//! we skip. Widgets pull the host surface in with `import "host"`
//! (bootstrapped to `~/.jim/widgets/host.ft`).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use std::time::Duration;

use funct::{Cause, Fault, Funct, RunResult, Status, Value, VmState};
use serde_json::Value as Json;

use crate::protocol::Element;
use crate::script_widget::{HostToWorker, OutMsg, WorkerSlots};

/// Wall-clock budget per `run` slice, expressed in epoch ticks. A handler that
/// finishes within this behaves like run-to-completion (the common case); one
/// that doesn't is paused and resumed next loop turn, so the worker stays
/// responsive to `Shutdown`/new events and the editor never stalls behind a
/// pathological script.
///
/// TIME, not instruction count, is the right metric: one funct "instruction"
/// can be a host call (`highlight`/`parse_json`/a big allocation) costing
/// milliseconds, so a fixed instruction budget bounds the wrong thing. We
/// drive funct's epoch-interruption (`StopWhen::Epoch` + `set_deadline`) from
/// ONE shared ~1ms ticker ([`shared_epoch_ticker`]) rather than funct's own
/// per-VM `Deadline` ticker — which would be one always-on thread PER WIDGET.
/// At [`EPOCH_TICK`] (1ms) per tick, this is a ~4ms slice.
///
/// (Caveat: the epoch is only checked BETWEEN funct instructions, never inside
/// a single native host call, so a slow host fn still runs to completion.)
const SLICE_TICKS: u64 = 4;

/// After this many consecutive paused slices on one job we log a warning
/// (a handler is looping or doing far too much work) — but keep slicing,
/// never busy-block the worker.
const RUNAWAY_SLICE_WARN: u64 = 50;

/// Granularity of the shared epoch ticker.
const EPOCH_TICK: Duration = Duration::from_millis(1);

/// Register a VM's epoch counter with the single process-wide ticker thread,
/// which advances every live registered epoch every [`EPOCH_TICK`]. This is
/// what makes time-based slicing cost ONE background thread total instead of
/// funct's lazily-spawned per-VM `Deadline` ticker (one always-on 1ms thread
/// per widget — exactly the kind of idle CPU we avoid). Dead VMs' epochs
/// (held weakly) are pruned as the ticker runs.
fn register_shared_epoch(epoch: &std::sync::Arc<std::sync::atomic::AtomicU64>) {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    type Reg = Arc<Mutex<Vec<Weak<std::sync::atomic::AtomicU64>>>>;
    static REG: OnceLock<Reg> = OnceLock::new();
    let reg = REG.get_or_init(|| {
        let reg: Reg = Arc::new(Mutex::new(Vec::new()));
        let ticker = reg.clone();
        let _ = std::thread::Builder::new()
            .name("funct-shared-epoch-ticker".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                let mut live = ticker.lock().unwrap();
                live.retain(|w| match w.upgrade() {
                    Some(e) => {
                        e.fetch_add(1, Ordering::Relaxed);
                        true
                    }
                    None => false,
                });
            });
        reg
    });
    reg.lock().unwrap().push(Arc::downgrade(epoch));
}

/// The shared host interface every funct widget imports with
/// `import "host"`. Bootstrapped to `~/.jim/widgets/host.ft` so module
/// resolution finds it. Mirrors `examples/widgets/host.ft` in the funct
/// repo and the surface `register_host_surface` actually provides.
pub(crate) const HOST_FT: &str = r#"// host.ft — the jim-editor widget host interface (auto-written).
//
// A funct widget pulls this whole surface in with `import "host"`. The
// editor registers the natives it provides before evaluating the widget;
// anything declared here but not registered faults loudly only if CALLED,
// so a widget can import the whole interface and use only part of it.

// --- globals the host injects ---
extern let canvas_w
extern let canvas_h
// Vertical scroll offset (px from top). Updated as the pane scrolls; a
// widget that defines `on_scroll(y)` is re-rendered on change so it can
// window its content to the visible slice (see the diff viewer).
extern let scroll_y
// host-owned persistent state atom (survives hot reload + restart)
extern let state
// per-instance params (config) set at spawn — same .ft, different URL /
// columns / topics. A plain record (not an atom); read-only.
extern let params

// --- render / animation control ---
extern fn request_render()
extern fn set_animating(on)
extern fn set_tick_interval(secs)

// --- subprocess bridge (UCI engines, language servers, …) ---
extern fn proc_spawn(cmd)
extern fn proc_write(handle, line)
extern fn proc_read(handle)
extern fn proc_alive(handle)
extern fn proc_kill(handle)

// --- style / drawing surface ---
extern fn uniform_set(name, value)
extern fn mask_paint(name, x, y, radius, value)
extern fn oklch(l, c, h)

// --- widget<->widget message bus ---
extern fn emit(topic, payload)
extern fn emit_retained(topic, payload)
extern fn my_id()

// --- misc ---
extern fn host_log(msg)
extern fn host_env(name)
extern fn widget_asset(rel)
extern fn clipboard_set(text)
extern fn parse_json(s)
extern fn to_json(v)
extern fn rand()
extern fn rand_int(lo, hi)
extern fn hash_str(s)
extern fn time()

// --- syntax highlighting + filesystem (the diff / code-review widget) ---
// highlight(code, lang) -> [ [ { text, kind }, … ], … ] (one line per entry)
extern fn highlight(code, lang)
// read_file(path) -> { ok, text, error };  write_file(path, text) -> bool
extern fn read_file(path)
extern fn write_file(path, text)
"#;

/// Expand a leading `~` / `~/` to the user's home dir for the filesystem
/// host fns. Anything else is returned unchanged.
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}{}", &path[1..]);
        }
    }
    path.to_string()
}

/// Wrapper key used to mark a persisted funct VM snapshot inside the
/// generic `PaneSnapshot.config.state` JSON, so the funct worker can tell
/// its own snapshot apart from a rhai state map.
const SNAPSHOT_KEY: &str = "__funct_vmstate__";

/// Key for a STATE-ONLY snapshot: the serialized value of the widget's
/// `state` atom (data), NOT the whole VM (code + execution). Snapshotting
/// data instead of execution is what lets a widget keep its state across
/// restarts while still re-evaluating the CURRENT source + modules — so
/// hot-reloaded code (e.g. `df.ft`) actually takes effect. (A full
/// execution snapshot would restore stale baked-in code.)
const STATE_KEY: &str = "__funct_state__";

/// What the in-flight job is, so its completion routes correctly.
#[derive(Clone, Copy)]
enum JobKind {
    /// A lifecycle/event handler. `persist` marks handlers that change
    /// durable state (everything except per-frame Tick), so we only pay
    /// `save_state` after meaningful changes, not 60×/sec.
    Handler { persist: bool },
    /// The `render(w, h)` call; its result Value is the frame.
    Render,
}

/// A paused, resumable unit of script execution.
struct Job {
    kind: JobKind,
    st: VmState,
    /// Slices spent so far (runaway detection / logging).
    slices: u64,
}

struct FunctWorker {
    vm: Funct,
    slots: WorkerSlots,
    /// Kept so a failed snapshot restore can fall back to a fresh eval,
    /// and so a Reload that fails to parse leaves the old code running.
    source: Option<String>,
    canvas_w: f32,
    canvas_h: f32,
    /// Vertical scroll offset pushed from the host. Exposed as the `scroll_y`
    /// global so a windowing widget renders only the visible slice.
    scroll_y: f32,
    /// True once the script has been eval'd (or restored) successfully.
    loaded: bool,
    /// The current paused job, if any. Exactly one runs at a time — the
    /// VM is single-threaded and handlers share atom state, so events
    /// that arrive mid-job queue rather than interleave.
    job: Option<Job>,
    /// A non-Tick handler ran since the last persist, so the next render
    /// completion should snapshot durable state.
    persist_dirty: bool,
    /// Hash of the last frame we actually published. A widget on a tick
    /// (or a bus/http poller) re-runs `render` constantly but usually emits
    /// the *same* tree; without this we'd bump `frame_gen` every time and the
    /// host would despawn+respawn the whole entity subtree for an unchanged
    /// picture — the dominant source of frame spikes. Skipping the bump when
    /// the tree is identical means the host never re-renders, zero churn.
    last_frame_hash: Option<u64>,
}

/// Stable content hash of a produced frame. Serializes the `Element` tree
/// (cheap — microseconds for a few hundred nodes) and hashes the bytes, so
/// two renders that describe the same picture compare equal regardless of how
/// the script built them. A 64-bit collision would only skip one redraw, which
/// is cosmetically harmless, so this is plenty.
fn frame_hash(element: &Option<Element>) -> u64 {
    use std::hash::Hasher;
    let bytes = serde_json::to_vec(element).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(&bytes);
    h.finish()
}

impl FunctWorker {
    fn set_error(&self, msg: String) {
        eprintln!("[funct] {}", msg);
        if let Ok(mut slot) = self.slots.last_error.lock() {
            *slot = Some(msg);
        }
    }
    fn clear_error(&self) {
        if let Ok(mut slot) = self.slots.last_error.lock() {
            *slot = None;
        }
    }

    /// Does the loaded script define a callable handler of this name?
    fn defines(&self, name: &str) -> bool {
        matches!(self.vm.global(name), Some(Value::Closure(_)))
    }

    /// Recompute the "has any pointer-interaction handler" flag the host
    /// uses to hot-zone a pinned widget's clicks.
    fn update_wants_clicks(&self) {
        const INTERACTIVE: &[&str] = &[
            "on_click",
            "on_toggle",
            "on_tab_select",
            "on_radio_select",
            "on_number_change",
            "on_select_change",
            "on_dialog_close",
            "on_toast_dismiss",
            "on_slider_change",
            "on_input_focus",
            "on_input_change",
            "on_input_submit",
            "on_editor_change",
            "on_editor_submit",
        ];
        let wants = INTERACTIVE.iter().any(|n| self.defines(n));
        self.slots.wants_clicks.store(wants, Ordering::Release);
        // Hover is reported separately: a chart with `on_hover` but no
        // `on_click` must still publish a content hot-zone so pinned-pane
        // hover hit-testing reaches it (same gate clicks use).
        self.slots
            .wants_hover
            .store(self.defines("on_hover"), Ordering::Release);
        self.slots
            .wants_pinch
            .store(self.defines("on_pinch"), Ordering::Release);
    }

    /// Evaluate the script source: defines functions, runs top-level
    /// statements, then calls `on_init` once. Re-injects the host globals
    /// (canvas size, persistent `state` atom) first. On a reload this
    /// hot-swaps functions by name; top-level `let`s re-run.
    fn load(&mut self, src: &str) {
        self.inject_globals();
        match self.vm.eval(src) {
            Ok(_) => {
                self.loaded = true;
                self.clear_error();
            }
            Err(e) => {
                self.set_error(format!("load: {e}"));
                return; // keep any previously-loaded code running
            }
        }
        self.source = Some(src.to_string());
        self.update_wants_clicks();
        if self.defines("on_init") {
            if let Err(e) = self.vm.call("on_init", vec![]) {
                self.set_error(format!("on_init: {e}"));
            }
        }
        self.slots.render_dirty.store(true, Ordering::Release);
    }

    /// Rehydrate saved DATA into the `state` atom AFTER source has been
    /// evaluated (so code/modules are current). Reuses the atom the script
    /// bound (or the host-injected one); otherwise installs a host-owned
    /// `state`. This is the restore half of state-only snapshots.
    fn seed_state(&mut self, data: &serde_json::Value) {
        let v = Value::from_json(data);
        match self.vm.global("state") {
            Some(Value::Atom(a)) => {
                *a.value.write() = v;
            }
            _ => {
                let atom = self.vm.make_atom(v);
                self.vm.set_global("state", atom);
            }
        }
        self.slots.render_dirty.store(true, Ordering::Release);
    }

    /// Call a zero-arg lifecycle handler (e.g. `on_start`) if the script
    /// defines it. `on_start` is the place for SIDE EFFECTS — fetches,
    /// `proc_spawn`, `set_animating`, bus subscriptions — because it runs
    /// every time the widget starts (fresh spawn, restart, hot-reload)
    /// AFTER `state` is set/rehydrated, whereas `on_init` runs during
    /// source eval (before rehydration). Effects live outside the snapshot,
    /// so they must be re-established on each start.
    fn call_lifecycle(&mut self, name: &str) {
        if self.loaded && self.defines(name) {
            if let Err(e) = self.vm.call(name, vec![]) {
                self.set_error(format!("{name}: {e}"));
            }
        }
    }

    /// Current `state` value as JSON — used to preserve data across a
    /// hot-reload (which re-evals top-level and would otherwise reset it).
    fn capture_state(&self) -> Option<serde_json::Value> {
        match self.vm.global("state") {
            Some(Value::Atom(a)) => a.value.read().clone().to_json().ok(),
            Some(other) => other.to_json().ok(),
            None => None,
        }
    }

    fn inject_globals(&mut self) {
        self.vm
            .set_global("canvas_w", Value::Float(self.canvas_w as f64));
        self.vm
            .set_global("canvas_h", Value::Float(self.canvas_h as f64));
        self.vm
            .set_global("scroll_y", Value::Float(self.scroll_y as f64));
        // Provide the host-owned `state` atom only if the script hasn't
        // already bound `state` itself. Created once; persists across
        // reloads so `extern let state` widgets keep their data.
        if !matches!(self.vm.global("state"), Some(Value::Atom(_))) {
            let atom = self.vm.make_atom(Value::Unit);
            self.vm.set_global("state", atom);
        }
    }

    // ---- job lifecycle ----

    /// Start a handler running as a fuel-sliced job. A missing optional
    /// handler is a no-op (not an error).
    fn start_handler(&mut self, name: &str, args: Vec<Value>, persist: bool) {
        if !self.defines(name) {
            return;
        }
        match self.vm.start(name, args) {
            Ok(st) => {
                self.job = Some(Job {
                    kind: JobKind::Handler { persist },
                    st,
                    slices: 0,
                });
            }
            Err(e) => self.set_error(format!("{name}: {e}")),
        }
    }

    /// Begin a render job (if the script draws). Consumes the dirty flag.
    fn start_render(&mut self) {
        self.slots.render_dirty.store(false, Ordering::Release);
        if !self.defines("render") {
            return; // a purely reactive widget need not draw
        }
        let args = vec![
            Value::Float(self.canvas_w as f64),
            Value::Float(self.canvas_h as f64),
        ];
        match self.vm.start("render", args) {
            Ok(st) => {
                self.job = Some(Job {
                    kind: JobKind::Render,
                    st,
                    slices: 0,
                });
            }
            Err(e) => self.set_error(format!("render: {e}")),
        }
    }

    /// Advance the in-flight job by one fuel slice. Finishes (publishing a
    /// frame / persisting) when the job completes; keeps the paused state
    /// when fuel runs out so the next loop turn resumes it.
    fn run_slice(&mut self) {
        let Some(mut job) = self.job.take() else {
            return;
        };
        // Pause once the shared ticker has advanced our epoch SLICE_TICKS past
        // now — a wall-clock budget, driven by the one shared ticker thread.
        self.vm.set_deadline(self.vm.epoch_now() + SLICE_TICKS);
        match self.vm.run(&mut job.st, funct::StopWhen::Epoch) {
            RunResult::Paused(Cause::DeadlineReached) => {
                job.slices += 1;
                if job.slices == RUNAWAY_SLICE_WARN {
                    let what = match job.kind {
                        JobKind::Render => "render",
                        JobKind::Handler { .. } => "handler",
                    };
                    eprintln!(
                        "[funct] {what} still running after {} time slices (~{}ms each) — \
                         time-slicing it across frames",
                        job.slices,
                        SLICE_TICKS * EPOCH_TICK.as_millis() as u64
                    );
                }
                self.job = Some(job); // resume next turn
            }
            RunResult::Done(v) => self.finish_job(job.kind, Ok(v)),
            RunResult::Faulted(f) => self.finish_job(job.kind, Err(f)),
            // We only use fuel/deadline budgets, so any other pause is a bug.
            RunResult::Paused(other) => {
                self.set_error(format!("unexpected pause cause: {other:?}"));
            }
        }
    }

    fn finish_job(&mut self, kind: JobKind, result: Result<Value, Fault>) {
        match kind {
            JobKind::Handler { persist } => match result {
                Ok(_) => {
                    self.clear_error();
                    if persist {
                        self.persist_dirty = true;
                    }
                }
                Err(f) => self.set_error(format!("handler: {f}")),
            },
            JobKind::Render => match result {
                Ok(frame) => match funct_frame_to_element(&frame) {
                    Ok(element) => {
                        // Only publish (and bump frame_gen, which drives the
                        // host's despawn+respawn of the whole subtree) when the
                        // rendered tree actually changed. An unchanged re-render
                        // — the common case for tick/bus/poll widgets — is
                        // dropped here, so the host does zero work and zero
                        // entity churn for it.
                        //
                        // Exception: a widget in `animating` (60fps) mode is
                        // explicitly driving per-frame visual motion, and some
                        // of that motion is interpolated host-side from anim
                        // specs in an otherwise-identical tree — so it needs the
                        // re-render even when the tree hashes equal. Its churn is
                        // the documented cost of opting into animation.
                        let animating = self.slots.animating.load(Ordering::Acquire);
                        let hash = frame_hash(&element);
                        if animating || self.last_frame_hash != Some(hash) {
                            self.last_frame_hash = Some(hash);
                            if let Ok(mut slot) = self.slots.latest_frame.lock() {
                                *slot = element;
                            }
                            self.slots.frame_gen.fetch_add(1, Ordering::Release);
                            // Worker runs off the main thread; nudge the
                            // reactive loop so async re-renders show promptly.
                            crate::request_main_loop_wakeup();
                        }
                        // Persist durable state whenever a handler that touched
                        // it has rendered — independent of whether the visible
                        // tree changed (state can change without the UI moving).
                        if self.persist_dirty {
                            self.persist_state();
                            self.persist_dirty = false;
                        }
                    }
                    Err(msg) => self.set_error(msg),
                },
                Err(f) => self.set_error(format!("render: {f}")),
            },
        }
    }

    /// Snapshot the widget's DATA (the `state` atom's value), not the VM.
    /// Survives close/restart while letting the next spawn re-evaluate the
    /// current source + modules. Best-effort: a `state` value holding a
    /// non-serializable (native handle, closure) is skipped rather than
    /// faulting the widget.
    fn persist_state(&mut self) {
        let value = match self.vm.global("state") {
            Some(Value::Atom(a)) => a.value.read().clone(),
            Some(other) => other,
            None => return, // no state to persist
        };
        match value.to_json() {
            Ok(data) => {
                if let Ok(mut slot) = self.slots.snapshot.lock() {
                    *slot = serde_json::json!({ STATE_KEY: data });
                }
            }
            Err(e) => {
                // state holds something unserializable (e.g. a proc handle
                // wrapper). Not fatal; just no persistence this tick.
                eprintln!("[funct] state not persisted: {e}");
            }
        }
    }

    // ---- message dispatch ----

    /// Handle one host message. Returns false only for Shutdown. Events
    /// that drive a script handler start a (fuel-sliced) job; control
    /// messages (Resize/Reload/Rerender) are handled inline.
    fn dispatch(&mut self, msg: HostToWorker) -> bool {
        match msg {
            HostToWorker::Shutdown => return false,
            HostToWorker::Reload { source } => {
                // Preserve live data across a code edit: capture state,
                // re-eval the new source (which would reset it), restore it.
                // `on_start` is NOT re-run — a code edit shouldn't redo side
                // effects (e.g. re-fetch).
                let saved = self.capture_state();
                self.load(&source);
                if let Some(s) = saved {
                    self.seed_state(&s);
                }
            }
            HostToWorker::ReloadModule { name } => {
                // Hot-swap an imported library module (e.g. `df`) whose file
                // changed, so widgets that imported it pick up new code (chart
                // colors, helpers) without a respawn — then re-render. Only
                // meaningful for funct file modules; `reload_module` rejects
                // host modules, which we ignore.
                if self.loaded {
                    match self.vm.reload_module(&name) {
                        Ok(_) => self.slots.render_dirty.store(true, Ordering::Release),
                        // Not a file module (host) / not loadable — ignore.
                        Err(_) => {}
                    }
                }
            }
            HostToWorker::Rerender => {
                if self.loaded {
                    self.slots.render_dirty.store(true, Ordering::Release);
                }
            }
            HostToWorker::Resize { canvas_w, canvas_h } => {
                self.canvas_w = canvas_w;
                self.canvas_h = canvas_h;
                self.vm
                    .set_global("canvas_w", Value::Float(canvas_w as f64));
                self.vm
                    .set_global("canvas_h", Value::Float(canvas_h as f64));
                self.start_handler(
                    "on_resize",
                    vec![Value::Float(canvas_w as f64), Value::Float(canvas_h as f64)],
                    true,
                );
                self.slots.render_dirty.store(true, Ordering::Release);
            }
            HostToWorker::Scroll { y } => {
                self.scroll_y = y;
                self.vm.set_global("scroll_y", Value::Float(y as f64));
                // Only a widget that opts in (defines `on_scroll`) re-renders
                // on scroll — keeps non-virtualized widgets cost-free here.
                if self.defines("on_scroll") {
                    self.start_handler("on_scroll", vec![Value::Float(y as f64)], false);
                }
            }
            HostToWorker::Wheel { local_x, local_y, dx, dy } => {
                // Cursor-aware wheel: on_wheel(x, y, dx, dy). dx/dy are the
                // horizontal/vertical deltas so a widget can pan a timeline by
                // the dominant axis (avoids jitter from the minor axis).
                if self.defines("on_wheel") {
                    self.start_handler(
                        "on_wheel",
                        vec![
                            Value::Float(local_x as f64),
                            Value::Float(local_y as f64),
                            Value::Float(dx as f64),
                            Value::Float(dy as f64),
                        ],
                        false,
                    );
                }
            }
            HostToWorker::Pinch { local_x, local_y, delta } => {
                if self.defines("on_pinch") {
                    self.start_handler(
                        "on_pinch",
                        vec![
                            Value::Float(local_x as f64),
                            Value::Float(local_y as f64),
                            Value::Float(delta as f64),
                        ],
                        false,
                    );
                }
            }
            other if !self.loaded => {
                // Script not up yet (read failed / awaiting Reload). Drop
                // the event rather than fault; mirrors rhai's behavior.
                let _ = other;
            }
            HostToWorker::Key { key } => {
                self.start_handler("on_key", vec![Value::str(key)], true);
            }
            HostToWorker::ClaudeEvent { kind, payload } => {
                // `on_bus` is current; `on_event` is the deprecated alias.
                let name = if self.defines("on_bus") {
                    "on_bus"
                } else {
                    "on_event"
                };
                self.start_handler(name, vec![Value::str(kind), Value::from_json(&payload)], true);
            }
            HostToWorker::Toggle { id, checked } => {
                self.start_handler("on_toggle", vec![Value::str(id), Value::Bool(checked)], true);
            }
            HostToWorker::TabSelect { id, tab } => {
                self.start_handler("on_tab_select", vec![Value::str(id), Value::str(tab)], true);
            }
            HostToWorker::RadioSelect { id, option } => {
                self.start_handler(
                    "on_radio_select",
                    vec![Value::str(id), Value::str(option)],
                    true,
                );
            }
            HostToWorker::NumberChange { id, value } => {
                self.start_handler(
                    "on_number_change",
                    vec![Value::str(id), Value::Float(value as f64)],
                    true,
                );
            }
            HostToWorker::SelectChange { id, value } => {
                self.start_handler(
                    "on_select_change",
                    vec![Value::str(id), Value::str(value)],
                    true,
                );
            }
            HostToWorker::DialogClose { id } => {
                self.start_handler("on_dialog_close", vec![Value::str(id)], true);
            }
            HostToWorker::ToastDismiss { id } => {
                self.start_handler("on_toast_dismiss", vec![Value::str(id)], true);
            }
            HostToWorker::SliderChange { id, value } => {
                self.start_handler(
                    "on_slider_change",
                    vec![Value::str(id), Value::Float(value as f64)],
                    true,
                );
            }
            HostToWorker::InputFocus { id, focused } => {
                self.start_handler(
                    "on_input_focus",
                    vec![Value::str(id), Value::Bool(focused)],
                    true,
                );
            }
            HostToWorker::InputChange { id, value } => {
                self.start_handler("on_input_change", vec![Value::str(id), Value::str(value)], true);
            }
            HostToWorker::EditorChange { id, value } => {
                self.start_handler(
                    "on_editor_change",
                    vec![Value::str(id), Value::str(value)],
                    true,
                );
            }
            HostToWorker::EditorSubmit {
                id,
                selection,
                full,
            } => {
                self.start_handler(
                    "on_editor_submit",
                    vec![Value::str(id), Value::str(selection), Value::str(full)],
                    true,
                );
            }
            HostToWorker::InputSubmit { id, value } => {
                self.start_handler("on_input_submit", vec![Value::str(id), Value::str(value)], true);
            }
            HostToWorker::Message {
                topic,
                payload,
                sender,
            } => {
                self.start_handler(
                    "on_message",
                    vec![
                        Value::str(topic),
                        Value::from_json(&payload),
                        Value::str(sender),
                    ],
                    true,
                );
            }
            HostToWorker::ProcOutput { handle, line } => {
                self.start_handler(
                    "on_proc_output",
                    vec![Value::Int(handle), Value::str(line)],
                    true,
                );
            }
            HostToWorker::ProcExit { handle, code } => {
                self.start_handler(
                    "on_proc_exit",
                    vec![Value::Int(handle), Value::Int(code)],
                    true,
                );
            }
            HostToWorker::Click {
                local_x,
                local_y,
                shift,
                cmd,
                button_id,
            } => {
                let id = button_id.unwrap_or_default();
                self.start_handler(
                    "on_click",
                    vec![
                        Value::Float(local_x as f64),
                        Value::Float(local_y as f64),
                        Value::Bool(shift),
                        Value::Bool(cmd),
                        Value::str(id),
                    ],
                    true,
                );
            }
            HostToWorker::Drag { local_x, local_y } => {
                self.start_handler(
                    "on_drag",
                    vec![Value::Float(local_x as f64), Value::Float(local_y as f64)],
                    true,
                );
            }
            HostToWorker::Release { local_x, local_y } => {
                self.start_handler(
                    "on_release",
                    vec![Value::Float(local_x as f64), Value::Float(local_y as f64)],
                    true,
                );
            }
            HostToWorker::Hover { local_x, local_y } => {
                self.start_handler(
                    "on_hover",
                    vec![Value::Float(local_x as f64), Value::Float(local_y as f64)],
                    // Hover fires a lot and rarely changes durable state;
                    // don't trigger a persist for it.
                    false,
                );
            }
            HostToWorker::Scroll { y } => {
                // Mirror the Rhai worker: drives the optional `on_scroll(y)`.
                self.start_handler("on_scroll", vec![Value::Float(y as f64)], false);
            }
            HostToWorker::Tick { dt_secs } => {
                self.start_handler("on_frame", vec![Value::Float(dt_secs as f64)], false);
            }
        }
        true
    }
}

/// Worker-thread entry point for `.ft` widgets. Same signature shape as
/// `script_widget::worker_main` so `spawn_worker` can pick either.
pub(crate) fn funct_worker_main(
    rx: Receiver<HostToWorker>,
    self_tx: Sender<HostToWorker>,
    slots: WorkerSlots,
    initial_source: Option<String>,
    initial_state: serde_json::Value,
    params: serde_json::Value,
    widget_id: String,
) {
    let mut vm = Funct::new();
    if let Some(dir) = crate::script_widget::widgets_dir() {
        vm.set_module_root(dir); // so `import "host"` resolves
    }
    // Drive this VM's epoch from the one shared ticker so time-based slicing
    // (`StopWhen::Epoch` in `run_slice`) doesn't spin up a per-widget thread.
    register_shared_epoch(&vm.epoch());
    register_host_surface(&mut vm, &slots, &widget_id, self_tx);
    // Per-instance params → funct global `params` (host.ft: `extern let
    // params`). Set before the script evaluates so `on_init` can read it.
    // This is the primitive-config channel: same `.ft`, different URL /
    // columns / topics per instance.
    vm.set_global("params", Value::from_json(&params));

    let mut worker = FunctWorker {
        vm,
        slots,
        source: initial_source.clone(),
        canvas_w: 0.0,
        canvas_h: 0.0,
        scroll_y: 0.0,
        loaded: false,
        job: None,
        persist_dirty: false,
        last_frame_hash: None,
    };

    // ALWAYS evaluate the current source first — this re-reads the widget
    // script AND re-imports its modules, so hot-reloaded code (e.g. df.ft)
    // takes effect across restarts. THEN rehydrate the saved data into the
    // `state` atom. We snapshot state, not execution.
    if let Some(src) = initial_source.as_deref() {
        worker.load(src);
    }
    match initial_state.get(STATE_KEY) {
        // RESTART: rehydrate the saved state and STOP. `on_start` (side
        // effects — fetches, etc.) is deliberately NOT run: we restore the
        // data we already had instead of redoing the work.
        Some(data) => worker.seed_state(data),
        // FRESH spawn (never persisted): run `on_start` once to do the
        // initial side effects (e.g. the first fetch).
        None => worker.call_lifecycle("on_start"),
    }
    // Legacy whole-VM snapshots (SNAPSHOT_KEY) are intentionally ignored:
    // restoring them would bring back stale baked-in code. Such widgets
    // re-init their state once, then persist in the new state-only format.

    // Time-sliced event loop. Idle = block on recv (zero CPU). With a job
    // in flight, run fuel slices back-to-back (still draining the channel
    // each turn so Shutdown is honored promptly). With no job, dispatch
    // the next queued message, else render if dirty, else block.
    let mut queue: VecDeque<HostToWorker> = VecDeque::new();
    loop {
        loop {
            match rx.try_recv() {
                Ok(HostToWorker::Shutdown) => return,
                Ok(m) => queue.push_back(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if worker.job.is_some() {
            worker.run_slice();
            continue;
        }

        if let Some(m) = queue.pop_front() {
            if !worker.dispatch(m) {
                return;
            }
        } else if worker.loaded && worker.slots.render_dirty.load(Ordering::Acquire) {
            worker.start_render();
        } else {
            match rx.recv() {
                Ok(HostToWorker::Shutdown) | Err(_) => return,
                Ok(m) => queue.push_back(m),
            }
        }
    }
}

/// Convert a funct `render` result into a `protocol::Element`. `Unit`
/// means "no visual this frame" (valid). The one funct-specific quirk:
/// `type` is a funct keyword, so widgets use `kind:` as the element
/// discriminator — rename it back to `type` before deserializing, since
/// `protocol::Element` is `#[serde(tag = "type")]`.
fn funct_frame_to_element(frame: &Value) -> Result<Option<Element>, String> {
    if matches!(frame, Value::Unit) {
        return Ok(None);
    }
    let mut json = frame.to_json().map_err(|e| format!("frame to_json: {e}"))?;
    rename_kind_to_type(&mut json);
    serde_json::from_value::<Element>(json)
        .map(Some)
        .map_err(|e| format!("frame deserialize: {e}"))
}

/// Recursively move every object's `kind` field to `type` (unless `type`
/// is already present). Element discriminator values are kebab-case
/// strings (`"canvas"`, `"sprite"`, `"rect"`, `"text"`, `"vstack"`, …),
/// so nothing else changes.
fn rename_kind_to_type(v: &mut Json) {
    match v {
        Json::Object(map) => {
            if !map.contains_key("type") {
                if let Some(k) = map.remove("kind") {
                    map.insert("type".to_string(), k);
                }
            }
            for child in map.values_mut() {
                rename_kind_to_type(child);
            }
        }
        Json::Array(items) => {
            for it in items {
                rename_kind_to_type(it);
            }
        }
        _ => {}
    }
}

/// Register the host natives a funct widget can call. Mirrors
/// `script_widget::register_host_functions`, pointing at the *same* editor
/// subsystems (subprocess registry, msgbus outbox, clipboard). Natives
/// declared in `host.ft` but not registered here fault loudly only if a
/// widget actually calls them.
fn register_host_surface(
    vm: &mut Funct,
    slots: &WorkerSlots,
    widget_id: &str,
    self_tx: Sender<HostToWorker>,
) {
    // ---- render / animation control ----
    let animating = slots.animating.clone();
    vm.register1("set_animating", move |on: bool| {
        animating.store(on, Ordering::Release);
        // Turning animation on off the main thread is inert unless we wake
        // the reactive loop so it flips to Continuous (same reasoning as
        // the rhai path). Waking on `false` is harmless.
        if on {
            crate::request_main_loop_wakeup();
        }
    });
    // Slow tick: the widget wants `on_frame` called roughly every `secs`
    // seconds (e.g. a 300s auto-refresh poll) WITHOUT pinning the whole
    // app to 60fps `Continuous`. The host serves these ticks from the
    // reactive loop instead, so an idle poller costs ~nothing. Passing 0
    // (or negative) cancels. Use this — not `set_animating(true)` — for
    // any periodic background work that isn't a real visual animation.
    let tick_ms = slots.tick_interval_ms.clone();
    vm.register1("set_tick_interval", move |secs: f64| {
        let ms = if secs > 0.0 {
            (secs * 1000.0).round().clamp(1.0, u32::MAX as f64) as u32
        } else {
            0
        };
        tick_ms.store(ms, Ordering::Release);
        if ms > 0 {
            crate::request_main_loop_wakeup();
        }
    });
    let dirty = slots.render_dirty.clone();
    vm.register0("request_render", move || {
        dirty.store(true, Ordering::Release);
    });

    // ---- logging / misc scalars ----
    vm.register_raw("host_log", |_vm, args| {
        let parts: Vec<String> = args.iter().map(|v| format!("{v}")).collect();
        eprintln!("[funct] {}", parts.join(" "));
        Ok(Value::Unit)
    });
    vm.register0("time", || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    });
    vm.register0("rand", || {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        (rand_state(nanos as u64) as f64) / (u32::MAX as f64)
    });
    vm.register2("rand_int", |lo: i64, hi: i64| -> i64 {
        if hi <= lo {
            return lo;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let r = rand_state(nanos as u64) as i64;
        lo + r.rem_euclid(hi - lo + 1)
    });
    // Real text measurement for canvas layout (transcript word-wrap, etc.).
    // The default canvas font is monospace, so the host's measured
    // cell_width gives an EXACT width — far better than a guessed
    // `chars * size * ratio`. Metrics are pushed in by the main thread
    // (WorkerSlots.font_metrics); fall back to a 0.6 ratio before the
    // first push.
    {
        let fm = slots.font_metrics.clone();
        let advance = move |size: f64| -> f64 {
            let (cw, fs) = fm.lock().map(|g| *g).unwrap_or((0.0, 0.0));
            if cw > 0.0 && fs > 0.0 {
                (cw as f64) * (size / fs as f64)
            } else {
                size * 0.6
            }
        };
        let a1 = advance.clone();
        vm.register2("measure_text", move |s: String, size: f64| -> f64 {
            a1(size) * (s.chars().count() as f64)
        });
        let a2 = advance.clone();
        vm.register1("char_width", move |size: f64| -> f64 { a2(size) });
    }
    vm.register1("hash_str", |s: String| -> i64 {
        let mut h: u64 = 14695981039346656037;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h as i64
    });
    vm.register1("widget_asset", |rel: String| -> String {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("assets");
        path.push(rel);
        path.to_string_lossy().into_owned()
    });
    vm.register1("host_env", |name: String| -> String {
        std::env::var(name).unwrap_or_default()
    });
    vm.register1("clipboard_set", |text: String| -> bool {
        crate::subprocess::clipboard_set(&text)
    });
    vm.register3("oklch", |l: f64, c: f64, h: f64| -> String {
        format!("oklch({l:.3} {c:.3} {h:.1})")
    });

    // ---- JSON bridge (subprocess protocols are plain text lines) ----
    vm.register1("parse_json", |s: String| -> Value {
        match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(j) => Value::from_json(&j),
            Err(_) => Value::Unit,
        }
    });
    vm.register_raw("to_json", |_vm, args| {
        let v = args.into_iter().next().unwrap_or(Value::Unit);
        match v.to_json() {
            Ok(j) => Ok(Value::str(j.to_string())),
            Err(e) => Err(e),
        }
    });

    // ---- syntax highlighting (the diff / code-review widget) ----
    // highlight(code, lang) -> [ line: [ { text, kind }, … ], … ]. One entry
    // per `\n`-split line; each line is its left-to-right colored runs. The
    // widget maps `kind` ("keyword"/"string"/…) to a theme color. Unknown
    // language → one `{text, kind:"default"}` run per line (renders plain).
    vm.register2("highlight", |code: String, lang: String| -> Value {
        let lines = crate::syntax::highlight_lines(&code, &lang);
        let json = serde_json::Value::Array(
            lines
                .into_iter()
                .map(|line| {
                    serde_json::Value::Array(
                        line.into_iter()
                            .map(|(text, kind)| serde_json::json!({ "text": text, "kind": kind }))
                            .collect(),
                    )
                })
                .collect(),
        );
        Value::from_json(&json)
    });

    // ---- filesystem bridge ----
    // read_file(path) -> { ok, text, error }. `~` is expanded. Lets a widget
    // load a file (a diff, a .md) without the `cat` subprocess dance, and is
    // the read side of comment/review persistence.
    vm.register1("read_file", |path: String| -> Value {
        match std::fs::read_to_string(expand_tilde(&path)) {
            Ok(text) => Value::from_json(&serde_json::json!({ "ok": true, "text": text })),
            Err(e) => Value::from_json(
                &serde_json::json!({ "ok": false, "text": "", "error": e.to_string() }),
            ),
        }
    });
    // write_file(path, text) -> bool. Creates parent dirs. `~` is expanded.
    // The write side of review-comment persistence (~/.jim/reviews/*.json).
    vm.register2("write_file", |path: String, text: String| -> bool {
        let p = expand_tilde(&path);
        if let Some(parent) = std::path::Path::new(&p).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&p, text).is_ok()
    });

    // ---- subprocess bridge (event-driven, same as rhai) ----
    let procs = Arc::new(Mutex::new(crate::subprocess::ProcRegistry::new()));
    {
        let tx = self_tx.clone();
        let notifier: crate::subprocess::ProcNotifier = Arc::new(move |ev| match ev {
            crate::subprocess::ProcEvent::Output { handle, line } => {
                let _ = tx.send(HostToWorker::ProcOutput { handle, line });
            }
            crate::subprocess::ProcEvent::Exit { handle, code } => {
                let _ = tx.send(HostToWorker::ProcExit {
                    handle,
                    code: code.map(|c| c as i64).unwrap_or(-1),
                });
            }
        });
        if let Ok(mut r) = procs.lock() {
            r.set_notifier(notifier);
        }
    }
    {
        let procs = procs.clone();
        // proc_spawn(cmd) or proc_spawn(cmd, [args]) — variadic, so raw.
        vm.register_raw("proc_spawn", move |_vm, args| {
            let (cmd, extra): (String, Vec<String>) = match args.as_slice() {
                [Value::Str(c)] => (c.to_string(), vec![]),
                [Value::Str(c), Value::List(items)] => {
                    let mut a = Vec::new();
                    for it in items.iter() {
                        match it {
                            Value::Str(s) => a.push(s.to_string()),
                            other => {
                                return Err(Fault::new(format!(
                                    "proc_spawn: args must be strings, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    (c.to_string(), a)
                }
                _ => return Err(Fault::new("proc_spawn expects (cmd) or (cmd, [args])")),
            };
            let id = procs.lock().map(|mut r| r.spawn(&cmd, &extra)).unwrap_or(-1);
            Ok(Value::Int(id))
        });
    }
    {
        let procs = procs.clone();
        vm.register2("proc_write", move |id: i64, line: String| -> bool {
            procs
                .lock()
                .map(|mut r| r.write_line(id, &line))
                .unwrap_or(false)
        });
    }
    {
        let procs = procs.clone();
        vm.register1("proc_read", move |id: i64| -> String {
            procs.lock().map(|mut r| r.read_line(id)).unwrap_or_default()
        });
    }
    {
        let procs = procs.clone();
        vm.register1("proc_alive", move |id: i64| -> bool {
            procs.lock().map(|r| r.alive(id)).unwrap_or(false)
        });
    }
    {
        let procs = procs.clone();
        vm.register1("proc_kill", move |id: i64| {
            if let Ok(mut r) = procs.lock() {
                r.kill(id);
            }
        });
    }

    // ---- time + directory listing (audio-recorder sidebar) ----
    // Local "YYYY-MM-DD_HH-MM-SS" stamp for unique, human-readable
    // filenames. funct has no date formatting, so we format via libc's
    // localtime_r (libc is already a dep).
    vm.register0("datetime_stamp", || -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as libc::time_t;
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&secs, &mut tm);
            format!(
                "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec
            )
        }
    });
    // list_dir(path) -> [{ name, size, mtime }] for regular files only,
    // newest first. `~` is expanded. Powers the recordings sidebar.
    vm.register1("list_dir", |path: String| -> Value {
        let p = expand_tilde(&path);
        let mut entries: Vec<(String, u64, u64)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&p) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let (size, mtime) = e
                        .metadata()
                        .map(|m| {
                            let mt = m
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            (m.len(), mt)
                        })
                        .unwrap_or((0, 0));
                    entries.push((name, size, mtime));
                }
            }
        }
        entries.sort_by(|a, b| b.2.cmp(&a.2)); // newest first
        let arr: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|(name, size, mtime)| serde_json::json!({ "name": name, "size": size, "mtime": mtime }))
            .collect();
        Value::from_json(&serde_json::Value::Array(arr))
    });

    // ---- native audio capture (audio-recorder widget) ----
    // Recording runs on cpal's own realtime thread (see audio.rs), so its
    // quality is decoupled from GUI/worker load; the widget just polls the
    // level stream. No subprocess, no log-line parsing.
    vm.register0("audio_inputs", || -> Value {
        let arr: Vec<serde_json::Value> = crate::audio::inputs()
            .into_iter()
            .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
            .collect();
        Value::from_json(&serde_json::Value::Array(arr))
    });
    vm.register3(
        "audio_record_start",
        |device: String, path: String, dual: bool| -> bool {
            crate::audio::record_start(&device, &path, dual)
        },
    );
    vm.register0("audio_record_stop", || -> bool { crate::audio::record_stop() });
    vm.register0("audio_levels", || -> Value {
        let arr: Vec<serde_json::Value> = crate::audio::take_levels()
            .into_iter()
            .map(|v| serde_json::json!(v))
            .collect();
        Value::from_json(&serde_json::Value::Array(arr))
    });
    vm.register0("audio_recording", || -> bool { crate::audio::is_recording() });
    vm.register0("audio_status", || -> String { crate::audio::status() });

    // ---- widget<->widget message bus ----
    {
        let outbox = slots.outbox.clone();
        let push = move |topic: String, payload: Value, retain: bool| -> Result<Value, Fault> {
            let json = payload.to_json()?; // loud on non-JSON payloads
            if let Ok(mut v) = outbox.lock() {
                v.push(OutMsg {
                    topic,
                    payload: json,
                    retain,
                });
            }
            crate::request_main_loop_wakeup();
            Ok(Value::Unit)
        };
        let p = push.clone();
        vm.register_raw("emit", move |_vm, args| emit_impl(&p, args, false));
        let p = push.clone();
        vm.register_raw("emit_retained", move |_vm, args| emit_impl(&p, args, true));
    }
    {
        let id = widget_id.to_string();
        vm.register0("my_id", move || id.clone());
    }

    // The jim_style widget surface (theme tokens, style presets, the
    // shader uniform/mask pipeline, per-project state, color helpers),
    // exposed for funct the same way the rhai worker pulls in
    // `register_*_host_fns`. Registered LAST so its hex `oklch` overrides
    // the core placeholder above; it deliberately does not register
    // `emit`, leaving the widget-bus `emit`/`emit_retained` registered
    // above intact. Backing state (snapshot mirrors + write channels) is
    // shared with the rhai registrations.
    jim_style::register_theme_host_fns_funct(vm);
    jim_style::register_preset_host_fns_funct(vm);
    jim_style::register_script_host_fns_funct(vm);
}

/// Shared body for `emit` / `emit_retained`: `emit(topic)` or
/// `emit(topic, payload)`.
fn emit_impl(
    push: &impl Fn(String, Value, bool) -> Result<Value, Fault>,
    args: Vec<Value>,
    retain: bool,
) -> Result<Value, Fault> {
    match args.as_slice() {
        [Value::Str(t)] => push(t.to_string(), Value::Unit, retain),
        [Value::Str(t), payload] => push(t.to_string(), payload.clone(), retain),
        _ => Err(Fault::new("emit expects (topic) or (topic, payload)")),
    }
}

fn rand_state(seed: u64) -> u32 {
    let mut x = (seed as u32) | 1;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Element;

    /// End-to-end of the adapter: a funct `render` returning a record with
    /// `kind:` discriminators round-trips through to_json + the rename into
    /// a real `protocol::Element` tree. This is the one funct-specific seam.
    #[test]
    fn render_record_becomes_element_tree() {
        let mut vm = Funct::new();
        vm.eval(
            r##"
            fn render(w, h) = {
                kind: "vstack", gap: 10.0, pad: 14.0, children: [
                    { kind: "text", value: "n = 3", size: 15.0, color: "#cfd2d8" },
                    { kind: "hstack", gap: 6.0, children: [
                        { kind: "button", id: "inc", label: "+" },
                    ]},
                ]
            }
            "##,
        )
        .expect("compile render");
        let frame = vm
            .call("render", vec![Value::Float(800.0), Value::Float(600.0)])
            .expect("call render");
        let el = funct_frame_to_element(&frame).expect("convert frame");
        match el {
            Some(Element::Vstack { children, .. }) => {
                assert_eq!(children.len(), 2, "vstack should have two children");
                assert!(
                    matches!(children[0], Element::Text { .. }),
                    "first child is text"
                );
                assert!(
                    matches!(children[1], Element::Hstack { .. }),
                    "second child is an hstack"
                );
            }
            other => panic!("expected Vstack, got {other:?}"),
        }
    }

    /// `()` (no render / nothing to draw) is a valid frame, not an error.
    #[test]
    fn unit_frame_is_none() {
        assert!(matches!(funct_frame_to_element(&Value::Unit), Ok(None)));
    }

    /// The headline capability: a long-running handler is *paused* between
    /// instructions when its fuel runs out and *resumed* later to the same
    /// correct result — exactly how the worker time-slices a heavy widget
    /// across frames instead of blocking. Rhai cannot do this.
    #[test]
    fn long_handler_is_paused_and_resumed() {
        use funct::{Cause, RunResult, StopWhen};
        let mut vm = Funct::new();
        vm.eval(
            r#"
            fn busy(n) {
                let mut total = 0
                let mut i = 0
                while i < n {
                    total = total + i
                    i = i + 1
                }
                total
            }
            "#,
        )
        .expect("compile busy");

        // 20k iterations is many more instructions than one small slice.
        let mut st = vm.start("busy", vec![Value::Int(20_000)]).expect("start");
        let mut pauses = 0;
        let result = loop {
            match vm.run(&mut st, StopWhen::Fuel(1_000)) {
                RunResult::Paused(Cause::FuelExhausted) => pauses += 1,
                RunResult::Done(v) => break v,
                other => panic!("unexpected: {other:?}"),
            }
        };
        // It genuinely yielded mid-execution (not run-to-completion)...
        assert!(pauses > 1, "expected multiple fuel pauses, got {pauses}");
        // ...and resuming produced the right answer: sum(0..20000).
        let expected: i64 = (0..20_000).sum();
        assert_eq!(result, Value::Int(expected));
    }

    /// The rename only fires when `type` is absent and recurses into arrays.
    #[test]
    fn rename_is_recursive_and_non_clobbering() {
        let mut j = serde_json::json!({
            "kind": "a",
            "children": [{ "kind": "b" }, { "type": "c", "kind": "ignored" }],
        });
        rename_kind_to_type(&mut j);
        assert_eq!(j["type"], "a");
        assert!(j.get("kind").is_none());
        assert_eq!(j["children"][0]["type"], "b");
        // pre-existing "type" wins; its sibling "kind" is left as-is
        assert_eq!(j["children"][1]["type"], "c");
        assert_eq!(j["children"][1]["kind"], "ignored");
    }
}
