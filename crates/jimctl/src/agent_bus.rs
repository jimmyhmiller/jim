//! Shared helpers for jimctl agent adapters to ride the jim widget message
//! bus (see AGENTS-ON-THE-BUS.md). The Claude `cmd_channel` adapter predates
//! this module and still carries its own copies; new adapters (`cmd_codex`,
//! …) use these so the wire format lives in one place.
//!
//! Wire format is duplicated from the GUI on purpose (same rationale as the
//! other `cmd_*` modules): staying lib-free of `jim-app` keeps the CLI off
//! the libghostty dylib / @rpath dance.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::{json, Value};

fn home(rel: &str) -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    for seg in rel.split('/') {
        p.push(seg);
    }
    Some(p)
}

pub fn socket_path() -> Option<PathBuf> {
    home(".jim/socket")
}

pub fn bus_log_path() -> Option<PathBuf> {
    home(".jim/widget-bus.log")
}

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

/// Publish one message onto the bus via the `widget_message` IPC action on
/// `~/.jim/socket`. `project:"global"` is the cross-project channel the
/// `agent.*` topics ride on; `sender` carries the real origin.
pub fn publish(topic: &str, payload: Value, retain: bool, sender: &str) -> Result<(), String> {
    let sock = socket_path().ok_or_else(|| "HOME not set".to_string())?;
    let mut stream = UnixStream::connect(&sock)
        .map_err(|e| format!("connect {}: {} (is the jim app running?)", sock.display(), e))?;
    let req = json!({
        "action": "widget_message",
        "project": "global",
        "topic": topic,
        "payload": payload,
        "retain": retain,
        "sender": sender,
    });
    let body = serde_json::to_vec(&req).map_err(|e| format!("serialize: {e}"))?;
    stream.write_all(&body).map_err(|e| format!("write: {e}"))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
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
/// sessions that never tombstoned.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Latest retained `agent.hello.<id>` per session (tombstones dropped), read
/// from the bus log. Truncated on app start, so it reflects the current run.
pub fn read_roster() -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let Some(log) = bus_log_path() else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(&log) else {
        return out;
    };
    for line in text.lines() {
        let Ok(m) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let topic = m.get("topic").and_then(Value::as_str).unwrap_or("");
        let Some(sid) = topic.strip_prefix("agent.hello.") else {
            continue;
        };
        let payload = m.get("payload").cloned().unwrap_or(Value::Null);
        if payload.is_null() {
            out.remove(sid);
        } else {
            out.insert(sid.to_string(), payload);
        }
    }
    out
}

/// Tombstone any roster entry whose announced pid is gone, so the roster and
/// the viewer widget self-heal. Skips our own id.
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
