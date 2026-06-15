//! `jimctl pi` — put a `pi` (coding CLI) session on the jim agent bus, with
//! an interactive front-end.
//!
//! pi has no async-push protocol, but it has stable session resume
//! (`--session-id`, created if missing). So each message — whether typed
//! here or arriving on the bus — continues one persistent pi session via
//! `pi --mode json --print`; pi's reply goes back to the asker's inbox
//! (point-to-point), or prints here if you typed it. A worker serializes
//! invocations so concurrent asks don't race the session.
//!
//! Interactive: type a line to ask pi; `/name <label>` renames live, `/who`,
//! `/quit`. Identity: id is fixed (`--id` / `$JIM_PI_ID` / `pi-<dir>`); name
//! is `--name` / `/name`.

use std::io::{self, BufRead, Seek, SeekFrom};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::agent_bus;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

const LOCAL: &str = "local";

pub fn run() -> ExitCode {
    let (id_arg, name_arg) = parse_args();
    let id = id_arg
        .or_else(|| std::env::var("JIM_PI_ID").ok())
        .unwrap_or_else(|| format!("pi-{}", agent_bus::default_label()));
    let label = Arc::new(Mutex::new(name_arg.unwrap_or_else(agent_bus::default_label)));
    let session_id = format!("jim-pi-{id}");
    eprintln!("jimctl pi: id={id} name={:?} session={session_id}", *label.lock().unwrap());

    if Command::new("pi")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("jimctl pi: `pi` CLI not found on PATH.");
        return ExitCode::from(1);
    }

    unsafe {
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(i32) as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(i32) as libc::sighandler_t);
    }

    // Worker: one pi invocation at a time, continuing the same session.
    let (tx, rx) = mpsc::channel::<(String, String)>();
    {
        let id = id.clone();
        let session_id = session_id.clone();
        std::thread::spawn(move || pi_worker(rx, id, session_id));
    }

    agent_bus::announce(&id, &label.lock().unwrap());

    // Interactive stdin thread.
    {
        let id = id.clone();
        let label = label.clone();
        let tx = tx.clone();
        std::thread::spawn(move || stdin_loop(id, label, tx));
    }

    eprintln!(
        "jimctl pi: `{id}` is live on the bus and interactive.\n  \
         Type to ask pi. /name <label> to rename, /who, /quit. \
         Other agents reach you at agent.inbox.{id}."
    );

    let Some(log) = agent_bus::bus_log_path() else {
        eprintln!("jimctl pi: HOME not set");
        return ExitCode::from(1);
    };
    let inbox = format!("agent.inbox.{id}");
    let mut pos: u64 = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let mut sweep_ctr: u32 = 0;

    while !SHUTDOWN.load(Ordering::SeqCst) {
        if let Ok(mut f) = std::fs::File::open(&log) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0;
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut reader = io::BufReader::new(&mut f);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if line.ends_with('\n') {
                                if let Some((from, text)) = inbound(line.trim_end(), &id, &inbox) {
                                    eprintln!("jimctl pi: ← {from}: {text}");
                                    let _ = tx.send((from, text));
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
        sweep_ctr += 1;
        if sweep_ctr >= 25 {
            sweep_ctr = 0;
            agent_bus::sweep_dead_sessions(&id);
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    eprintln!("jimctl pi: shutting down…");
    agent_bus::tombstone(&id);
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

fn stdin_loop(id: String, label: Arc<Mutex<String>>, tx: mpsc::Sender<(String, String)>) {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("/name ") {
            let new = rest.trim().to_string();
            if !new.is_empty() {
                *label.lock().unwrap() = new.clone();
                agent_bus::announce(&id, &new);
                println!("(renamed to \"{new}\")");
            }
            continue;
        }
        match t {
            "/who" => println!("id={id}  name=\"{}\"", label.lock().unwrap()),
            "/quit" | "/exit" => {
                SHUTDOWN.store(true, Ordering::SeqCst);
                break;
            }
            _ if t.starts_with('/') => println!("commands: /name <label>, /who, /quit"),
            _ => {
                let _ = tx.send((LOCAL.to_string(), t.to_string()));
            }
        }
    }
}

/// Parse a bus-log line; return `(from, text)` if addressed to us and not our
/// own broadcast.
fn inbound(line: &str, self_id: &str, inbox: &str) -> Option<(String, String)> {
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

/// Worker: one pi invocation per message, continuing the same session.
/// Replies route to the asker's inbox, or print here for locally-typed asks.
fn pi_worker(rx: mpsc::Receiver<(String, String)>, self_id: String, session_id: String) {
    for (from, text) in rx {
        let prompt = if from == LOCAL {
            text.clone()
        } else {
            format!("[from {from} via jim] {text}")
        };
        eprintln!("jimctl pi: → pi (from {from})");
        let out = Command::new("pi")
            .args(["--mode", "json", "--print", "--session-id", &session_id, &prompt])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match out {
            Ok(o) => match parse_pi_reply(&o.stdout) {
                Some(reply) => {
                    println!("\n[pi] {reply}\n");
                    if from != LOCAL {
                        let topic = format!("agent.inbox.{from}");
                        let _ = agent_bus::publish(
                            &topic,
                            json!({ "from": self_id, "to": from, "text": reply }),
                            false,
                            &self_id,
                        );
                        eprintln!("jimctl pi: → {topic} ({} chars)", reply.len());
                    }
                }
                None => eprintln!("jimctl pi: no assistant text parsed from pi output"),
            },
            Err(e) => eprintln!("jimctl pi: pi invocation failed: {e}"),
        }
    }
}

/// Pull pi's final assistant text from its `--mode json` NDJSON: the last
/// assistant message in the terminal `agent_end` event.
fn parse_pi_reply(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut reply = None;
    for line in text.lines() {
        let Ok(m) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if m.get("type").and_then(Value::as_str) != Some("agent_end") {
            continue;
        }
        let Some(msgs) = m.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for msg in msgs.iter().rev() {
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let mut s = String::new();
            if let Some(content) = msg.get("content").and_then(Value::as_array) {
                for c in content {
                    if c.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(t) = c.get("text").and_then(Value::as_str) {
                            s.push_str(t);
                        }
                    }
                }
            }
            if !s.is_empty() {
                reply = Some(s);
                break;
            }
        }
    }
    reply
}
