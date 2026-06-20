//! `jimctl lsp` — structural code queries via the `jim-lsp` rust-analyzer
//! sidecar.
//!
//! The daemon (one per Cargo workspace root) is spawned on demand and outlives
//! the GUI so rust-analyzer's index persists. These subcommands are thin
//! clients over its Unix socket:
//!
//!   jimctl lsp ensure     [--root P]            start RA, wait for indexing
//!   jimctl lsp symbols    <file> [--root P]     document symbol tree (JSON)
//!   jimctl lsp source     <file> --range L:C-L:C   slice a range to text
//!   jimctl lsp references <file> --pos L:C
//!   jimctl lsp definition <file> --pos L:C
//!   jimctl lsp hover      <file> --pos L:C
//!   jimctl lsp rpc        [--root P]            long-lived JSONL bridge
//!
//! One-shot subcommands print the daemon's JSON reply (an `{"error":…}` line
//! and a non-zero exit on failure). `rpc` is what the LSP explorer widget
//! drives: it forwards request lines from stdin to the daemon and the daemon's
//! reply lines to stdout, staying alive until stdin closes.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::process::ExitCode;

use jim_lsp::{Op, Position, Range, RequestLine};

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let Some(sub) = args.first() else {
        print_usage();
        return ExitCode::from(2);
    };
    match sub.as_str() {
        "ensure" => one_shot(&args[1..], |_| Ok(Op::Ensure)),
        "symbols" => one_shot(&args[1..], |a| Ok(Op::Symbols { file: need_file(a)? })),
        "outline" => one_shot(&args[1..], |_| Ok(Op::Outline)),
        "types" => one_shot(&args[1..], |_| Ok(Op::TypeOutline)),
        "impls" => one_shot(&args[1..], |a| {
            Ok(Op::Impls {
                name: a.file.clone().unwrap_or_default(),
            })
        }),
        "ws" | "workspace-symbols" => one_shot(&args[1..], |a| {
            Ok(Op::WorkspaceSymbols {
                query: a.file.clone().unwrap_or_default(),
            })
        }),
        "source" => one_shot(&args[1..], |a| {
            Ok(Op::Source {
                file: need_file(a)?,
                range: need_range(a)?,
            })
        }),
        "references" => one_shot(&args[1..], |a| {
            Ok(Op::References {
                file: need_file(a)?,
                position: need_pos(a)?,
            })
        }),
        "definition" => one_shot(&args[1..], |a| {
            Ok(Op::Definition {
                file: need_file(a)?,
                position: need_pos(a)?,
            })
        }),
        "hover" => one_shot(&args[1..], |a| {
            Ok(Op::Hover {
                file: need_file(a)?,
                position: need_pos(a)?,
            })
        }),
        "rpc" => cmd_rpc(&args[1..]),
        "-h" | "--help" | "help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("jimctl lsp: unknown subcommand `{other}`");
            print_usage();
            ExitCode::from(2)
        }
    }
}

/// Resolve the workspace root for this invocation: `--root` if given, else the
/// target file's location, else the current directory. Then connect to (or
/// spawn) the daemon and run one request.
fn one_shot(
    args: &[String],
    build: impl FnOnce(&Parsed) -> Result<Op, String>,
) -> ExitCode {
    let parsed = Parsed::from(args);
    let op = match build(&parsed) {
        Ok(op) => op,
        Err(e) => {
            eprintln!("jimctl lsp: {e}");
            return ExitCode::from(2);
        }
    };

    // Pick the workspace-root hint: an explicit --root, else the file arg when
    // it's a real path (symbols/source), else cwd (e.g. `ws <query>`, where the
    // positional is a search string, not a file).
    let start = parsed
        .root
        .clone()
        .or_else(|| {
            parsed
                .file
                .as_ref()
                .map(PathBuf::from)
                .filter(|p| p.exists())
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let root = match jim_lsp::workspace_root(&start) {
        Some(r) => r,
        None => {
            return emit_error(
                "no_workspace",
                format!("{} is not inside a Cargo workspace", start.display()),
            );
        }
    };

    let stream = match jim_lsp::connect_or_spawn(&root) {
        Ok(s) => s,
        Err(e) => return emit_error("spawn", e),
    };
    let req = RequestLine { id: None, op };
    match jim_lsp::request_once(stream, &req) {
        Ok(resp) => {
            // Print the whole response line (result or structured error).
            match serde_json::to_string(&resp) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("jimctl lsp: serialize response: {e}");
                    return ExitCode::from(1);
                }
            }
            if resp.error.is_some() {
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(e) => emit_error("transport", e),
    }
}

/// Print a structured error on STDOUT (not just stderr) so a widget driving us
/// via `proc_spawn` — whose stderr is discarded — still sees what went wrong.
/// Mirrors the daemon's `{"error":{code,message}}` response shape.
fn emit_error(code: &str, message: impl Into<String>) -> ExitCode {
    let msg = message.into();
    let line = serde_json::json!({ "error": { "code": code, "message": msg } });
    println!("{line}");
    eprintln!("jimctl lsp: {msg}");
    ExitCode::from(1)
}

/// Long-lived JSONL bridge for the LSP explorer widget. Stdin lines → daemon;
/// daemon lines → stdout. Exits when stdin closes (the widget's `proc_kill`)
/// or the daemon goes away.
fn cmd_rpc(args: &[String]) -> ExitCode {
    let parsed = Parsed::from(args);
    let start = parsed
        .root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let root = match jim_lsp::workspace_root(&start) {
        Some(r) => r,
        None => {
            // Surface as a JSON error line on stdout — the widget's stderr is
            // discarded, so errors MUST come back on the reply channel.
            println!(
                "{{\"error\":{{\"code\":\"no_workspace\",\"message\":\"{} is not in a Cargo workspace\"}}}}",
                start.display()
            );
            return ExitCode::from(1);
        }
    };

    let stream = match jim_lsp::connect_or_spawn(&root) {
        Ok(s) => s,
        Err(e) => {
            println!(
                "{{\"error\":{{\"code\":\"spawn\",\"message\":{}}}}}",
                serde_json::Value::String(e)
            );
            return ExitCode::from(1);
        }
    };
    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jimctl lsp rpc: clone socket: {e}");
            return ExitCode::from(1);
        }
    };

    // Daemon → stdout.
    let reader = std::thread::spawn(move || {
        let mut out = std::io::stdout();
        let r = BufReader::new(read_stream);
        for line in r.lines() {
            let Ok(line) = line else { break };
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    });

    // Stdin → daemon.
    let mut writer = stream;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if writeln!(writer, "{line}").and_then(|_| writer.flush()).is_err() {
            break;
        }
    }
    // Stdin closed: half-close our write side so the daemon sees EOF and
    // finishes any in-flight replies, then drain the reader until it closes.
    let _ = writer.shutdown(Shutdown::Write);
    let _ = reader.join();
    ExitCode::SUCCESS
}

// --- arg parsing ---------------------------------------------------------

struct Parsed {
    file: Option<String>,
    root: Option<PathBuf>,
    range: Option<String>,
    pos: Option<String>,
}

impl Parsed {
    fn from(args: &[String]) -> Self {
        let mut file = None;
        let mut root = None;
        let mut range = None;
        let mut pos = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--root" | "-r" => root = it.next().map(PathBuf::from),
                "--range" => range = it.next().cloned(),
                "--pos" | "--position" => pos = it.next().cloned(),
                s if s.starts_with("--") => { /* ignore unknown flags */ }
                s => {
                    if file.is_none() {
                        file = Some(s.to_string());
                    }
                }
            }
        }
        Self { file, root, range, pos }
    }
}

fn need_file(p: &Parsed) -> Result<String, String> {
    p.file
        .clone()
        .ok_or_else(|| "a <file> argument is required".to_string())
}

fn need_pos(p: &Parsed) -> Result<Position, String> {
    let raw = p.pos.as_deref().ok_or("--pos L:C is required")?;
    parse_pos(raw).ok_or_else(|| format!("invalid --pos `{raw}` (want LINE:COL)"))
}

fn need_range(p: &Parsed) -> Result<Range, String> {
    let raw = p.range.as_deref().ok_or("--range L:C-L:C is required")?;
    let (a, b) = raw
        .split_once('-')
        .ok_or_else(|| format!("invalid --range `{raw}` (want L:C-L:C)"))?;
    let start = parse_pos(a).ok_or_else(|| format!("invalid range start `{a}`"))?;
    let end = parse_pos(b).ok_or_else(|| format!("invalid range end `{b}`"))?;
    Ok(Range { start, end })
}

/// Parse `LINE:COL` (0-based, matching LSP) into a `Position`.
fn parse_pos(s: &str) -> Option<Position> {
    let (l, c) = s.trim().split_once(':')?;
    Some(Position {
        line: l.trim().parse().ok()?,
        character: c.trim().parse().ok()?,
    })
}

fn print_usage() {
    eprintln!(
        "usage:\n  \
         jimctl lsp ensure     [--root P]\n  \
         jimctl lsp symbols    <file> [--root P]\n  \
         jimctl lsp source     <file> --range L:C-L:C [--root P]\n  \
         jimctl lsp references <file> --pos L:C [--root P]\n  \
         jimctl lsp definition <file> --pos L:C [--root P]\n  \
         jimctl lsp hover      <file> --pos L:C [--root P]\n  \
         jimctl lsp rpc        [--root P]      long-lived JSONL bridge (one request per line)\n\
         \n  \
         Positions are 0-based LINE:COL (LSP convention). <file> may be absolute\n  \
         or relative to the workspace root. The daemon is started on demand and\n  \
         persists across GUI restarts."
    );
}
