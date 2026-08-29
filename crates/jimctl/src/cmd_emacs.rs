//! `jimctl emacs` — open the Emacs workspace in a project.
//!
//! A file tree docked beside a native Emacs pane, rooted at the
//! project's own directory. This is the normal way to get an editor,
//! for a person or an agent; spawning a bare `emacs-native` pane gives
//! you the editor with no navigation beside it.
//!
//! Usage:
//!   jimctl emacs [--project P] [--path DIR]
//!
//!   --project P   project name, or `active` (the default).
//!   --path DIR    root the tree here instead of the project directory.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum IpcRequest {
    EmacsWorkspace {
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let mut project: Option<String> = None;
    let mut path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--project" | "-p" => {
                project = args.get(i + 1).cloned();
                i += 1;
            }
            "--path" | "-d" => {
                path = args.get(i + 1).cloned();
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: jimctl emacs [--project P] [--path DIR]\n\
                     \n\
                     Opens a file tree docked beside a native Emacs pane.\n\
                     Defaults to the active project, rooted at its directory."
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("jimctl emacs: unexpected argument {other:?}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    // A relative --path is only meaningful to the caller's shell, so
    // resolve it here rather than against the GUI's working directory.
    if let Some(p) = path.take() {
        path = Some(match PathBuf::from(&p).canonicalize() {
            Ok(abs) => abs.to_string_lossy().into_owned(),
            Err(e) => {
                eprintln!("jimctl emacs: {p}: {e}");
                return ExitCode::from(1);
            }
        });
    }

    let Some(sock) = socket_path() else {
        eprintln!("jimctl emacs: HOME not set");
        return ExitCode::from(1);
    };
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl emacs: connect {}: {e} (is the jim app running?)",
                sock.display()
            );
            return ExitCode::from(1);
        }
    };
    let req = IpcRequest::EmacsWorkspace { project, path };
    let line = serde_json::to_string(&req).expect("serialize EmacsWorkspace");
    if let Err(e) = writeln!(stream, "{line}") {
        eprintln!("jimctl emacs: write: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
