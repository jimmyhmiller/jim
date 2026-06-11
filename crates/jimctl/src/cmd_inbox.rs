//! `jimctl inbox` — push to, or read from, a project's inbox.
//!
//! Writing goes over the running app's Unix socket; reading reads the
//! on-disk JSONL store directly (`~/.jim/inbox/<project_id>.jsonl`) so
//! it works even when the app isn't running.
//!
//! Usage:
//!     jimctl inbox --body "hello" [--project NAME] [--sender X] [--subject Y]
//!     echo "stdin body" | jimctl inbox --project alpha
//!     jimctl inbox read [--project NAME] [--json] [--unread]
//!
//! `--project` defaults to whichever project is currently active. The
//! body can be passed via `--body` OR piped on stdin (stdin wins when
//! both are present).
//!
//! Like `open`, this binary deliberately stays lib-free: the parent
//! `jim_app` crate links a dylib (libghostty-vt) we don't want
//! to pull into a tiny CLI.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum IpcRequest {
    SendInbox {
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
        body: String,
    },
}

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

fn data_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim"))
}

fn print_usage() {
    eprintln!(
        "usage:\n\
         \tjimctl inbox [--project NAME] [--sender X] [--subject Y] (--body TEXT | < stdin)\n\
         \tjimctl inbox read [--project NAME] [--json] [--unread]"
    );
}

pub fn run() -> ExitCode {
    // `read` is the only subcommand; everything else is the send path
    // (kept argv-compatible with the original `tbinbox` binary).
    let raw: Vec<String> = crate::sub_args().collect();
    if raw.first().map(|s| s.as_str()) == Some("read") {
        return read_inbox(raw.into_iter().skip(1).collect());
    }

    let mut args = raw.into_iter();
    let mut project: Option<String> = None;
    let mut sender: Option<String> = None;
    let mut subject: Option<String> = None;
    let mut body_arg: Option<String> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--project" => project = args.next(),
            "--sender" => sender = args.next(),
            "--subject" => subject = args.next(),
            "--body" => body_arg = args.next(),
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("jimctl inbox: unknown arg {:?}", other);
                print_usage();
                return ExitCode::from(2);
            }
        }
    }

    // Stdin wins over --body when stdin is piped (not a tty).
    let body = match read_stdin_if_piped() {
        Some(s) if !s.is_empty() => s,
        _ => match body_arg {
            Some(s) => s,
            None => {
                eprintln!("jimctl inbox: need --body TEXT or piped stdin");
                print_usage();
                return ExitCode::from(2);
            }
        },
    };

    let Some(path) = socket_path() else {
        eprintln!("jimctl inbox: $HOME not set");
        return ExitCode::from(1);
    };

    let mut sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl inbox: connect {}: {}\n  (is terminal-bevy running?)",
                path.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    let req = IpcRequest::SendInbox {
        project,
        sender,
        subject,
        body,
    };
    let bytes = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl inbox: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = sock.write_all(&bytes) {
        eprintln!("jimctl inbox: write: {}", e);
        return ExitCode::from(1);
    }
    // EOF tells the app side we're done — it expects single-shot
    // JSON-to-EOF on each connection.
    let _ = sock.shutdown(std::net::Shutdown::Write);
    ExitCode::SUCCESS
}

fn read_stdin_if_piped() -> Option<String> {
    use std::io::IsTerminal;
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buf = String::new();
    let mut handle = stdin.lock();
    handle.read_to_string(&mut buf).ok()?;
    Some(buf.trim_end_matches('\n').to_string())
}

// ---------- read ----------

/// Mirror of `jim_app::inbox::InboxMessage` for parsing the JSONL store.
/// We keep our own copy so the CLI stays lib-free of `jim_app`.
#[derive(Deserialize)]
struct InboxMessage {
    #[allow(dead_code)]
    id: u64,
    /// Unix milliseconds.
    ts: u64,
    #[serde(default)]
    sender: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    read: bool,
}

/// Minimal view of `~/.jim/projects.json` — just enough to resolve a
/// project name (or "active") to its numeric id.
#[derive(Deserialize)]
struct PersistedProjects {
    #[serde(default)]
    projects: Vec<ProjectData>,
    #[serde(default)]
    active: Option<u64>,
}

#[derive(Deserialize)]
struct ProjectData {
    id: u64,
    name: String,
}

fn print_read_usage() {
    eprintln!("usage: jimctl inbox read [--project NAME] [--json] [--unread]");
}

fn read_inbox(args: Vec<String>) -> ExitCode {
    let mut project: Option<String> = None;
    let mut json = false;
    let mut unread_only = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--project" => match it.next() {
                Some(v) => project = Some(v),
                None => {
                    eprintln!("jimctl inbox read: --project needs a value");
                    return ExitCode::from(2);
                }
            },
            "--json" => json = true,
            "--unread" => unread_only = true,
            "-h" | "--help" => {
                print_read_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("jimctl inbox read: unknown arg {:?}", other);
                print_read_usage();
                return ExitCode::from(2);
            }
        }
    }

    let Some(root) = data_root() else {
        eprintln!("jimctl inbox read: $HOME not set");
        return ExitCode::from(1);
    };

    // Resolve project name → id by reading projects.json directly. This
    // matches the app's name match (case-insensitive) and "active"
    // fallback, but works without the GUI running.
    let state: PersistedProjects = {
        let path = root.join("projects.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("jimctl inbox read: parse {}: {}", path.display(), e);
                PersistedProjects { projects: Vec::new(), active: None }
            }),
            Err(e) => {
                eprintln!("jimctl inbox read: read {}: {}", path.display(), e);
                return ExitCode::from(1);
            }
        }
    };

    let project_id = match project.as_deref() {
        Some("active") | None => match state.active {
            Some(id) => id,
            None => {
                eprintln!("jimctl inbox read: no active project");
                return ExitCode::from(1);
            }
        },
        Some(name) => {
            match state
                .projects
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
            {
                Some(p) => p.id,
                None => {
                    eprintln!("jimctl inbox read: no project named {:?}", name);
                    return ExitCode::from(1);
                }
            }
        }
    };

    let path = root.join("inbox").join(format!("{}.jsonl", project_id));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // No file yet just means an empty inbox — not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            eprintln!("jimctl inbox read: read {}: {}", path.display(), e);
            return ExitCode::from(1);
        }
    };

    let mut messages: Vec<InboxMessage> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<InboxMessage>(line) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("jimctl inbox read: parse line: {} ({:?})", e, line);
                None
            }
        })
        .filter(|m| !unread_only || !m.read)
        .collect();
    messages.sort_by_key(|m| m.ts);

    if json {
        // Re-emit the raw JSONL lines we kept (post-filter).
        for m in &messages {
            let v = serde_json::json!({
                "id": m.id,
                "ts": m.ts,
                "sender": m.sender,
                "subject": m.subject,
                "body": m.body,
                "read": m.read,
            });
            println!("{}", v);
        }
        return ExitCode::SUCCESS;
    }

    if messages.is_empty() {
        println!("(inbox empty)");
        return ExitCode::SUCCESS;
    }

    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let flag = if m.read { " " } else { "•" };
        let when = format_ts(m.ts);
        let sender = if m.sender.is_empty() { "external" } else { &m.sender };
        match &m.subject {
            Some(s) if !s.is_empty() => {
                println!("{} [{}] {} — {}", flag, when, sender, s);
            }
            _ => println!("{} [{}] {}", flag, when, sender),
        }
        for line in m.body.lines() {
            println!("    {}", line);
        }
    }

    ExitCode::SUCCESS
}

/// Format unix-millis as `YYYY-MM-DD HH:MM` (UTC). Dependency-free so we
/// don't pull `chrono` into the CLI; uses Howard Hinnant's civil-from-days.
fn format_ts(ts_millis: u64) -> String {
    let secs = (ts_millis / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hh, mm) = ((secs_of_day / 3600) % 24, (secs_of_day / 60) % 60);

    // civil_from_days: days since 1970-01-01 → (year, month, day).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, hh, mm)
}
