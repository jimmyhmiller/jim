//! Widget↔widget message bus — a general signalling channel that lets
//! several widget panes coordinate as one app (e.g. an editor pane, a
//! results pane and a schema browser making up a "SQL IDE").
//!
//! This is deliberately **separate** from the Claude Code event bus
//! (`ClaudeEvent` / `on_bus`). The Claude bus carries hook events the
//! host receives from Claude Code; this carries control messages widgets
//! send each other ("run this query", "query finished", "table
//! selected"). They never share a channel.
//!
//! ## Model
//!
//! - A widget **publishes** with `emit(topic, payload)` (funct host fn) or
//!   `WidgetMsg::Emit` (subprocess) or the `tbmsg` CLI (via IPC). The host
//!   serializes the payload; scripts pass native values, never JSON.
//! - Every widget in the **same editor project** receives the message as
//!   a pushed `on_message(topic, payload, sender)` (funct) /
//!   `HostEvent::Message` (subprocess). Delivery wakes the receiver —
//!   there is no polling and no `set_animating` requirement.
//! - `sender` is the publishing widget's id, so a widget can ignore
//!   echoes of its own emits and address targeted replies.
//! - `emit_retained` keeps a message as the topic's last value. A widget
//!   that spawns later receives the retained value for every topic in its
//!   project on init (MQTT-style retain), so late joiners learn current
//!   state without asking.
//!
//! ## Flow
//!
//! `pump_widget_messages` runs every frame:
//!   1. Drain each funct widget's outbox + the `external` queue (CLI/IPC).
//!   2. Update the retained store for any `retain` messages.
//!   3. Deliver this frame's messages to every same-project widget, and
//!      deliver the retained backlog to any widget seen for the first time.
//!
//! Project scoping uses `jim_pane::PaneProject` (a `u64` id). Widgets
//! with no project share the `None` channel. Nothing crosses projects.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;

use bevy::prelude::*;
use serde_json::Value;

use jim_pane::{PaneKindMarker, PaneProject};

use crate::protocol::HostEvent;
use crate::script_widget::{self, ScriptWidget};
use crate::{WidgetIO, WidgetRender};

/// One message awaiting delivery on the widget↔widget bus. Produced by
/// draining widget outboxes and the external (CLI/IPC) queue.
pub struct PendingMsg {
    /// Project channel. `Some(p)` is delivered only to widgets in project
    /// `p`; `None` is the GLOBAL channel, delivered to every widget (this
    /// is what the cross-project `agent.*` bus rides on — see CHANNELS.md).
    pub project: Option<u64>,
    pub topic: String,
    pub payload: Value,
    /// Publishing widget's id (`"tbmsg"` for the CLI).
    pub sender: String,
    /// Keep as the topic's retained last value for late joiners.
    pub retain: bool,
}

/// A bus message as delivered this frame, surfaced as a Bevy message so
/// app-side systems can observe bus traffic without being widgets. The
/// `jim.action` consumer (see CHANNELS.md) reads these to let any
/// participant — a Claude session via the bridge, or a funct widget —
/// drive the editor. Emitted once per delivered message; NOT for the
/// retained backlog redelivered to a late joiner.
#[derive(Message, Debug, Clone)]
pub struct BusMessageObserved {
    pub project: Option<u64>,
    pub topic: String,
    pub payload: Value,
    pub sender: String,
}

/// Central state for the widget↔widget bus. Ephemeral: retained values
/// live only in memory (the debug log on disk is for `tbmsg tail`, not a
/// persistence layer — it is truncated on app start).
#[derive(Resource, Default)]
pub struct WidgetMsgBus {
    /// (project, topic) → last retained (payload, sender).
    retained: HashMap<(Option<u64>, String), (Value, String)>,
    /// Widget ids that have already received the retained backlog, so a
    /// late joiner gets it exactly once. Pruned to live widgets each pump.
    seen: HashSet<String>,
    /// Messages injected from outside the ECS (the `tbmsg` CLI via IPC),
    /// drained next pump. The host pushes here from its IPC handler.
    external: Vec<PendingMsg>,
}

impl WidgetMsgBus {
    /// Inject a message from outside the ECS (the `tbmsg` CLI / IPC).
    /// Delivered on the next `pump_widget_messages` tick.
    pub fn push_external(&mut self, msg: PendingMsg) {
        self.external.push(msg);
    }
}

/// Best-effort NDJSON debug log of every delivered message, for
/// `tbmsg tail`. Truncated when the app starts (this fn is called once at
/// resource init) so it never grows without bound across restarts.
fn bus_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".jim");
    p.push("widget-bus.log");
    Some(p)
}

fn truncate_bus_log() {
    if let Some(p) = bus_log_path() {
        let _ = std::fs::write(&p, b"");
    }
}

fn append_bus_log(m: &PendingMsg) {
    let Some(p) = bus_log_path() else { return };
    let line = serde_json::json!({
        "project": m.project,
        "topic": m.topic,
        "sender": m.sender,
        "retain": m.retain,
        "payload": m.payload,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{}", line);
    }
}

/// Drives the bus once per frame. See module docs for the three phases.
fn pump_widget_messages(
    mut bus: ResMut<WidgetMsgBus>,
    mut observed: MessageWriter<BusMessageObserved>,
    script_widgets: Query<(&PaneKindMarker, &ScriptWidget, Option<&PaneProject>)>,
    sub_widgets: Query<(
        Entity,
        &PaneKindMarker,
        &WidgetIO,
        &WidgetRender,
        Option<&PaneProject>,
    )>,
) {
    // ---- Phase 1: collect this frame's outbound messages ----
    // External (CLI/IPC) messages first so their relative order is kept.
    let mut pending: Vec<PendingMsg> = std::mem::take(&mut bus.external);
    let mut live_ids: HashSet<String> = HashSet::new();

    for (kind, w, proj) in &script_widgets {
        if kind.0 != script_widget::PANE_KIND {
            continue;
        }
        live_ids.insert(w.widget_id.clone());
        let project = proj.map(|p| p.0);
        for out in w.drain_bus_outbox() {
            pending.push(PendingMsg {
                project,
                topic: out.topic,
                payload: out.payload,
                sender: w.widget_id.clone(),
                retain: out.retain,
            });
        }
    }
    // Subprocess widgets only join `live_ids` here; their *emits* are
    // collected in `tick_widget_io` (which owns the stdout channel) and
    // pushed onto `bus.external`.
    for (entity, kind, _io, _render, _proj) in &sub_widgets {
        if kind.0 != crate::PANE_KIND {
            continue;
        }
        live_ids.insert(subprocess_widget_id(entity));
    }

    // ---- Phase 2: update the retained store ----
    for m in &pending {
        if m.retain {
            bus.retained.insert(
                (m.project, m.topic.clone()),
                (m.payload.clone(), m.sender.clone()),
            );
        }
        append_bus_log(m);
        // Surface every delivered message to app-side observers (the
        // `jim.action` consumer, monitors, …).
        observed.write(BusMessageObserved {
            project: m.project,
            topic: m.topic.clone(),
            payload: m.payload.clone(),
            sender: m.sender.clone(),
        });
    }
    // Drop a widget id from `seen` once it's gone so a future widget that
    // happens to reuse the id (entity bits recycle) still gets a backlog.
    bus.seen.retain(|id| live_ids.contains(id));

    // ---- Phase 3: deliver ----
    // Project scoping: a message on a project channel (`Some(p)`) goes only
    // to widgets in project `p`; a message on the GLOBAL channel (`None`)
    // goes to EVERY widget regardless of project. That global broadcast is
    // what the `agent.*` bus (Claude sessions ↔ editor, see CHANNELS.md)
    // rides on, so any widget in any project can observe/participate.
    // Collect ids that need the retained backlog this pass, then mark them
    // seen after the immutable delivery loops (can't mutate `bus` mid-read).
    let mut newly_seen: Vec<String> = Vec::new();

    for (kind, w, proj) in &script_widgets {
        if kind.0 != script_widget::PANE_KIND {
            continue;
        }
        let pk = proj.map(|p| p.0);
        if !bus.seen.contains(&w.widget_id) {
            for ((rpk, topic), (payload, sender)) in &bus.retained {
                if rpk.is_none() || *rpk == pk {
                    w.deliver_bus_message(topic.clone(), payload.clone(), sender.clone());
                }
            }
            newly_seen.push(w.widget_id.clone());
        }
        for m in &pending {
            if m.project.is_none() || m.project == pk {
                w.deliver_bus_message(m.topic.clone(), m.payload.clone(), m.sender.clone());
            }
        }
    }

    for (entity, kind, io, render, proj) in &sub_widgets {
        if kind.0 != crate::PANE_KIND || !render.init_sent {
            // Not initialized yet: it'll pick up the retained backlog on
            // the first pump after its `init` line goes out.
            continue;
        }
        let pk = proj.map(|p| p.0);
        let id = subprocess_widget_id(entity);
        if !bus.seen.contains(&id) {
            for ((rpk, topic), (payload, sender)) in &bus.retained {
                if rpk.is_none() || *rpk == pk {
                    send_sub_message(io, topic.clone(), payload.clone(), sender.clone());
                }
            }
            newly_seen.push(id);
        }
        for m in &pending {
            if m.project.is_none() || m.project == pk {
                send_sub_message(io, m.topic.clone(), m.payload.clone(), m.sender.clone());
            }
        }
    }

    bus.seen.extend(newly_seen);
}

/// Stable bus id for a subprocess widget pane. Mirrors the funct side's
/// `rw{bits}` scheme so the two id namespaces never collide.
pub(crate) fn subprocess_widget_id(entity: Entity) -> String {
    format!("sw{:x}", entity.to_bits())
}

fn send_sub_message(io: &WidgetIO, topic: String, payload: Value, sender: String) {
    let ev = HostEvent::Message {
        topic,
        payload,
        sender,
    };
    if let Ok(json) = serde_json::to_string(&ev) {
        let _ = io.tx.send(json);
    }
}

/// An `agent.hello.<id>` entry is stale once its heartbeat is this old.
const ROSTER_STALE_SECS: u64 = 35;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Is a process still alive? (Prunes dead roster entries whose session never
/// tombstoned.) `kill(pid, 0)`: 0 = alive; EPERM = alive but not ours.
fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Periodically expire stale `agent.hello.<id>` roster entries from the
/// retained store, so the agent roster / viewer widget self-heal when a
/// session dies without tombstoning. Unlike the bridges (which sweep by
/// reading the bus log), this runs in the editor and sees the in-memory
/// retained store directly — so it also clears legacy entries that predate
/// the log's last truncation. An entry is stale when its announced `pid` is
/// gone, or (for heartbeating sessions) its `ts` is too old. We publish a
/// retained tombstone so live widgets drop it immediately, exactly like a
/// clean exit would.
fn sweep_stale_roster(mut bus: ResMut<WidgetMsgBus>, time: Res<Time>, mut acc: Local<f32>) {
    *acc += time.delta_secs();
    if *acc < 10.0 {
        return;
    }
    *acc = 0.0;

    let now = now_secs();
    let stale: Vec<String> = bus
        .retained
        .iter()
        .filter_map(|((proj, topic), (payload, _))| {
            if proj.is_some() || payload.is_null() {
                return None; // agent bus is the global (None) channel; skip tombstones
            }
            let id = topic.strip_prefix("agent.hello.")?;
            let pid_dead = payload
                .get("pid")
                .and_then(Value::as_i64)
                .map(|p| !pid_alive(p))
                .unwrap_or(false);
            let ts_stale = payload
                .get("ts")
                .and_then(Value::as_u64)
                .map(|ts| now.saturating_sub(ts) > ROSTER_STALE_SECS)
                .unwrap_or(false);
            (pid_dead || ts_stale).then(|| id.to_string())
        })
        .collect();

    for id in stale {
        bus.push_external(PendingMsg {
            project: None,
            topic: format!("agent.hello.{id}"),
            payload: Value::Null,
            sender: "roster-sweep".to_string(),
            retain: true,
        });
    }
}

pub struct WidgetMsgBusPlugin;

impl Plugin for WidgetMsgBusPlugin {
    fn build(&self, app: &mut App) {
        truncate_bus_log();
        app.init_resource::<WidgetMsgBus>()
            .add_message::<BusMessageObserved>()
            .add_systems(Update, (pump_widget_messages, sweep_stale_roster));
    }
}
