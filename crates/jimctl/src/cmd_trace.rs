//! `jimctl trace` — control the running app's rich frame-trace recorder.
//!
//! The recorder (`jim_pane::trace`) captures every span of a slow frame and
//! dumps it to `~/.jim/traces/frame-*.json`. It's armed at launch by
//! `JIMTRACE=1` and thresholded by `JIMTRACE_MS`, but those env vars are
//! frozen once the GUI starts. This command drives the same two knobs over the
//! IPC socket so you can arm/disarm capture and retune the slow-frame
//! threshold WITHOUT restarting the app.
//!
//! Usage:
//!   jimctl trace [--arm | --disarm] [--ms N]
//!
//!   --arm         enable capture (same as Cmd+Shift+G in the app)
//!   --disarm      disable capture
//!   --ms N        set the slow-frame dump threshold to N active-ms
//!
//! With no flags it just queries and prints the current state. The app writes
//! back `{"armed":bool,"threshold_ms":N}`, which we echo.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

const USAGE: &str = "usage: jimctl trace [--arm | --disarm] [--ms N]";

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let mut arm: Option<bool> = None;
    let mut ms: Option<f32> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--arm" | "-a" => arm = Some(true),
            "--disarm" | "-d" => arm = Some(false),
            "--ms" | "-m" => {
                match args.get(i + 1).map(|v| v.parse::<f32>()) {
                    Some(Ok(v)) => ms = Some(v),
                    _ => {
                        eprintln!("jimctl trace: --ms needs a number");
                        return ExitCode::from(2);
                    }
                }
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("jimctl trace: unexpected arg `{}`\n{}", other, USAGE);
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let req = serde_json::json!({
        "action": "trace_control",
        "arm": arm,
        "ms": ms,
    });

    let Some(sock) = socket_path() else {
        eprintln!("jimctl trace: $HOME not set; can't locate socket");
        return ExitCode::from(1);
    };
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl trace: connect {}: {} (is the Jim app running?)",
                sock.display(),
                e
            );
            return ExitCode::from(1);
        }
    };
    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl trace: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = stream.write_all(&body) {
        eprintln!("jimctl trace: write: {}", e);
        return ExitCode::from(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);

    // Read the status echo the app writes back.
    let mut resp = String::new();
    match stream.read_to_string(&mut resp) {
        Ok(_) if !resp.trim().is_empty() => println!("{}", resp.trim()),
        _ => {}
    }
    ExitCode::SUCCESS
}
