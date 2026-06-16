//! `jimctl agent` — the convenience layer over the jim agent bus (see
//! AGENTS-ON-THE-BUS.md). Everything here is a thin wrapper on the wire
//! format in `agent_bus`; the point is that a shell script or a non-Claude
//! agent runtime can become a first-class bus participant without re-deriving
//! the roster/tombstone/sweep/inbox-stream boilerplate.
//!
//!   jimctl agent roster [--json]
//!   jimctl agent send --to <dest> --text <msg> [--data '{…}'] [--from <id>] [--retain]
//!   jimctl agent recv --id <id> [--name <label>] [--json] [--no-announce]
//!   jimctl agent announce --id <id> [--name <label>] [--pid <n>]
//!   jimctl agent tombstone --id <id>
//!
//! `recv` is the workhorse: it announces presence, streams every message
//! addressed to `<id>` (its inbox + `agent.all`) as it arrives, sweeps dead
//! peers, and tombstones itself on exit — i.e. the whole adapter inbound loop
//! as one command. `--json` emits NDJSON (`{from,text,topic,data?}`) so an
//! agent loop can pipe it straight in; the default is human-readable lines.

use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use crate::agent_bus;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let Some(sub) = args.first() else {
        print_usage();
        return ExitCode::from(2);
    };
    match sub.as_str() {
        "roster" => cmd_roster(&args[1..]),
        "send" => cmd_send(&args[1..]),
        "recv" => cmd_recv(&args[1..]),
        "announce" => cmd_announce(&args[1..]),
        "tombstone" => cmd_tombstone(&args[1..]),
        "-h" | "--help" | "help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("jimctl agent: unknown subcommand `{other}`");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage:\n  \
         jimctl agent roster [--json]\n  \
         jimctl agent send --to <dest> --text <msg> [--data '{{…}}'] [--from <id>] [--retain]\n  \
         jimctl agent recv --id <id> [--name <label>] [--json] [--no-announce]\n  \
         jimctl agent announce --id <id> [--name <label>] [--pid <n>]\n  \
         jimctl agent tombstone --id <id>\n\
         \n  \
         <dest>: \"agent:<id>\" (one peer) | \"all\" (broadcast) | \"topic:<name>\" (raw bus topic)"
    );
}

/// Pull `--flag value` / `--flag=value` pairs and bare `--flag` switches out
/// of an argv slice. Mirrors `cmd_msg::parse_flags`.
fn parse_flags(args: &[String]) -> (Vec<(String, String)>, Vec<String>) {
    let mut named = Vec::new();
    let mut switches = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(rest) = args[i].strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                named.push((k.to_string(), v.to_string()));
            } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                named.push((rest.to_string(), args[i + 1].clone()));
                i += 1;
            } else {
                switches.push(rest.to_string());
            }
        }
        i += 1;
    }
    (named, switches)
}

fn get<'a>(named: &'a [(String, String)], key: &str) -> Option<&'a str> {
    named.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn cmd_roster(args: &[String]) -> ExitCode {
    let (_, switches) = parse_flags(args);
    let as_json = switches.iter().any(|s| s == "json");
    // Live entries only: drop any whose announced pid is gone.
    let live: Vec<(String, Value)> = agent_bus::read_roster()
        .into_iter()
        .filter(|(_, info)| {
            info.get("pid")
                .and_then(Value::as_u64)
                .map(|pid| agent_bus::pid_alive(pid as u32))
                .unwrap_or(true)
        })
        .collect();

    if as_json {
        let arr: Vec<Value> = live.into_iter().map(|(_, info)| info).collect();
        println!("{}", Value::Array(arr));
        return ExitCode::SUCCESS;
    }
    if live.is_empty() {
        println!("no live agents on the bus");
        return ExitCode::SUCCESS;
    }
    for (sid, info) in &live {
        let label = info.get("label").and_then(Value::as_str).filter(|s| !s.is_empty());
        let cwd = info.get("cwd").and_then(Value::as_str).unwrap_or("");
        match label {
            Some(l) => println!("{sid} — \"{l}\"  {cwd}"),
            None => println!("{sid}  {cwd}"),
        }
    }
    ExitCode::SUCCESS
}

fn cmd_send(args: &[String]) -> ExitCode {
    let (named, switches) = parse_flags(args);
    let (Some(to), Some(text)) = (get(&named, "to"), get(&named, "text")) else {
        eprintln!("jimctl agent send: --to and --text are required");
        return ExitCode::from(2);
    };
    let Some(topic) = agent_bus::resolve_topic(to) else {
        eprintln!("jimctl agent send: --to must be \"agent:<id>\", \"all\", or \"topic:<name>\"");
        return ExitCode::from(2);
    };
    // `--from` stamps the origin (so replies can route back); default to a
    // generic CLI id.
    let from = get(&named, "from").unwrap_or("jimctl-agent").to_string();
    let mut payload = json!({ "from": from, "text": text });
    if let Some(raw) = get(&named, "data") {
        match serde_json::from_str::<Value>(raw) {
            Ok(v) => payload["data"] = v,
            Err(e) => {
                eprintln!("jimctl agent send: --data is not valid JSON: {e}");
                return ExitCode::from(2);
            }
        }
    }
    let retain = switches.iter().any(|s| s == "retain");
    match agent_bus::publish(&topic, payload, retain, &from) {
        Ok(()) => {
            eprintln!("sent to {topic}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("jimctl agent send: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_recv(args: &[String]) -> ExitCode {
    let (named, switches) = parse_flags(args);
    let Some(id) = get(&named, "id") else {
        eprintln!("jimctl agent recv: --id is required");
        return ExitCode::from(2);
    };
    let id = id.to_string();
    let as_json = switches.iter().any(|s| s == "json");
    let announce = !switches.iter().any(|s| s == "no-announce");
    let label = get(&named, "name").map(str::to_string).unwrap_or_else(agent_bus::default_label);

    install_signal_handlers();
    if announce {
        agent_bus::announce(&id, &label);
        eprintln!("jimctl agent recv: `{id}` (\"{label}\") live; streaming agent.inbox.{id} + agent.all. Ctrl-C to stop.");
    } else {
        eprintln!("jimctl agent recv: streaming agent.inbox.{id} + agent.all (no roster announce). Ctrl-C to stop.");
    }

    let stdout = std::io::stdout();
    agent_bus::follow_inbox(
        &id,
        announce, // only sweep peers if we're a roster participant
        |msg| {
            let mut out = stdout.lock();
            if as_json {
                let line = json!({
                    "from": msg.from,
                    "text": msg.text,
                    "topic": msg.topic,
                    "data": msg.payload.get("data").cloned().unwrap_or(Value::Null),
                });
                let _ = writeln!(out, "{line}");
            } else {
                let scope = if msg.topic == "agent.all" { " (broadcast)" } else { "" };
                let _ = writeln!(out, "{}{scope}: {}", msg.from, msg.text);
            }
            let _ = out.flush();
        },
        || SHUTDOWN.load(Ordering::SeqCst),
    );

    if announce {
        agent_bus::tombstone(&id);
    }
    ExitCode::SUCCESS
}

fn cmd_announce(args: &[String]) -> ExitCode {
    let (named, _) = parse_flags(args);
    let Some(id) = get(&named, "id") else {
        eprintln!("jimctl agent announce: --id is required");
        return ExitCode::from(2);
    };
    let label = get(&named, "name").map(str::to_string).unwrap_or_else(agent_bus::default_label);
    // A one-shot announce records the caller's long-lived pid (via --pid) so
    // the sweep tracks the *real* process, not this short-lived jimctl. Absent
    // → our own pid (fine when the caller keeps re-announcing).
    let pid = get(&named, "pid")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or_else(std::process::id);
    let payload = json!({ "id": id, "pid": pid, "cwd": agent_bus::current_cwd(), "label": label });
    match agent_bus::publish(&format!("agent.hello.{id}"), payload, true, id) {
        Ok(()) => {
            eprintln!("announced {id} (\"{label}\", pid {pid})");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("jimctl agent announce: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_tombstone(args: &[String]) -> ExitCode {
    let (named, _) = parse_flags(args);
    let Some(id) = get(&named, "id") else {
        eprintln!("jimctl agent tombstone: --id is required");
        return ExitCode::from(2);
    };
    agent_bus::tombstone(id);
    eprintln!("tombstoned {id}");
    ExitCode::SUCCESS
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(i32) as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(i32) as libc::sighandler_t);
    }
}
