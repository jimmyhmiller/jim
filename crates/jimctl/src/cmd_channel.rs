//! `jimctl channel` — an MCP "channel" that bridges a Claude Code session
//! to jim's widget↔widget message bus. See `CHANNELS.md` for the full
//! design. This implements **Phase 0 + Phase 1**:
//!
//!   - inbound (bus → Claude): subscribe to the `jim_bus` daemon, forward
//!     subscribed topics as `notifications/claude/channel`;
//!   - outbound (Claude → bus) tools: `jim_send`, `jim_subscribe`,
//!     `jim_unsubscribe`, `jim_identify`, `jim_roster`.
//!
//! Claude Code spawns this as a stdio subprocess and speaks newline-
//! delimited JSON-RPC 2.0 to it. The bus transport is the standalone
//! `jim_bus` daemon (spawned on demand), so the bridge works whether or
//! not the editor GUI is open — the agent bus is GUI-independent, just
//! like the terminal.
//!
//! IMPORTANT: stdout is the JSON-RPC channel — only well-formed messages
//! may go there. All diagnostics must use stderr (`eprintln!`).

use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::agent_bus;
use jim_bus::client;
use jim_bus::proto::BusMessage;

const SERVER_NAME: &str = "jim";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Fallback MCP protocol version if the client doesn't send one. We echo
/// the client's requested version when present.
const DEFAULT_PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "\
You are connected to the jim editor over its agent bus. Messages from jim \
arrive as <channel source=\"jim\" topic=\"...\" sender=\"...\">; the body is \
the message text. Use jim_send to talk back: to=\"agent:<sender>\" replies to \
one session, to=\"all\" broadcasts, to=\"topic:<name>\" publishes on any bus \
topic (e.g. a funct widget's). jim_roster lists the other live sessions; \
jim_identify sets your display name; jim_subscribe/jim_unsubscribe control \
which extra topics reach you. jim_do drives the editor itself (open a file, \
spawn a widget, file an issue, …). You always receive your own inbox and agent.all.";

/// Shared mutable channel state: which topics we forward inbound, and our
/// current roster label. Guarded so the request loop and the tail thread
/// agree on the subscription set.
struct State {
    subs: HashSet<String>,
    label: Option<String>,
}

type Shared = Arc<Mutex<State>>;
type Out = Arc<Mutex<io::Stdout>>;

/// This session's bus address. Overridable for testing via `JIM_CHANNEL_ID`;
/// otherwise derived from the process id so it's unique per live session.
fn self_id() -> String {
    std::env::var("JIM_CHANNEL_ID").unwrap_or_else(|_| format!("sess_{}", std::process::id()))
}

fn current_cwd() -> String {
    std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Seconds since the epoch (for roster heartbeats / staleness).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How often we re-announce, and when a heartbeat is considered stale.
const HEARTBEAT_SECS: u64 = 10;
const STALE_SECS: u64 = 35;

/// Was this session actually launched as a channel (`--channels … jim …`)?
///
/// A channel MCP server cannot tell from the MCP handshake whether Claude Code
/// loaded it as a *channel* (inbound delivery on) vs an ordinary plugin MCP
/// server (tools work, but `notifications/claude/channel` are silently dropped)
/// — notifications are unacknowledged. The one place the truth lives is the
/// launch flag, which is per-session (no config form), so we detect it by
/// walking up our process ancestry for a `claude … --channels … jim` argv.
/// Only a real channel announces on the roster, so the roster reflects
/// reachable sessions, not merely "plugin loaded".
fn on_channel() -> bool {
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..6 {
        if pid <= 1 {
            break;
        }
        let args = proc_args(pid);
        // Match only the real `claude` process launched with --channels — NOT
        // any ancestor that merely mentions those words (a shell, a script,
        // this very tool's command line). Require argv[0]'s basename to be
        // `claude` and `--channels` to appear as its own token.
        if let Some(prog) = args.split_whitespace().next() {
            let is_claude = std::path::Path::new(prog)
                .file_name()
                .map(|n| n == "claude")
                .unwrap_or(false);
            if is_claude && args.split_whitespace().any(|t| t == "--channels") {
                return true;
            }
        }
        pid = proc_ppid(pid);
    }
    false
}

/// `ps -o args=` for a pid (empty string on failure).
fn proc_args(pid: i32) -> String {
    std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `ps -o ppid=` for a pid (0 on failure → stops the walk).
fn proc_ppid(pid: i32) -> i32 {
    std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Write one JSON-RPC message as a single line on stdout, serialized
/// against the shared lock so the tail thread and the request loop don't
/// interleave.
fn write_msg(out: &Out, v: &Value) {
    if let Ok(mut o) = out.lock() {
        let _ = serde_json::to_writer(&mut *o, v);
        let _ = o.write_all(b"\n");
        let _ = o.flush();
    }
}

fn respond_result(out: &Out, id: Value, result: Value) {
    write_msg(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn respond_error(out: &Out, id: Value, code: i64, message: &str) {
    write_msg(
        out,
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    );
}

/// Publish one message onto the agent bus (the GUI-independent `jim_bus`
/// daemon, spawned on demand). `sender` carries this session's id so
/// `on_message` and reply-by-sender see the real origin.
fn publish(topic: &str, payload: Value, retain: bool, sender: &str) -> Result<(), String> {
    agent_bus::publish(topic, payload, retain, sender)
}

pub fn run() -> ExitCode {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let id = self_id();
    let inbox = format!("agent.inbox.{id}");
    eprintln!("jimctl channel: session id = {id}");

    // Always-on subscriptions: our own inbox and the broadcast topic.
    let mut subs = HashSet::new();
    subs.insert(inbox.clone());
    subs.insert("agent.all".to_string());
    let shared: Shared = Arc::new(Mutex::new(State { subs, label: None }));

    let stdin = io::stdin();
    let mut tail_started = false;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // stdin closed → Claude Code exited
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("jimctl channel: bad JSON-RPC line: {e}");
                continue;
            }
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        // Requests carry an `id`; notifications don't.
        let req_id = msg.get("id").cloned();

        match method {
            "initialize" => {
                let proto = msg
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL)
                    .to_string();
                let result = json!({
                    "protocolVersion": proto,
                    "capabilities": {
                        // Presence of this key is what makes us a channel.
                        "experimental": { "claude/channel": {} },
                        "tools": {},
                    },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                    "instructions": INSTRUCTIONS,
                });
                if let Some(id) = req_id {
                    respond_result(&out, id, result);
                }
            }

            "notifications/initialized" => {
                // Safe to start emitting now. But only a session actually
                // launched with `--channels` can RECEIVE — so only those tail
                // the bus, announce on the roster, and heartbeat. A plain
                // plugin-MCP load still has working tools (tools/call below);
                // it just doesn't pollute the roster with an unreachable entry.
                if !tail_started {
                    tail_started = true;
                    if on_channel() {
                        spawn_tail(out.clone(), shared.clone(), id.clone());
                        announce(&id, &shared);
                        spawn_heartbeat(id.clone(), shared.clone());
                    } else {
                        eprintln!(
                            "jimctl channel: not launched with `--channels` — tools work, \
                             but this session can't receive pings, so it is NOT listed on \
                             the roster. Relaunch with `claude --channels plugin:jim@jim-local` \
                             to be reachable."
                        );
                    }
                }
            }

            "tools/list" => {
                if let Some(id) = req_id {
                    respond_result(&out, id, json!({ "tools": tool_schemas() }));
                }
            }

            "tools/call" => {
                if let Some(id) = req_id {
                    let result = handle_tool_call(&self_id(), &shared, msg.get("params"));
                    respond_result(&out, id, result);
                }
            }

            "ping" => {
                if let Some(id) = req_id {
                    respond_result(&out, id, json!({}));
                }
            }

            "notifications/cancelled" | "" => { /* notification or a response we ignore */ }

            other => {
                if let Some(id) = req_id {
                    respond_error(&out, id, -32601, &format!("method not found: {other}"));
                }
            }
        }
    }

    // Clean exit: tombstone our roster entry so viewers drop us. (A hard
    // kill skips this; viewers should treat a stale hello tolerantly.)
    let _ = publish(&format!("agent.hello.{id}"), Value::Null, true, &id);
    ExitCode::SUCCESS
}

/// Every tool we advertise in `tools/list`.
fn tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "jim_send",
            "description": "Send a message onto the jim agent bus: to another \
                Claude session, to all sessions, or to an arbitrary bus topic \
                (e.g. a funct widget's topic).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Destination: \
                        \"agent:<id>\" for one session, \"all\" to broadcast, \
                        or \"topic:<name>\" for a raw bus topic." },
                    "text": { "type": "string", "description": "Human-readable message body." },
                    "data": { "description": "Optional structured JSON payload." }
                },
                "required": ["to", "text"]
            }
        }),
        json!({
            "name": "jim_subscribe",
            "description": "Start forwarding messages on a bus topic into this \
                session (in addition to your inbox and agent.all).",
            "inputSchema": {
                "type": "object",
                "properties": { "topic": { "type": "string" } },
                "required": ["topic"]
            }
        }),
        json!({
            "name": "jim_unsubscribe",
            "description": "Stop forwarding a previously-subscribed bus topic.",
            "inputSchema": {
                "type": "object",
                "properties": { "topic": { "type": "string" } },
                "required": ["topic"]
            }
        }),
        json!({
            "name": "jim_identify",
            "description": "Set this session's display name on the editor's \
                agent roster.",
            "inputSchema": {
                "type": "object",
                "properties": { "label": { "type": "string" } },
                "required": ["label"]
            }
        }),
        json!({
            "name": "jim_roster",
            "description": "List the live Claude sessions currently on the agent \
                bus (id, label, cwd).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "jim_do",
            "description": "Drive the jim editor: dispatch an editor action (the \
                same action surface the CLIs use). `action` is the action name; \
                `params` are its fields.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "e.g. open_file, \
                        spawn_widget, suggest_pane, add_issue, open_palette, \
                        activate_project, screenshot, close_project_panes." },
                    "params": { "type": "object", "description": "Fields for that \
                        action, e.g. {\"path\":\"/x/y.rs\",\"project\":\"Recursion\"}." }
                },
                "required": ["action"]
            }
        }),
    ]
}

/// Map a `to` address onto a bus topic. Returns `None` for malformed input.
fn resolve_topic(to: &str) -> Option<String> {
    if to == "all" {
        return Some("agent.all".to_string());
    }
    if let Some(rest) = to.strip_prefix("agent:") {
        if rest.is_empty() {
            return None;
        }
        return Some(format!("agent.inbox.{rest}"));
    }
    if let Some(rest) = to.strip_prefix("topic:") {
        if rest.is_empty() {
            return None;
        }
        return Some(rest.to_string());
    }
    None
}

/// Build a tool-call result. Tool-level failures are reported as an
/// `isError` result (per MCP), not a JSON-RPC error.
fn tool_text(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn handle_tool_call(self_id: &str, shared: &Shared, params: Option<&Value>) -> Value {
    let Some(params) = params else {
        return tool_text("missing params", true);
    };
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "jim_send" => tool_send(self_id, &args),
        "jim_subscribe" => tool_subscribe(shared, &args, true),
        "jim_unsubscribe" => tool_subscribe(shared, &args, false),
        "jim_identify" => tool_identify(self_id, shared, &args),
        "jim_roster" => tool_roster(self_id),
        "jim_do" => tool_do(self_id, &args),
        other => tool_text(&format!("unknown tool: {other}"), true),
    }
}

fn tool_send(self_id: &str, args: &Value) -> Value {
    let to = args.get("to").and_then(Value::as_str).unwrap_or("");
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if to.is_empty() || text.is_empty() {
        return tool_text("jim_send requires non-empty `to` and `text`", true);
    }
    let Some(topic) = resolve_topic(to) else {
        return tool_text(
            "jim_send `to` must be \"agent:<id>\", \"all\", or \"topic:<name>\"",
            true,
        );
    };
    let mut payload = json!({ "from": self_id, "text": text });
    if let Some(data) = args.get("data") {
        if !data.is_null() {
            payload["data"] = data.clone();
        }
    }
    match publish(&topic, payload, false, self_id) {
        Ok(()) => tool_text(&format!("sent to {topic}"), false),
        Err(e) => tool_text(&format!("send failed: {e}"), true),
    }
}

/// Drive the editor by publishing a `jim.action` bus message whose payload
/// is an `IpcRequest` (`{action, ...params}`). The app's `dispatch_bus_actions`
/// consumer re-dispatches it. Goes on the global channel so it's not tied
/// to a project.
fn tool_do(self_id: &str, args: &Value) -> Value {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if action.is_empty() {
        return tool_text(
            "jim_do requires an `action` (e.g. \"open_file\", \"spawn_widget\")",
            true,
        );
    }
    let mut payload = match args.get("params") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(_) => return tool_text("jim_do `params` must be a JSON object", true),
    };
    payload["action"] = Value::String(action.to_string());
    match publish("jim.action", payload, false, self_id) {
        Ok(()) => tool_text(&format!("dispatched editor action '{action}'"), false),
        Err(e) => tool_text(&format!("dispatch failed: {e}"), true),
    }
}

fn tool_subscribe(shared: &Shared, args: &Value, add: bool) -> Value {
    let topic = args.get("topic").and_then(Value::as_str).unwrap_or("");
    if topic.is_empty() {
        return tool_text("requires a non-empty `topic`", true);
    }
    if let Ok(mut st) = shared.lock() {
        if add {
            st.subs.insert(topic.to_string());
            tool_text(&format!("subscribed to {topic}"), false)
        } else {
            st.subs.remove(topic);
            tool_text(&format!("unsubscribed from {topic}"), false)
        }
    } else {
        tool_text("internal lock error", true)
    }
}

fn tool_identify(self_id: &str, shared: &Shared, args: &Value) -> Value {
    let label = args.get("label").and_then(Value::as_str).unwrap_or("");
    if label.is_empty() {
        return tool_text("requires a non-empty `label`", true);
    }
    if let Ok(mut st) = shared.lock() {
        st.label = Some(label.to_string());
    }
    announce(self_id, shared);
    tool_text(&format!("identified as \"{label}\""), false)
}

fn tool_roster(self_id: &str) -> Value {
    let sessions = read_roster();
    if sessions.is_empty() {
        return tool_text("no live sessions on the agent bus", false);
    }
    let mut lines = Vec::new();
    for (sid, info) in &sessions {
        // Skip ghosts whose announced pid is gone (defensive; the tail
        // thread also tombstones them).
        if let Some(pid) = info.get("pid").and_then(Value::as_u64) {
            if sid != self_id && !pid_alive(pid as u32) {
                continue;
            }
        }
        let me = if sid == self_id { " (you)" } else { "" };
        let label = info
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let cwd = info.get("cwd").and_then(Value::as_str).unwrap_or("");
        match label {
            Some(l) => lines.push(format!("{sid}{me} — \"{l}\"  {cwd}")),
            None => lines.push(format!("{sid}{me}  {cwd}")),
        }
    }
    tool_text(&lines.join("\n"), false)
}

/// Announce this session on the roster as a retained `agent.hello.<id>`
/// so late-joining viewers (and other sessions) can discover it.
fn announce(id: &str, shared: &Shared) {
    // Explicit label (jim_identify) wins; otherwise default to the cwd's
    // basename so the roster reads nicely without a manual identify.
    let label = shared
        .lock()
        .ok()
        .and_then(|s| s.label.clone())
        .unwrap_or_else(default_label);
    let payload = json!({
        "id": id,
        "pid": std::process::id(),
        "cwd": current_cwd(),
        "label": label,
        "ts": now_secs(),       // heartbeat timestamp for staleness
        "channel": true,        // marks this as a real --channels session
    });
    if let Err(e) = publish(&format!("agent.hello.{id}"), payload, true, id) {
        eprintln!("jimctl channel: roster announce failed: {e}");
    }
}

/// Re-announce on a timer so the roster reflects a *currently-live* session:
/// the `ts` lets viewers/sweepers expire entries that stop heartbeating, which
/// is what makes dead/stale sessions drop off (a hard-killed session simply
/// stops refreshing its `ts`).
fn spawn_heartbeat(id: String, shared: Shared) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(HEARTBEAT_SECS));
        announce(&id, &shared);
    });
}

/// Default roster label: the current directory's basename.
fn default_label() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string())
}

/// Is a process still alive? Used to prune ghost roster entries left by
/// sessions that were hard-killed (no clean-exit tombstone).
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // kill(pid, 0) probes existence without sending a signal: 0 = alive;
    // EPERM = alive but not ours (still counts as alive).
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Tombstone any roster entry whose announced pid is no longer running, so
/// the roster (and the viewer widget) self-heal when a session dies without
/// a clean exit. Skips our own entry. Runs from a live session's bridge.
fn sweep_dead_sessions(self_id: &str) {
    let now = now_secs();
    for (sid, info) in read_roster() {
        if sid == self_id {
            continue;
        }
        // A heartbeating session that has gone quiet (`ts` too old) is gone,
        // even if its pid was reused by something else. Only applies to
        // entries that carry a `ts` (i.e. heartbeating channel sessions);
        // others fall back to the pid check so non-heartbeat agents
        // (codex/pi/CLI) aren't wrongly reaped.
        let stale_ts = info
            .get("ts")
            .and_then(Value::as_u64)
            .map(|ts| now.saturating_sub(ts) > STALE_SECS)
            .unwrap_or(false);
        let dead_pid = info
            .get("pid")
            .and_then(Value::as_u64)
            .map(|pid| !pid_alive(pid as u32))
            .unwrap_or(false);
        if stale_ts || dead_pid {
            let _ = publish(&format!("agent.hello.{sid}"), Value::Null, true, "sweep");
        }
    }
}

/// Latest retained `agent.hello.<id>` per session, from the daemon's
/// retained replay (tombstones already dropped). Delegates to the shared
/// SDK, which talks to the GUI-independent `jim_bus` daemon.
fn read_roster() -> BTreeMap<String, Value> {
    agent_bus::read_roster()
}

/// Subscribe to the bus daemon and forward subscribed messages into Claude
/// as channel notifications. Filters by the live subscription set. The
/// daemon (re)connects + re-spawns automatically, so the bridge survives a
/// GUI restart. The retained replay on (re)connect is skipped — channels
/// are about live events — by waiting for the first `ReplayEnd`.
fn spawn_tail(out: Out, shared: Shared, id: String) {
    std::thread::spawn(move || {
        let handle = client::BusHandle::spawn();
        // Skip the retained backlog the daemon replays on connect; forward
        // only what arrives live afterward.
        let mut live = false;
        let mut sweep_ctr: u32 = 0;
        loop {
            for item in handle.drain() {
                match item {
                    client::Inbound::ReplayEnd => live = true,
                    client::Inbound::Message(msg) => {
                        if live {
                            handle_bus_msg(&out, &shared, &id, &msg);
                        }
                    }
                }
            }
            sweep_ctr += 1;
            if sweep_ctr >= 50 {
                // ~5s at the 100ms cadence below
                sweep_ctr = 0;
                sweep_dead_sessions(&id);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

/// Decide whether a delivered bus message is for us and, if so, deliver it.
fn handle_bus_msg(out: &Out, shared: &Shared, self_id: &str, msg: &BusMessage) {
    let topic = msg.topic.as_str();
    let subscribed = shared
        .lock()
        .map(|s| s.subs.contains(topic))
        .unwrap_or(false);
    if !subscribed {
        return;
    }
    let bus_sender = msg.sender.as_str();
    if bus_sender == self_id {
        return; // don't echo our own emits back to ourselves
    }
    let payload: Value = serde_json::from_str(&msg.payload_json).unwrap_or(Value::Null);
    // The real origin: prefer the payload's `from` (survives any path that
    // doesn't stamp a real bus sender), else the bus sender. This is what
    // Claude passes back as `to: "agent:<sender>"`.
    let sender = payload
        .get("from")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(bus_sender);
    if sender == self_id {
        return;
    }
    // Prefer the conventional `text` field; otherwise hand Claude the raw
    // payload as compact JSON.
    let content = match payload.get("text").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => serde_json::to_string(&payload).unwrap_or_default(),
    };
    let note = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": { "topic": topic, "sender": sender },
        }
    });
    write_msg(out, &note);
}
