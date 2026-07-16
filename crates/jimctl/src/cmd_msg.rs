//! `jimctl msg` — talk to the widget↔widget message bus from the shell.
//!
//! The bus lets widget panes in the same editor project coordinate (an
//! editor pane tells a results pane "run this query", the results pane
//! tells everyone "query finished", etc.). This CLI is the shell-side
//! door into it — handy for driving a widget from a `proc_spawn`ed child
//! or verifying message flow without the GUI. It mirrors `claude-bus-tail`.
//!
//! Usage:
//!   jimctl msg emit --project P --topic T [--json '{...}'] [--retain]
//!   jimctl msg tail [--project P]
//!
//!   emit   Publish one message. Delivered to every widget in project P
//!          as `on_message(topic, payload, "jimctl msg")`. `--retain` keeps it
//!          as the topic's last value for widgets that spawn later.
//!   tail   Follow the bus live, printing each delivered message as a
//!          JSON line. `--project P` filters to that project.
//!
//! `--project` accepts a project name (`datalog-db`) or `active`.

use std::io::Write;
use std::process::ExitCode;

fn print_usage() {
    eprintln!(
        "usage:\n  \
         jimctl msg emit --project P --topic T [--json '{{...}}'] [--retain]\n  \
         jimctl msg tail [--project P]"
    );
}

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let Some(sub) = args.first() else {
        print_usage();
        return ExitCode::from(2);
    };
    match sub.as_str() {
        "emit" => cmd_emit(&args[1..]),
        "tail" => cmd_tail(&args[1..]),
        "-h" | "--help" | "help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("jimctl msg: unknown subcommand `{}`", other);
            print_usage();
            ExitCode::from(2)
        }
    }
}

/// Pull `--flag value` / `--flag=value` pairs and bare `--flag` switches
/// out of an argv slice. Returns (named, switches).
fn parse_flags(args: &[String]) -> (Vec<(String, String)>, Vec<String>) {
    let mut named = Vec::new();
    let mut switches = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(rest) = a.strip_prefix("--") {
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

fn cmd_emit(args: &[String]) -> ExitCode {
    let (named, switches) = parse_flags(args);
    let Some(topic) = get(&named, "topic") else {
        eprintln!("jimctl msg emit: --topic is required");
        print_usage();
        return ExitCode::from(2);
    };
    let payload: serde_json::Value = match get(&named, "json") {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("jimctl msg emit: --json is not valid JSON: {}", e);
                return ExitCode::from(2);
            }
        },
        None => serde_json::Value::Null,
    };
    let retain = switches.iter().any(|s| s == "retain");

    // Resolve `--project NAME` to its numeric id; absent / "global" / "*"
    // → the global channel. Project resolution still asks the GUI (it owns
    // the project list), but is optional — most bus traffic is global and
    // reaches the daemon whether or not the GUI is open.
    let project: Option<u64> = match get(&named, "project") {
        None | Some("global") | Some("*") => None,
        Some(name) => match resolve_project_id(name) {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!("jimctl msg emit: {}", e);
                return ExitCode::from(1);
            }
        },
    };

    let msg = jim_bus::proto::BusMessage {
        project,
        topic: topic.to_string(),
        payload_json: payload.to_string(),
        sender: "jimctl msg".to_string(),
        retain,
    };
    match jim_bus::client::publish_oneshot(&msg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jimctl msg: publish to jim-bus: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_tail(args: &[String]) -> ExitCode {
    let (named, _switches) = parse_flags(args);
    // Resolve `--project NAME` to its numeric id (the log stores ids) by
    // asking the running app. Absent → show every project.
    let filter_id: Option<u64> = match get(&named, "project") {
        Some(name) => match resolve_project_id(name) {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!("jimctl msg tail: {}", e);
                return ExitCode::from(1);
            }
        },
        None => None,
    };

    // Subscribe to the bus daemon (spawning it if needed) and print each
    // delivered message as a JSON line — same shape the old tail log had.
    // The daemon replays the retained store first, then streams live.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let handle = jim_bus::client::BusHandle::spawn();
    loop {
        for item in handle.drain() {
            if let jim_bus::client::Inbound::Message(msg) = item {
                print_tail_msg(&mut out, &msg, filter_id);
            }
        }
        let _ = out.flush();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn print_tail_msg(out: &mut impl Write, msg: &jim_bus::proto::BusMessage, filter_id: Option<u64>) {
    if let Some(want) = filter_id {
        if msg.project != Some(want) {
            return;
        }
    }
    let payload: serde_json::Value =
        serde_json::from_str(&msg.payload_json).unwrap_or(serde_json::Value::Null);
    let line = serde_json::json!({
        "project": msg.project,
        "topic": msg.topic,
        "sender": msg.sender,
        "retain": msg.retain,
        "payload": payload,
    });
    let _ = writeln!(out, "{}", line);
}

/// Ask the running app for its project list and resolve `name` to an id.
use crate::project_resolve::resolve_project_id;
