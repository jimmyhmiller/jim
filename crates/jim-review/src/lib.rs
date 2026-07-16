//! jim-review — local, GitHub-independent code-review threads.
//!
//! One JSON file per repo at `~/.jim/reviews/<repo_hash>.json` (hex
//! names; coexists with the legacy human-named flat files diff.ft used
//! to write). Threads anchor to a file+line and carry a small context
//! snippet so [`reanchor`] can relocate them after edits/rebases;
//! threads it cannot relocate are marked `stale`, never dropped.
//!
//! Writes are atomic (tmp + rename), same pattern as the issues store.
//! Change notification is the caller's job: `jimctl review` publishes
//! `review.changed` on the bus after every mutation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// FNV-1a 64. MUST match `jim_git::fnv1a` and funct's `hash_str` —
/// all three key repos by the same hash of the canonicalized root.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Stable 16-hex key for a repo root (canonicalized path hash).
pub fn repo_key(root: &Path) -> String {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    format!("{:016x}", fnv1a(&canon.display().to_string()))
}

/// `~/.jim/reviews`, created on demand.
pub fn reviews_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".jim").join("reviews");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

pub fn repo_file_path(root: &Path) -> Option<PathBuf> {
    Some(reviews_dir()?.join(format!("{}.json", repo_key(root))))
}

/// How many lines of context to snapshot on each side of the anchor.
pub const ANCHOR_CONTEXT: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Repo-relative path.
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    #[serde(default)]
    pub context_before: Vec<String>,
    #[serde(default)]
    pub context_line: String,
    #[serde(default)]
    pub context_after: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Author {
    User,
    Agent { name: String },
}

impl Author {
    /// Parse the CLI form: `user` or `agent:<name>`.
    pub fn parse(s: &str) -> Result<Author, String> {
        if s == "user" {
            Ok(Author::User)
        } else if let Some(name) = s.strip_prefix("agent:") {
            if name.is_empty() {
                Err("agent author needs a name: agent:<name>".into())
            } else {
                Ok(Author::Agent { name: name.to_string() })
            }
        } else {
            Err(format!("bad author '{s}': expected 'user' or 'agent:<name>'"))
        }
    }

    pub fn display(&self) -> String {
        match self {
            Author::User => "user".into(),
            Author::Agent { name } => format!("agent:{name}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadStatus {
    Open,
    Resolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    pub author: Author,
    pub body: String,
    pub ts_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: u64,
    /// Canonicalized repo root the thread belongs to.
    pub repo: String,
    /// What the comment was made against: "working", a branch, a sha,
    /// or a "base head" range string — mirrors diff.ft's source/arg.
    pub base_ref: String,
    pub anchor: Anchor,
    pub status: ThreadStatus,
    /// True when [`reanchor`] could not relocate the anchor after the
    /// underlying file changed.
    #[serde(default)]
    pub stale: bool,
    pub author: Author,
    pub body: String,
    #[serde(default)]
    pub replies: Vec<Reply>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReviewFile {
    pub next_id: u64,
    #[serde(default)]
    pub threads: Vec<Thread>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn load(root: &Path) -> ReviewFile {
    let Some(path) = repo_file_path(root) else {
        return ReviewFile { next_id: 1, threads: Vec::new() };
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or(ReviewFile { next_id: 1, threads: Vec::new() }),
        Err(_) => ReviewFile { next_id: 1, threads: Vec::new() },
    }
}

/// Atomic write: serialize to `<file>.tmp`, then rename over the target.
pub fn save_atomic(root: &Path, data: &ReviewFile) -> std::io::Result<()> {
    let path = repo_file_path(root).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no HOME for ~/.jim/reviews")
    })?;
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// Build an anchor at `line` (1-based) capturing surrounding context
/// from `lines` (the file's lines at comment time).
pub fn make_anchor(file: &str, line: u32, lines: &[&str]) -> Anchor {
    let idx = (line as usize).saturating_sub(1);
    let context_line = lines.get(idx).map(|s| s.to_string()).unwrap_or_default();
    let before_start = idx.saturating_sub(ANCHOR_CONTEXT);
    let context_before = lines[before_start..idx.min(lines.len())]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let after_end = (idx + 1 + ANCHOR_CONTEXT).min(lines.len());
    let context_after = if idx + 1 < lines.len() {
        lines[idx + 1..after_end].iter().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    };
    Anchor {
        file: file.to_string(),
        line,
        context_before,
        context_line,
        context_after,
    }
}

pub fn add_thread(
    data: &mut ReviewFile,
    repo: &Path,
    base_ref: &str,
    anchor: Anchor,
    author: Author,
    body: &str,
) -> u64 {
    let id = data.next_id.max(1);
    data.next_id = id + 1;
    let now = now_ms();
    let canon = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    data.threads.push(Thread {
        id,
        repo: canon.display().to_string(),
        base_ref: base_ref.to_string(),
        anchor,
        status: ThreadStatus::Open,
        stale: false,
        author,
        body: body.to_string(),
        replies: Vec::new(),
        created_ms: now,
        updated_ms: now,
    });
    id
}

pub fn add_reply(data: &mut ReviewFile, thread_id: u64, author: Author, body: &str) -> bool {
    let now = now_ms();
    let Some(t) = data.threads.iter_mut().find(|t| t.id == thread_id) else {
        return false;
    };
    let reply_id = t.replies.iter().map(|r| r.id).max().unwrap_or(0) + 1;
    t.replies.push(Reply {
        id: reply_id,
        author,
        body: body.to_string(),
        ts_ms: now,
    });
    t.updated_ms = now;
    true
}

pub fn set_status(data: &mut ReviewFile, thread_id: u64, status: ThreadStatus) -> bool {
    let Some(t) = data.threads.iter_mut().find(|t| t.id == thread_id) else {
        return false;
    };
    t.status = status;
    t.updated_ms = now_ms();
    true
}

/// Best-effort relocation of every thread anchored in `file` against the
/// file's current `new_lines`. A thread whose context line is found at
/// (or near) its recorded position keeps/updates `line`; one that can't
/// be located within ±`search_radius` lines is marked `stale`.
pub fn reanchor(data: &mut ReviewFile, file: &str, new_lines: &[&str], search_radius: usize) {
    for t in data.threads.iter_mut().filter(|t| t.anchor.file == file) {
        if t.anchor.context_line.is_empty() {
            continue; // legacy/contextless anchor: nothing to match on
        }
        let old_idx = (t.anchor.line as usize).saturating_sub(1);
        let matches_at = |idx: usize| -> bool {
            new_lines.get(idx).map(|l| *l == t.anchor.context_line).unwrap_or(false)
        };
        let score_at = |idx: usize| -> usize {
            // context_line match is required; neighbors break ties.
            let mut s = 0;
            for (off, want) in t.anchor.context_before.iter().rev().enumerate() {
                if idx > off {
                    if new_lines.get(idx - 1 - off).map(|l| l == want).unwrap_or(false) {
                        s += 1;
                    }
                }
            }
            for (off, want) in t.anchor.context_after.iter().enumerate() {
                if new_lines.get(idx + 1 + off).map(|l| l == want).unwrap_or(false) {
                    s += 1;
                }
            }
            s
        };

        if matches_at(old_idx) {
            t.stale = false;
            continue;
        }
        let mut best: Option<(usize, usize)> = None; // (idx, score)
        for d in 1..=search_radius {
            for idx in [old_idx.checked_sub(d), Some(old_idx + d)].into_iter().flatten() {
                if matches_at(idx) {
                    let s = score_at(idx);
                    if best.map(|(_, bs)| s > bs).unwrap_or(true) {
                        best = Some((idx, s));
                    }
                }
            }
            // Nearest strong match wins; stop once we have any match and
            // have looked one ring further for a better-scored one.
            if let Some((bi, _)) = best {
                if bi.abs_diff(old_idx) < d {
                    break;
                }
            }
        }
        match best {
            Some((idx, _)) => {
                t.anchor.line = (idx + 1) as u32;
                t.stale = false;
            }
            None => t.stale = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor_for(lines: &[&str], line: u32) -> Anchor {
        make_anchor("src/x.rs", line, lines)
    }

    #[test]
    fn round_trip_thread_lifecycle() {
        let mut data = ReviewFile { next_id: 1, threads: Vec::new() };
        let lines = ["fn a() {}", "fn b() {}", "fn c() {}"];
        let id = add_thread(
            &mut data,
            Path::new("/tmp/repo"),
            "working",
            anchor_for(&lines, 2),
            Author::User,
            "why is b empty?",
        );
        assert_eq!(id, 1);
        assert!(add_reply(&mut data, id, Author::Agent { name: "claude".into() }, "fixing"));
        assert!(set_status(&mut data, id, ThreadStatus::Resolved));
        let t = &data.threads[0];
        assert_eq!(t.replies.len(), 1);
        assert_eq!(t.status, ThreadStatus::Resolved);
        assert_eq!(t.anchor.context_line, "fn b() {}");

        // Serde round trip preserves everything.
        let json = serde_json::to_string(&data).unwrap();
        let back: ReviewFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.threads, data.threads);
    }

    #[test]
    fn author_parse() {
        assert_eq!(Author::parse("user").unwrap(), Author::User);
        assert_eq!(
            Author::parse("agent:claude").unwrap(),
            Author::Agent { name: "claude".into() }
        );
        assert!(Author::parse("agent:").is_err());
        assert!(Author::parse("bob").is_err());
    }

    #[test]
    fn reanchor_follows_inserted_lines() {
        let mut data = ReviewFile { next_id: 1, threads: Vec::new() };
        let old = ["a", "b", "target", "c", "d"];
        add_thread(
            &mut data,
            Path::new("/tmp/repo"),
            "working",
            anchor_for(&old, 3),
            Author::User,
            "note",
        );
        // Insert 4 lines above: target moves 3 -> 7.
        let new = ["x", "x", "x", "x", "a", "b", "target", "c", "d"];
        reanchor(&mut data, "src/x.rs", &new, 50);
        assert_eq!(data.threads[0].anchor.line, 7);
        assert!(!data.threads[0].stale);

        // Delete the line entirely -> stale, not dropped.
        let gone = ["a", "b", "c", "d"];
        reanchor(&mut data, "src/x.rs", &gone, 50);
        assert!(data.threads[0].stale);
        assert_eq!(data.threads.len(), 1);
    }

    #[test]
    fn save_and_load_atomic() {
        // Use a fake HOME so the test doesn't touch ~/.jim.
        let fake_home = std::env::temp_dir().join(format!("jim-review-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fake_home);
        std::fs::create_dir_all(&fake_home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &fake_home);

        let repo = fake_home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut data = ReviewFile { next_id: 1, threads: Vec::new() };
        add_thread(
            &mut data,
            &repo,
            "working",
            anchor_for(&["only line"], 1),
            Author::User,
            "hello",
        );
        save_atomic(&repo, &data).unwrap();
        let back = load(&repo);
        assert_eq!(back.threads.len(), 1);
        assert_eq!(back.threads[0].body, "hello");
        assert_eq!(back.next_id, 2);

        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        }
        let _ = std::fs::remove_dir_all(&fake_home);
    }
}
