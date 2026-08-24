//! `jimctl workspace` — the sidebar's saved configurations.
//!
//! A workspace is one arrangement of the project sidebar: which projects
//! are listed, which are parked, and which one you were last working in.
//! Every project exists in every workspace — a workspace does not own
//! projects, it only remembers what to show — so switching never moves a
//! project, closes a pane, or kills a shell.
//!
//! In the app they are swiped between: two fingers sideways over the
//! sidebar, or a click on the page-indicator bars in its header. This is
//! the scriptable version of the same thing.
//!
//! Usage:
//!   jimctl workspace list                       what exists, and what each shows
//!   jimctl workspace new [NAME]                 add one (a fork of the current)
//!   jimctl workspace switch NAME                show it
//!   jimctl workspace next | prev                step along the swipe order
//!   jimctl workspace rename [NAME] --to NEW     rename (default: the current one)
//!   jimctl workspace rm NAME                    delete (never the last one)
//!   jimctl workspace show PROJECT [--in NAME]   list PROJECT in a workspace
//!   jimctl workspace hide PROJECT [--in NAME]   park PROJECT in a workspace
//!
//! Stays lib-free like the other `jimctl` subcommands — a hand-written
//! request struct instead of a dependency on `jim_app`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum IpcRequest {
    Workspace {
        op: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
}

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

const USAGE: &str = "\
usage: jimctl workspace <command>

  list                          what exists, and which projects each shows
  new [NAME]                    add a workspace (starts as a fork of the current one)
  switch NAME                   show that workspace
  next | prev                   step along the swipe order
  rename [NAME] --to NEW        rename a workspace (default: the current one)
  rm NAME                       delete a workspace (never the last one)
  show PROJECT [--in NAME]      list PROJECT in a workspace
  hide PROJECT [--in NAME]      park PROJECT in a workspace

A workspace is a saved sidebar configuration. Every project exists in
every workspace; a workspace only remembers which ones to show and which
you were last in. Swipe two fingers sideways over the sidebar to switch.";

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let Some(op) = args.first().map(|s| s.as_str()) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if op == "-h" || op == "--help" {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    // Positional argument, plus the flags each op accepts.
    let mut positional: Option<String> = None;
    let mut to: Option<String> = None;
    let mut in_workspace: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                to = args.get(i + 1).cloned();
                i += 1;
            }
            "--in" | "-w" => {
                in_workspace = args.get(i + 1).cloned();
                i += 1;
            }
            other if other.starts_with('-') => {
                eprintln!("jimctl workspace: unknown flag {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
            other => {
                if positional.is_some() {
                    eprintln!("jimctl workspace: unexpected argument {other:?}\n\n{USAGE}");
                    return ExitCode::from(2);
                }
                positional = Some(other.to_string());
            }
        }
        i += 1;
    }

    // Which field the positional lands in depends on the op: `show` and
    // `hide` name a PROJECT, everything else names a workspace.
    let (name, project) = match op {
        "show" | "hide" => {
            let Some(p) = positional else {
                eprintln!("jimctl workspace {op}: needs a project name\n\n{USAGE}");
                return ExitCode::from(2);
            };
            (in_workspace, Some(p))
        }
        _ => (positional, None),
    };

    match op {
        "list" | "new" | "switch" | "next" | "prev" | "rename" | "rm" | "show" | "hide" => {}
        other => {
            eprintln!("jimctl workspace: unknown command {other:?}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    // Catch the arg-shape mistakes here rather than letting the app log
    // them to a console the user isn't reading.
    if matches!(op, "switch" | "rm") && name.is_none() {
        eprintln!("jimctl workspace {op}: needs a workspace name\n\n{USAGE}");
        return ExitCode::from(2);
    }
    if op == "rename" && to.is_none() {
        eprintln!("jimctl workspace rename: needs --to NEW\n\n{USAGE}");
        return ExitCode::from(2);
    }
    // `new NAME` reads more naturally than `new --to NAME`; the wire
    // field is `to` either way.
    let to = if op == "new" { to.or(name.clone()) } else { to };
    let name = if op == "new" { None } else { name };

    let req = IpcRequest::Workspace {
        op: op.to_string(),
        name,
        to,
        project,
    };
    let Some(path) = socket_path() else {
        eprintln!("jimctl workspace: $HOME is not set");
        return ExitCode::FAILURE;
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jimctl workspace: connect {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let bytes = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl workspace: serialize: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = stream.write_all(&bytes) {
        eprintln!("jimctl workspace: write: {e}");
        return ExitCode::FAILURE;
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);

    if op != "list" {
        return ExitCode::SUCCESS;
    }
    let mut reply = String::new();
    if let Err(e) = stream.read_to_string(&mut reply) {
        eprintln!("jimctl workspace list: read: {e}");
        return ExitCode::FAILURE;
    }
    print_list(&reply)
}

fn print_list(reply: &str) -> ExitCode {
    let parsed: serde_json::Value = match serde_json::from_str(reply) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("jimctl workspace list: parse reply: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(list) = parsed.get("workspaces").and_then(|v| v.as_array()) else {
        eprintln!("jimctl workspace list: reply had no workspaces");
        return ExitCode::FAILURE;
    };
    for w in list {
        let name = w.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let current = w.get("current").and_then(|v| v.as_bool()).unwrap_or(false);
        let projects: Vec<&str> = w
            .get("projects")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|p| p.as_str()).collect())
            .unwrap_or_default();
        println!("{} {name}", if current { "*" } else { " " });
        if projects.is_empty() {
            println!("    (every project parked)");
        } else {
            println!("    {}", projects.join(", "));
        }
    }
    ExitCode::SUCCESS
}
