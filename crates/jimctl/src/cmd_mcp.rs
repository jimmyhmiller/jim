//! `jimctl mcp` — a tiny MCP stdio server that gives ANY MCP-client agent the
//! jim bus **outbound** tools: `jim_send`, `jim_roster`, `jim_do`. It is the
//! "deliberate outbound" half of an adapter (see AGENTS-ON-THE-BUS.md),
//! decoupled from inbound so it can be dropped into agents whose inbound is
//! handled elsewhere — notably **Codex** (`jimctl codex` injects bus messages
//! as live turns; this server lets the codex agent reply/broadcast/message
//! peers *by choice*).
//!
//! Register it with codex once:
//!   codex mcp add jim --env JIM_AGENT_ID=codex-<dir> -- jimctl mcp
//! (`jimctl codex` does this for you.) Then the agent has jim_send/jim_roster/
//! jim_do as tools.
//!
//! This server does NOT announce a roster entry or tail the bus — it is pure
//! outbound. Presence/inbound belong to the adapter that owns the session.
//!
//! Identity: publishes with sender = $JIM_AGENT_ID (else "mcp-agent"). Set it
//! to the owning adapter's id so replies route correctly.
//!
//! IMPORTANT: stdout is the JSON-RPC channel — only well-formed messages there;
//! diagnostics go to stderr.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use serde_json::{json, Value};

use crate::agent_bus;

const SERVER_NAME: &str = "jim";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "\
You are connected to the jim editor's agent bus. Use these tools to collaborate \
with other agents and drive the editor — only when it's useful. jim_send \
messages another agent (to=their id), replies to whoever messaged you, or \
broadcasts (to=\"all\"). jim_roster lists who's online. jim_do runs an editor \
action. Incoming bus messages arrive as normal turns prefixed with their \
sender; reply with jim_send if warranted.";

fn self_id() -> String {
    std::env::var("JIM_AGENT_ID").unwrap_or_else(|_| "mcp-agent".to_string())
}

pub fn run() -> ExitCode {
    let id = self_id();
    eprintln!("jimctl mcp: bus tool server, sender id = {id}");
    let stdout = io::stdout();
    let stdin = io::stdin();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // stdin closed → client exited
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("jimctl mcp: bad JSON-RPC line: {e}");
                continue;
            }
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let req_id = msg.get("id").cloned();

        match method {
            "initialize" => {
                let proto = msg
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL)
                    .to_string();
                if let Some(rid) = req_id {
                    respond(
                        &stdout,
                        rid,
                        json!({
                            "protocolVersion": proto,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                            "instructions": INSTRUCTIONS,
                        }),
                    );
                }
            }
            "tools/list" => {
                if let Some(rid) = req_id {
                    respond(&stdout, rid, json!({ "tools": tool_schemas() }));
                }
            }
            "tools/call" => {
                if let Some(rid) = req_id {
                    let result = handle_tool_call(&id, msg.get("params"));
                    respond(&stdout, rid, result);
                }
            }
            "ping" => {
                if let Some(rid) = req_id {
                    respond(&stdout, rid, json!({}));
                }
            }
            "notifications/initialized" | "notifications/cancelled" | "" => { /* ignore */ }
            other => {
                if let Some(rid) = req_id {
                    error(&stdout, rid, -32601, &format!("method not found: {other}"));
                }
            }
        }
    }
    ExitCode::SUCCESS
}

fn write_msg(out: &io::Stdout, v: &Value) {
    let mut o = out.lock();
    let _ = serde_json::to_writer(&mut o, v);
    let _ = o.write_all(b"\n");
    let _ = o.flush();
}
fn respond(out: &io::Stdout, id: Value, result: Value) {
    write_msg(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}
fn error(out: &io::Stdout, id: Value, code: i64, message: &str) {
    write_msg(out, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
}

fn tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "jim_send",
            "description": "Send a message to other agents on the jim bus. `to` is an \
                agent id (to reply to whoever messaged you, or reach a specific peer) or \
                'all' to broadcast to everyone. Use it to reply, ask a peer for help, \
                hand off work, or announce something.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "agent id, or 'all' to broadcast" },
                    "text": { "type": "string", "description": "the message" }
                },
                "required": ["to", "text"]
            }
        }),
        json!({
            "name": "jim_roster",
            "description": "List the other agents currently live on the jim bus (id, name, cwd).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "jim_do",
            "description": "Drive the jim editor: dispatch an editor action (open_file, \
                spawn_widget, add_issue, …). `params` are that action's fields.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string" },
                    "params": { "type": "object" }
                },
                "required": ["action"]
            }
        }),
    ]
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn handle_tool_call(self_id: &str, params: Option<&Value>) -> Value {
    let Some(params) = params else {
        return tool_text("missing params", true);
    };
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "jim_send" => tool_send(self_id, &args),
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
    // Accept "all", "agent:<id>", "topic:<name>", or a bare id (→ that inbox).
    let topic = agent_bus::resolve_topic(to)
        .unwrap_or_else(|| format!("agent.inbox.{to}"));
    let payload = json!({ "from": self_id, "text": text });
    match agent_bus::publish(&topic, payload, false, self_id) {
        Ok(()) => tool_text(&format!("sent to {topic}"), false),
        Err(e) => tool_text(&format!("send failed: {e}"), true),
    }
}

fn tool_roster(self_id: &str) -> Value {
    let mut lines = Vec::new();
    for (sid, info) in agent_bus::read_roster() {
        if sid == self_id {
            continue;
        }
        if let Some(pid) = info.get("pid").and_then(Value::as_u64) {
            if !agent_bus::pid_alive(pid as u32) {
                continue;
            }
        }
        let label = info.get("label").and_then(Value::as_str).filter(|s| !s.is_empty());
        let cwd = info.get("cwd").and_then(Value::as_str).unwrap_or("");
        match label {
            Some(l) => lines.push(format!("{sid} — \"{l}\"  {cwd}")),
            None => lines.push(format!("{sid}  {cwd}")),
        }
    }
    let out = if lines.is_empty() { "no other agents on the bus".to_string() } else { lines.join("\n") };
    tool_text(&out, false)
}

fn tool_do(self_id: &str, args: &Value) -> Value {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if action.is_empty() {
        return tool_text("jim_do requires an `action`", true);
    }
    let mut payload = match args.get("params") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(_) => return tool_text("jim_do `params` must be a JSON object", true),
    };
    payload["action"] = Value::String(action.to_string());
    match agent_bus::publish("jim.action", payload, false, self_id) {
        Ok(()) => tool_text(&format!("dispatched editor action '{action}'"), false),
        Err(e) => tool_text(&format!("dispatch failed: {e}"), true),
    }
}
