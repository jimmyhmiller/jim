//! jim-git — per-repo state snapshots.
//!
//! [`compute_repo_state`] shells out to `git` and produces the canonical
//! [`RepoState`] payload the git widget suite renders, reached via
//! `jimctl git status`.
//!
//! ON DEMAND ONLY. There is deliberately no watcher and no background
//! thread here: jim used to keep every project's repo state live off an
//! fs-watch, which meant an idle editor with no git UI on screen still
//! ran `git status` over a monorepo. Now git runs only when an open
//! widget asks. Widgets re-read after their own mutations by announcing
//! `git.changed.<repo_id>` to each other on the widget bus; nothing in
//! the host produces that topic.
//!
//! Every worktree is its own repo for our purposes: it has its own
//! HEAD/index and its own `RepoState`. `common_root` + `worktrees` let a
//! consumer correlate siblings (and fetch each one's state itself).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// FNV-1a 64. MUST match funct's `hash_str` host fn
/// (crates/jim-widget/src/funct_widget.rs) and `jim_review::repo_key` —
/// widgets and the review store key repos with the same hash.
pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Canonicalized repo root (falls back to the given path unchanged).
pub fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// Stable 16-hex repo id derived from the canonicalized root path.
pub fn repo_id(root: &Path) -> String {
    format!("{:016x}", fnv1a(&canonical_root(root).display().to_string()))
}

/// Widget→widget announcement topic: "I mutated this repo, re-read it if
/// you care". Nothing in the host publishes or watches it; it exists so
/// open git widgets can refresh each other without anyone polling git.
pub fn changed_topic(root: &Path) -> String {
    format!("git.changed.{}", repo_id(root))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub ts_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub head: String,
    pub is_main: bool,
    pub locked: bool,
    /// Agent session that created/owns this worktree, when known (from
    /// the `<worktree>/.jim-agent` sidecar; roster inference can fill it
    /// in downstream).
    pub agent_id: Option<String>,
}

/// Snapshot of everything the widget suite wants to know about a repo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    /// Canonicalized worktree root this state describes.
    pub root: String,
    /// Stable id (see [`repo_id`]); embedded so widgets never hash paths.
    pub repo_id: String,
    /// Root of the main worktree (same as `root` unless this is a linked
    /// worktree). Ties worktree siblings together.
    pub common_root: String,
    /// Current branch, `None` when detached.
    pub branch: Option<String>,
    pub head_sha: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub merge_in_progress: bool,
    pub rebase_in_progress: bool,
    pub last_commit: Option<CommitInfo>,
    pub worktrees: Vec<WorktreeInfo>,
    pub updated_ms: u64,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run git in `root`; `Ok(stdout)` on success, `Err(stderr)` otherwise.
///
/// Read-only invocations ONLY. `--no-optional-locks` keeps `status` from
/// taking `index.lock` to write back its refreshed index: that write is a
/// `.git` change, so the watcher would see it, recompute, and re-trigger
/// itself forever (and contend with real `git commit` runs for the lock).
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Like [`git`] but a failure just yields `None` (for optional facts
/// like "is there an upstream").
fn git_opt(root: &Path, args: &[&str]) -> Option<String> {
    git(root, args).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Resolve the worktree root containing `start` (`git rev-parse
/// --show-toplevel`). `None` when not inside a repo.
pub fn repo_root(start: &Path) -> Option<PathBuf> {
    git_opt(start, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// The repo's private git dir (handles the linked-worktree `.git` file
/// indirection).
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    git_opt(root, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// The shared git dir (equals [`git_dir`] for the main worktree).
pub fn git_common_dir(root: &Path) -> Option<PathBuf> {
    git_opt(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map(PathBuf::from)
}

fn parse_worktrees(root: &Path) -> Vec<WorktreeInfo> {
    let Ok(raw) = git(root, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur: Option<WorktreeInfo> = None;
    let mut first = true;
    for line in raw.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(wt) = cur.take() {
                out.push(wt);
            }
            cur = Some(WorktreeInfo {
                path: path.to_string(),
                branch: None,
                head: String::new(),
                is_main: first,
                locked: false,
                agent_id: None,
            });
            first = false;
        } else if let Some(wt) = cur.as_mut() {
            if let Some(sha) = line.strip_prefix("HEAD ") {
                wt.head = sha.to_string();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                wt.branch = Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string());
            } else if line == "locked" || line.starts_with("locked ") {
                wt.locked = true;
            }
        }
    }
    if let Some(wt) = cur.take() {
        out.push(wt);
    }
    // Enrich with the `.jim-agent` sidecar written by
    // `jimctl git worktree-add --agent`.
    for wt in &mut out {
        let sidecar = Path::new(&wt.path).join(".jim-agent");
        if let Ok(text) = std::fs::read_to_string(&sidecar) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                wt.agent_id = v
                    .get("agent_id")
                    .and_then(|a| a.as_str())
                    .map(|s| s.to_string());
            }
        }
    }
    out
}

/// Compute the full [`RepoState`] for the worktree at `root`.
pub fn compute_repo_state(root: &Path) -> Result<RepoState, String> {
    let inside = git(root, &["rev-parse", "--is-inside-work-tree"])
        .map_err(|e| format!("not a git work tree: {e}"))?;
    if inside.trim() != "true" {
        return Err(format!("not a git work tree: {}", root.display()));
    }
    let root = canonical_root(root);

    let branch = git_opt(&root, &["symbolic-ref", "--short", "-q", "HEAD"]);
    let head_sha = git_opt(&root, &["rev-parse", "HEAD"]).unwrap_or_default();
    let upstream = git_opt(
        &root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    );

    let (ahead, behind) = if upstream.is_some() {
        git_opt(&root, &["rev-list", "--left-right", "--count", "@{u}...HEAD"])
            .and_then(|s| {
                let mut it = s.split_whitespace();
                let behind = it.next()?.parse::<u32>().ok()?;
                let ahead = it.next()?.parse::<u32>().ok()?;
                Some((ahead, behind))
            })
            .unwrap_or((0, 0))
    } else {
        (0, 0)
    };

    let (mut staged, mut unstaged, mut untracked, mut conflicted) = (0u32, 0u32, 0u32, 0u32);
    if let Ok(raw) = git(&root, &["status", "--porcelain", "-z", "--untracked-files=all"]) {
        let mut fields = raw.split('\0').filter(|s| !s.is_empty());
        while let Some(entry) = fields.next() {
            if entry.len() < 3 {
                continue;
            }
            let x = entry.as_bytes()[0] as char;
            let y = entry.as_bytes()[1] as char;
            // Renames carry the original path as the next field.
            if x == 'R' || x == 'C' {
                let _ = fields.next();
            }
            let is_conflict = x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D');
            if x == '?' && y == '?' {
                untracked += 1;
            } else if is_conflict {
                conflicted += 1;
            } else {
                if x != ' ' {
                    staged += 1;
                }
                if y != ' ' {
                    unstaged += 1;
                }
            }
        }
    }

    let gd = git_dir(&root);
    let merge_in_progress = gd
        .as_ref()
        .map(|d| d.join("MERGE_HEAD").exists())
        .unwrap_or(false);
    let rebase_in_progress = gd
        .as_ref()
        .map(|d| d.join("rebase-merge").exists() || d.join("rebase-apply").exists())
        .unwrap_or(false);

    let last_commit = git_opt(&root, &["log", "-1", "--format=%H%x1f%s%x1f%an%x1f%at"])
        .and_then(|s| {
            let mut it = s.split('\u{1f}');
            Some(CommitInfo {
                sha: it.next()?.to_string(),
                subject: it.next()?.to_string(),
                author: it.next()?.to_string(),
                ts_ms: it.next()?.trim().parse::<u64>().ok()? * 1000,
            })
        });

    let worktrees = parse_worktrees(&root);
    let common_root = worktrees
        .iter()
        .find(|w| w.is_main)
        .map(|w| w.path.clone())
        .unwrap_or_else(|| root.display().to_string());

    Ok(RepoState {
        repo_id: repo_id(&root),
        root: root.display().to_string(),
        common_root,
        branch,
        head_sha,
        upstream,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
        conflicted,
        merge_in_progress,
        rebase_in_progress,
        last_commit,
        worktrees,
        updated_ms: now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jim-git-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn state_of_scratch_repo() {
        let dir = scratch_repo("state");
        let st = compute_repo_state(&dir).unwrap();
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.staged + st.unstaged + st.untracked, 0);
        assert!(st.last_commit.as_ref().unwrap().subject == "init");
        assert_eq!(st.worktrees.len(), 1);
        assert!(st.worktrees[0].is_main);
        assert_eq!(st.repo_id.len(), 16);

        // Dirty it: one staged, one untracked.
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["add", "a.txt"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let st = compute_repo_state(&dir).unwrap();
        assert_eq!(st.staged, 1);
        assert_eq!(st.untracked, 1);
        assert_eq!(st.unstaged, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_stable_and_hex() {
        let a = repo_id(Path::new("/tmp/nonexistent-repo-path"));
        let b = repo_id(Path::new("/tmp/nonexistent-repo-path"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(changed_topic(Path::new("/tmp/x")).starts_with("git.changed."));
    }

    /// Reading state must never write the repo. Without
    /// `--no-optional-locks`, `status` takes `index.lock` to write back
    /// its refreshed index — which contends with the user's own commits
    /// (it once failed a commit 60 times running).
    #[test]
    fn read_only_state_never_writes_index() {
        let dir = scratch_repo("noindexwrite");
        // Stale the cached stat data: that is what tempts status into
        // rewriting the index.
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "u\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let index = dir.join(".git").join("index");
        let before = std::fs::metadata(&index).unwrap().modified().unwrap();
        for _ in 0..5 {
            compute_repo_state(&dir).unwrap();
        }
        let after = std::fs::metadata(&index).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "compute_repo_state wrote .git/index — a read-only path took index.lock"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fnv_matches_funct_hash_str() {
        // Same constants/algorithm as the funct `hash_str` host fn; if
        // this drifts, widgets and Rust will disagree on repo ids.
        assert_eq!(fnv1a(""), 14695981039346656037);
        assert_eq!(fnv1a("a"), {
            let mut h: u64 = 14695981039346656037;
            h ^= b'a' as u64;
            h.wrapping_mul(1099511628211)
        });
    }
}
