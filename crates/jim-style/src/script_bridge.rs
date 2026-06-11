//! Bridge between funct workers and the main thread.
//!
//! funct workers run on background threads with no ECS access. They
//! issue host calls (`uniform_set("name", v)`, `mask_paint(...)`,
//! `emit(...)`) which are encoded as [`DynamicMsg`] variants and
//! pushed through an mpsc channel. A main-thread system
//! ([`drain_script_msgs`]) consumes the queue each frame and applies
//! the effects via the ECS.
//!
//! For *reads* (e.g. `pane_rects()`, `uniform_get(...)`) the worker
//! reads from a shared snapshot that the main thread refreshes each
//! frame — no blocking, no waiting on a reply.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock, RwLock, Arc};

use bevy::prelude::*;
use serde_json::Value;

/// All host calls a worker can issue. Each variant maps 1:1 to a
/// funct-callable host function. Adding a new variant + a matching
/// `register_fn` block in [`register_script_host_fns`] is how a new
/// primitive gets added — but the goal is to keep this enum *small
/// and frozen*: every "new behavior" should be doable via the
/// existing variants composed in scripts.
#[derive(Debug)]
pub enum DynamicMsg {
    /// `uniform_set("name", scalar | [f32;2] | [f32;4])` — writes the
    /// value into the active material's uniform buffer at the offset
    /// the schema records for `name`. Silently no-ops if the name
    /// doesn't exist in the current shader.
    SetUniformF32(String, f32),
    SetUniformVec2(String, [f32; 2]),
    SetUniformVec4(String, [f32; 4]),
    /// `mask_paint("name", x, y, radius, value)` — stamp a soft brush
    /// (cosine falloff) at `(x, y)` in window-logical pixels into the
    /// named texture. Texture is auto-allocated on first reference.
    MaskPaint {
        name: String,
        x: f32,
        y: f32,
        radius: f32,
        value: f32,
    },
    /// `mask_fill("name", value)` — set every R-channel pixel to
    /// `value * 255`. Sugar: `mask_clear(name)` = `mask_fill(name, 0)`.
    MaskFill(String, f32),
    /// `emit("kind", payload)` — push an event onto the global bus
    /// that every script worker can listen to via the `events` array.
    Emit(String, Value),
    /// `schedule(delay_secs, "kind", payload)` — fire an emit after
    /// `delay_secs`. Bookkeeping lives on the main thread.
    Schedule {
        delay_secs: f32,
        kind: String,
        payload: Value,
    },
    /// `state_set(key, value)` — write per-project script state.
    StateSet(String, Value),
}

#[derive(Resource)]
pub struct ScriptReceiver(Mutex<Receiver<DynamicMsg>>);

/// Events waiting to be delivered to scripts on their next tick.
/// Drained by [`tick_system_script`] each frame and fed into the
/// script's `events` scope variable.
///
/// Anyone — Rust systems or scripts via `emit(...)` — can push here.
/// Rust producers use [`EventBus::push`]; the funct bridge routes
/// `emit`/`schedule` host calls through the dynamic-msg channel which
/// drain into this bus.
#[derive(Resource, Default)]
pub struct EventBus {
    pub pending: Vec<(String, Value)>,
}

impl EventBus {
    pub fn push(&mut self, kind: impl Into<String>, payload: Value) {
        self.pending.push((kind.into(), payload));
    }
}

/// Timed events that will fire later. Maintained by the dynamic-msg
/// drain (which moves `Schedule` variants here) and flushed each
/// frame into the bus once their delay has elapsed.
#[derive(Resource, Default)]
pub struct ScheduledEvents {
    pub items: Vec<ScheduledEvent>,
}

#[derive(Clone, Debug)]
pub struct ScheduledEvent {
    pub fire_at: f32,
    pub kind: String,
    pub payload: Value,
}

/// Shared snapshot the main thread refreshes each frame, accessible
/// to workers without blocking. Holds whatever's needed for read-side
/// host fns (uniform_get, pane_rects, state_get).
#[derive(Resource, Default, Clone)]
pub struct ScriptSnapshot {
    inner: Arc<RwLock<SnapshotData>>,
}

#[derive(Default, Clone)]
pub struct SnapshotData {
    /// Per-project state, serialized for cheap reads from any thread.
    pub state: serde_json::Map<String, Value>,
    /// Pane rects in window-local logical pixels.
    pub pane_rects: Vec<PaneRectSnap>,
    /// Uniform values from the last main-thread tick, keyed by name.
    pub uniforms: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct PaneRectSnap {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub kind: String,
}

impl ScriptSnapshot {
    pub fn read<R>(&self, f: impl FnOnce(&SnapshotData) -> R) -> R {
        let g = self.inner.read().unwrap_or_else(|p| p.into_inner());
        f(&g)
    }
    pub fn write(&self, f: impl FnOnce(&mut SnapshotData)) {
        if let Ok(mut g) = self.inner.write() {
            f(&mut g);
        }
    }
}

static SCRIPT_SENDER: OnceLock<Sender<DynamicMsg>> = OnceLock::new();
static SNAPSHOT: OnceLock<ScriptSnapshot> = OnceLock::new();

pub fn script_sender() -> Option<Sender<DynamicMsg>> {
    SCRIPT_SENDER.get().cloned()
}

pub fn snapshot() -> Option<ScriptSnapshot> {
    SNAPSHOT.get().cloned()
}

pub struct ScriptBridgePlugin;

impl Plugin for ScriptBridgePlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::channel::<DynamicMsg>();
        let _ = SCRIPT_SENDER.set(tx);
        let snap = ScriptSnapshot::default();
        let _ = SNAPSHOT.set(snap.clone());
        app.insert_resource(ScriptReceiver(Mutex::new(rx)))
            .insert_resource(snap)
            .init_resource::<EventBus>()
            .init_resource::<ScheduledEvents>();
    }
}
pub fn register_script_host_fns_funct(vm: &mut funct::Funct) {
    use funct::{Fault, Value as V};

    fn num_to_f32(v: &V) -> f32 {
        match v {
            V::Float(f) => *f as f32,
            V::Int(i) => *i as f32,
            _ => 0.0,
        }
    }

    // ---- uniform_set(name, scalar | [2] | [4]) ----
    vm.register_raw("uniform_set", |_vm, args| {
        let Some(tx) = script_sender() else {
            return Ok(V::Unit);
        };
        let name = match args.first() {
            Some(V::Str(s)) => s.to_string(),
            _ => return Err(Fault::new("uniform_set: name must be a string")),
        };
        match args.get(1) {
            Some(V::Float(_)) | Some(V::Int(_)) => {
                let _ = tx.send(DynamicMsg::SetUniformF32(name, num_to_f32(&args[1])));
            }
            Some(V::List(items)) => {
                let nums: Vec<f32> = items.iter().map(num_to_f32).collect();
                match nums.len() {
                    2 => {
                        let _ = tx.send(DynamicMsg::SetUniformVec2(name, [nums[0], nums[1]]));
                    }
                    4 => {
                        let _ = tx.send(DynamicMsg::SetUniformVec4(
                            name,
                            [nums[0], nums[1], nums[2], nums[3]],
                        ));
                    }
                    _ => return Err(Fault::new("uniform_set([..]) expects array of len 2 or 4")),
                }
            }
            _ => return Err(Fault::new("uniform_set expects (name, number | [..])")),
        }
        Ok(V::Unit)
    });

    // ---- uniform_get ----
    vm.register_raw("uniform_get", |_vm, args| {
        let name = match args.first() {
            Some(V::Str(s)) => s.to_string(),
            _ => return Err(Fault::new("uniform_get expects a name string")),
        };
        let Some(snap) = snapshot() else {
            return Ok(V::Unit);
        };
        Ok(snap.read(|d| {
            d.uniforms
                .get(&name)
                .map(V::from_json)
                .unwrap_or(V::Unit)
        }))
    });

    // ---- mask_paint / mask_fill / mask_clear ----
    vm.register5(
        "mask_paint",
        move |name: String, x: f64, y: f64, radius: f64, value: f64| {
            if let Some(tx) = script_sender() {
                let _ = tx.send(DynamicMsg::MaskPaint {
                    name,
                    x: x as f32,
                    y: y as f32,
                    radius: radius as f32,
                    value: value as f32,
                });
            }
        },
    );
    vm.register2("mask_fill", move |name: String, value: f64| {
        if let Some(tx) = script_sender() {
            let _ = tx.send(DynamicMsg::MaskFill(name, value as f32));
        }
    });
    vm.register1("mask_clear", move |name: String| {
        if let Some(tx) = script_sender() {
            let _ = tx.send(DynamicMsg::MaskFill(name, 0.0));
        }
    });

    // ---- pane_rects ----
    vm.register0("pane_rects", || -> V {
        let Some(snap) = snapshot() else {
            return V::list(vec![]);
        };
        snap.read(|d| {
            let arr: Vec<V> = d
                .pane_rects
                .iter()
                .map(|r| {
                    V::from_json(&serde_json::json!({
                        "x": r.x, "y": r.y, "w": r.w, "h": r.h, "kind": r.kind,
                    }))
                })
                .collect();
            V::list(arr)
        })
    });

    // ---- state_get / state_set ----
    vm.register_raw("state_get", |_vm, args| {
        let key = match args.first() {
            Some(V::Str(s)) => s.to_string(),
            _ => return Err(Fault::new("state_get expects (key, default)")),
        };
        let default = args.get(1).cloned().unwrap_or(V::Unit);
        let Some(snap) = snapshot() else {
            return Ok(default);
        };
        Ok(snap.read(|d| d.state.get(&key).map(V::from_json).unwrap_or(default)))
    });
    vm.register_raw("state_set", |_vm, args| {
        let key = match args.first() {
            Some(V::Str(s)) => s.to_string(),
            _ => return Err(Fault::new("state_set expects (key, value)")),
        };
        let value = args.get(1).cloned().unwrap_or(V::Unit);
        let json = value.to_json()?; // loud on non-JSON values
        if let Some(tx) = script_sender() {
            let _ = tx.send(DynamicMsg::StateSet(key, json));
        }
        Ok(V::Unit)
    });

    // ---- OkLCh / OkLab color helpers (hex, matching the funct surface) ----
    vm.register3("oklch", |l: f64, c: f64, h: f64| -> String {
        linear_rgba_to_hex(crate::oklab::oklch_to_linear_srgb(l as f32, c as f32, h as f32))
    });
    vm.register3("oklab", |l: f64, a: f64, b: f64| -> String {
        linear_rgba_to_hex(crate::oklab::oklab_to_linear_srgb(l as f32, a as f32, b as f32))
    });

    // ---- theme_contrast(a, b) → OkLab L difference [0, 100] ----
    vm.register2("theme_contrast", |a: String, b: String| -> f64 {
        let (Ok(ca), Ok(cb)) = (
            crate::theme::parse_color_string(&a),
            crate::theme::parse_color_string(&b),
        ) else {
            return 0.0;
        };
        crate::oklab::lightness_delta(ca, cb) as f64
    });
}

/// Format a `LinearRgba` as `#rrggbbaa` (alpha appended only when not 1).
fn linear_rgba_to_hex(c: bevy::color::LinearRgba) -> String {
    use bevy::color::Color;
    let srgb = Color::LinearRgba(c).to_srgba();
    let r = (srgb.red.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (srgb.green.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (srgb.blue.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = (srgb.alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    if a == 255 {
        format!("#{:02x}{:02x}{:02x}", r, g, b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", r, g, b, a)
    }
}

/// Drain queued messages on the main thread. The actual side-effects
/// (writing to material, painting masks, emitting events) are routed
/// to specific systems via small intermediate resources so this file
/// doesn't depend on every sibling module's internals.
#[derive(Resource, Default)]
pub struct PendingScriptOps {
    pub uniform_writes: Vec<UniformWrite>,
    pub mask_ops: Vec<MaskOp>,
    pub emits: Vec<(String, Value)>,
    pub schedules: Vec<(f32, String, Value)>,
    pub state_writes: Vec<(String, Value)>,
}

#[derive(Debug)]
pub enum UniformWrite {
    F32(String, f32),
    Vec2(String, [f32; 2]),
    Vec4(String, [f32; 4]),
}

#[derive(Debug)]
pub enum MaskOp {
    Paint {
        name: String,
        x: f32,
        y: f32,
        radius: f32,
        value: f32,
    },
    Fill(String, f32),
}

pub fn drain_script_msgs(
    rx: Res<ScriptReceiver>,
    mut pending: ResMut<PendingScriptOps>,
    mut bus: ResMut<EventBus>,
    mut scheduled: ResMut<ScheduledEvents>,
    time: Res<Time>,
) {
    let Ok(rx) = rx.0.lock() else { return };
    while let Ok(msg) = rx.try_recv() {
        match msg {
            DynamicMsg::SetUniformF32(n, v) => pending.uniform_writes.push(UniformWrite::F32(n, v)),
            DynamicMsg::SetUniformVec2(n, v) => pending.uniform_writes.push(UniformWrite::Vec2(n, v)),
            DynamicMsg::SetUniformVec4(n, v) => pending.uniform_writes.push(UniformWrite::Vec4(n, v)),
            DynamicMsg::MaskPaint { name, x, y, radius, value } => {
                pending.mask_ops.push(MaskOp::Paint { name, x, y, radius, value });
            }
            DynamicMsg::MaskFill(n, v) => pending.mask_ops.push(MaskOp::Fill(n, v)),
            DynamicMsg::Emit(k, p) => bus.push(k, p),
            DynamicMsg::Schedule { delay_secs, kind, payload } => {
                scheduled.items.push(ScheduledEvent {
                    fire_at: time.elapsed_secs() + delay_secs.max(0.0),
                    kind,
                    payload,
                });
            }
            DynamicMsg::StateSet(k, v) => pending.state_writes.push((k, v)),
        }
    }
}

/// Move any scheduled events whose `fire_at` has passed into the
/// pending event bus. Runs each frame after `drain_script_msgs`.
pub fn fire_scheduled_events(
    time: Res<Time>,
    mut scheduled: ResMut<ScheduledEvents>,
    mut bus: ResMut<EventBus>,
) {
    let now = time.elapsed_secs();
    let mut still_pending = Vec::with_capacity(scheduled.items.len());
    for ev in scheduled.items.drain(..) {
        if ev.fire_at <= now {
            bus.push(ev.kind, ev.payload);
        } else {
            still_pending.push(ev);
        }
    }
    scheduled.items = still_pending;
}
