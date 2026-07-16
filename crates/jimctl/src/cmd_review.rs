//! `jimctl review` — local code-review threads (the GitHub-independent
//! review loop between Jimmy and agent sessions).
//!
//! Store: `~/.jim/reviews/<repo_hash>.json` via the `jim_review` crate.
//! Every mutation publishes a `review.changed` event on the GLOBAL bus
//! channel (`{repo, id, action}` — a pointer, never the comment text),
//! plus a mirror on `--project`'s channel when given, so widgets can
//! listen on their own project without hashing repo paths.
//!
//! Output is one JSON line on stdout (widgets `proc_spawn` this; their
//! stderr is discarded). Agents: `jimctl review list --repo <root>` to
//! read comments, `reply`/`resolve` to respond — author defaults to
//! `agent:<id>` when `JIM_AGENT_ID`/`JIM_CHANNEL_ID` is set, `user`
//! otherwise.
//!
//! Usage:
//!   jimctl review list    [--repo P] [--status open|resolved|all] [--file F]
//!   jimctl review show    --id N [--repo P]
//!   jimctl review add     --file F --line N --body "..." [--base-ref REF]
//!                         [--author user|agent:NAME] [--project NAME] [--repo P]
//!   jimctl review reply   --id N --body "..." [--author ...] [--project NAME] [--repo P]
//!   jimctl review resolve --id N [--reopen] [--project NAME] [--repo P]
//!   jimctl review reanchor [--repo P]     re-locate anchors against current files
//!
//! `list` with no `--repo` aggregates every repo's store (the review
//! inbox view). Legacy flat comment files (diff.ft's old format) in the
//! same directory are skipped, not migrated.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use jim_review::{Author, ReviewFile, Thread, ThreadStatus};
use serde_json::{json, Value};

pub fn run() -> ExitCode {
    let args: Vec<String> = crate::sub_args().collect();
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        print_usage();
        return ExitCode::from(2);
    };
    if matches!(sub, "-h" | "--help" | "help") {
        print_usage();
        return ExitCode::SUCCESS;
    }
    let (named, switches) = parse_flags(&args[1..]);

    let result = match sub {
        "list" => cmd_list(&named, &switches),
        "show" => cmd_show(&named),
        "add" => cmd_add(&named),
        "reply" => cmd_reply(&named),
        "resolve" => cmd_resolve(&named, &switches),
        "reanchor" => cmd_reanchor(&named),
        other => Err(format!("unknown subcommand '{other}' (see jimctl review --help)")),
    };
    match result {
        Ok(mut v) => {
            if v.get("ok").is_none() {
                v["ok"] = json!(true);
            }
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("{}", json!({"ok": false, "error": e}));
            ExitCode::from(1)
        }
    }
}

fn parse_flags(args: &[String]) -> (Vec<(String, String)>, Vec<String>) {
    let mut named = Vec::new();
    let mut switches = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(rest) = a.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                named.push((k.to_string(), v.to_string()));
            } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                named.push((rest.to_string(), args[i + 1].clone()));
                i += 1;
            } else {
                switches.push(rest.to_string());
            }
        }
        i += 1;
    }
    (named, switches)
}

fn get<'a>(named: &'a [(String, String)], key: &str) -> Option<&'a str> {
    named.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn resolve_repo(named: &[(String, String)]) -> Result<PathBuf, String> {
    let start = match get(named, "repo") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().map_err(|e| format!("cwd: {e}"))?,
    };
    jim_git::repo_root(&start)
        .ok_or_else(|| format!("not inside a git repository: {}", start.display()))
}

/// Author from `--author`, else agent env identity, else `user`.
fn resolve_author(named: &[(String, String)]) -> Result<Author, String> {
    if let Some(a) = get(named, "author") {
        return Author::parse(a);
    }
    if let Ok(id) = std::env::var("JIM_AGENT_ID").or_else(|_| std::env::var("JIM_CHANNEL_ID")) {
        if !id.is_empty() {
            return Ok(Author::Agent { name: id });
        }
    }
    Ok(Author::User)
}

fn thread_json(t: &Thread) -> Value {
    json!({
        "id": t.id,
        "repo": t.repo,
        "base_ref": t.base_ref,
        "file": t.anchor.file,
        "line": t.anchor.line,
        "context_line": t.anchor.context_line,
        "status": match t.status { ThreadStatus::Open => "open", ThreadStatus::Resolved => "resolved" },
        "stale": t.stale,
        "author": t.author.display(),
        "body": t.body,
        "replies": t.replies.iter().map(|r| json!({
            "id": r.id,
            "author": r.author.display(),
            "body": r.body,
            "ts_ms": r.ts_ms,
        })).collect::<Vec<_>>(),
        "created_ms": t.created_ms,
        "updated_ms": t.updated_ms,
    })
}

/// Publish `review.changed` after a mutation: once on the global
/// channel, optionally mirrored onto `--project`'s channel.
fn publish_changed(named: &[(String, String)], repo: &Path, id: u64, action: &str) {
    let payload = json!({
        "repo": repo.display().to_string(),
        "id": id,
        "action": action,
    })
    .to_string();
    let publish = |project: Option<u64>| {
        let msg = jim_bus::proto::BusMessage {
            project,
            topic: "review.changed".to_string(),
            payload_json: payload.clone(),
            sender: "jimctl review".to_string(),
            retain: false,
        };
        if let Err(e) = jim_bus::client::publish_oneshot(&msg) {
            eprintln!("jimctl review: publish review.changed: {e}");
        }
    };
    publish(None);
    if let Some(name) = get(named, "project") {
        match crate::project_resolve::resolve_project_id(name) {
            Ok(pid) => publish(Some(pid)),
            Err(e) => eprintln!("jimctl review: --project {name}: {e}"),
        }
    }
}

fn status_matches(filter: &str, t: &Thread) -> bool {
    match filter {
        "all" => true,
        "resolved" => t.status == ThreadStatus::Resolved,
        _ => t.status == ThreadStatus::Open,
    }
}

// ---------------- subcommands ----------------

fn cmd_list(named: &[(String, String)], switches: &[String]) -> Result<Value, String> {
    let status = get(named, "status").unwrap_or("open");
    let file_filter = get(named, "file");
    let in_repo = get(named, "repo").is_some()
        || std::env::current_dir()
            .ok()
            .and_then(|c| jim_git::repo_root(&c))
            .is_some();
    let aggregate = switches.iter().any(|s| s == "all-repos") || !in_repo;

    let matches = |t: &&Thread| {
        status_matches(status, t) && file_filter.map(|f| t.anchor.file == f).unwrap_or(true)
    };

    if aggregate {
        // The review-inbox view: every repo's store in one list.
        let mut all = Vec::new();
        if let Some(dir) = jim_review::reviews_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    // Legacy diff.ft flat files are arrays — skip quietly.
                    let Ok(data) = serde_json::from_str::<ReviewFile>(&text) else { continue };
                    all.extend(data.threads.iter().filter(matches).map(thread_json));
                }
            }
        }
        return Ok(json!({ "threads": all }));
    }

    let repo = resolve_repo(named)?;
    let data = jim_review::load(&repo);
    let threads: Vec<Value> = data.threads.iter().filter(matches).map(thread_json).collect();
    Ok(json!({ "threads": threads }))
}

fn cmd_show(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let id: u64 = get(named, "id")
        .ok_or("--id is required")?
        .parse()
        .map_err(|_| "bad --id")?;
    let data = jim_review::load(&repo);
    let t = data
        .threads
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("no thread {id} for this repo"))?;
    Ok(json!({ "thread": thread_json(t) }))
}

fn cmd_add(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let file = get(named, "file").ok_or("--file is required (repo-relative)")?;
    let line: u32 = get(named, "line")
        .ok_or("--line is required")?
        .parse()
        .map_err(|_| "bad --line")?;
    let body = get(named, "body").ok_or("--body is required")?;
    let base_ref = get(named, "base-ref").unwrap_or("working");
    let author = resolve_author(named)?;

    // Capture anchor context from the file as it exists right now; a
    // missing/binary file just yields a context-less anchor.
    let full = repo.join(file);
    let text = std::fs::read_to_string(&full).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let anchor = jim_review::make_anchor(file, line, &lines);

    let mut data = jim_review::load(&repo);
    let id = jim_review::add_thread(&mut data, &repo, base_ref, anchor, author, body);
    jim_review::save_atomic(&repo, &data).map_err(|e| format!("save: {e}"))?;
    publish_changed(named, &repo, id, "add");
    Ok(json!({ "id": id }))
}

fn cmd_reply(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let id: u64 = get(named, "id")
        .ok_or("--id is required")?
        .parse()
        .map_err(|_| "bad --id")?;
    let body = get(named, "body").ok_or("--body is required")?;
    let author = resolve_author(named)?;

    let mut data = jim_review::load(&repo);
    if !jim_review::add_reply(&mut data, id, author, body) {
        return Err(format!("no thread {id} for this repo"));
    }
    jim_review::save_atomic(&repo, &data).map_err(|e| format!("save: {e}"))?;
    publish_changed(named, &repo, id, "reply");
    Ok(json!({ "id": id }))
}

fn cmd_resolve(named: &[(String, String)], switches: &[String]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let id: u64 = get(named, "id")
        .ok_or("--id is required")?
        .parse()
        .map_err(|_| "bad --id")?;
    let reopen = switches.iter().any(|s| s == "reopen");
    let status = if reopen { ThreadStatus::Open } else { ThreadStatus::Resolved };

    let mut data = jim_review::load(&repo);
    if !jim_review::set_status(&mut data, id, status) {
        return Err(format!("no thread {id} for this repo"));
    }
    jim_review::save_atomic(&repo, &data).map_err(|e| format!("save: {e}"))?;
    publish_changed(named, &repo, id, if reopen { "reopen" } else { "resolve" });
    Ok(json!({ "id": id, "status": if reopen { "open" } else { "resolved" } }))
}

fn cmd_reanchor(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let mut data = jim_review::load(&repo);
    let files: std::collections::HashSet<String> =
        data.threads.iter().map(|t| t.anchor.file.clone()).collect();
    for file in files {
        let text = std::fs::read_to_string(repo.join(&file)).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        jim_review::reanchor(&mut data, &file, &lines, 50);
    }
    let stale = data.threads.iter().filter(|t| t.stale).count();
    jim_review::save_atomic(&repo, &data).map_err(|e| format!("save: {e}"))?;
    publish_changed(named, &repo, 0, "reanchor");
    Ok(json!({ "threads": data.threads.len(), "stale": stale }))
}

fn print_usage() {
    eprintln!(
        "jimctl review <subcommand> [flags]   (one JSON line on stdout)\n\
         \n\
         \tlist    [--repo P] [--status open|resolved|all] [--file F] [--all-repos]\n\
         \t        (--all-repos, or running outside any repo, aggregates every store)\n\
         \tshow    --id N [--repo P]\n\
         \tadd     --file F --line N --body TEXT [--base-ref REF]\n\
         \t        [--author user|agent:NAME] [--project NAME] [--repo P]\n\
         \treply   --id N --body TEXT [--author ...] [--project NAME] [--repo P]\n\
         \tresolve --id N [--reopen] [--project NAME] [--repo P]\n\
         \treanchor [--repo P]   re-locate anchors after file edits\n\
         \n\
         Author defaults to agent:<$JIM_AGENT_ID|$JIM_CHANNEL_ID> when set, else user.\n\
         Mutations publish `review.changed` {{repo,id,action}} on the bus."
    );
}
