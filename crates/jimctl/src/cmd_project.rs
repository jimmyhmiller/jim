//! `jimctl project` — manage projects in the running terminal-bevy app
//! over its Unix socket.
//!
//! `set-cwd` writes a project's `default_cwd` (the directory new
//! terminals are spawned in for that project, also the cwd a Claude
//! widget falls back to). Without an explicit path, the current shell
//! cwd is used. `new` creates a project and prints `{"id","name"}`;
//! `fit` pans/zooms a project's canvas to show its panes.
//!
//! Usage:
//!     jimctl project set-cwd                     # active project ← $PWD
//!     jimctl project set-cwd .                   # same, explicit
//!     jimctl project set-cwd /some/path
//!     jimctl project set-cwd --project Recursion /some/path
//!     jimctl project set-cwd --project Recursion --clear
//!     jimctl project new [--name N] [--cwd P] [--activate]
//!     jimctl project fit [--project P] [--bounds x,y,w,h]
//!
//! Stays lib-free for the same reason as `open` / `inbox` — we don't
//! want to drag libghostty-vt into a tiny CLI.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum IpcRequest {
    SetProjectDefaultCwd {
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
}

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

fn print_usage() {
    eprintln!(
        "usage: jimctl project set-cwd [--project NAME] [PATH | --clear]\n\
         \x20      jimctl project new [--name NAME] [--cwd PATH] [--activate]\n\
         \x20      jimctl project fit [--project NAME] [--bounds x,y,w,h]\n\
         \n\
         set-cwd  Sets a project's default_cwd. PATH defaults to the current\n\
         \x20        shell directory. --project defaults to the active project.\n\
         \x20        --clear removes the override so new terminals fall back to $HOME.\n\
         new      Creates a project and prints {{\"id\":N,\"name\":\"…\"}}. A taken\n\
         \x20        name gets a \" (2)\"-style suffix; use the printed name.\n\
         fit      Pans/zooms the project's root canvas to show all its panes,\n\
         \x20        or the canvas-space rect given by --bounds."
    );
}

fn new_project(args: impl Iterator<Item = String>) -> ExitCode {
    let mut name: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut activate = false;
    let mut it = args;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next(),
            "--cwd" => {
                cwd = it
                    .next()
                    .map(|p| std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p)))
            }
            "--activate" => activate = true,
            other => {
                eprintln!("jimctl project new: unknown arg {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let req = serde_json::json!({
        "action": "create_project",
        "name": name,
        "cwd": cwd,
        "activate": activate,
    });
    match crate::app_ipc::request(&req, true) {
        Ok(Some(reply)) => {
            println!("{reply}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jimctl project new: {e}");
            ExitCode::from(1)
        }
    }
}

fn fit(args: impl Iterator<Item = String>) -> ExitCode {
    let mut project: Option<String> = None;
    let mut bounds: Option<[f32; 4]> = None;
    let mut it = args;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--project" => project = it.next(),
            "--bounds" => {
                let parsed: Vec<f32> = it
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                let [x, y, w, h] = parsed[..] else {
                    eprintln!("jimctl project fit: --bounds wants x,y,w,h");
                    return ExitCode::from(2);
                };
                bounds = Some([x, y, w, h]);
            }
            other => {
                eprintln!("jimctl project fit: unknown arg {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let req = serde_json::json!({ "action": "fit_canvas", "project": project, "bounds": bounds });
    match crate::app_ipc::request(&req, false) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jimctl project fit: {e}");
            ExitCode::from(1)
        }
    }
}

pub fn run() -> ExitCode {
    let mut args = crate::sub_args();
    let Some(sub) = args.next() else {
        print_usage();
        return ExitCode::from(2);
    };
    match sub.as_str() {
        "set-cwd" => set_cwd(args),
        "new" => new_project(args),
        "fit" => fit(args),
        "-h" | "--help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("jimctl project: unknown subcommand {:?}", other);
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn set_cwd(args: impl Iterator<Item = String>) -> ExitCode {
    let mut project: Option<String> = None;
    let mut path_arg: Option<String> = None;
    let mut clear = false;
    let mut it = args.peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--project" => match it.next() {
                Some(v) => project = Some(v),
                None => {
                    eprintln!("jimctl project: --project needs a value");
                    return ExitCode::from(2);
                }
            },
            "--clear" => clear = true,
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with("--") => {
                eprintln!("jimctl project set-cwd: unknown flag {:?}", other);
                return ExitCode::from(2);
            }
            _ => {
                if path_arg.is_some() {
                    eprintln!("jimctl project set-cwd: extra positional arg {:?}", a);
                    return ExitCode::from(2);
                }
                path_arg = Some(a);
            }
        }
    }

    if clear && path_arg.is_some() {
        eprintln!("jimctl project set-cwd: --clear can't be combined with a PATH");
        return ExitCode::from(2);
    }

    let cwd: Option<PathBuf> = if clear {
        None
    } else {
        let raw = path_arg.unwrap_or_else(|| ".".to_string());
        let p = PathBuf::from(&raw);
        let abs = if p.is_absolute() {
            p
        } else {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join(&p),
                Err(e) => {
                    eprintln!("jimctl project set-cwd: getcwd: {}", e);
                    return ExitCode::from(1);
                }
            }
        };
        // Canonicalize when possible so the app stores a stable path,
        // but fall back to the joined form if the directory doesn't
        // exist yet (matches macOS Terminal's "open in" behavior).
        let canon = std::fs::canonicalize(&abs).unwrap_or(abs);
        Some(canon)
    };

    let Some(socket) = socket_path() else {
        eprintln!("jimctl project: $HOME not set");
        return ExitCode::from(1);
    };

    let mut sock = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl project: connect {}: {}\n  (is terminal-bevy running?)",
                socket.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    let req = IpcRequest::SetProjectDefaultCwd {
        project: project.clone(),
        cwd: cwd.clone(),
    };
    let bytes = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl project: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = sock.write_all(&bytes) {
        eprintln!("jimctl project: write: {}", e);
        return ExitCode::from(1);
    }
    let _ = sock.shutdown(std::net::Shutdown::Write);

    match (&project, &cwd) {
        (Some(p), Some(c)) => println!(
            "jimctl project: project {:?} default_cwd ← {}",
            p,
            c.display()
        ),
        (None, Some(c)) => println!(
            "jimctl project: active project default_cwd ← {}",
            c.display()
        ),
        (Some(p), None) => println!("jimctl project: project {:?} default_cwd cleared", p),
        (None, None) => println!("jimctl project: active project default_cwd cleared"),
    }
    ExitCode::SUCCESS
}
