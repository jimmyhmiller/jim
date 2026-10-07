//! Keep file-backed editors in step with their file on disk.
//!
//! Each editor with an [`EditorFilePath`] has a *baseline*: a hash of the
//! file contents the buffer last agreed with (opened from, saved to, or
//! reloaded from). When the file changes on disk, the buffer is either
//!
//! - **equal to the new contents** — our own save arriving back, or an
//!   identical write. Just move the baseline.
//! - **still at the baseline** (no unsaved edits) — reload it from disk.
//! - **neither** — the buffer has unsaved edits AND the file changed.
//!   Leave the buffer alone; clobbering typing is worse than a stale view.
//!
//! A reload is one history transaction over the changed middle of the
//! document (common prefix/suffix trimmed), so ⌘Z undoes it and carets
//! outside the changed region stay put.
//!
//! The baseline is persisted in the pane snapshot (`disk_hash`) because a
//! restored pane gets its text from the snapshot, not the file: without
//! it, a file edited while Jim was down would be indistinguishable from
//! unsaved edits carried across the restart.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;

use bevy::prelude::*;
use editor_core::transaction::{Change, Transaction};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::{EditorFilePath, EditorStateComp};

/// FNV-1a. Stable across builds, which `DefaultHasher` does not promise —
/// the hash outlives the process in the pane snapshot.
pub(crate) fn content_hash(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Marks a pane restored from a snapshot, carrying the baseline the
/// snapshot recorded. Consumed when the pane's path is first tracked.
/// `None` is a snapshot from before baselines were persisted: the buffer
/// is taken as clean, so it catches up with the file — undoably, since
/// the reload is a history transaction.
#[derive(Component)]
pub(crate) struct RestoredDiskHash(pub Option<u64>);

#[derive(Resource, Default)]
pub struct EditorDiskSync {
    baselines: HashMap<Entity, u64>,
    /// Canonical file path per tracked editor — what watcher events are
    /// matched against.
    files: HashMap<Entity, PathBuf>,
    watched_dirs: HashSet<PathBuf>,
    watcher: Option<RecommendedWatcher>,
    rx: Option<Mutex<mpsc::Receiver<PathBuf>>>,
}

impl EditorDiskSync {
    pub(crate) fn baseline(&self, entity: Entity) -> Option<u64> {
        self.baselines.get(&entity).copied()
    }

    /// Record that `entity`'s file now holds `text` (after a save).
    pub(crate) fn record_saved(&mut self, entity: Entity, text: &str) {
        self.baselines.insert(entity, content_hash(text));
    }
}

/// Resolve symlinks (FSEvents reports real paths — `/private/tmp`, not
/// `/tmp`). The file itself may be mid-replace, so canonicalize its
/// directory and re-attach the name.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => dir
            .canonicalize()
            .map(|d| d.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

pub(crate) fn setup_watcher(
    mut sync: ResMut<EditorDiskSync>,
    proxy: Option<Res<bevy::winit::EventLoopProxyWrapper>>,
) {
    let (tx, rx) = mpsc::channel::<PathBuf>();
    // Jim runs a reactive event loop: a change that doesn't wake it would
    // sit in the channel until the next input.
    let proxy = proxy.map(|p| bevy::winit::EventLoopProxy::clone(&p));
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let ev = match res {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("[editor] file watcher error: {e}");
                return;
            }
        };
        if ev.kind.is_access() {
            return;
        }
        for path in ev.paths {
            let _ = tx.send(path);
        }
        if let Some(proxy) = &proxy {
            let _ = proxy.send_event(bevy::winit::WinitUserEvent::WakeUp);
        }
    });
    match watcher {
        Ok(w) => {
            sync.watcher = Some(w);
            sync.rx = Some(Mutex::new(rx));
        }
        Err(e) => eprintln!("[editor] file watcher failed to start: {e} — no reload on change"),
    }
}

/// Start/stop tracking as editors gain, change, or lose their file path.
/// Directories are watched rather than files: editors that save by
/// writing a temp file and renaming it over the original replace the
/// inode, and a watch on the old file would go quiet.
pub(crate) fn track_editor_files(
    mut commands: Commands,
    mut sync: ResMut<EditorDiskSync>,
    mut changed: Query<
        (
            Entity,
            &EditorFilePath,
            &mut EditorStateComp,
            Option<&RestoredDiskHash>,
        ),
        Changed<EditorFilePath>,
    >,
    mut removed: RemovedComponents<EditorFilePath>,
) {
    let mut touched = false;
    for entity in removed.read() {
        sync.baselines.remove(&entity);
        sync.files.remove(&entity);
        touched = true;
    }
    for (entity, path, mut state, restored) in &mut changed {
        touched = true;
        sync.files.insert(entity, canonical(&path.0));
        if restored.is_some() {
            commands.entity(entity).remove::<RestoredDiskHash>();
        }
        // Opened from the file, or just written to it by Save As: the
        // buffer IS the file.
        let baseline = restored
            .and_then(|r| r.0)
            .unwrap_or_else(|| content_hash(&state.0.doc.to_string()));
        sync.baselines.insert(entity, baseline);
        // A restored buffer came from the snapshot; the file may have
        // moved on while Jim was down.
        if restored.is_some() {
            reconcile(&mut sync, entity, &path.0, &mut state);
        }
    }
    if !touched {
        return;
    }
    let want: HashSet<PathBuf> = sync
        .files
        .values()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    let EditorDiskSync {
        watcher,
        watched_dirs,
        ..
    } = &mut *sync;
    let Some(watcher) = watcher else { return };
    for dir in watched_dirs.difference(&want) {
        if let Err(e) = watcher.unwatch(dir) {
            eprintln!("[editor] unwatch {} failed: {e}", dir.display());
        }
    }
    let mut now = HashSet::new();
    for dir in &want {
        if watched_dirs.contains(dir) {
            now.insert(dir.clone());
            continue;
        }
        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                now.insert(dir.clone());
            }
            Err(e) => eprintln!("[editor] watch {} failed: {e}", dir.display()),
        }
    }
    *watched_dirs = now;
}

/// Drain watcher events and reconcile each affected editor with its file.
pub(crate) fn apply_disk_changes(
    mut sync: ResMut<EditorDiskSync>,
    mut editors: Query<(&EditorFilePath, &mut EditorStateComp)>,
) {
    let changed: HashSet<PathBuf> = {
        let Some(rx) = &sync.rx else { return };
        let rx = rx.lock().expect("editor watcher channel poisoned");
        rx.try_iter().map(|p| canonical(&p)).collect()
    };
    if changed.is_empty() {
        return;
    }
    let hit: Vec<Entity> = sync
        .files
        .iter()
        .filter(|(_, p)| changed.contains(*p))
        .map(|(e, _)| *e)
        .collect();
    for entity in hit {
        let Ok((path, mut state)) = editors.get_mut(entity) else {
            continue;
        };
        reconcile(&mut sync, entity, &path.0, &mut state);
    }
}

fn reconcile(sync: &mut EditorDiskSync, entity: Entity, path: &Path, state: &mut Mut<EditorStateComp>) {
    let disk = match std::fs::read_to_string(path) {
        Ok(s) => s,
        // Deleted, or caught between an atomic save's unlink and rename.
        // The rename's own event follows; keep the buffer as it is.
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[editor] reload {} failed: {e}", path.display());
            }
            return;
        }
    };
    let buffer = state.0.doc.to_string();
    let disk_hash = content_hash(&disk);
    if buffer == disk {
        sync.baselines.insert(entity, disk_hash);
        return;
    }
    let Some(baseline) = sync.baselines.get(&entity).copied() else {
        return;
    };
    if content_hash(&buffer) != baseline {
        eprintln!(
            "[editor] {} changed on disk but the buffer has unsaved edits; not reloading",
            path.display()
        );
        return;
    }
    let tr = replace_changed_span(&buffer, &disk);
    let new_state = state.0.apply_with_history_isolated(&tr);
    state.0 = new_state;
    sync.baselines.insert(entity, disk_hash);
    eprintln!("[editor] reloaded {} from disk", path.display());
}

/// One change covering only the part of `old` that differs from `new`, in
/// char offsets (the rope's unit).
fn replace_changed_span(old: &str, new: &str) -> Transaction {
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    let prefix = old_chars
        .iter()
        .zip(&new_chars)
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = old_chars.len().min(new_chars.len()) - prefix;
    let suffix = old_chars
        .iter()
        .rev()
        .zip(new_chars.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let insert: String = new_chars[prefix..new_chars.len() - suffix].iter().collect();
    Transaction::new().change(Change::new(prefix, old_chars.len() - suffix, insert))
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::selection::Selection;
    use editor_core::state::EditorState;

    fn apply(old: &str, new: &str) -> String {
        let st = EditorState::new(ropey::Rope::from_str(old), Selection::cursor(0));
        st.apply(&replace_changed_span(old, new)).doc.to_string()
    }

    #[test]
    fn span_replace_round_trips() {
        for (a, b) in [
            ("abc", "abc"),
            ("", "hello"),
            ("hello", ""),
            ("aaa", "aaaa"),
            ("aaaa", "aaa"),
            ("one\ntwo\nthree", "one\n2\nthree"),
            ("héllo wörld", "héllo brave wörld"),
            ("abcabc", "abc"),
        ] {
            assert_eq!(apply(a, b), b, "{a:?} -> {b:?}");
        }
    }

    #[test]
    fn span_is_minimal() {
        let tr = replace_changed_span("one\ntwo\nthree", "one\n2\nthree");
        assert_eq!(tr.changes.len(), 1);
        let c = &tr.changes[0];
        assert_eq!((c.from, c.to, c.insert.as_str()), (4, 7, "2"));
    }
}
