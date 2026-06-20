//! `jimctl dock` — snap existing panes into a new dock from the shell.
//!
//! The scriptable equivalent of dragging panes together: the matched
//! panes become columns of a new dock (sidebar + main), the first title
//! the leftmost column. Same path the snap gesture uses, so an agent can
//! compose a layout (e.g. file-tree + editor) without a mouse.
//!
//! Usage:
//!   jimctl dock --project P [--title T ...] [--template T]
//!
//!   --project P    project name (or `active`). Required.
//!   --title T      a pane title to include, repeatable; order = member
//!                  order. Omit all titles to dock EVERY free top-level
//!                  pane in P. Needs ≥2 matches.
//!   --template T   layout: columns (default), rows, sidebar, grid,
//!                  main-bottom.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let mut project: Option<String> = None;
    let mut titles: Vec<String> = Vec::new();
    let mut template: Option<String> = None;
    let mut empty = false;
    let mut slots: Option<usize> = None;
    let mut i = 0;
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
            "--template" | "-T" => {
                template = args.get(i + 1).cloned();
                i += 1;
            }
            "--empty" | "-e" => {
                empty = true;
            }
            "--slots" | "-s" => {
                slots = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: jimctl dock --project P [--title T ...] [--template columns|rows|sidebar|grid|main-bottom|columns-bottom] [--empty [--slots N]]"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("jimctl dock: unexpected arg `{}`", other);
                eprintln!("usage: jimctl dock --project P [--title T ...] [--template T] [--empty [--slots N]]");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    if project.is_none() {
        eprintln!("jimctl dock: --project is required");
        return ExitCode::from(2);
    }

    let req = serde_json::json!({
        "action": "dock_panes",
        "project": project,
        "titles": titles,
        "template": template,
        "empty": empty,
        "slots": slots,
    });

    let Some(sock) = socket_path() else {
        eprintln!("jimctl dock: $HOME not set; can't locate socket");
        return ExitCode::from(1);
    };
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl dock: connect {}: {} (is the jim app running?)",
                sock.display(),
                e
            );
            return ExitCode::from(1);
        }
    };
    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl dock: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = stream.write_all(&body) {
        eprintln!("jimctl dock: write: {}", e);
        return ExitCode::from(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    ExitCode::SUCCESS
}
