//! `jimctl group` — named pane groups: wire panes into a group once, then
//! reveal the whole group by name.
//!
//! A grouped pane is hidden until its group is revealed. Nothing is spawned
//! on reveal — the panes are already alive — so a terminal keeps its shell
//! and a widget keeps its fetched data between reveals. That is what makes
//! a presentation deck able to show a live dashboard the instant a slide
//! advances, instead of respawning it mid-demo.
//!
//! Usage:
//!   jimctl group assign --name N --title T ... [--project P]
//!   jimctl group clear  --title T ... [--project P]
//!   jimctl group show   --name N ...            # reveal exactly these
//!   jimctl group hide                           # reveal nothing
//!   jimctl group list [--project P]
//!
//! `show`/`hide` publish on the bus topic `pane.groups` — the same channel
//! `deck.ft` uses as slides advance. They're for building and rehearsing a
//! deck by hand; during a talk the deck drives it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage:
  jimctl group assign --name N --title T ... [--project P]
  jimctl group clear  --title T ... [--project P]
  jimctl group show   --name N ...
  jimctl group hide
  jimctl group list [--project P]";

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

/// Send a request; optionally read a JSON reply back.
fn send(req: &serde_json::Value, want_reply: bool) -> Result<Option<String>, String> {
    let sock = socket_path().ok_or_else(|| "$HOME not set; can't locate socket".to_string())?;
    let mut stream = UnixStream::connect(&sock)
        .map_err(|e| format!("connect {}: {} (is Jim running?)", sock.display(), e))?;
    let body = serde_json::to_vec(req).map_err(|e| format!("serialize: {e}"))?;
    stream.write_all(&body).map_err(|e| format!("write: {e}"))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    if !want_reply {
        return Ok(None);
    }
    let mut out = String::new();
    stream
        .read_to_string(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    Ok(Some(out))
}

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let Some(verb) = args.first().cloned() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let mut project: Option<String> = None;
    let mut titles: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--project" | "-p" => {
                project = args.get(i + 1).cloned();
                i += 1;
            }
            "--title" | "-t" => {
                if let Some(t) = args.get(i + 1).cloned() {
                    titles.push(t);
                }
                i += 1;
            }
            "--name" | "-n" => {
                if let Some(n) = args.get(i + 1).cloned() {
                    names.push(n);
                }
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("jimctl group: unexpected arg `{other}`\n{USAGE}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let req = match verb.as_str() {
        "assign" | "clear" => {
            // Titles are the whole safety story here: without them the GUI
            // would have to guess which panes you meant, and a wrong guess
            // hides working panes. The app rejects an empty list too.
            if titles.is_empty() {
                eprintln!("jimctl group {verb}: --title is required (repeatable)\n{USAGE}");
                return ExitCode::from(2);
            }
            let group = if verb == "assign" {
                match names.first() {
                    Some(n) => serde_json::Value::String(n.clone()),
                    None => {
                        eprintln!("jimctl group assign: --name is required\n{USAGE}");
                        return ExitCode::from(2);
                    }
                }
            } else {
                serde_json::Value::Null
            };
            serde_json::json!({
                "action": "set_pane_group",
                "project": project,
                "titles": titles,
                "group": group,
            })
        }
        "show" | "hide" => {
            let show = if verb == "show" {
                names.clone()
            } else {
                Vec::new()
            };
            if verb == "show" && show.is_empty() {
                eprintln!("jimctl group show: --name is required (repeatable)\n{USAGE}");
                return ExitCode::from(2);
            }
            // The visible set is stated in full, never toggled: revealing a
            // different group hides the previous one, and `hide` (an empty
            // list) hides everything.
            serde_json::json!({
                "action": "widget_message",
                "project": "global",
                "topic": "pane.groups",
                "payload": { "show": show },
                "retain": true,
                "sender": "jimctl group",
            })
        }
        "list" => serde_json::json!({
            "action": "list_pane_groups",
            "project": project,
        }),
        other => {
            eprintln!("jimctl group: unknown verb `{other}`\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match send(&req, verb == "list") {
        Ok(Some(reply)) => {
            print!("{reply}");
            if !reply.ends_with('\n') {
                println!();
            }
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jimctl group: {e}");
            ExitCode::from(1)
        }
    }
}
