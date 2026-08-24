//! `jimctl move` — move panes from one project to another.
//!
//! Project membership (`PaneProject`) is what confines a pane to a project,
//! so this is the scriptable equivalent of "this pane belongs over there
//! instead". Nothing is closed and no pane state is touched: the pane keeps
//! its terminal session, its widget state and its size.
//!
//! Usage:
//!   jimctl move --to DEST [--project SRC] (--kind K | --title T ... | --all)
//!
//!   --to DEST     destination project name (or `active`). Required.
//!   --project SRC source project name (or `active`). Defaults to active.
//!   --kind K      only move panes of this kind (e.g. `script_widget`).
//!   --title T     only move panes with this exact title; repeatable.
//!   --all         move EVERY pane in the source project. Required to run
//!                 with no `--kind`/`--title` filter, so an unfiltered mass
//!                 move can only happen on purpose, never by accident.
//!
//! Moved panes land on the destination's root canvas and lose any named
//! group, because both gate visibility per-project — carrying them over
//! would make the pane arrive invisible. Docked panes are refused; undock
//! them first, since a dock owns its members' layout.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

const USAGE: &str =
    "usage: jimctl move --to DEST [--project SRC] (--kind K | --title T ... | --all)";

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let mut project: Option<String> = None;
    let mut to: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut titles: Vec<String> = Vec::new();
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--project" | "-p" => {
                project = args.get(i + 1).cloned();
                i += 1;
            }
            "--to" => {
                to = args.get(i + 1).cloned();
                i += 1;
            }
            "--kind" | "-k" => {
                kind = args.get(i + 1).cloned();
                i += 1;
            }
            "--title" | "-t" => {
                if let Some(t) = args.get(i + 1).cloned() {
                    titles.push(t);
                }
                i += 1;
            }
            "--all" => all = true,
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("jimctl move: unexpected arg `{}`", other);
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    if to.is_none() {
        eprintln!("jimctl move: --to is required");
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    // Same guard as `close`: moving EVERY pane out of a project must be
    // explicit, so a forgotten filter can't silently empty one.
    if kind.is_none() && titles.is_empty() && !all {
        eprintln!(
            "jimctl move: refusing to move ALL panes without a filter.\n  \
             pass --kind K or --title T to target panes, or --all to move every pane \
             in the project."
        );
        return ExitCode::from(2);
    }

    let req = serde_json::json!({
        "action": "move_panes",
        "project": project,
        "to": to,
        "kind": kind,
        "titles": if titles.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(titles)
        },
    });

    let Some(sock) = socket_path() else {
        eprintln!("jimctl move: $HOME not set; can't locate socket");
        return ExitCode::from(1);
    };
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl move: connect {}: {} (is the terminal-bevy app running?)",
                sock.display(),
                e
            );
            return ExitCode::from(1);
        }
    };
    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl move: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = stream.write_all(&body) {
        eprintln!("jimctl move: write: {}", e);
        return ExitCode::from(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    ExitCode::SUCCESS
}
