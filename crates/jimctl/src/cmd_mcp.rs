//! `jimctl mcp` — a tiny MCP stdio server that gives ANY MCP-client agent the
//! jim bus **outbound** tools: `jim_send`, `jim_roster`, `jim_do`. It is the
//! "deliberate outbound" half of an adapter (see AGENTS-ON-THE-BUS.md),
//! decoupled from inbound so it can be dropped into agents whose inbound is
//! handled elsewhere — notably **Codex** (`jimctl codex` injects bus messages
//! as live turns; this server lets the codex agent reply/broadcast/message
//! peers *by choice*).
//!
//! Codex uses the shared Streamable HTTP service, installed as a per-user
//! launch agent and registered once:
//!   jimctl mcp install
//!   codex mcp add jim --url http://127.0.0.1:37423/mcp
//! (`jimctl codex` does both.) A bare `jimctl mcp` remains as a compatibility
//! stdio transport for clients that cannot connect to Streamable HTTP.
//!
//! This server does NOT announce a roster entry or tail the bus — it is pure
//! outbound. Presence/inbound belong to the adapter that owns the session.
//!
//! The shared endpoint publishes as `codex`; message destination remains an
//! explicit tool argument. Legacy stdio uses $JIM_AGENT_ID when provided.
//!
//! IMPORTANT: stdout is the JSON-RPC channel — only well-formed messages there;
//! diagnostics go to stderr.

use std::io::{self, BufRead, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::agent_bus;

const SERVER_NAME: &str = "jim";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_PROTOCOL: &str = "2025-06-18";
const MCP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 37423);
pub const MCP_URL: &str = "http://127.0.0.1:37423/mcp";
const MAX_HTTP_BODY: u64 = 1024 * 1024;
const LAUNCHD_LABEL: &str = "com.jimmyhmiller.jim-mcp";

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
    match crate::sub_args().next().as_deref() {
        Some("serve") => run_http(),
        Some("install") => {
            let executable = match std::env::current_exe() {
                Ok(path) => path.to_string_lossy().into_owned(),
                Err(e) => {
                    eprintln!("jimctl mcp install: cannot resolve executable: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match install_shared_service(&executable) {
                Ok(()) => {
                    println!("shared Jim MCP service is listening at {MCP_URL}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("jimctl mcp install: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("jimctl mcp: unknown argument {other:?}; expected `serve` or `install`");
            ExitCode::FAILURE
        }
        None => run_stdio(),
    }
}

fn run_stdio() -> ExitCode {
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
        if let Some(reply) = dispatch(&id, &msg) {
            write_msg(&stdout, &reply);
        }
    }
    ExitCode::SUCCESS
}

fn dispatch(id: &str, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let req_id = msg.get("id").cloned()?;
    let result = match method {
        "initialize" => {
            let proto = msg
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL);
            Ok(json!({
                "protocolVersion": proto,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                "instructions": INSTRUCTIONS,
            }))
        }
        "tools/list" => Ok(json!({ "tools": tool_schemas() })),
        "tools/call" => Ok(handle_tool_call(id, msg.get("params"))),
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": req_id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": req_id, "error": { "code": code, "message": message } })
        }
    })
}

fn run_http() -> ExitCode {
    let server = match Server::http(MCP_ADDR) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("jimctl mcp serve: bind {MCP_ADDR}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("jimctl mcp serve: shared MCP service listening at {MCP_URL}");
    for mut request in server.incoming_requests() {
        let response = handle_http_request(&mut request);
        if let Err(e) = request.respond(response) {
            eprintln!("jimctl mcp serve: response failed: {e}");
        }
    }
    ExitCode::SUCCESS
}

fn handle_http_request(request: &mut tiny_http::Request) -> Response<std::io::Cursor<Vec<u8>>> {
    if request.url() != "/mcp" {
        return text_response(StatusCode(404), "not found");
    }
    if request.method() != &Method::Post {
        return text_response(
            StatusCode(405),
            "the sessionless endpoint accepts POST only",
        );
    }
    let origin_is_safe = request.headers().iter().all(|header| {
        if !header.field.equiv("Origin") {
            return true;
        }
        matches!(
            header.value.as_str(),
            "http://127.0.0.1:37423" | "http://localhost:37423"
        )
    });
    if !origin_is_safe {
        return text_response(StatusCode(403), "cross-origin requests are not allowed");
    }
    let content_type_ok = request.headers().iter().any(|header| {
        header.field.equiv("Content-Type")
            && header
                .value
                .as_str()
                .to_ascii_lowercase()
                .starts_with("application/json")
    });
    if !content_type_ok {
        return text_response(StatusCode(415), "Content-Type must be application/json");
    }

    let mut body = Vec::new();
    match request
        .as_reader()
        .take(MAX_HTTP_BODY + 1)
        .read_to_end(&mut body)
    {
        Ok(_) if body.len() as u64 <= MAX_HTTP_BODY => {}
        Ok(_) => return text_response(StatusCode(413), "request body too large"),
        Err(e) => return text_response(StatusCode(400), &format!("could not read body: {e}")),
    }
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(e) => return json_rpc_error(StatusCode(400), Value::Null, -32700, &e.to_string()),
    };
    let Some(reply) = dispatch("codex", &message) else {
        return empty_response(StatusCode(202));
    };
    json_response(StatusCode(200), &reply)
}

fn header(name: &[u8], value: &[u8]) -> Header {
    Header::from_bytes(name, value).expect("static HTTP header is valid")
}

fn empty_response(status: StatusCode) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(Vec::new()).with_status_code(status)
}

fn text_response(status: StatusCode, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(header(b"Content-Type", b"text/plain; charset=utf-8"))
}

fn json_response(status: StatusCode, value: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(serde_json::to_vec(value).expect("JSON value serializes"))
        .with_status_code(status)
        .with_header(header(b"Content-Type", b"application/json"))
}

fn json_rpc_error(
    status: StatusCode,
    id: Value,
    code: i64,
    message: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(
        status,
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    )
}

fn service_is_reachable() -> bool {
    TcpStream::connect_timeout(&MCP_ADDR, Duration::from_millis(200)).is_ok()
}

fn launch_agent_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Install and start the one per-user shared MCP service under launchd.
pub fn install_shared_service(executable: &str) -> Result<(), String> {
    if service_is_reachable() {
        return Ok(());
    }
    let plist_path = launch_agent_path()?;
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let log_dir = PathBuf::from(home).join(".jim").join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| format!("create {}: {e}", log_dir.display()))?;
    let parent = plist_path.parent().ok_or("invalid LaunchAgents path")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let stdout_log = xml_escape(&log_dir.join("mcp.log").to_string_lossy());
    let stderr_log = xml_escape(&log_dir.join("mcp.err.log").to_string_lossy());
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         <key>ProgramArguments</key><array><string>{}</string><string>mcp</string><string>serve</string></array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>KeepAlive</key><true/>\n\
         <key>ProcessType</key><string>Background</string>\n\
         <key>StandardOutPath</key><string>{stdout_log}</string>\n\
         <key>StandardErrorPath</key><string>{stderr_log}</string>\n\
         </dict></plist>\n",
        xml_escape(executable)
    );
    std::fs::write(&plist_path, plist)
        .map_err(|e| format!("write {}: {e}", plist_path.display()))?;

    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let service = format!("{domain}/{LAUNCHD_LABEL}");
    let _ = Command::new("launchctl")
        .args(["bootout", &service])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&plist_path)
        .status()
        .map_err(|e| format!("run launchctl: {e}"))?;
    if !status.success() {
        return Err(format!("launchctl bootstrap failed with {status}"));
    }
    for _ in 0..20 {
        if service_is_reachable() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "{LAUNCHD_LABEL} did not begin listening at {MCP_URL}"
    ))
}

fn write_msg(out: &io::Stdout, v: &Value) {
    let mut o = out.lock();
    let _ = serde_json::to_writer(&mut o, v);
    let _ = o.write_all(b"\n");
    let _ = o.flush();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_negotiates_the_client_protocol() {
        let reply = dispatch(
            "test-agent",
            &json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "initialize",
                "params": { "protocolVersion": "2025-03-26" }
            }),
        )
        .unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "jim");
    }

    #[test]
    fn notification_has_no_response() {
        assert!(
            dispatch(
                "test-agent",
                &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            )
            .is_none()
        );
    }

    #[test]
    fn tools_list_exposes_the_three_jim_operations() {
        let reply = dispatch(
            "test-agent",
            &json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/list" }),
        )
        .unwrap();
        let names: Vec<_> = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["jim_send", "jim_roster", "jim_do"]);
    }
}

fn tool_send(self_id: &str, args: &Value) -> Value {
    let to = args.get("to").and_then(Value::as_str).unwrap_or("");
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if to.is_empty() || text.is_empty() {
        return tool_text("jim_send requires non-empty `to` and `text`", true);
    }
    // Accept "all", "agent:<id>", "topic:<name>", or a bare id (→ that inbox).
    let topic = agent_bus::resolve_topic(to).unwrap_or_else(|| format!("agent.inbox.{to}"));
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
        let label = info
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let cwd = info.get("cwd").and_then(Value::as_str).unwrap_or("");
        match label {
            Some(l) => lines.push(format!("{sid} — \"{l}\"  {cwd}")),
            None => lines.push(format!("{sid}  {cwd}")),
        }
    }
    let out = if lines.is_empty() {
        "no other agents on the bus".to_string()
    } else {
        lines.join("\n")
    };
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
