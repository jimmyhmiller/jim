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

use funct::{Cause, Fault, Funct, RunResult, Status, Value, VmState};
use serde_json::Value as Json;

use crate::protocol::Element;
use crate::script_widget::{HostToWorker, OutMsg, WorkerSlots};

/// Instruction budget per `run` slice. A handler that finishes within
/// this many bytecode steps behaves exactly like run-to-completion (the
/// common case). One that doesn't is paused and resumed on the next loop
/// turn, so the worker can still see `Shutdown` and the editor never
/// stalls behind a pathological script. Tunable; sized so a normal
/// render/handler completes in a single slice.
const FUEL_PER_SLICE: u64 = 200_000;

/// After this many consecutive paused slices on one job we log a warning
/// (a handler is looping or doing far too much work) — but keep slicing,
/// never busy-block the worker.
const RUNAWAY_SLICE_WARN: u64 = 50;

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
// host-owned persistent state atom (survives hot reload + restart)
extern let state

// --- render / animation control ---
extern fn request_render()
extern fn set_animating(on)

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
"#;

/// Wrapper key used to mark a persisted funct VM snapshot inside the
/// generic `PaneSnapshot.config.state` JSON, so the funct worker can tell
/// its own snapshot apart from a rhai state map.
const SNAPSHOT_KEY: &str = "__funct_vmstate__";

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
    /// True once the script has been eval'd (or restored) successfully.
    loaded: bool,
    /// The current paused job, if any. Exactly one runs at a time — the
    /// VM is single-threaded and handlers share atom state, so events
    /// that arrive mid-job queue rather than interleave.
    job: Option<Job>,
    /// A non-Tick handler ran since the last persist, so the next render
    /// completion should snapshot durable state.
    persist_dirty: bool,
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
        ];
        let wants = INTERACTIVE.iter().any(|n| self.defines(n));
        self.slots.wants_clicks.store(wants, Ordering::Release);
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

    /// Restore a persisted VM snapshot instead of evaluating source. The
    /// host surface must already be registered (it is — `new` does it).
    /// Does NOT call `on_init`: durable state is already in the restored
    /// globals/atoms, and re-running `on_init` would reset it. Returns
    /// false (caller falls back to `load`) on any restore failure.
    fn restore(&mut self, snapshot_json: &str) -> bool {
        self.inject_globals();
        match self.vm.restore_state(snapshot_json) {
            Ok(_) => {
                self.loaded = true;
                self.clear_error();
                self.update_wants_clicks();
                self.slots.render_dirty.store(true, Ordering::Release);
                true
            }
            Err(e) => {
                eprintln!("[funct] snapshot restore failed ({e}); starting fresh");
                false
            }
        }
    }

    fn inject_globals(&mut self) {
        self.vm
            .set_global("canvas_w", Value::Float(self.canvas_w as f64));
        self.vm
            .set_global("canvas_h", Value::Float(self.canvas_h as f64));
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
        match self.vm.run(&mut job.st, funct::StopWhen::Fuel(FUEL_PER_SLICE)) {
            RunResult::Paused(Cause::FuelExhausted) => {
                job.slices += 1;
                if job.slices == RUNAWAY_SLICE_WARN {
                    let what = match job.kind {
                        JobKind::Render => "render",
                        JobKind::Handler { .. } => "handler",
                    };
                    eprintln!(
                        "[funct] {what} still running after {} fuel slices (~{} instrs) — \
                         time-slicing it across frames",
                        job.slices,
                        job.slices * FUEL_PER_SLICE
                    );
                }
                self.job = Some(job); // resume next turn
            }
            RunResult::Done(v) => self.finish_job(job.kind, Ok(v)),
            RunResult::Faulted(f) => self.finish_job(job.kind, Err(f)),
            // Only StopWhen::Fuel is used, which pauses solely on fuel.
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
                        if let Ok(mut slot) = self.slots.latest_frame.lock() {
                            *slot = element;
                        }
                        self.slots.frame_gen.fetch_add(1, Ordering::Release);
                        // Persist durable state once the frame reflecting a
                        // real change has been produced.
                        if self.persist_dirty {
                            self.persist_state();
                            self.persist_dirty = false;
                        }
                        // Worker runs off the main thread; nudge the
                        // reactive loop so async re-renders show promptly.
                        crate::request_main_loop_wakeup();
                    }
                    Err(msg) => self.set_error(msg),
                },
                Err(f) => self.set_error(format!("render: {f}")),
            },
        }
    }

    /// Snapshot the whole VM (code, globals, atoms) into the snapshot slot
    /// so the pane's state survives close/restart. Best-effort: a snapshot
    /// referencing a Native host value can't serialize and is skipped with
    /// a log rather than faulting the widget.
    fn persist_state(&mut self) {
        let st = VmState {
            frames: vec![],
            stack: vec![],
            status: Status::Done(Value::Unit),
        };
        match self.vm.save_state(&st) {
            Ok(json) => {
                if let Ok(mut slot) = self.slots.snapshot.lock() {
                    *slot = serde_json::json!({ SNAPSHOT_KEY: json });
                }
            }
            Err(e) => {
                // Common + expected for widgets holding native handles
                // (e.g. a spawned proc). Not fatal; just no persistence.
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
                self.load(&source);
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
    widget_id: String,
) {
    let mut vm = Funct::new();
    if let Some(dir) = crate::script_widget::widgets_dir() {
        vm.set_module_root(dir); // so `import "host"` resolves
    }
    register_host_surface(&mut vm, &slots, &widget_id, self_tx);

    let mut worker = FunctWorker {
        vm,
        slots,
        source: initial_source.clone(),
        canvas_w: 0.0,
        canvas_h: 0.0,
        loaded: false,
        job: None,
        persist_dirty: false,
    };

    // Prefer restoring a persisted VM snapshot; fall back to a fresh eval.
    let restored = initial_state
        .get(SNAPSHOT_KEY)
        .and_then(|v| v.as_str())
        .map(|json| worker.restore(json))
        .unwrap_or(false);
    if !restored {
        if let Some(src) = initial_source.as_deref() {
            worker.load(src);
        }
    }

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
