//! Pure diff model + computation. No Bevy, no rendering.
//!
//! Two layers:
//!  - [`compute_text_diff`]: diff two in-memory strings into [`Hunk`]s
//!    (Myers via the `similar` crate). This is the generic primitive.
//!  - [`git_working_tree`]: enumerate a git repo's working-tree changes
//!    (HEAD -> working tree, including staged + unstaged + untracked)
//!    and diff each file. Shells out to `git`; no libgit dependency.
//!
//! The model keeps full old/new file text on each [`FileDiff`] so a
//! renderer can run a whole-file syntax highlighter and map results back
//! by line number. For binary files the text is `None` and `hunks` is
//! empty.

use std::path::{Path, PathBuf};
use std::process::Command;

use similar::{ChangeTag, TextDiff};

/// How a file changed relative to the base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    Renamed,
    /// Tracked-but-untracked-content (git `??`). Treated like Added.
    Untracked,
}

impl ChangeKind {
    /// One-char status glyph for compact summaries.
    pub fn glyph(self) -> char {
        match self {
            ChangeKind::Added | ChangeKind::Untracked => 'A',
            ChangeKind::Removed => 'D',
            ChangeKind::Modified => 'M',
            ChangeKind::Renamed => 'R',
        }
    }
}

/// One contiguous changed region of a file, with surrounding context.
#[derive(Clone, Debug)]
pub struct Hunk {
    /// 1-based first line number on the old side (0 if none).
    pub old_start: usize,
    /// 1-based first line number on the new side (0 if none).
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// Classic `@@ -old,oldn +new,newn @@` header string.
    pub fn header(&self) -> String {
        let old_n = self
            .lines
            .iter()
            .filter(|l| l.old_lineno.is_some())
            .count();
        let new_n = self
            .lines
            .iter()
            .filter(|l| l.new_lineno.is_some())
            .count();
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, old_n, self.new_start, new_n
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line number on the old side, if the line exists there.
    pub old_lineno: Option<usize>,
    /// 1-based line number on the new side, if the line exists there.
    pub new_lineno: Option<usize>,
    /// Line text with the trailing newline stripped.
    pub text: String,
}

/// All changes for a single file.
#[derive(Clone, Debug)]
pub struct FileDiff {
    /// Path shown to the user (new path; for renames this is the dest).
    pub path: String,
    /// For renames, the source path; otherwise `None`.
    pub old_path: Option<String>,
    pub change: ChangeKind,
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    /// Full old-side file text (for whole-file syntax highlighting).
    /// `None` for added/untracked/binary files.
    pub old_text: Option<String>,
    /// Full new-side file text. `None` for deleted/binary files.
    pub new_text: Option<String>,
}

impl FileDiff {
    /// True when there is nothing meaningful to show (no hunks, not a
    /// pure add/delete of an empty file).
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty() && !self.binary && self.added == 0 && self.removed == 0
    }
}

/// A complete set of file diffs plus rolled-up totals.
#[derive(Clone, Debug, Default)]
pub struct DiffSet {
    pub files: Vec<FileDiff>,
    pub total_added: usize,
    pub total_removed: usize,
}

impl DiffSet {
    fn from_files(files: Vec<FileDiff>) -> Self {
        let total_added = files.iter().map(|f| f.added).sum();
        let total_removed = files.iter().map(|f| f.removed).sum();
        DiffSet {
            files,
            total_added,
            total_removed,
        }
    }
}

// ---------------- Text diff ----------------

/// True if a string looks binary (contains a NUL byte).
fn looks_binary(s: &str) -> bool {
    s.as_bytes().contains(&0)
}

/// Strip a single trailing '\n' (and a preceding '\r') from a line slice.
fn strip_newline(s: &str) -> &str {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s)
}

/// Diff two strings into hunks with `context` lines of surrounding
/// context. Returns `(hunks, added, removed)`.
pub fn compute_text_diff(old: &str, new: &str, context: usize) -> (Vec<Hunk>, usize, usize) {
    let diff = TextDiff::from_lines(old, new);

    let mut added = 0usize;
    let mut removed = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }

    let mut hunks = Vec::new();
    for group in diff.grouped_ops(context) {
        if group.is_empty() {
            continue;
        }
        let old_start = group.first().map(|op| op.old_range().start).unwrap_or(0) + 1;
        let new_start = group.first().map(|op| op.new_range().start).unwrap_or(0) + 1;
        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => LineKind::Context,
                    ChangeTag::Delete => LineKind::Removed,
                    ChangeTag::Insert => LineKind::Added,
                };
                lines.push(DiffLine {
                    kind,
                    old_lineno: change.old_index().map(|i| i + 1),
                    new_lineno: change.new_index().map(|i| i + 1),
                    text: strip_newline(change.value()).to_string(),
                });
            }
        }
        hunks.push(Hunk {
            old_start,
            new_start,
            lines,
        });
    }

    (hunks, added, removed)
}

/// Build a [`FileDiff`] from two file texts. `context` controls how many
/// unchanged lines surround each hunk.
pub fn file_diff_from_texts(
    path: impl Into<String>,
    old_path: Option<String>,
    change: ChangeKind,
    old: &str,
    new: &str,
    context: usize,
) -> FileDiff {
    let path = path.into();
    if looks_binary(old) || looks_binary(new) {
        return FileDiff {
            path,
            old_path,
            change,
            hunks: Vec::new(),
            added: 0,
            removed: 0,
            binary: true,
            old_text: None,
            new_text: None,
        };
    }
    let (hunks, added, removed) = compute_text_diff(old, new, context);
    FileDiff {
        path,
        old_path,
        change,
        hunks,
        added,
        removed,
        binary: false,
        old_text: if old.is_empty() && change == ChangeKind::Added {
            None
        } else {
            Some(old.to_string())
        },
        new_text: if new.is_empty() && change == ChangeKind::Removed {
            None
        } else {
            Some(new.to_string())
        },
    }
}

// ---------------- Git working tree ----------------

#[derive(Debug)]
pub enum DiffError {
    NotARepo(String),
    Git(String),
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffError::NotARepo(p) => write!(f, "not a git work tree: {p}"),
            DiffError::Git(msg) => write!(f, "git error: {msg}"),
        }
    }
}

impl std::error::Error for DiffError {}

const DEFAULT_CONTEXT: usize = 3;

/// Run `git` in `repo` with `args`, returning stdout bytes. On a nonzero
/// exit, returns `Err` unless `allow_fail` is set (then returns empty).
///
/// Read-only invocations ONLY: `--no-optional-locks` stops `status`/`diff`
/// from taking `index.lock` to write back a refreshed index, which would
/// both contend with real git operations and look like a `.git` change to
/// the git watcher.
fn git_raw(repo: &Path, args: &[&str], allow_fail: bool) -> Result<Vec<u8>, DiffError> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| DiffError::Git(format!("failed to spawn git: {e}")))?;
    if out.status.success() {
        Ok(out.stdout)
    } else if allow_fail {
        Ok(Vec::new())
    } else {
        Err(DiffError::Git(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// Show the contents of `<rev>:<path>` (e.g. `HEAD:src/lib.rs`). Returns
/// an empty string if the object does not exist (e.g. newly added file).
fn git_show(repo: &Path, spec: &str) -> String {
    match git_raw(repo, &["show", spec], true) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

/// Enumerate the working-tree changes of the git repository at `repo`
/// (HEAD vs working tree, including staged, unstaged, and untracked
/// files) and diff every changed file.
pub fn git_working_tree(repo: &Path) -> Result<DiffSet, DiffError> {
    // Confirm we're inside a work tree first so errors are clear.
    let inside = git_raw(repo, &["rev-parse", "--is-inside-work-tree"], true)?;
    if String::from_utf8_lossy(&inside).trim() != "true" {
        return Err(DiffError::NotARepo(repo.display().to_string()));
    }

    // `-z` gives NUL-separated, unquoted entries: each is "XY <path>",
    // and renames/copies are followed by a second NUL-separated path
    // (the original).
    let raw = git_raw(
        repo,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
        false,
    )?;
    let text = String::from_utf8_lossy(&raw);
    let mut fields = text.split('\0').filter(|s| !s.is_empty()).peekable();

    let mut files = Vec::new();
    while let Some(entry) = fields.next() {
        if entry.len() < 3 {
            continue;
        }
        let (status, path) = entry.split_at(2);
        let path = path.trim_start();
        let x = status.as_bytes()[0] as char;
        let y = status.as_bytes()[1] as char;

        // Rename/copy: the original path is the next NUL field.
        let is_rename = x == 'R' || y == 'R' || x == 'C' || y == 'C';
        let orig = if is_rename {
            fields.next().map(|s| s.to_string())
        } else {
            None
        };

        let untracked = x == '?' && y == '?';
        let deleted = x == 'D' || y == 'D';
        let added = x == 'A' || untracked;

        let change = if untracked || added {
            if untracked {
                ChangeKind::Untracked
            } else {
                ChangeKind::Added
            }
        } else if deleted {
            ChangeKind::Removed
        } else if is_rename {
            ChangeKind::Renamed
        } else {
            ChangeKind::Modified
        };

        // Old content: HEAD blob at the original (pre-rename) path.
        let old_spec_path = orig.as_deref().unwrap_or(path);
        let old = if untracked || added {
            String::new()
        } else {
            git_show(repo, &format!("HEAD:{old_spec_path}"))
        };

        // New content: the working-tree file (empty if deleted).
        let new = if deleted {
            String::new()
        } else {
            std::fs::read(repo.join(path))
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };

        files.push(file_diff_from_texts(
            path,
            orig,
            change,
            &old,
            &new,
            DEFAULT_CONTEXT,
        ));
    }

    // Stable, predictable ordering: by path.
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffSet::from_files(files))
}

/// Show the contents of the index blob for `path` (`git show :path`).
fn git_show_index(repo: &Path, path: &str) -> String {
    git_show(repo, &format!(":{path}"))
}

/// Parse `--name-status -z` output (`git diff` family) into
/// `(change, old_path, path)` triples.
fn parse_name_status(text: &str) -> Vec<(ChangeKind, Option<String>, String)> {
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    let mut out = Vec::new();
    while let Some(status) = fields.next() {
        let code = status.as_bytes().first().copied().unwrap_or(b'M') as char;
        let (change, orig, path) = match code {
            'A' => (ChangeKind::Added, None, fields.next()),
            'D' => (ChangeKind::Removed, None, fields.next()),
            'R' | 'C' => {
                let from = fields.next().map(|s| s.to_string());
                let to = fields.next();
                (ChangeKind::Renamed, from, to)
            }
            _ => (ChangeKind::Modified, None, fields.next()),
        };
        if let Some(path) = path {
            out.push((change, orig, path.to_string()));
        }
    }
    out
}

/// Diff HEAD against the index (what `git diff --cached` shows): the
/// currently staged changes. On an unborn branch every index entry is
/// reported as Added.
pub fn git_staged(repo: &Path) -> Result<DiffSet, DiffError> {
    let inside = git_raw(repo, &["rev-parse", "--is-inside-work-tree"], true)?;
    if String::from_utf8_lossy(&inside).trim() != "true" {
        return Err(DiffError::NotARepo(repo.display().to_string()));
    }
    let head_ok = git_raw(repo, &["rev-parse", "--verify", "-q", "HEAD"], true)
        .map(|b| !b.is_empty())
        .unwrap_or(false);

    let mut files = Vec::new();
    if head_ok {
        let raw = git_raw(
            repo,
            &["diff", "--cached", "--name-status", "-z", "--find-renames"],
            false,
        )?;
        for (change, orig, path) in parse_name_status(&String::from_utf8_lossy(&raw)) {
            let old_spec_path = orig.clone().unwrap_or_else(|| path.clone());
            let old = if change == ChangeKind::Added {
                String::new()
            } else {
                git_show(repo, &format!("HEAD:{old_spec_path}"))
            };
            let new = if change == ChangeKind::Removed {
                String::new()
            } else {
                git_show_index(repo, &path)
            };
            files.push(file_diff_from_texts(
                path,
                orig,
                change,
                &old,
                &new,
                DEFAULT_CONTEXT,
            ));
        }
    } else {
        // Unborn HEAD: everything in the index is a staged add.
        let raw = git_raw(repo, &["ls-files", "-z", "--cached"], false)?;
        for path in String::from_utf8_lossy(&raw).split('\0').filter(|s| !s.is_empty()) {
            let new = git_show_index(repo, path);
            files.push(file_diff_from_texts(
                path,
                None,
                ChangeKind::Added,
                "",
                &new,
                DEFAULT_CONTEXT,
            ));
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffSet::from_files(files))
}

/// Diff the index against the working tree (what a bare `git diff`
/// shows) plus untracked files: everything not yet staged.
pub fn git_unstaged(repo: &Path) -> Result<DiffSet, DiffError> {
    let inside = git_raw(repo, &["rev-parse", "--is-inside-work-tree"], true)?;
    if String::from_utf8_lossy(&inside).trim() != "true" {
        return Err(DiffError::NotARepo(repo.display().to_string()));
    }
    let raw = git_raw(repo, &["diff", "--name-status", "-z", "--find-renames"], false)?;
    let mut files = Vec::new();
    for (change, orig, path) in parse_name_status(&String::from_utf8_lossy(&raw)) {
        let old_spec_path = orig.clone().unwrap_or_else(|| path.clone());
        let old = if change == ChangeKind::Added {
            String::new()
        } else {
            git_show_index(repo, &old_spec_path)
        };
        let new = if change == ChangeKind::Removed {
            String::new()
        } else {
            std::fs::read(repo.join(&path))
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        files.push(file_diff_from_texts(
            path,
            orig,
            change,
            &old,
            &new,
            DEFAULT_CONTEXT,
        ));
    }

    // Untracked files are unstaged-by-definition; enumerate them too so a
    // staging UI can offer them (whole-file only — no index blob to hunk
    // against).
    let raw = git_raw(
        repo,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        false,
    )?;
    for path in String::from_utf8_lossy(&raw).split('\0').filter(|s| !s.is_empty()) {
        let new = std::fs::read(repo.join(path))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        files.push(file_diff_from_texts(
            path,
            None,
            ChangeKind::Untracked,
            "",
            &new,
            DEFAULT_CONTEXT,
        ));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffSet::from_files(files))
}

/// Number of lines in a file text ("a\nb\n" and "a\nb" are both 2).
fn line_count(s: &str) -> usize {
    s.lines().count()
}

/// Build a minimal, self-contained unified-diff patch containing exactly
/// one hunk of `file`, suitable for `git apply --cached [-R]`.
///
/// Handles the fiddly parts of the unified format: `/dev/null` sides for
/// adds/removes, zero-count start positions for pure insertions, and
/// `\ No newline at end of file` markers derived from the stored full
/// file texts.
pub fn hunk_patch(file: &FileDiff, hunk: &Hunk) -> String {
    let old_label = match file.change {
        ChangeKind::Added | ChangeKind::Untracked => "/dev/null".to_string(),
        _ => format!("a/{}", file.old_path.as_deref().unwrap_or(&file.path)),
    };
    let new_label = match file.change {
        ChangeKind::Removed => "/dev/null".to_string(),
        _ => format!("b/{}", file.path),
    };

    let old_n = hunk.lines.iter().filter(|l| l.old_lineno.is_some()).count();
    let new_n = hunk.lines.iter().filter(|l| l.new_lineno.is_some()).count();
    // With a count of 0 the unified format's start means "insert after
    // this line" (0 = at the top); with lines present it's the first
    // line's number.
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

    let old_total = file.old_text.as_deref().map(line_count).unwrap_or(0);
    let new_total = file.new_text.as_deref().map(line_count).unwrap_or(0);
    let old_ends_nl = file
        .old_text
        .as_deref()
        .map(|t| t.is_empty() || t.ends_with('\n'))
        .unwrap_or(true);
    let new_ends_nl = file
        .new_text
        .as_deref()
        .map(|t| t.is_empty() || t.ends_with('\n'))
        .unwrap_or(true);

    let mut out = String::new();
    out.push_str(&format!("--- {old_label}\n"));
    out.push_str(&format!("+++ {new_label}\n"));
    out.push_str(&format!(
        "@@ -{old_start},{old_n} +{new_start},{new_n} @@\n"
    ));
    for line in &hunk.lines {
        let sign = match line.kind {
            LineKind::Context => ' ',
            LineKind::Added => '+',
            LineKind::Removed => '-',
        };
        out.push(sign);
        out.push_str(&line.text);
        out.push('\n');
        let at_old_eof = !old_ends_nl && line.old_lineno == Some(old_total);
        let at_new_eof = !new_ends_nl && line.new_lineno == Some(new_total);
        if (line.kind != LineKind::Added && at_old_eof)
            || (line.kind != LineKind::Removed && at_new_eof)
        {
            out.push_str("\\ No newline at end of file\n");
        }
    }
    out
}

/// Diff between two arbitrary git refs (`git diff <base>..<head>`),
/// recomputed per-file so the renderer still gets full file text. Files
/// are enumerated via `git diff --name-status`.
pub fn git_ref_range(repo: &Path, base: &str, head: &str) -> Result<DiffSet, DiffError> {
    let inside = git_raw(repo, &["rev-parse", "--is-inside-work-tree"], true)?;
    if String::from_utf8_lossy(&inside).trim() != "true" {
        return Err(DiffError::NotARepo(repo.display().to_string()));
    }
    let range = format!("{base}..{head}");
    let raw = git_raw(
        repo,
        &["diff", "--name-status", "-z", "--find-renames", &range],
        false,
    )?;
    let mut files = Vec::new();
    for (change, orig, path) in parse_name_status(&String::from_utf8_lossy(&raw)) {
        let old_spec_path = orig.clone().unwrap_or_else(|| path.clone());
        let old = if change == ChangeKind::Added {
            String::new()
        } else {
            git_show(repo, &format!("{base}:{old_spec_path}"))
        };
        let new = if change == ChangeKind::Removed {
            String::new()
        } else {
            git_show(repo, &format!("{head}:{path}"))
        };
        files.push(file_diff_from_texts(
            path,
            orig,
            change,
            &old,
            &new,
            DEFAULT_CONTEXT,
        ));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffSet::from_files(files))
}

/// Resolve the repository root containing `start` (walks up via
/// `git rev-parse --show-toplevel`). Returns `None` if not in a repo.
pub fn repo_root(start: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_diff_counts_and_hunks() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\nd\n";
        let (hunks, added, removed) = compute_text_diff(old, new, 1);
        assert_eq!(added, 2); // B, d
        assert_eq!(removed, 1); // b
        assert!(!hunks.is_empty());
        // The hunk must contain a Removed "b" and an Added "B".
        let flat: Vec<_> = hunks.iter().flat_map(|h| h.lines.iter()).collect();
        assert!(flat
            .iter()
            .any(|l| l.kind == LineKind::Removed && l.text == "b"));
        assert!(flat
            .iter()
            .any(|l| l.kind == LineKind::Added && l.text == "B"));
        assert!(flat
            .iter()
            .any(|l| l.kind == LineKind::Added && l.text == "d"));
    }

    #[test]
    fn line_numbers_align() {
        let old = "one\ntwo\n";
        let new = "one\ntwo\nthree\n";
        let (hunks, _, _) = compute_text_diff(old, new, 3);
        let added: Vec<_> = hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.kind == LineKind::Added)
            .collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].text, "three");
        assert_eq!(added[0].new_lineno, Some(3));
        assert_eq!(added[0].old_lineno, None);
    }

    #[test]
    fn binary_detected() {
        let fd = file_diff_from_texts("x.bin", None, ChangeKind::Modified, "a\0b", "c\0d", 3);
        assert!(fd.binary);
        assert!(fd.hunks.is_empty());
    }

    #[test]
    fn hunk_patch_shape() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nd\ne\n";
        let fd = file_diff_from_texts("f.txt", None, ChangeKind::Modified, old, new, 1);
        assert_eq!(fd.hunks.len(), 1);
        let p = hunk_patch(&fd, &fd.hunks[0]);
        assert!(p.starts_with("--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n"));
        assert!(p.contains("\n-b\n"));
        assert!(p.contains("\n+B\n"));
        assert!(!p.contains("No newline"));
    }

    #[test]
    fn hunk_patch_no_trailing_newline() {
        let old = "a\nb";
        let new = "a\nB";
        let fd = file_diff_from_texts("f.txt", None, ChangeKind::Modified, old, new, 3);
        let p = hunk_patch(&fd, &fd.hunks[0]);
        // Both sides lack a trailing newline; both the '-' and '+' EOF
        // lines need the marker.
        assert_eq!(p.matches("\\ No newline at end of file\n").count(), 2);
    }

    #[test]
    fn hunk_patch_new_file() {
        let fd = file_diff_from_texts("n.txt", None, ChangeKind::Added, "", "x\ny\n", 3);
        let p = hunk_patch(&fd, &fd.hunks[0]);
        assert!(p.starts_with("--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1,2 @@\n"));
    }

    /// End-to-end: a hunk_patch built from git_unstaged must apply
    /// cleanly with `git apply --cached` and stage exactly that hunk.
    #[test]
    fn hunk_patch_applies_to_index() {
        let dir = std::env::temp_dir().join(format!("diff-core-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        let body: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        std::fs::write(dir.join("f.txt"), body.join("\n") + "\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        // Two well-separated edits -> two hunks; stage only the second.
        let mut edited = body.clone();
        edited[2] = "line 3 CHANGED".into();
        edited[15] = "line 16 CHANGED".into();
        std::fs::write(dir.join("f.txt"), edited.join("\n") + "\n").unwrap();

        let set = git_unstaged(&dir).unwrap();
        let fd = &set.files[0];
        assert_eq!(fd.hunks.len(), 2, "expected two hunks");
        let patch = hunk_patch(fd, &fd.hunks[1]);

        let mut child = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["apply", "--cached", "-"])
            .stdin(std::process::Stdio::piped())
            .output_with_stdin(patch.as_bytes());
        let out = child.take().unwrap();
        assert!(
            out.status.success(),
            "git apply failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let staged = run(&["diff", "--cached"]);
        assert!(staged.contains("line 16 CHANGED"));
        assert!(!staged.contains("line 3 CHANGED"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Small helper so the test above can pipe stdin and collect output.
    trait StdinExt {
        fn output_with_stdin(&mut self, input: &[u8]) -> Option<std::process::Output>;
    }
    impl StdinExt for Command {
        fn output_with_stdin(&mut self, input: &[u8]) -> Option<std::process::Output> {
            use std::io::Write;
            self.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = self.spawn().ok()?;
            child.stdin.as_mut()?.write_all(input).ok()?;
            child.wait_with_output().ok()
        }
    }
}
