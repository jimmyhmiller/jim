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

/// Pull the assistant's reply for `turn_id` out of a `thread/resume` result.
/// Falls back to the most-recent turn if the id isn't matched.
fn reply_from_resume(result: &Value, turn_id: &str) -> Option<String> {
    let turns = result.pointer("/thread/turns")?.as_array()?;
    let pick = turns
        .iter()
        .find(|t| t.get("id").and_then(Value::as_str) == Some(turn_id))
        .or_else(|| turns.first())?;
    let items = pick.get("items")?.as_array()?;
    for it in items.iter().rev() {
        if it.get("type").and_then(Value::as_str) == Some("agentMessage") {
            if let Some(t) = it.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                return Some(t.to_string());
            }
        }
    }
    None
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

    let (tx, rx) = mpsc::channel::<(String, String)>();
    {
        let id = id.clone();
        std::thread::spawn(move || bus_tail(id, tx));
    }
    eprintln!(
        "jimctl codex: `{id}` attached to the codex daemon. Run `codex` plainly in \
         another terminal — bus messages to agent.inbox.{id} / agent.all become turns \
         in that live session, and replies route back. Ctrl-C to stop."
    );

    let mut active_thread: Option<String> = None;
    let mut thread_busy = false;
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    // The single in-flight injected ask: (asker, our turn id once known, started_at).
    let mut pending: Option<(String, Option<String>, Instant)> = None;
    let mut turn_req: Option<u64> = None;   // id of our outstanding turn/start
    let mut resume_id: Option<u64> = None;  // id of an outstanding reply-poll resume
    let mut last_resume = Instant::now();
    let mut next_id: u64 = 2;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        loop {
            match ws_poll(&mut ws) {
                Ok(Some(m)) => {
                    // Responses (have an id, no method).
                    if let Some(rid) = m.get("id").and_then(Value::as_u64) {
                        if Some(rid) == turn_req {
                            turn_req = None;
                            if let (Some((_, slot, _)), Some(t)) =
                                (pending.as_mut(), m.pointer("/result/turn/id").and_then(Value::as_str))
                            {
                                *slot = Some(t.to_string());
                            }
                        } else if Some(rid) == resume_id {
                            resume_id = None;
                            if let Some((asker, Some(tid), _)) = pending.clone() {
                                if let Some(result) = m.get("result") {
                                    if let Some(reply) = reply_from_resume(result, &tid) {
                                        let topic = format!("agent.inbox.{asker}");
                                        let _ = agent_bus::publish(&topic, json!({ "from": id, "to": asker, "text": reply }), false, &id);
                                        eprintln!("jimctl codex: → {topic} ({} chars)", reply.len());
                                        pending = None;
                                    }
                                }
                            }
                        }
                    }
                    // Global notifications.
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
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(()) => { eprintln!("jimctl codex: connection closed"); SHUTDOWN.store(true, Ordering::SeqCst); break; }
            }
        }

        while let Ok(pair) = rx.try_recv() {
            queue.push_back(pair);
        }

        // Inject the next ask when idle and nothing is pending.
        if pending.is_none() && !thread_busy {
            if let Some(thread) = active_thread.clone() {
                if let Some((from, text)) = queue.pop_front() {
                    let rid = next_id;
                    next_id += 1;
                    turn_req = Some(rid);
                    pending = Some((from.clone(), None, Instant::now()));
                    let input = json!([{ "type": "text", "text": format!("[from {from} via jim] {text}") }]);
                    ws_req(&mut ws, rid, "turn/start", json!({ "threadId": thread, "input": input, "approvalPolicy": "never" }));
                    eprintln!("jimctl codex: injected a turn from {from}");
                }
            }
        }

        // Poll thread/resume for our reply once the turn id is known.
        if resume_id.is_none() && last_resume.elapsed() > Duration::from_millis(1000) {
            if let (Some((_, Some(_), started)), Some(thread)) = (pending.clone(), active_thread.clone()) {
                if started.elapsed() > Duration::from_secs(180) {
                    eprintln!("jimctl codex: reply timed out; dropping");
                    pending = None;
                } else {
                    let rid = next_id;
                    next_id += 1;
                    resume_id = Some(rid);
                    last_resume = Instant::now();
                    ws_req(&mut ws, rid, "thread/resume", json!({ "threadId": thread, "itemsView": "full", "limit": 3 }));
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

fn bus_tail(self_id: String, tx: mpsc::Sender<(String, String)>) {
    use std::io::{BufRead, Seek, SeekFrom};
    let Some(log) = agent_bus::bus_log_path() else { return };
    let inbox = format!("agent.inbox.{self_id}");
    let mut pos: u64 = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut f) = std::fs::File::open(&log) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0;
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut reader = std::io::BufReader::new(&mut f);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if line.ends_with('\n') {
                                if let Some(pair) = parse_inbound(line.trim_end(), &self_id, &inbox) {
                                    let _ = tx.send(pair);
                                }
                            } else {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                pos = f.stream_position().unwrap_or(len);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn parse_inbound(line: &str, self_id: &str, inbox: &str) -> Option<(String, String)> {
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
    if from == self_id || bus_sender == self_id {
        return None;
    }
    let text = match payload.get("text").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => serde_json::to_string(&payload).unwrap_or_default(),
    };
    Some((from, text))
}
