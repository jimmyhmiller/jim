//! `jimctl git` — the canonical git read/write surface for widgets and
//! agents.
//!
//! Every subcommand prints exactly one JSON line to stdout —
//! `{"ok":true,...}` or `{"ok":false,"error":"..."}` — even on failure,
//! because a funct widget's `proc_spawn` discards stderr; errors must
//! ride the reply channel (same rationale as `cmd_lsp`). Exit code is 0
//! on ok, 1 on error.
//!
//! Queries:
//!   jimctl git status    [--repo P]
//!   jimctl git branches  [--repo P]
//!   jimctl git worktrees [--repo P]
//!   jimctl git log       [--repo P] [--limit N] [--path F]
//!   jimctl git diff      [--repo P] [--mode working|staged|unstaged|range]
//!                        [--base B] [--head H] [--no-text]
//! Safe mutations:
//!   jimctl git checkout  --ref R [--create] [--repo P]
//!   jimctl git branch-new --name N [--from REF] [--repo P]
//!   jimctl git worktree-add --path WP --branch B [--new-branch] [--from REF]
//!                        [--agent ID] [--repo P]
//!   jimctl git worktree-rm  --path WP [--force] [--repo P]
//!   jimctl git fetch     [--remote R] [--repo P]
//! Selective staging (the `git add -p` heart of the suite):
//!   jimctl git stage-hunk    --file F --hunk-header "@@ -a,b +c,d @@" [--repo P]
//!   jimctl git unstage-hunk  --file F --hunk-header "@@ -a,b +c,d @@" [--repo P]
//!   jimctl git stage-file    --file F [--repo P]
//!   jimctl git unstage-file  --file F [--repo P]
//!
//! Hunk staging is race-safe by construction: the fresh unstaged (or
//! staged) diff is recomputed HERE, the hunk located by its header, a
//! minimal patch built via `diff_core::hunk_patch`, then
//! `git apply --cached [-R] --check` before the real apply. A header
//! that no longer matches, or a failed check, returns
//! `{"ok":false,"error":"stale"}` — the caller refreshes and retries.
//! Renamed/binary/untracked files fall back to whole-file add/reset.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use diff_core::{ChangeKind, DiffSet, Hunk, LineKind};
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
    let rest = &args[1..];
    let (named, switches) = parse_flags(rest);

    let result = match sub {
        "status" => cmd_status(&named),
        "branches" => cmd_branches(&named),
        "worktrees" => cmd_worktrees(&named),
        "log" => cmd_log(&named),
        "diff" => cmd_diff(&named, &switches),
        "checkout" => cmd_checkout(&named, &switches),
        "branch-new" => cmd_branch_new(&named),
        "worktree-add" => cmd_worktree_add(&named, &switches),
        "worktree-rm" => cmd_worktree_rm(&named, &switches),
        "fetch" => cmd_fetch(&named),
        "stage-hunk" => cmd_hunk(&named, true),
        "unstage-hunk" => cmd_hunk(&named, false),
        "stage-file" => cmd_whole_file(&named, true),
        "unstage-file" => cmd_whole_file(&named, false),
        other => Err(format!("unknown subcommand '{other}' (see jimctl git --help)")),
    };

    match result {
        Ok(v) => {
            let mut obj = v;
            if obj.get("ok").is_none() {
                obj["ok"] = json!(true);
            }
            println!("{}", obj);
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("{}", json!({"ok": false, "error": e}));
            ExitCode::from(1)
        }
    }
}

// ---------------- helpers ----------------

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

fn has(switches: &[String], key: &str) -> bool {
    switches.iter().any(|s| s == key)
}

/// Resolve `--repo` (defaulting to the repo containing the cwd).
fn resolve_repo(named: &[(String, String)]) -> Result<PathBuf, String> {
    let start = match get(named, "repo") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().map_err(|e| format!("cwd: {e}"))?,
    };
    jim_git::repo_root(&start)
        .ok_or_else(|| format!("not inside a git repository: {}", start.display()))
}

/// Run git in `repo`; Ok(stdout) or Err(stderr).
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Run git with `input` piped to stdin; Ok(stdout) or Err(stderr).
fn git_stdin(repo: &Path, args: &[&str], input: &str) -> Result<String, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("no stdin")?
        .write_all(input.as_bytes())
        .map_err(|e| format!("write to git: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait for git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

// ---------------- queries ----------------

fn cmd_status(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let state = jim_git::compute_repo_state(&repo)?;
    Ok(json!({ "state": serde_json::to_value(&state).map_err(|e| e.to_string())? }))
}

fn cmd_branches(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let raw = git(
        &repo,
        &[
            "for-each-ref",
            "refs/heads",
            "--format=%(HEAD)\u{1f}%(refname:short)\u{1f}%(upstream:short)\u{1f}%(upstream:track)\u{1f}%(objectname:short)\u{1f}%(committerdate:unix)\u{1f}%(contents:subject)",
        ],
    )?;
    let mut branches = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.split('\u{1f}').collect();
        if parts.len() < 7 {
            continue;
        }
        // "[ahead 2, behind 1]" / "[ahead 2]" / "[gone]" / ""
        let track = parts[3];
        let mut ahead = 0u32;
        let mut behind = 0u32;
        let mut gone = false;
        if track.contains("gone") {
            gone = true;
        } else {
            for piece in track.trim_matches(['[', ']']).split(',') {
                let piece = piece.trim();
                if let Some(n) = piece.strip_prefix("ahead ") {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = piece.strip_prefix("behind ") {
                    behind = n.parse().unwrap_or(0);
                }
            }
        }
        branches.push(json!({
            "name": parts[1],
            "current": parts[0] == "*",
            "upstream": if parts[2].is_empty() { Value::Null } else { json!(parts[2]) },
            "upstream_gone": gone,
            "ahead": ahead,
            "behind": behind,
            "head": parts[4],
            "last_ts_ms": parts[5].parse::<u64>().unwrap_or(0) * 1000,
            "last_subject": parts[6],
        }));
    }
    Ok(json!({ "branches": branches }))
}

fn cmd_worktrees(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let state = jim_git::compute_repo_state(&repo)?;
    Ok(json!({
        "worktrees": serde_json::to_value(&state.worktrees).map_err(|e| e.to_string())?
    }))
}

fn cmd_log(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let limit = get(named, "limit").unwrap_or("20");
    let limit_n: usize = limit.parse().map_err(|_| format!("bad --limit: {limit}"))?;
    let n_arg = format!("-n{limit_n}");
    let mut args = vec!["log", "--format=%H\u{1f}%s\u{1f}%an\u{1f}%at", n_arg.as_str()];
    let path_arg;
    if let Some(p) = get(named, "path") {
        path_arg = p.to_string();
        args.push("--");
        args.push(&path_arg);
    }
    let raw = git(&repo, &args)?;
    let commits: Vec<Value> = raw
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\u{1f}').collect();
            if parts.len() < 4 {
                return None;
            }
            Some(json!({
                "sha": parts[0],
                "subject": parts[1],
                "author": parts[2],
                "ts_ms": parts[3].trim().parse::<u64>().unwrap_or(0) * 1000,
            }))
        })
        .collect();
    Ok(json!({ "commits": commits }))
}

/// The hunk header used for addressing in stage-hunk/unstage-hunk. Must
/// be derived exactly the same way `diff_core::hunk_patch` derives its
/// header line, so a header taken from `jimctl git diff` output always
/// round-trips.
fn apply_header(hunk: &Hunk) -> String {
    let old_n = hunk.lines.iter().filter(|l| l.old_lineno.is_some()).count();
    let new_n = hunk.lines.iter().filter(|l| l.new_lineno.is_some()).count();
    let old_start = hunk
        .lines
        .iter()
        .find_map(|l| l.old_lineno)
        .unwrap_or(hunk.old_start.saturating_sub(1));
    let new_start = hunk
        .lines
        .iter()
        .find_map(|l| l.new_lineno)
        .unwrap_or(hunk.new_start.saturating_sub(1));
    format!("@@ -{old_start},{old_n} +{new_start},{new_n} @@")
}

fn change_kind_str(c: ChangeKind) -> &'static str {
    match c {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Untracked => "untracked",
    }
}

fn diffset_json(set: &DiffSet, include_text: bool) -> Value {
    let files: Vec<Value> = set
        .files
        .iter()
        .map(|f| {
            let hunks: Vec<Value> = f
                .hunks
                .iter()
                .map(|h| {
                    let lines: Vec<Value> = h
                        .lines
                        .iter()
                        .map(|l| {
                            json!({
                                "kind": match l.kind {
                                    LineKind::Context => "context",
                                    LineKind::Added => "added",
                                    LineKind::Removed => "removed",
                                },
                                "old": l.old_lineno,
                                "new": l.new_lineno,
                                "text": l.text,
                            })
                        })
                        .collect();
                    json!({
                        "header": apply_header(h),
                        "old_start": h.old_start,
                        "new_start": h.new_start,
                        "lines": lines,
                    })
                })
                .collect();
            let mut file = json!({
                "path": f.path,
                "old_path": f.old_path,
                "change": change_kind_str(f.change),
                "added": f.added,
                "removed": f.removed,
                "binary": f.binary,
                "hunks": hunks,
            });
            if include_text {
                file["old_text"] = f.old_text.clone().map(Value::String).unwrap_or(Value::Null);
                file["new_text"] = f.new_text.clone().map(Value::String).unwrap_or(Value::Null);
            }
            file
        })
        .collect();
    json!({
        "files": files,
        "total_added": set.total_added,
        "total_removed": set.total_removed,
    })
}

fn cmd_diff(named: &[(String, String)], switches: &[String]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let mode = get(named, "mode").unwrap_or("working");
    let set = match mode {
        "working" => diff_core::git_working_tree(&repo).map_err(|e| e.to_string())?,
        "staged" => diff_core::git_staged(&repo).map_err(|e| e.to_string())?,
        "unstaged" => diff_core::git_unstaged(&repo).map_err(|e| e.to_string())?,
        "range" => {
            let base = get(named, "base").ok_or("--mode range needs --base")?;
            let head = get(named, "head").unwrap_or("HEAD");
            diff_core::git_ref_range(&repo, base, head).map_err(|e| e.to_string())?
        }
        other => return Err(format!("bad --mode '{other}' (working|staged|unstaged|range)")),
    };
    Ok(json!({ "diff": diffset_json(&set, !has(switches, "no-text")) }))
}

// ---------------- safe mutations ----------------

fn cmd_checkout(named: &[(String, String)], switches: &[String]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let r = get(named, "ref").ok_or("--ref is required")?;
    if has(switches, "create") {
        git(&repo, &["checkout", "-b", r])?;
    } else {
        git(&repo, &["checkout", r])?;
    }
    Ok(json!({ "ref": r }))
}

fn cmd_branch_new(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let name = get(named, "name").ok_or("--name is required")?;
    match get(named, "from") {
        Some(from) => git(&repo, &["branch", name, from])?,
        None => git(&repo, &["branch", name])?,
    };
    Ok(json!({ "branch": name }))
}

fn cmd_worktree_add(named: &[(String, String)], switches: &[String]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let path = get(named, "path").ok_or("--path is required")?;
    let branch = get(named, "branch").ok_or("--branch is required")?;
    if has(switches, "new-branch") {
        match get(named, "from") {
            Some(from) => git(&repo, &["worktree", "add", "-b", branch, path, from])?,
            None => git(&repo, &["worktree", "add", "-b", branch, path])?,
        };
    } else {
        git(&repo, &["worktree", "add", path, branch])?;
    }

    // Ownership sidecar for the AI-work feed: --agent, or the session
    // env vars the channel bridges set.
    let agent = get(named, "agent")
        .map(|s| s.to_string())
        .or_else(|| std::env::var("JIM_AGENT_ID").ok())
        .or_else(|| std::env::var("JIM_CHANNEL_ID").ok());
    if let Some(agent_id) = agent.filter(|a| !a.is_empty()) {
        let sidecar = json!({
            "agent_id": agent_id,
            "created_ms": jim_git::now_ms(),
        });
        let sidecar_path = Path::new(path).join(".jim-agent");
        if let Err(e) = std::fs::write(&sidecar_path, sidecar.to_string()) {
            // The worktree exists; a failed sidecar is a warning, not a
            // failed command — but say so in the reply.
            return Ok(json!({
                "path": path,
                "branch": branch,
                "warning": format!("worktree created but sidecar write failed: {e}"),
            }));
        }
    }
    Ok(json!({ "path": path, "branch": branch }))
}

fn cmd_worktree_rm(named: &[(String, String)], switches: &[String]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let path = get(named, "path").ok_or("--path is required")?;
    if has(switches, "force") {
        git(&repo, &["worktree", "remove", "--force", path])?;
    } else {
        git(&repo, &["worktree", "remove", path])?;
    }
    Ok(json!({ "removed": path }))
}

fn cmd_fetch(named: &[(String, String)]) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    match get(named, "remote") {
        Some(remote) => git(&repo, &["fetch", remote])?,
        None => git(&repo, &["fetch"])?,
    };
    Ok(json!({ "fetched": true }))
}

// ---------------- selective staging ----------------

/// Whole-file stage/unstage — also the documented fallback for cases
/// hunk-level can't handle (renames, binary, untracked).
fn cmd_whole_file(named: &[(String, String)], stage: bool) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let file = get(named, "file").ok_or("--file is required")?;
    if stage {
        git(&repo, &["add", "--", file])?;
    } else {
        git(&repo, &["reset", "-q", "--", file])?;
    }
    Ok(json!({ "file": file, "staged": stage }))
}

fn cmd_hunk(named: &[(String, String)], stage: bool) -> Result<Value, String> {
    let repo = resolve_repo(named)?;
    let file = get(named, "file").ok_or("--file is required")?;
    let header = get(named, "hunk-header").ok_or("--hunk-header is required")?;

    // Recompute the RELEVANT side fresh: staging picks from the unstaged
    // diff, unstaging from the staged diff. Header matching against this
    // fresh diff is what makes the operation race-safe.
    let set = if stage {
        diff_core::git_unstaged(&repo).map_err(|e| e.to_string())?
    } else {
        diff_core::git_staged(&repo).map_err(|e| e.to_string())?
    };
    let Some(fd) = set.files.iter().find(|f| f.path == file) else {
        return Err("stale".into());
    };

    // Cases a single-hunk patch can't express safely → whole-file ops.
    if fd.binary || fd.change == ChangeKind::Renamed || fd.change == ChangeKind::Untracked {
        let reason = if fd.binary {
            "binary"
        } else if fd.change == ChangeKind::Renamed {
            "renamed"
        } else {
            "untracked"
        };
        return Err(format!(
            "hunk-level staging not supported for {reason} files; use {} --file {file}",
            if stage { "stage-file" } else { "unstage-file" }
        ));
    }

    let Some(hunk) = fd.hunks.iter().find(|h| apply_header(h) == header) else {
        return Err("stale".into());
    };
    let patch = diff_core::hunk_patch(fd, hunk);

    // Zero-context hunks (edge-of-file) need --unidiff-zero.
    let zero_context = !hunk.lines.iter().any(|l| l.kind == LineKind::Context);
    let mut apply_args: Vec<&str> = vec!["apply", "--cached"];
    if !stage {
        apply_args.push("-R");
    }
    if zero_context {
        apply_args.push("--unidiff-zero");
    }

    // Pre-flight, then the real apply.
    let mut check_args = apply_args.clone();
    check_args.push("--check");
    check_args.push("-");
    if let Err(detail) = git_stdin(&repo, &check_args, &patch) {
        return Ok(json!({ "ok": false, "error": "stale", "detail": detail }));
    }
    apply_args.push("-");
    git_stdin(&repo, &apply_args, &patch)?;
    Ok(json!({
        "file": file,
        "hunk": header,
        "staged": stage,
    }))
}

fn print_usage() {
    eprintln!(
        "jimctl git <subcommand> [flags]   (all output: one JSON line on stdout)\n\
         \n\
         queries:\n\
         \tstatus     [--repo P]                       full RepoState (branch/dirty/worktrees)\n\
         \tbranches   [--repo P]                       local branches + tracking info\n\
         \tworktrees  [--repo P]                       worktree list (with agent ownership)\n\
         \tlog        [--repo P] [--limit N] [--path F] recent commits\n\
         \tdiff       [--repo P] [--mode working|staged|unstaged|range]\n\
         \t           [--base B] [--head H] [--no-text]  structured diff (hunks carry `header`)\n\
         safe mutations:\n\
         \tcheckout   --ref R [--create] [--repo P]\n\
         \tbranch-new --name N [--from REF] [--repo P]\n\
         \tworktree-add --path WP --branch B [--new-branch] [--from REF] [--agent ID] [--repo P]\n\
         \tworktree-rm  --path WP [--force] [--repo P]\n\
         \tfetch      [--remote R] [--repo P]\n\
         selective staging:\n\
         \tstage-hunk / unstage-hunk --file F --hunk-header \"@@ -a,b +c,d @@\" [--repo P]\n\
         \t           (headers come from `jimctl git diff` output; a non-matching\n\
         \t            header returns {{\"ok\":false,\"error\":\"stale\"}} — refresh and retry)\n\
         \tstage-file / unstage-file --file F [--repo P]   whole file (also: renames/binary/untracked)"
    );
}
