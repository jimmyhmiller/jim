//! Generic subprocess host API for funct widgets.
//!
//! Lets a script spawn a child process and talk to it over its
//! stdin/stdout, without any process-specific Rust. The chess widget
//! uses it to drive a UCI engine (Stockfish), but nothing here is
//! chess-aware — any widget that wants to pipe lines to a CLI tool can
//! use the same verbs.
//!
//! Host functions registered in `script_widget::register_host_functions`:
//!
//! ```text
//!   proc_spawn(cmd)            -> handle   spawn `cmd` (no args)
//!   proc_spawn(cmd, args)      -> handle   spawn with an array of args
//!                                          handle is a positive id, or
//!                                          -1 if the spawn failed
//!   proc_write(handle, line)   -> bool     write `line` + "\n" to stdin
//!   proc_read(handle)          -> String   next buffered stdout line, or
//!                                          "" if none is ready (non-
//!                                          blocking — call from on_frame)
//!   proc_alive(handle)         -> bool     is the child still running?
//!   proc_kill(handle)                      kill it now
//!   host_env(name)             -> String   env var, or "" if unset
//! ```
//!
//! # Threading & delivery
//!
//! Each child gets a reader thread that pushes stdout lines into a
//! shared queue (popped by the non-blocking `proc_read`) AND, if a
//! [`ProcNotifier`] is installed, fires a [`ProcEvent`] per line and once
//! on exit. The event path is the recommended one: the funct layer turns
//! those events into `on_proc_output(handle, line)` /
//! `on_proc_exit(handle, code)` handler calls, so a widget is fully
//! event-driven — no `set_animating`, no polling `proc_read` from
//! `on_frame`. The polling API stays for back-compat / explicit use.
//!
//! # Lifecycle
//!
//! One [`ProcRegistry`] per worker thread, captured by the registered
//! closures. When the worker shuts down (pane close), the funct engine
//! holding those closures drops, the registry drops, and every child
//! it still owns is killed. So panes can't leak processes.
//!
//! # Capability note
//!
//! This hands scripts the ability to spawn arbitrary local processes.
//! That's fine while widget scripts are trusted local files (they
//! already run arbitrary logic on a worker thread). If scripts ever
//! become untrusted, this is the API that would need gating.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Cap on buffered stdout lines per child, so a chatty process that's
/// never read can't grow memory without bound. Oldest lines drop.
const MAX_BUFFERED_LINES: usize = 4096;

/// Common locations for third-party CLI tools (Homebrew, MacPorts, the
/// `~/.local/bin`/`~/.cargo/bin` user dirs). When the GUI is launched from
/// its `.app` by Finder/Dock, the inherited PATH is launchd's minimal
/// `/usr/bin:/bin:/usr/sbin:/sbin` — which contains none of these — so bare
/// `proc_spawn("ffmpeg", …)` / `sh -c "ffplay …"` from a widget would fail
/// to resolve the binary. We append these as a fallback (after the real
/// PATH, so a terminal launch's richer PATH still wins) to keep widgets that
/// shell out to brew-installed tools working regardless of launch route.
fn extra_tool_dirs() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/opt/homebrew/sbin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs
}

/// Build a PATH from the exe dir, the current PATH, and the third-party tool
/// dirs — deduped, exe dir first, tool dirs last. Pure (no env reads) so it's
/// unit-testable against a simulated launchd-minimal PATH.
fn build_path(
    exe_dir: std::path::PathBuf,
    current: &std::ffi::OsStr,
    extra: Vec<std::path::PathBuf>,
) -> Option<std::ffi::OsString> {
    let mut seen = std::collections::HashSet::new();
    let entries = std::iter::once(exe_dir)
        .chain(std::env::split_paths(current))
        .chain(extra)
        .filter(|p| seen.insert(p.clone()));
    std::env::join_paths(entries).ok()
}

/// PATH with the running executable's directory prepended (so bare-name spawns
/// of our sibling binaries `jimctl`/`jim-lsp` resolve under the minimal PATH a
/// Finder/AppKit-launched `.app` inherits) and the standard third-party tool
/// dirs ([`extra_tool_dirs`]) appended as a fallback. `None` if the exe dir
/// can't be determined (then the child just inherits the ambient PATH).
pub fn augmented_path() -> Option<std::ffi::OsString> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let current = std::env::var_os("PATH").unwrap_or_default();
    build_path(dir, &current, extra_tool_dirs())
}

/// An event the subprocess reader thread emits to its owner. Lets a host
/// deliver child output/exit to a script event-driven, instead of the
/// script polling `read_line` from an animation tick. The owner installs
/// a [`ProcNotifier`] via [`ProcRegistry::set_notifier`].
pub enum ProcEvent {
    /// One stdout line (already trimmed of the trailing newline).
    Output { handle: i64, line: String },
    /// Stdout hit EOF (process exited). `code` is the exit status if it
    /// could be reaped without blocking, else `None` (e.g. killed).
    Exit { handle: i64, code: Option<i32> },
}

/// Callback the reader threads invoke. Must be `Send + Sync` because each
/// child's reader runs on its own thread.
pub type ProcNotifier = Arc<dyn Fn(ProcEvent) + Send + Sync>;

struct Proc {
    /// Shared with the reader thread so it can reap the exit code on EOF;
    /// also used here for `kill`. Wrapped so both sides can touch it
    /// without the reader holding the lock across a blocking `wait`.
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    /// Stdout lines pushed by the reader thread, popped by `read_line`.
    /// Retained for the back-compat polling API alongside the event push.
    out: Arc<Mutex<VecDeque<String>>>,
    /// Flipped to false by the reader thread when stdout hits EOF
    /// (i.e. the process exited / closed its output).
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct ProcRegistry {
    next_id: i64,
    procs: HashMap<i64, Proc>,
    /// Optional push sink for `proc-output` / `proc-exit` events.
    notifier: Option<ProcNotifier>,
}

impl ProcRegistry {
    pub fn new() -> Self {
        ProcRegistry {
            next_id: 1,
            procs: HashMap::new(),
            notifier: None,
        }
    }

    /// Install the event sink. Children spawned after this push their
    /// output/exit through `notifier` (in addition to the polling queue).
    pub fn set_notifier(&mut self, notifier: ProcNotifier) {
        self.notifier = Some(notifier);
    }

    /// Spawn `cmd args...`. Returns a positive handle, or -1 if the
    /// process couldn't be started.
    pub fn spawn(&mut self, cmd: &str, args: &[String]) -> i64 {
        let mut command = Command::new(cmd);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Prepend the running executable's directory to PATH so a widget can
        // `proc_spawn("jimctl", …)` / `proc_spawn("jim-lsp", …)` by bare name.
        // When the GUI is launched from its `.app` by Finder/AppKit the
        // inherited PATH is minimal and wouldn't otherwise find our sibling
        // binaries (the same reason `agent.rs` augments PATH for `jimctl`).
        if let Some(path) = augmented_path() {
            command.env("PATH", path);
        }
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(_) => return -1,
        };
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            let _ = child.kill();
            return -1;
        };
        let out = Arc::new(Mutex::new(VecDeque::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let child = Arc::new(Mutex::new(child));
        let id = self.next_id;
        self.next_id += 1;
        {
            let out = out.clone();
            let alive = alive.clone();
            let child = child.clone();
            let notifier = self.notifier.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let trimmed = line.trim_end().to_string();
                    if let Ok(mut q) = out.lock() {
                        q.push_back(trimmed.clone());
                        while q.len() > MAX_BUFFERED_LINES {
                            q.pop_front();
                        }
                    }
                    if let Some(n) = &notifier {
                        n(ProcEvent::Output {
                            handle: id,
                            line: trimmed,
                        });
                    }
                }
                alive.store(false, Ordering::Release);
                // Best-effort exit code. `try_wait` is non-blocking and we
                // never hold the lock across a blocking wait, so this can't
                // deadlock with a concurrent `kill`. Poll briefly because
                // stdout EOF can land a hair before the process is reaped.
                let mut code = None;
                for _ in 0..40 {
                    if let Ok(mut c) = child.lock() {
                        if let Ok(Some(status)) = c.try_wait() {
                            code = status.code();
                            break;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                if let Some(n) = &notifier {
                    n(ProcEvent::Exit { handle: id, code });
                }
            });
        }
        self.procs.insert(
            id,
            Proc {
                child,
                stdin: Some(stdin),
                out,
                alive,
            },
        );
        id
    }

    pub fn write_line(&mut self, id: i64, line: &str) -> bool {
        if let Some(p) = self.procs.get_mut(&id) {
            if let Some(stdin) = p.stdin.as_mut() {
                return stdin.write_all(line.as_bytes()).is_ok()
                    && stdin.write_all(b"\n").is_ok()
                    && stdin.flush().is_ok();
            }
        }
        false
    }

    /// Next buffered stdout line, or "" if none is available right now.
    pub fn read_line(&mut self, id: i64) -> String {
        if let Some(p) = self.procs.get(&id) {
            if let Ok(mut q) = p.out.lock() {
                if let Some(l) = q.pop_front() {
                    return l;
                }
            }
        }
        String::new()
    }

    pub fn alive(&self, id: i64) -> bool {
        self.procs
            .get(&id)
            .map(|p| p.alive.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    pub fn kill(&mut self, id: i64) {
        if let Some(p) = self.procs.remove(&id) {
            if let Ok(mut c) = p.child.lock() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

impl Drop for ProcRegistry {
    fn drop(&mut self) {
        for p in self.procs.values_mut() {
            if let Ok(mut c) = p.child.lock() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// Put `text` on the system clipboard. Implemented via `pbcopy` rather
/// than touching NSPasteboard directly, because the caller is a widget
/// worker thread and AppKit pasteboard access off the main thread is
/// not safe. `pbcopy` is a child process, so it sidesteps that. Returns
/// whether the copy succeeded. (macOS-only, like the rest of this app.)
pub fn clipboard_set(text: &str) -> bool {
    let mut child = match Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Write the payload, then drop stdin so pbcopy sees EOF and commits.
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        // stdin dropped here at end of scope -> EOF.
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Reproduces the Dock-launch case: launchd hands a GUI app the minimal
    // `/usr/bin:/bin:/usr/sbin:/sbin` PATH, so Homebrew (`/opt/homebrew/bin`,
    // where ffmpeg/ffplay live) is absent and a widget's bare `proc_spawn`
    // would fail. `build_path` must splice the tool dirs back in.
    #[test]
    fn dock_minimal_path_gains_homebrew() {
        let minimal = std::ffi::OsString::from("/usr/bin:/bin:/usr/sbin:/sbin");
        let extra = vec![
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ];
        let joined = build_path(PathBuf::from("/exe/dir"), &minimal, extra).unwrap();
        let dirs: Vec<_> = std::env::split_paths(&joined).collect();

        assert_eq!(dirs.first().unwrap(), &PathBuf::from("/exe/dir"));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(dirs.contains(&PathBuf::from("/usr/bin"))); // original entries kept
    }

    // A terminal launch already has /opt/homebrew/bin; appending the tool dirs
    // must not duplicate it (and the original, earlier position wins).
    #[test]
    fn no_duplicate_when_already_present() {
        let rich = std::ffi::OsString::from("/opt/homebrew/bin:/usr/bin");
        let extra = vec![PathBuf::from("/opt/homebrew/bin")];
        let joined = build_path(PathBuf::from("/exe/dir"), &rich, extra).unwrap();
        let count = std::env::split_paths(&joined)
            .filter(|p| p == &PathBuf::from("/opt/homebrew/bin"))
            .count();
        assert_eq!(count, 1);
    }
}
