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
//! ## GUI-independent transport
//!
//! The bus itself no longer lives in this process. A standalone `jim_bus`
//! daemon owns the socket, the retained store (persisted to disk), and the
//! agent roster — so the bus (and every cross-session agent on it) keeps
//! working when the GUI is closed, exactly like the terminal keeps working
//! via `jim_daemon`. The GUI is just a client:
//!
//! - A widget **publishes** with `emit(topic, payload)` (funct host fn),
//!   `WidgetMsg::Emit` (subprocess), or the `jimctl msg`/agent CLIs. The
//!   GUI forwards every emit to the daemon via [`BusHandle`].
//! - The GUI **subscribes** to the daemon; every delivered message is
//!   pushed back to the matching widgets as `on_message(topic, payload,
//!   sender)` (funct) / `HostEvent::Message` (subprocess). A widget's own
//!   emit round-trips through the daemon and comes back too — widgets
//!   ignore echoes of their own emits by `sender`.
//! - `emit_retained` keeps a message as the topic's last value (in the
//!   daemon's persisted store). A widget that spawns later receives the
//!   retained value for every topic in its project on init (MQTT-style
//!   retain) from the GUI's local mirror of that store.
//!
//! ## Flow
//!
//! `pump_widget_messages` runs every frame:
//!   1. Drain each funct widget's outbox + the external (CLI/IPC) queue and
//!      publish them to the daemon.
//!   2. Drain everything the daemon delivered to our subscription; update
//!      the local retained mirror and surface each message.
//!   3. Deliver this frame's messages to every same-project widget, and
//!      deliver the retained backlog to any widget seen for the first time.
//!
//! Project scoping uses `jim_pane::PaneProject` (a `u64` id). Widgets
//! with no project share the `None` channel. Nothing crosses projects.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use serde_json::Value;

use jim_bus::client::{BusHandle, Inbound};
use jim_bus::proto::BusMessage;
use jim_pane::{PaneKindMarker, PaneProject};

use crate::protocol::HostEvent;
use crate::script_widget::{self, ScriptWidget};
use crate::{WidgetIO, WidgetRender};

/// One message awaiting publication to the bus daemon. Produced by draining
/// widget outboxes and the external (CLI/IPC) queue.
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

/// The GUI's connection to the bus. Holds the daemon client plus a local
/// mirror of the retained store (fed by the subscription) so late-joining
/// widgets get the retained backlog without a round-trip.
#[derive(Resource)]
pub struct WidgetMsgBus {
    /// Daemon client: publishes our emits, subscribes for deliveries.
    handle: BusHandle,
    /// Local mirror of the daemon's retained store: `(project, topic) →
    /// (payload, sender)`. Populated from the subscription replay + live
    /// retained messages; used to seed late-joining widgets.
    retained: HashMap<(Option<u64>, String), (Value, String)>,
    /// Widget ids that have already received the retained backlog, so a
    /// late joiner gets it exactly once. Pruned to live widgets each pump.
    seen: HashSet<String>,
    /// Set once the daemon's initial retained replay has finished. Until
    /// then we hold off seeding widgets with the (still-arriving) backlog.
    replay_done: bool,
}

impl Default for WidgetMsgBus {
    fn default() -> Self {
        Self {
            handle: BusHandle::spawn(),
            retained: HashMap::new(),
            seen: HashSet::new(),
            replay_done: false,
        }
    }
}

impl WidgetMsgBus {
    /// Publish a message from outside the ECS (the `jimctl msg` / agent
    /// CLIs via IPC, or `ipc_stats`). Forwarded to the daemon; it comes
    /// back to us (and every other subscriber) via the subscription.
    pub fn push_external(&mut self, msg: PendingMsg) {
        self.handle.publish(BusMessage {
            project: msg.project,
            topic: msg.topic,
            payload_json: msg.payload.to_string(),
            sender: msg.sender,
            retain: msg.retain,
        });
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
    // ---- Phase 1: publish this frame's outbound emits to the daemon ----
    let mut live_ids: HashSet<String> = HashSet::new();

    for (kind, w, proj) in &script_widgets {
        if kind.0 != script_widget::PANE_KIND {
            continue;
        }
        live_ids.insert(w.widget_id.clone());
        let project = proj.map(|p| p.0);
        for out in w.drain_bus_outbox() {
            bus.handle.publish(BusMessage {
                project,
                topic: out.topic,
                payload_json: out.payload.to_string(),
                sender: w.widget_id.clone(),
                retain: out.retain,
            });
        }
    }
    // Subprocess widgets only join `live_ids` here; their *emits* are
    // collected in `tick_widget_io` (which owns the stdout channel) and
    // forwarded via `push_external`.
    for (entity, kind, _io, _render, _proj) in &sub_widgets {
        if kind.0 != crate::PANE_KIND {
            continue;
        }
        live_ids.insert(subprocess_widget_id(entity));
    }

    // ---- Phase 2: drain everything the daemon delivered to us ----
    // These are the authoritative messages (our own emits round-trip back
    // here too). Update the retained mirror and surface each one.
    let mut pending: Vec<PendingMsg> = Vec::new();
    for item in bus.handle.drain() {
        match item {
            Inbound::ReplayEnd => bus.replay_done = true,
            Inbound::Message(msg) => {
                let payload: Value = serde_json::from_str(&msg.payload_json).unwrap_or(Value::Null);
                if msg.retain {
                    let key = (msg.project, msg.topic.clone());
                    if payload.is_null() {
                        bus.retained.remove(&key); // tombstone
                    } else {
                        bus.retained.insert(key, (payload.clone(), msg.sender.clone()));
                    }
                }
                observed.write(BusMessageObserved {
                    project: msg.project,
                    topic: msg.topic.clone(),
                    payload: payload.clone(),
                    sender: msg.sender.clone(),
                });
                pending.push(PendingMsg {
                    project: msg.project,
                    topic: msg.topic,
                    payload,
                    sender: msg.sender,
                    retain: msg.retain,
                });
            }
        }
    }

    // Drop a widget id from `seen` once it's gone so a future widget that
    // happens to reuse the id (entity bits recycle) still gets a backlog.
    bus.seen.retain(|id| live_ids.contains(id));

    // ---- Phase 3: deliver ----
    // Project scoping: a message on a project channel (`Some(p)`) goes only
    // to widgets in project `p`; a message on the GLOBAL channel (`None`)
    // goes to EVERY widget regardless of project. That global broadcast is
    // what the `agent.*` bus (Claude sessions ↔ editor, see CHANNELS.md)
    // rides on. The retained backlog is only seeded once the daemon's
    // initial replay has completed, so a late joiner sees the full store.
    let seed_backlog = bus.replay_done;
    let mut newly_seen: Vec<String> = Vec::new();

    for (kind, w, proj) in &script_widgets {
        if kind.0 != script_widget::PANE_KIND {
            continue;
        }
        let pk = proj.map(|p| p.0);
        if seed_backlog && !bus.seen.contains(&w.widget_id) {
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
        if seed_backlog && !bus.seen.contains(&id) {
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

pub struct WidgetMsgBusPlugin;

impl Plugin for WidgetMsgBusPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WidgetMsgBus>()
            .add_message::<BusMessageObserved>()
            .add_systems(Update, pump_widget_messages);
    }
}
