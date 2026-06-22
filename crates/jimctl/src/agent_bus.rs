//! Shared helpers for jimctl agent adapters to ride the jim message bus
//! (see AGENTS-ON-THE-BUS.md).
//!
//! As of the GUI-independent bus, this talks to the `jim_bus` daemon
//! (spawning it on demand) rather than the GUI's `~/.jim/socket` + the
//! tail log. Publishing is a one-shot connect; the roster is the daemon's
//! retained replay; `follow_inbox` is a live subscription. The agent bus
//! therefore works whether or not the editor GUI is open — exactly like
//! the terminal works whether or not the GUI is open.
//!
//! The public API (signatures, the `Inbound` shape) is unchanged so the
//! `cmd_*` adapters built on it keep compiling untouched.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use jim_bus::client;
use jim_bus::proto::BusMessage;

pub fn current_cwd() -> String {
    std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Default roster label: the current directory's basename.
pub fn default_label() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string())
}

/// Publish one message onto the bus. Rides the GLOBAL channel (`project:
/// None`) the `agent.*` topics live on; `sender` carries the real origin.
/// Spawns the bus daemon if it isn't running.
pub fn publish(topic: &str, payload: Value, retain: bool, sender: &str) -> Result<(), String> {
    let msg = BusMessage {
        project: None,
        topic: topic.to_string(),
        payload_json: payload.to_string(),
        sender: sender.to_string(),
        retain,
    };
    client::publish_oneshot(&msg).map_err(|e| format!("publish to jim-bus: {e}"))
}

/// Announce presence as a retained `agent.hello.<id>` so the roster + viewer
/// discover this adapter.
pub fn announce(id: &str, label: &str) {
    let payload = json!({
        "id": id,
        "pid": std::process::id(),
        "cwd": current_cwd(),
        "label": label,
    });
    if let Err(e) = publish(&format!("agent.hello.{id}"), payload, true, id) {
        eprintln!("agent_bus: announce failed: {e}");
    }
}

/// Retract our roster entry (retained null) on clean exit.
pub fn tombstone(id: &str) {
    let _ = publish(&format!("agent.hello.{id}"), Value::Null, true, id);
}

/// Is a process still alive? Prunes ghost roster entries from hard-killed
/// sessions that never tombstoned. (The daemon sweeps too, but adapters
/// still expose this for their own roster views.)
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Latest retained `agent.hello.<id>` per session, from the daemon's
/// retained replay. Tombstoned/expired entries are already gone from the
/// store, so what comes back is the current roster.
pub fn read_roster() -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let Ok(msgs) = client::fetch_retained() else {
        return out;
    };
    for m in msgs {
        if m.project.is_some() {
            continue; // agent bus is the global (None) channel
        }
        let Some(sid) = m.topic.strip_prefix("agent.hello.") else {
            continue;
        };
        let payload: Value = serde_json::from_str(&m.payload_json).unwrap_or(Value::Null);
        if payload.is_null() {
            out.remove(sid);
        } else {
            out.insert(sid.to_string(), payload);
        }
    }
    out
}

/// Tombstone any roster entry whose announced pid is gone. The daemon does
/// this on its own timer; adapters can still nudge it (e.g. right after
/// listing the roster) so their view self-heals immediately.
pub fn sweep_dead_sessions(self_id: &str) {
    for (sid, info) in read_roster() {
        if sid == self_id {
            continue;
        }
        let Some(pid) = info.get("pid").and_then(Value::as_u64) else {
            continue;
        };
        if !pid_alive(pid as u32) {
            let _ = publish(&format!("agent.hello.{sid}"), Value::Null, true, "sweep");
        }
    }
}

/// Map an addressing string onto a bus topic, the shared convention across
/// every adapter and the `jim_send` tool: `all` → `agent.all`,
/// `agent:<id>` → `agent.inbox.<id>`, `topic:<name>` → `<name>` (raw).
/// Returns `None` for malformed input.
pub fn resolve_topic(to: &str) -> Option<String> {
    if to == "all" {
        return Some("agent.all".to_string());
    }
    if let Some(rest) = to.strip_prefix("agent:") {
        return (!rest.is_empty()).then(|| format!("agent.inbox.{rest}"));
    }
    if let Some(rest) = to.strip_prefix("topic:") {
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    None
}

/// One bus message addressed to us, unwrapped to the agent payload
/// convention (`{from, text, data?}`).
#[derive(Clone)]
pub struct Inbound {
    /// The real origin: payload `from` if present, else the bus `sender`.
    pub from: String,
    /// Human-readable body (payload `text`, or the whole payload as JSON).
    pub text: String,
    /// The topic it arrived on (`agent.inbox.<id>` or `agent.all`).
    pub topic: String,
    /// The full raw payload, for callers that want `data` etc.
    pub payload: Value,
}

/// Decode one delivered [`BusMessage`] into an [`Inbound`] iff it is
/// addressed to `self_id` (its inbox or `agent.all`) and is not our own
/// emit. The single decoder every adapter shares.
pub fn inbound_from(msg: &BusMessage, self_id: &str, inbox: &str) -> Option<Inbound> {
    if msg.topic != inbox && msg.topic != "agent.all" {
        return None;
    }
    let payload: Value = serde_json::from_str(&msg.payload_json).unwrap_or(Value::Null);
    let from = payload
        .get("from")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&msg.sender)
        .to_string();
    // Skip our own emits (either stamping path).
    if from == self_id || msg.sender == self_id {
        return None;
    }
    let text = match payload.get("text").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => serde_json::to_string(&payload).unwrap_or_default(),
    };
    Some(Inbound {
        from,
        text,
        topic: msg.topic.clone(),
        payload,
    })
}

/// Follow the bus live, invoking `on_msg` for every message addressed to
/// `self_id` (see [`inbound_from`]). Subscribes to the daemon (which
/// reconnects + re-spawns automatically), so this survives GUI restarts.
/// Returns when `should_stop()` reports true. `sweep` nudges the daemon to
/// prune dead peers (it also prunes on its own timer).
pub fn follow_inbox<F, S>(self_id: &str, sweep: bool, mut on_msg: F, should_stop: S)
where
    F: FnMut(Inbound),
    S: Fn() -> bool,
{
    let inbox = format!("agent.inbox.{self_id}");
    let handle = client::BusHandle::spawn();
    let mut sweep_ctr: u32 = 0;

    while !should_stop() {
        for item in handle.drain() {
            if let client::Inbound::Message(msg) = item {
                if let Some(inbound) = inbound_from(&msg, self_id, &inbox) {
                    on_msg(inbound);
                }
            }
        }
        if sweep {
            sweep_ctr += 1;
            if sweep_ctr >= 50 {
                // ~5s at the 100ms cadence below
                sweep_ctr = 0;
                sweep_dead_sessions(self_id);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
