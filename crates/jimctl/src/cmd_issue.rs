//! `jimctl issue` — file an issue into a project's Issues pane from the shell.
//!
//! Usage:
//!     jimctl issue "Fix the flaky test"
//!     jimctl issue --title "Fix the flaky test" --body "races on startup"
//!     jimctl issue -t "Cross-project note" --project editor-idea
//!
//! The bare first positional argument is taken as the title, so the
//! common case is just `jimctl issue "some title"`. By default the issue
//! lands in the project that owns the current directory (matched by the
//! project's `default_cwd`); pass `--project NAME` to override, or it
//! falls back to the app's active project.
//!
//! The app must already be running. The wire format is duplicated here
//! on purpose so this bin stays free of the libghostty-vt dylib (see
//! `open` for the rationale).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum IpcRequest {
    AddIssue {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        /// The dir jimctl issue was invoked in. The app maps it to the owning
        /// project (by `default_cwd`) when `project` isn't given.
        #[serde(skip_serializing_if = "Option::is_none")]
        from_cwd: Option<PathBuf>,
    },
}

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

pub fn run() -> ExitCode {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{}", msg);
            print_usage();
            return ExitCode::from(2);
        }
    };

    let Some(title) = args.title else {
        eprintln!("jimctl issue: need a title (positional or --title)");
        print_usage();
        return ExitCode::from(2);
    };

    let Some(sock) = socket_path() else {
        eprintln!("jimctl issue: $HOME not set; can't locate socket");
        return ExitCode::from(1);
    };

    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl issue: connect {}: {} (is the terminal-bevy app running?)",
                sock.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    let req = IpcRequest::AddIssue {
        title,
        body: args.body,
        project: args.project,
        from_cwd: std::env::current_dir().ok(),
    };
    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl issue: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = stream.write_all(&body) {
        eprintln!("jimctl issue: write: {}", e);
        return ExitCode::from(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    ExitCode::SUCCESS
}

#[derive(Default)]
struct Args {
    title: Option<String>,
    body: Option<String>,
    project: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut a = Args::default();
        let mut it = crate::sub_args();
        while let Some(arg) = it.next() {
            let mut take = |name: &str| -> Result<String, String> {
                it.next().ok_or_else(|| format!("{} requires a value", name))
            };
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--title" | "-t" => a.title = Some(take("--title")?),
                "--body" | "-b" => a.body = Some(take("--body")?),
                "--project" | "-p" => a.project = Some(take("--project")?),
                other if other.starts_with('-') => {
                    return Err(format!("unknown argument: {}", other));
                }
                // First bare positional is the title.
                other => {
                    if a.title.is_none() {
                        a.title = Some(other.to_string());
                    } else {
                        return Err(format!("unexpected argument: {}", other));
                    }
                }
            }
        }
        Ok(a)
    }
}

fn print_usage() {
    eprintln!(
        "jimctl issue TITLE [--body B] [--project NAME]\n\
         jimctl issue --title TITLE [--body B] [--project NAME]\n\
         \n\
         File an issue into a project's Issues pane from the shell.\n\
         Defaults to the project owning the current directory; falls\n\
         back to the active project. Requires the app to be running."
    );
}
