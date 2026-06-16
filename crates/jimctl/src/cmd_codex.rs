//! `jimctl codex` — bridge your **live, interactive `codex` session** onto
//! the jim agent bus by attaching to codex's app-server **daemon** (the same
//! backend the TUI uses). Bus messages become real turns in your session;
//! the model's reply routes back to whoever asked (point-to-point).
//!
//! How it works (verified against codex 0.139): the TUI is a client of a
//! shared `codex app-server`. We connect to its daemon control socket as a
//! WebSocket-over-unix client. Global `thread/started` broadcasts tell us the
//! TUI's live thread; `turn/start` injects a bus message into it (the TUI
//! renders it). The connection that starts a turn is excluded from that
//! turn's event stream, so once the turn finishes (the thread persists) we
//! `thread/resume` and read the assistant's reply from the returned turns
//! page, then publish it to the asker.
//!
//! Usage: run `jimctl codex` first, then `codex` plainly in another terminal
//! (it auto-attaches to the same daemon). Experimental protocol.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

use crate::agent_bus;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

type Ws = WebSocket<UnixStream>;

fn control_socket() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".codex").join("app-server-control").join("app-server-control.sock"))
}

fn ws_req(ws: &mut Ws, id: u64, method: &str, params: Value) {
    let _ = ws.send(Message::Text(json!({ "id": id, "method": method, "params": params }).to_string().into()));
}
fn ws_note(ws: &mut Ws, method: &str) {
    let _ = ws.send(Message::Text(json!({ "method": method }).to_string().into()));
}
fn ws_poll(ws: &mut Ws) -> Result<Option<Value>, ()> {
    match ws.read() {
        Ok(Message::Text(t)) => Ok(serde_json::from_str::<Value>(&t).ok()),
        Ok(Message::Close(_)) => Err(()),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(_) => Err(()),
    }
}

pub fn run() -> ExitCode {
    let (id_arg, name_arg) = parse_args();
    let id = id_arg
        .or_else(|| std::env::var("JIM_CODEX_ID").ok())
        .unwrap_or_else(|| format!("codex-{}", agent_bus::default_label()));
    let label = name_arg.unwrap_or_else(agent_bus::default_label);
    eprintln!("jimctl codex: id={id} name={label:?}");

    match Command::new("codex").args(["app-server", "daemon", "start"])
        .stdout(Stdio::null()).stderr(Stdio::null()).status()
    {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("jimctl codex: `codex app-server daemon start` failed.");
            eprintln!("  needs the standalone codex install (curl -fsSL https://chatgpt.com/codex/install.sh | sh)");
            return ExitCode::from(1);
        }
    }
    let Some(sock) = control_socket() else {
        eprintln!("jimctl codex: HOME not set");
        return ExitCode::from(1);
    };

    let stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => { eprintln!("jimctl codex: connect {}: {e}", sock.display()); return ExitCode::from(1); }
    };
    let (mut ws, _r) = match tungstenite::client("ws://localhost/", stream) {
        Ok(ok) => ok,
        Err(e) => { eprintln!("jimctl codex: ws handshake: {e}"); return ExitCode::from(1); }
    };
    if ws.get_mut().set_nonblocking(true).is_err() {
        eprintln!("jimctl codex: could not set non-blocking");
        return ExitCode::from(1);
    }

    unsafe {
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(i32) as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(i32) as libc::sighandler_t);
    }

    ws_req(&mut ws, 1, "initialize", json!({ "clientInfo": { "name": "jim", "version": env!("CARGO_PKG_VERSION") } }));
    ws_note(&mut ws, "initialized");
    agent_bus::announce(&id, &label);
    ensure_codex_mcp(&id);

    let (tx, rx) = mpsc::channel::<(String, String, bool)>();
    {
        let id = id.clone();
        std::thread::spawn(move || bus_tail(id, tx));
    }
    eprintln!(
        "jimctl codex: `{id}` attached to the codex daemon. Run `codex` plainly in \
         another terminal — bus messages to agent.inbox.{id} / agent.all become turns \
         in that live session, and the agent replies/collaborates via the jim_send \
         tool when it chooses. Ctrl-C to stop."
    );

    let mut active_thread: Option<String> = None;
    let mut thread_busy = false;
    let mut queue: VecDeque<(String, String, bool)> = VecDeque::new();
    // One injected turn in flight at a time. Set on inject; cleared when the
    // thread returns to idle (with a long safety timeout if we miss the edge).
    let mut in_flight: Option<Instant> = None;
    let mut next_id: u64 = 2;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        loop {
            match ws_poll(&mut ws) {
                Ok(Some(m)) => {
                    // We don't read turn replies anymore — the agent replies via
                    // the jim_send tool. We only track the thread + its busy state.
                    match m.get("method").and_then(Value::as_str).unwrap_or("") {
                        "thread/started" => {
                            if let Some(t) = m.pointer("/params/thread/id").and_then(Value::as_str) {
                                if active_thread.as_deref() != Some(t) {
                                    active_thread = Some(t.to_string());
                                    eprintln!("jimctl codex: attached to live thread {t}");
                                }
                            }
                        }
                        "thread/status/changed" => {
                            let st = m.pointer("/params/status/type").and_then(Value::as_str)
                                .or_else(|| m.pointer("/params/status").and_then(Value::as_str))
                                .unwrap_or("");
                            thread_busy = st != "idle" && !st.is_empty();
                            // Returned to idle → the injected turn finished.
                            if !thread_busy {
                                in_flight = None;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(()) => { eprintln!("jimctl codex: connection closed"); SHUTDOWN.store(true, Ordering::SeqCst); break; }
            }
        }

        while let Ok(triple) = rx.try_recv() {
            queue.push_back(triple);
        }

        // Safety: if we never saw the idle edge, free the slot after a while.
        if in_flight.map_or(false, |t| t.elapsed() > Duration::from_secs(600)) {
            in_flight = None;
        }

        // Inject the next ask when the thread is idle and nothing is in flight.
        if in_flight.is_none() && !thread_busy {
            if let Some(thread) = active_thread.clone() {
                if let Some((from, text, broadcast)) = queue.pop_front() {
                    let rid = next_id;
                    next_id += 1;
                    in_flight = Some(Instant::now());
                    let framed = frame_turn(&id, &from, &text, broadcast);
                    let input = json!([{ "type": "text", "text": framed }]);
                    ws_req(&mut ws, rid, "turn/start", json!({ "threadId": thread, "input": input, "approvalPolicy": "never" }));
                    eprintln!("jimctl codex: injected a turn from {from}");
                }
            }
        }

        let _ = ws.flush();
        std::thread::sleep(Duration::from_millis(60));
    }

    eprintln!("jimctl codex: shutting down…");
    agent_bus::tombstone(&id);
    let _ = ws.close(None);
    ExitCode::SUCCESS
}

fn parse_args() -> (Option<String>, Option<String>) {
    let mut id = None;
    let mut name = None;
    let mut it = crate::sub_args();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--id" | "-i" => id = it.next(),
            "--name" | "-n" => name = it.next(),
            _ => {}
        }
    }
    (id, name)
}

/// Tail the bus for messages addressed to us; forward `(from, text, broadcast)`.
fn bus_tail(self_id: String, tx: mpsc::Sender<(String, String, bool)>) {
    agent_bus::follow_inbox(
        &self_id,
        false, // codex bridge doesn't sweep (it announces; keep it simple)
        |m| {
            let broadcast = m.topic == "agent.all";
            let _ = tx.send((m.from, m.text, broadcast));
        },
        || SHUTDOWN.load(Ordering::SeqCst),
    );
}

/// Ensure the `jim` MCP tool server is registered with codex so the live
/// session gets jim_send/jim_roster/jim_do. Re-registers each start to keep
/// `JIM_AGENT_ID` in sync with this bridge's id. Best-effort.
fn ensure_codex_mcp(id: &str) {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "jimctl".to_string());
    // Remove any stale entry, then add fresh with our id.
    let _ = Command::new("codex").args(["mcp", "remove", "jim"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let status = Command::new("codex")
        .args(["mcp", "add", "jim", "--env", &format!("JIM_AGENT_ID={id}"), "--", &exe, "mcp"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            eprintln!("jimctl codex: registered the `jim` MCP server (jim_send/jim_roster/jim_do) for codex.");
        }
        _ => {
            eprintln!(
                "jimctl codex: could not auto-register the MCP server. Add it manually:\n  \
                 codex mcp add jim --env JIM_AGENT_ID={id} -- {exe} mcp"
            );
        }
    }
}

/// Frame an incoming bus message as a codex turn: bus context + the agent's
/// own id + how to reply/collaborate with the jim_send tool.
fn frame_turn(self_id: &str, from: &str, text: &str, broadcast: bool) -> String {
    let scope = if broadcast { "broadcast" } else { "direct" };
    format!(
        "[jim bus · {scope} message from \"{from}\"]\n{text}\n\n\
         (You are agent \"{self_id}\" on the jim bus. To reply, use the jim_send tool \
         with to=\"{from}\". To reach another agent use their id, or to=\"all\" to \
         broadcast. jim_roster lists who's online. Reply once if it's useful, then stop.)"
    )
}
