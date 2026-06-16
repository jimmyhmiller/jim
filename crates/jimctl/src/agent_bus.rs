//! Shared helpers for jimctl agent adapters to ride the jim widget message
//! bus (see AGENTS-ON-THE-BUS.md). The Claude `cmd_channel` adapter predates
//! this module and still carries its own copies; new adapters (`cmd_codex`,
//! …) use these so the wire format lives in one place.
//!
//! Wire format is duplicated from the GUI on purpose (same rationale as the
//! other `cmd_*` modules): staying lib-free of `jim-app` keeps the CLI off
//! the libghostty dylib / @rpath dance.

use std::collections::BTreeMap;
use std::io::{BufRead, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

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

/// One bus message addressed to us, already unwrapped to the agent payload
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

/// Parse one bus-log line into an [`Inbound`] iff it is addressed to
/// `self_id` (its inbox or `agent.all`) and is not our own emit. This is the
/// single decoder every adapter shares so the "addressed to me, by whom,
/// saying what" rules live in one place.
pub fn parse_inbound(line: &str, self_id: &str, inbox: &str) -> Option<Inbound> {
    if line.is_empty() {
        return None;
    }
    let m: Value = serde_json::from_str(line).ok()?;
    let topic = m.get("topic").and_then(Value::as_str).unwrap_or("");
    if topic != inbox && topic != "agent.all" {
        return None;
    }
    let bus_sender = m.get("sender").and_then(Value::as_str).unwrap_or("");
    let payload = m.get("payload").cloned().unwrap_or(Value::Null);
    let from = payload
        .get("from")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(bus_sender)
        .to_string();
    // Skip our own emits (either stamping path).
    if from == self_id || bus_sender == self_id {
        return None;
    }
    let text = match payload.get("text").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => serde_json::to_string(&payload).unwrap_or_default(),
    };
    Some(Inbound { from, text, topic: topic.to_string(), payload })
}

/// Follow the bus log from the current end, invoking `on_msg` for every
/// message addressed to `self_id` (see [`parse_inbound`]). Polls every 200ms
/// like `tail -f`, resets on truncation (app restart), and — when `sweep` is
/// set — prunes dead roster peers every ~5s. Returns when `should_stop()`
/// reports true. This is the inbox stream the adapters used to hand-roll.
pub fn follow_inbox<F, S>(self_id: &str, sweep: bool, mut on_msg: F, should_stop: S)
where
    F: FnMut(Inbound),
    S: Fn() -> bool,
{
    let Some(log) = bus_log_path() else {
        eprintln!("agent_bus: HOME not set; cannot tail bus");
        return;
    };
    let inbox = format!("agent.inbox.{self_id}");
    let mut pos: u64 = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let mut sweep_ctr: u32 = 0;

    while !should_stop() {
        if let Ok(mut f) = std::fs::File::open(&log) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0; // truncated on app restart → start over
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut reader = std::io::BufReader::new(&mut f);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line.ends_with('\n') => {
                            if let Some(msg) = parse_inbound(line.trim_end(), self_id, &inbox) {
                                on_msg(msg);
                            }
                        }
                        // Partial line (mid-write) or read error: re-read next pass.
                        _ => break,
                    }
                }
                pos = f.stream_position().unwrap_or(len);
            }
        }
        if sweep {
            sweep_ctr += 1;
            if sweep_ctr >= 25 {
                sweep_ctr = 0;
                sweep_dead_sessions(self_id);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
