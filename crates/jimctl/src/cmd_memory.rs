//! `jimctl memory` — manage the DeepSeek action-planner's memory.
//!
//! The "Ask DeepSeek" command palette flow builds a system prompt from a
//! tool catalog (see `jim-app/src/tools.rs`). This CLI lets you append
//! open-ended, durable notes — facts and instructions about this system
//! and your preferences — that get folded into that system prompt on every
//! call. Use it to teach the planner things it can't infer from the live
//! context (project conventions, "always do X", what a custom widget does).
//!
//! Notes live in `~/.jim/actions-memory.jsonl`, one JSON object per line
//! (`{"ts":<unix secs>,"text":"<note>"}`). The format is shared with
//! `jim-app` (duplicated, per the workspace convention) — keep them in sync.
//!
//! Usage:
//!   jimctl memory add "<note>"   append a note (or read note from stdin)
//!   jimctl memory list           print numbered notes
//!   jimctl memory remove <n>     drop note #n (as shown by `list`)
//!   jimctl memory clear          remove all notes

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct MemoryEntry {
    #[serde(default)]
    ts: u64,
    text: String,
}

fn memory_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("actions-memory.jsonl"))
}

fn print_usage() {
    eprintln!(
        "usage:\n  \
         jimctl memory add \"<note>\"   append a note (omit to read from stdin)\n  \
         jimctl memory list           print numbered notes\n  \
         jimctl memory remove <n>     drop note #n\n  \
         jimctl memory clear          remove all notes"
    );
}

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let Some(sub) = args.first() else {
        print_usage();
        return ExitCode::from(2);
    };
    match sub.as_str() {
        "add" => cmd_add(&args[1..]),
        "list" => cmd_list(),
        "remove" | "rm" => cmd_remove(&args[1..]),
        "clear" => cmd_clear(),
        "-h" | "--help" | "help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("jimctl memory: unknown subcommand `{}`", other);
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn load() -> Vec<MemoryEntry> {
    let Some(path) = memory_path() else {
        return Vec::new();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<MemoryEntry>(l).ok())
        .collect()
}

fn save(entries: &[MemoryEntry]) -> ExitCode {
    let Some(path) = memory_path() else {
        eprintln!("jimctl memory: HOME not set");
        return ExitCode::from(1);
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("jimctl memory: create {}: {}", dir.display(), e);
            return ExitCode::from(1);
        }
    }
    let mut body = String::new();
    for e in entries {
        match serde_json::to_string(e) {
            Ok(line) => {
                body.push_str(&line);
                body.push('\n');
            }
            Err(e) => {
                eprintln!("jimctl memory: serialize: {}", e);
                return ExitCode::from(1);
            }
        }
    }
    if let Err(e) = std::fs::write(&path, body) {
        eprintln!("jimctl memory: write {}: {}", path.display(), e);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn cmd_add(args: &[String]) -> ExitCode {
    let text = if args.is_empty() {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            eprintln!("jimctl memory add: failed to read stdin");
            return ExitCode::from(1);
        }
        buf
    } else {
        args.join(" ")
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        eprintln!("jimctl memory add: empty note");
        return ExitCode::from(2);
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Append a single line — no need to rewrite the whole file.
    let Some(path) = memory_path() else {
        eprintln!("jimctl memory: HOME not set");
        return ExitCode::from(1);
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("jimctl memory: create {}: {}", dir.display(), e);
            return ExitCode::from(1);
        }
    }
    let line = match serde_json::to_string(&MemoryEntry { ts, text }) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("jimctl memory: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("jimctl memory: open {}: {}", path.display(), e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = writeln!(file, "{line}") {
        eprintln!("jimctl memory: write {}: {}", path.display(), e);
        return ExitCode::from(1);
    }
    eprintln!("jimctl memory: added");
    ExitCode::SUCCESS
}

fn cmd_list() -> ExitCode {
    let entries = load();
    if entries.is_empty() {
        eprintln!("jimctl memory: (empty)");
        return ExitCode::SUCCESS;
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (i, e) in entries.iter().enumerate() {
        // Indent continuation lines so multi-line notes stay readable.
        let body = e.text.replace('\n', "\n     ");
        let _ = writeln!(out, "{:>3}. {}", i + 1, body);
    }
    ExitCode::SUCCESS
}

fn cmd_remove(args: &[String]) -> ExitCode {
    let Some(n) = args.first().and_then(|s| s.parse::<usize>().ok()) else {
        eprintln!("jimctl memory remove: expected a note number (see `jimctl memory list`)");
        return ExitCode::from(2);
    };
    let mut entries = load();
    if n == 0 || n > entries.len() {
        eprintln!(
            "jimctl memory remove: no note #{} (have {})",
            n,
            entries.len()
        );
        return ExitCode::from(1);
    }
    entries.remove(n - 1);
    let code = save(&entries);
    if code == ExitCode::SUCCESS {
        eprintln!("jimctl memory: removed #{n}");
    }
    code
}

fn cmd_clear() -> ExitCode {
    let code = save(&[]);
    if code == ExitCode::SUCCESS {
        eprintln!("jimctl memory: cleared");
    }
    code
}
