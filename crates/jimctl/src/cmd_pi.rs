//! `jimctl pi` — run a headless `pi` agent on the jim bus.
//!
//! The bus integration lives in the **`jim-bus.ts` pi extension**
//! (`integrations/pi/jim-bus.ts`, installed to `~/.pi/agent/extensions/`),
//! which auto-loads in this process and owns ALL bus I/O:
//!   - inbound: tails the bus, injects each `agent.inbox.<id>`/`agent.all`
//!     message into the session as a real turn (framed with the sender);
//!   - outbound: gives the agent the `jim_send` / `jim_roster` / `jim_do`
//!     tools so it **replies and collaborates when it chooses** (reply to the
//!     asker, message a peer, or broadcast) — there is no forced auto-reply.
//!
//! This command is just the headless host. `pi --mode rpc` is a long-running
//! session that reads commands on stdin and **exits the moment stdin hits
//! EOF** — so the one job here is to spawn it (extension loaded), HOLD ITS
//! STDIN OPEN so it stays alive, restart it if it crashes, and let you type a
//! line to prompt the agent locally. It does NOT do bus I/O itself — that
//! would double up with the extension. (For a session you watch/steer in a
//! TUI, just run `pi` directly; the same extension loads.)
//!
//! Identity passed to the child as env so the extension uses it: id =
//! `--id`/`$JIM_PI_ID`/`pi-<dir>`, name = `--name`/`$JIM_PI_NAME`/`<dir>`.
//! The pi *session* is `--session-id jim-pi-<id>`, so context survives a
//! restart. Local controls: type a line to prompt; `/who`, `/quit`.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::agent_bus;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn run() -> ExitCode {
    if crate::sub_args().any(|a| a == "-h" || a == "--help" || a == "help") {
        eprintln!(
            "usage: jimctl pi [--id <id>] [--name <label>]\n\n  \
             Runs a headless `pi --mode rpc` agent on the jim bus. Bus I/O is handled by\n  \
             the jim-bus pi extension (~/.pi/agent/extensions/jim-bus.ts), which gives the\n  \
             agent jim_send/jim_roster/jim_do and injects incoming messages as turns.\n  \
             Type a line to prompt the agent locally; /who, /quit."
        );
        return ExitCode::SUCCESS;
    }
    let (id_arg, name_arg) = parse_args();
    let id = id_arg
        .or_else(|| std::env::var("JIM_PI_ID").ok())
        .unwrap_or_else(|| format!("pi-{}", agent_bus::default_label()));
    let name = name_arg
        .or_else(|| std::env::var("JIM_PI_NAME").ok())
        .unwrap_or_else(agent_bus::default_label);
    let session_id = format!("jim-pi-{id}");

    // pi must be on PATH.
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
    // The bus integration is the extension — refuse to run without it (else
    // the agent would be on no bus at all, silently).
    if let Some(ext) = extension_path() {
        if !ext.exists() {
            eprintln!("jimctl pi: jim-bus extension not installed at {}.", ext.display());
            eprintln!("  install it:  cp integrations/pi/jim-bus.ts ~/.pi/agent/extensions/");
            return ExitCode::from(1);
        }
    }

    unsafe {
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(i32) as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(i32) as libc::sighandler_t);
    }

    eprintln!(
        "jimctl pi: headless `pi --mode rpc` as `{id}` (\"{name}\"), session {session_id}.\n  \
         Bus I/O is handled by the jim-bus extension (inbound inject + jim_send/jim_roster/jim_do).\n  \
         Type a line to prompt the agent locally; /who, /quit."
    );

    // Local typing → prompt commands for the live session.
    let (tx, rx) = mpsc::channel::<String>();
    {
        let id = id.clone();
        let name = name.clone();
        std::thread::spawn(move || local_stdin_loop(id, name, tx));
    }

    // Supervise: keep exactly one `pi --mode rpc` alive, restarting on crash.
    while !SHUTDOWN.load(Ordering::SeqCst) {
        let mut child = match spawn_pi_rpc(&id, &name, &session_id) {
            Some(c) => c,
            None => {
                eprintln!("jimctl pi: could not spawn pi; retrying in 2s");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        // Holding `stdin` is what keeps pi alive — pi --mode rpc exits on stdin
        // EOF. Do NOT drop it until we're tearing the child down.
        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = child.kill();
                continue;
            }
        };
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || print_pi_events(out));
        }

        // Forward local prompts; watch for shutdown or child exit (→ respawn).
        loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                break;
            }
            while let Ok(msg) = rx.try_recv() {
                let cmd = json!({ "type": "prompt", "message": msg }).to_string();
                if writeln!(stdin, "{cmd}").and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !SHUTDOWN.load(Ordering::SeqCst) {
                        eprintln!("jimctl pi: pi exited ({status}); restarting");
                    }
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        drop(stdin); // close pi's stdin so it shuts down cleanly
        let _ = child.kill();
        let _ = child.wait();
    }

    eprintln!("jimctl pi: shutting down…");
    // Safety net: the extension tombstones on its own clean exit, but a hard
    // kill skips that — tombstone here too (idempotent).
    agent_bus::tombstone(&id);
    ExitCode::SUCCESS
}

fn extension_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".pi/agent/extensions/jim-bus.ts"))
}

/// Spawn the long-lived `pi --mode rpc` session with the bus identity in env
/// (the extension reads `JIM_PI_ID`/`JIM_PI_NAME`). stdin piped + held open by
/// the caller; stdout piped for display; stderr discarded.
fn spawn_pi_rpc(id: &str, name: &str, session_id: &str) -> Option<Child> {
    Command::new("pi")
        .args(["--mode", "rpc", "--session-id", session_id])
        .env("JIM_PI_ID", id)
        .env("JIM_PI_NAME", name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("jimctl pi: spawn failed: {e}"))
        .ok()
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

/// Read this terminal's stdin: a bare line prompts the agent locally; `/who`
/// and `/quit` are local controls. (Renaming lives in the extension's
/// `/jim-name`; the roster identity is owned there.)
fn local_stdin_loop(id: String, name: String, tx: mpsc::Sender<String>) {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match t {
            "/who" => println!("id={id}  name=\"{name}\""),
            "/quit" | "/exit" => {
                SHUTDOWN.store(true, Ordering::SeqCst);
                break;
            }
            _ if t.starts_with('/') => println!("commands: /who, /quit (type a line to prompt the agent)"),
            _ => {
                if tx.send(t.to_string()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Surface what the agent says: print the assistant text from each `agent_end`
/// event on the rpc stdout stream. Display-only — the extension already
/// published any bus output. Ignores the rest of the (noisy) event stream.
fn print_pi_events(stdout: std::process::ChildStdout) {
    let reader = io::BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(m) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if m.get("type").and_then(Value::as_str) == Some("agent_end") {
            if let Some(text) = reply_from_agent_end(&m) {
                println!("\n[pi] {text}\n");
            }
        }
    }
}

/// Pull the last assistant message's text out of a pi `agent_end` event,
/// concatenating its `text` content blocks (skipping `thinking`, etc.).
fn reply_from_agent_end(m: &Value) -> Option<String> {
    let msgs = m.get("messages").and_then(Value::as_array)?;
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
            return Some(s);
        }
    }
    None
}
