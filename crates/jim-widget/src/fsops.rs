//! Filesystem *mutations* for widgets: rename, create, copy, trash.
//!
//! The read side (`read_file`, `list_dir`, `list_entries`) lives in
//! `funct_widget.rs`; this is the half that can lose data, so it is kept
//! together and guarded in one place.
//!
//! Two rules shape everything here:
//!
//! - **Deleting means the Trash, never `unlink`.** These operations hang off
//!   a right-click menu in a file tree, one slip away from destroying a
//!   directory tree, and `std::fs::remove_dir_all` has no undo. macOS's
//!   `NSFileManager -trashItemAtURL:` is the only recoverable answer, and it
//!   is what Finder itself does.
//! - **Refuse the paths a mis-click would make catastrophic.** `/`, `$HOME`,
//!   and the empty path are rejected outright: no legitimate file-tree action
//!   targets them, and every destructive accident does.
//!
//! Every entry point returns `Result<_, String>` and the host bridge turns
//! that into `{ ok, error }` — a widget never gets a silent no-op it would
//! render as "nothing happened".

use std::path::{Path, PathBuf};

/// Expand a leading `~` against `$HOME`. Same rule as the read-side host
/// functions, so a widget can pass the paths it got from `list_entries`
/// straight back in.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

/// Reject the paths that make a mis-click unrecoverable: nothing, the root,
/// and the home directory itself. Canonicalizes first so `~/Code/..` cannot
/// smuggle `$HOME` past the comparison.
fn guard(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("empty path".into());
    }
    // A path that doesn't exist yet (a rename destination) can't be one of
    // the forbidden roots, and canonicalize would fail on it.
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if resolved.parent().is_none() {
        return Err("refusing to operate on the filesystem root".into());
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        let home = home.canonicalize().unwrap_or(home);
        if resolved == home {
            return Err("refusing to operate on the home directory".into());
        }
    }
    Ok(())
}

/// Rename/move `from` to `to`. Refuses to clobber an existing `to` — a file
/// tree's rename box is exactly where a typo silently overwrites a sibling.
pub fn rename(from: &str, to: &str) -> Result<(), String> {
    let from = expand_tilde(from);
    let to = expand_tilde(to);
    guard(&from)?;
    guard(&to)?;
    if !from.exists() {
        return Err(format!("{} does not exist", from.display()));
    }
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&from, &to).map_err(|e| e.to_string())
}

/// Create a directory (and any missing parents).
pub fn create_dir(path: &str) -> Result<(), String> {
    let path = expand_tilde(path);
    guard(&path)?;
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())
}

/// Create an empty file, failing if something is already there. Uses
/// `create_new` rather than `write`, so an existing file is never truncated
/// by a "New File…" that collided with a name already on disk.
pub fn create_file(path: &str) -> Result<(), String> {
    let path = expand_tilde(path);
    guard(&path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Copy a file, or a directory tree, to a new path. Refuses an existing
/// destination, and refuses to copy a directory into itself (which would
/// otherwise recurse until the disk filled).
pub fn copy(from: &str, to: &str) -> Result<(), String> {
    let from = expand_tilde(from);
    let to = expand_tilde(to);
    guard(&from)?;
    guard(&to)?;
    if !from.exists() {
        return Err(format!("{} does not exist", from.display()));
    }
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
    if to.starts_with(&from) {
        return Err("cannot copy a directory into itself".into());
    }
    copy_tree(&from, &to)
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(from).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// A free path beside `path`, formed by inserting ` copy` (then ` copy 2`,
/// ` copy 3`, …) before the extension. `src/main.rs` → `src/main copy.rs`,
/// the way Finder names a duplicate.
pub fn duplicate_path(path: &str) -> Result<String, String> {
    let p = expand_tilde(path);
    let parent = p
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;
    let name = p
        .file_name()
        .ok_or_else(|| "path has no file name".to_string())?
        .to_string_lossy()
        .into_owned();
    // Split on the LAST dot, and only when it isn't a leading dot: `.gitignore`
    // is a name, not an extension, and duplicating it must not produce
    // ` copy.gitignore`.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    for n in 1..1000 {
        let suffix = if n == 1 {
            " copy".to_string()
        } else {
            format!(" copy {n}")
        };
        let candidate = parent.join(format!("{stem}{suffix}{ext}"));
        if !candidate.exists() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Err("no free duplicate name".into())
}

/// Move `path` to the Trash. Recoverable by design — see the module note.
pub fn trash(path: &str) -> Result<(), String> {
    let p = expand_tilde(path);
    guard(&p)?;
    if !p.exists() {
        return Err(format!("{} does not exist", p.display()));
    }
    trash_impl(&p)
}

#[cfg(target_os = "macos")]
fn trash_impl(path: &Path) -> Result<(), String> {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    autoreleasepool(|_| {
        let s = NSString::from_str(&path.to_string_lossy());
        // `fileURLWithPath:` (not `URLWithString:`) — the latter would need
        // percent-encoding and returns nil for a path with a space in it.
        let url = unsafe { NSURL::fileURLWithPath(&s) };
        let fm = unsafe { NSFileManager::defaultManager() };
        // `None` for the out-param: we do not need the resulting Trash URL.
        unsafe { fm.trashItemAtURL_resultingItemURL_error(&url, None) }
            .map_err(|e| e.localizedDescription().to_string())
    })
}

#[cfg(not(target_os = "macos"))]
fn trash_impl(path: &Path) -> Result<(), String> {
    // No portable recoverable delete, and an irrecoverable one is not an
    // acceptable substitute for what the menu item promises.
    Err(format!(
        "moving {} to the trash is only implemented on macOS",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "jim-fsops-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rename_moves_and_refuses_to_clobber() {
        let d = tmpdir("rename");
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        std::fs::write(&a, "hi").unwrap();
        rename(a.to_str().unwrap(), b.to_str().unwrap()).unwrap();
        assert!(!a.exists() && b.exists());

        std::fs::write(&a, "other").unwrap();
        let err = rename(a.to_str().unwrap(), b.to_str().unwrap()).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        // The loser of the collision is still there, untouched.
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "other");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "hi");
    }

    #[test]
    fn create_file_never_truncates() {
        let d = tmpdir("create");
        let f = d.join("nested/deep/new.txt");
        create_file(f.to_str().unwrap()).unwrap();
        assert!(f.exists());
        std::fs::write(&f, "content").unwrap();
        assert!(create_file(f.to_str().unwrap()).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "content");
    }

    #[test]
    fn copy_handles_trees_and_rejects_self_nesting() {
        let d = tmpdir("copy");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/x.txt"), "x").unwrap();
        let dst = d.join("dst");
        copy(src.to_str().unwrap(), dst.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("sub/x.txt")).unwrap(), "x");

        let inside = src.join("inner");
        assert!(copy(src.to_str().unwrap(), inside.to_str().unwrap()).is_err());
    }

    #[test]
    fn duplicate_path_keeps_the_extension_and_dotfiles_whole() {
        let d = tmpdir("dup");
        let f = d.join("main.rs");
        std::fs::write(&f, "").unwrap();
        let first = duplicate_path(f.to_str().unwrap()).unwrap();
        assert!(first.ends_with("main copy.rs"), "{first}");

        // Taken → the next name, not a collision.
        std::fs::write(&first, "").unwrap();
        let second = duplicate_path(f.to_str().unwrap()).unwrap();
        assert!(second.ends_with("main copy 2.rs"), "{second}");

        // A leading dot is a name, not an extension.
        let dot = d.join(".gitignore");
        std::fs::write(&dot, "").unwrap();
        let dup = duplicate_path(dot.to_str().unwrap()).unwrap();
        assert!(dup.ends_with(".gitignore copy"), "{dup}");
    }

    #[test]
    fn guard_refuses_root_and_home() {
        assert!(rename("/", "/tmp/x").is_err());
        assert!(trash("/").is_err());
        let home = std::env::var("HOME").unwrap();
        assert!(trash(&home).is_err());
        assert!(trash("~").is_err());
        // A path that only *resolves* to $HOME must be refused too — that is
        // what canonicalizing before the comparison buys.
        assert!(trash(&format!("{home}/.")).is_err());
        assert!(create_dir("").is_err());
    }
}
