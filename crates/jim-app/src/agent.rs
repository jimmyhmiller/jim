//! In-app autonomous agent. A ReAct-style loop driven by DeepSeek (or any
//! OpenAI-compatible model via [`jim_inference::llm`]): each turn the model
//! emits a JSON object that is either a tool call or a final answer; the app
//! runs the tool, feeds the result back as an observation, and loops.
//!
//! v1 exposes a single tool — `run_shell` — which runs a command on the
//! user's machine with their full environment and **no confirmation**
//! (the user opted into "YOLO" shell access). That one tool is enough to
//! drive the whole app: `jimctl` (on PATH) controls Jim, `curl` +
//! `$JIMMY_API_KEY` reach the analytics API, and ordinary file/Unix tools
//! cover everything else. The model learns *how* from the same user notes
//! the planner uses ([`crate::tools::load_memory`]).
//!
//! The loop runs on a worker thread (LLM HTTP and shell both block) and
//! streams [`AgentMsg`] events back over an mpsc channel so the UI can show
//! the transcript live. A shared `AtomicBool` lets the UI cancel between
//! steps. Nothing here touches the Bevy `World` — app control happens
//! out-of-process through `jimctl`/the IPC socket.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jim_inference::llm::{self, LlmConfig, Msg};
use serde::Deserialize;

/// Max model turns before the loop bails (a runaway guard).
const MAX_STEPS: usize = 16;
/// Per-command wall-clock budget; the child is killed past this.
const SHELL_TIMEOUT: Duration = Duration::from_secs(60);
/// Cap on captured stdout/stderr fed back to the model (bytes, each).
const OUTPUT_CAP: usize = 16 * 1024;

/// One streamed event from the running loop, for the UI transcript.
pub enum AgentMsg {
    /// The model's brief reasoning for this step.
    Thought(String),
    /// A shell command about to run.
    Action(String),
    /// A short preview of a command's result.
    Observation(String),
    /// The loop finished with this answer/summary.
    Done(String),
    /// The loop aborted (LLM/transport error, bad output, etc.).
    Error(String),
}

/// One model turn, parsed from the assistant's JSON object. Lenient: extra
/// fields are ignored and a malformed turn deserializes to all-empty (the
/// loop then nudges the model to try again).
#[derive(Deserialize, Default)]
struct Turn {
    #[serde(default)]
    thought: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    args: serde_json::Value,
    #[serde(rename = "final", default, alias = "final_answer", alias = "answer")]
    final_answer: Option<String>,
}

/// Run the agent loop to completion, streaming events to `tx`. Blocking —
/// spawn on a dedicated thread. Returns when the model finishes, errors, or
/// `cancel` is set.
pub fn run(cfg: LlmConfig, system: String, goal: String, tx: Sender<AgentMsg>, cancel: Arc<AtomicBool>) {
    let mut messages = vec![Msg::system(system), Msg::user(format!("User goal: {goal}"))];

    for _ in 0..MAX_STEPS {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        let raw = match llm::chat_json(&cfg, &messages, 0.2) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(AgentMsg::Error(e.to_string()));
                return;
            }
        };
        // Keep the model's own reply verbatim in history so it sees its
        // prior turns exactly as it wrote them.
        messages.push(Msg::assistant(raw.clone()));

        let turn: Turn = serde_json::from_str(&raw).unwrap_or_default();
        if !turn.thought.trim().is_empty() {
            let _ = tx.send(AgentMsg::Thought(turn.thought.clone()));
        }

        if let Some(answer) = turn.final_answer.filter(|s| !s.trim().is_empty()) {
            let _ = tx.send(AgentMsg::Done(answer));
            return;
        }

        match turn.tool.as_deref() {
            Some("run_shell") | Some("shell") | Some("bash") | Some("sh") => {
                let cmd = turn
                    .args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .or_else(|| turn.args.as_str())
                    .unwrap_or("")
                    .to_string();
                if cmd.trim().is_empty() {
                    messages.push(Msg::user(
                        "ERROR: run_shell needs args.command (a non-empty string). Try again.",
                    ));
                    continue;
                }
                let _ = tx.send(AgentMsg::Action(cmd.clone()));
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let obs = run_shell(&cmd);
                let _ = tx.send(AgentMsg::Observation(preview(&obs, 240)));
                messages.push(Msg::user(format!(
                    "OBSERVATION (run_shell `{}`):\n{}",
                    preview(&cmd, 100),
                    obs
                )));
            }
            Some(other) => {
                messages.push(Msg::user(format!(
                    "ERROR: '{other}' is not a tool. The only tool is 'run_shell'. \
                     Respond with a run_shell call or a final answer."
                )));
            }
            None => {
                messages.push(Msg::user(
                    "Your reply had no tool call and no final answer. Respond with a single \
                     JSON object: either {\"thought\":..,\"tool\":\"run_shell\",\"args\":{\"command\":..}} \
                     or {\"thought\":..,\"final\":..}.",
                ));
            }
        }
    }

    let _ = tx.send(AgentMsg::Done(
        "Reached the step limit before finishing. Ask again to continue.".into(),
    ));
}

/// Build the agent's system prompt: role + protocol + the user's durable
/// notes (shared with the planner) + the live app context.
pub fn build_system_prompt(context: &str) -> String {
    let mut s = String::from(ROLE);
    let notes = crate::tools::load_memory();
    if !notes.is_empty() {
        s.push_str("\n\n# Notes / memory (durable facts and instructions — honor them)\n");
        for n in &notes {
            s.push_str("- ");
            s.push_str(n);
            s.push('\n');
        }
    }
    s.push_str("\n# Current app context\n");
    s.push_str(context);
    s
}

const ROLE: &str = "\
You are an autonomous agent embedded in 'Jim', a Bevy canvas app of floating panes \
(terminals, editors, widgets, charts, an inbox, a project cube) running on the user's \
macOS machine. You accomplish the user's goal by running shell commands and observing \
their output, one step at a time.

PROTOCOL — every message you send MUST be exactly one JSON object and nothing else:
  to act:    {\"thought\":\"<brief reasoning>\",\"tool\":\"run_shell\",\"args\":{\"command\":\"<shell command>\"}}
  to finish: {\"thought\":\"<brief>\",\"final\":\"<concise summary/answer for the user>\"}
After each run_shell you receive an OBSERVATION (exit code + stdout/stderr, truncated). \
Use it to decide the next step. Take the fewest steps that achieve the goal, then return a final. \
If the goal is impossible or unsafe, say so in a final instead of guessing.

TOOL — run_shell runs ONE command via `sh -c` with the user's full environment and NO \
confirmation. Output is truncated (~16KB each of stdout/stderr) and the command is KILLED \
after 60s, so never start long-running or blocking processes (no servers, watchers, GUIs, \
REPLs, or the `jim` binary itself); prefer one-shot commands. Chain steps across turns rather \
than packing everything into one command when you need to read results in between.

CONTROLLING JIM — the running app is driven by the `jimctl` CLI (already on PATH): spawn \
widgets/charts, send inbox messages, publish on the widget bus, open files, file issues, \
close panes, etc. The Notes below document jimctl, the chart recipe, and the analytics API. \
`$JIMMY_API_KEY` is set in your environment for that private API. For experimental or dev \
panes ALWAYS target the project 'Recursion' — never the user's active project.

SAFETY — inspect before you change (cat/ls/`jimctl ... read`/curl) and verify after when it's \
cheap. Never run destructive commands (no `rm -rf`, no formatting, no force-pushes) and NEVER \
kill Jim's daemon processes, unless the user explicitly asked. Keep the user's data and panes intact.";

/// Run one shell command, capturing bounded stdout/stderr with a wall-clock
/// timeout. Reader threads drain the pipes continuously so a chatty command
/// can't deadlock on a full pipe buffer; bytes past [`OUTPUT_CAP`] are
/// discarded (still drained) so memory stays bounded.
fn run_shell(cmd: &str) -> String {
    use std::process::{Command, Stdio};

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The agent drives the app by shelling out to `jimctl`, which ships next
    // to `jim` (the bundle's Contents/MacOS, or target/release in dev) but is
    // NOT on the system PATH on a fresh machine. Prepend the exe dir so those
    // commands resolve without the user having to install jimctl globally.
    if let Some(dir) = crate::exe_dir() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut entries = vec![dir];
        entries.extend(std::env::split_paths(&path));
        if let Ok(joined) = std::env::join_paths(entries) {
            command.env("PATH", joined);
        }
    }
    let child = command.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("exit: -1\nfailed to spawn shell: {e}"),
    };

    let out_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let err_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut readers = Vec::new();
    if let Some(so) = child.stdout.take() {
        readers.push(spawn_drain(so, out_buf.clone()));
    }
    if let Some(se) = child.stderr.take() {
        readers.push(spawn_drain(se, err_buf.clone()));
    }

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() > SHELL_TIMEOUT {
                    let _ = child.kill();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };
    for h in readers {
        let _ = h.join();
    }

    let so = String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned();
    let se = String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned();

    let mut s = match status {
        Some(st) => format!("exit: {}\n", st.code().unwrap_or(-1)),
        None => "exit: timeout (killed after 60s)\n".to_string(),
    };
    if !so.trim().is_empty() {
        s.push_str("stdout:\n");
        s.push_str(so.trim_end());
        s.push('\n');
    }
    if !se.trim().is_empty() {
        s.push_str("stderr:\n");
        s.push_str(se.trim_end());
        s.push('\n');
    }
    if so.trim().is_empty() && se.trim().is_empty() {
        s.push_str("(no output)\n");
    }
    s
}

/// Continuously read a child pipe into `buf`, keeping at most [`OUTPUT_CAP`]
/// bytes but draining the rest so the child never blocks on a full pipe.
fn spawn_drain<R: Read + Send + 'static>(
    mut r: R,
    buf: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut g = buf.lock().unwrap();
                    if g.len() < OUTPUT_CAP {
                        let take = (OUTPUT_CAP - g.len()).min(n);
                        g.extend_from_slice(&chunk[..take]);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// Single-line, length-bounded preview of `s` for the transcript.
fn preview(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        let kept: String = one.chars().take(max).collect();
        format!("{kept}…")
    } else {
        one
    }
}
