//! The `jim-lsp` daemon: owns one rust-analyzer session for a workspace root,
//! serves structural queries over a Unix socket (NDJSON), and watches the tree
//! so pulled-out symbol panes can re-sync when files change.
//!
//! Lifecycle mirrors `jim-daemon`: double-fork + setsid to detach, a pidfile
//! that makes the daemon a singleton per root, and a `Cleanup` guard that
//! unlinks the socket + pidfile on exit. Run with `JIM_LSP_FOREGROUND=1` to
//! stay attached (tests / debugging).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::lsp::RaClient;
use crate::{Op, RequestLine, ResponseLine};

/// How long a single structural query waits for indexing before returning a
/// retryable "indexing" error. `Ensure` (the explicit warm-up) waits much
/// longer; per-query we keep it short so a UI driving us never hangs.
const QUERY_READY_WAIT: Duration = Duration::from_secs(30);

/// Entry point. Resolves the workspace root, daemonizes, and serves until the
/// process is killed. Never returns.
pub fn run(start: &Path) -> ! {
    let root = match crate::workspace_root(start) {
        Some(r) => r,
        None => {
            eprintln!(
                "jim-lsp: {} is not inside a Cargo workspace (no Cargo.toml found walking up)",
                start.display()
            );
            std::process::exit(2);
        }
    };

    let foreground = std::env::var_os("JIM_LSP_FOREGROUND").is_some();
    if !foreground {
        if let Err(e) = daemonize() {
            eprintln!("jim-lsp: daemonize failed: {e}");
            std::process::exit(1);
        }
    }

    match run_loop(&root) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("jim-lsp[{}]: {e}", root.display());
            std::process::exit(1);
        }
    }
}

fn run_loop(root: &Path) -> Result<(), String> {
    let socket_path = crate::socket_path_for(root);
    let pid_path = crate::pid_path_for(root);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create runtime dir: {e}"))?;
    }

    // Singleton guard: only one daemon per root. Bails out cleanly if one is
    // already live; reclaims a stale pidfile/socket otherwise.
    match acquire_singleton(&pid_path, &socket_path) {
        Singleton::Acquired => {}
        Singleton::AlreadyRunning => std::process::exit(0),
    }
    let _cleanup = Cleanup {
        socket_path: socket_path.clone(),
        pid_path: pid_path.clone(),
    };

    let exe = crate::find_rust_analyzer()?;
    let client = RaClient::start(&exe, root)?;

    // Kick off the file watcher (best-effort; a failure here just means no live
    // updates, not a dead daemon).
    spawn_watcher(root.to_path_buf(), client.clone());

    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    let listener =
        UnixListener::bind(&socket_path).map_err(|e| format!("bind {}: {e}", socket_path.display()))?;

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let client = client.clone();
                std::thread::spawn(move || serve_conn(stream, client));
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

/// Handle one client connection: NDJSON requests in, NDJSON responses out.
/// One-shot clients send a single line; `jimctl lsp rpc` keeps the connection
/// open and streams many.
fn serve_conn(stream: UnixStream, client: Arc<RaClient>) {
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut writer = write_stream;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<RequestLine>(trimmed) {
            Ok(req) => dispatch(&client, req),
            Err(e) => ResponseLine::err(None, "bad_request", format!("invalid request JSON: {e}")),
        };
        let mut out = match serde_json::to_string(&resp) {
            Ok(s) => s,
            Err(_) => continue,
        };
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

fn dispatch(client: &Arc<RaClient>, req: RequestLine) -> ResponseLine {
    let id = req.id.clone();
    match req.op {
        Op::Ensure => {
            let ready = client.wait_ready(Duration::from_secs(180));
            if ready {
                // Kick the lazy workspace-symbol index in the background so the
                // user's first search doesn't pay the warm-up. Don't block the
                // ensure response on it.
                let c = client.clone();
                std::thread::spawn(move || {
                    let _ = c.workspace_symbols("a");
                });
            }
            ResponseLine::ok(
                id,
                json!({ "ready": ready, "root": client.root().to_string_lossy() }),
            )
        }
        Op::Symbols { file } => {
            // Gate on readiness so we never report a real symbol as missing
            // just because indexing hasn't finished. `Ensure` is the long
            // wait; per-query we return "indexing" promptly so the UI can show
            // it and retry instead of hanging.
            if !client.wait_ready(QUERY_READY_WAIT) {
                return ResponseLine::err(
                    id,
                    "indexing",
                    "rust-analyzer is still indexing; retry shortly",
                );
            }
            match client.symbols(&abs(client, &file)) {
                Ok(syms) => ResponseLine::ok(id, json!({ "symbols": syms })),
                Err(e) => ResponseLine { id, result: None, error: Some(e) },
            }
        }
        Op::Outline => {
            if !client.wait_ready(QUERY_READY_WAIT) {
                return ResponseLine::err(id, "indexing", "rust-analyzer is still indexing");
            }
            match client.outline() {
                Ok(files) => {
                    let arr: Vec<_> = files
                        .into_iter()
                        .map(|(file, symbols)| json!({ "file": file, "symbols": symbols }))
                        .collect();
                    ResponseLine::ok(id, json!({ "files": arr }))
                }
                Err(e) => ResponseLine { id, result: None, error: Some(e) },
            }
        }
        Op::Impls { name } => {
            if !client.wait_ready(QUERY_READY_WAIT) {
                return ResponseLine::err(id, "indexing", "rust-analyzer is still indexing");
            }
            match client.type_impls(&name) {
                Ok(impls) => ResponseLine::ok(id, json!({ "impls": impls })),
                Err(e) => ResponseLine { id, result: None, error: Some(e) },
            }
        }
        Op::TypeOutline => {
            if !client.wait_ready(QUERY_READY_WAIT) {
                return ResponseLine::err(id, "indexing", "rust-analyzer is still indexing");
            }
            match client.type_outline() {
                Ok(v) => ResponseLine::ok(id, v),
                Err(e) => ResponseLine { id, result: None, error: Some(e) },
            }
        }
        Op::Source { file, range } => match client.source(&abs(client, &file), &range) {
            Ok(text) => ResponseLine::ok(id, json!({ "text": text })),
            Err(e) => ResponseLine { id, result: None, error: Some(e) },
        },
        Op::WorkspaceSymbols { query } => {
            if !client.wait_ready(QUERY_READY_WAIT) {
                return ResponseLine::err(id, "indexing", "rust-analyzer is still indexing");
            }
            match client.workspace_symbols(&query) {
                Ok(v) => ResponseLine::ok(id, json!({ "symbols": v })),
                Err(e) => ResponseLine { id, result: None, error: Some(e) },
            }
        }
        Op::References { file, position } => {
            match client.references(&abs(client, &file), position) {
                Ok(v) => ResponseLine::ok(id, json!({ "locations": client.enrich_locations(&v) })),
                Err(e) => ResponseLine { id, result: None, error: Some(e) },
            }
        }
        Op::Definition { file, position } => {
            match client.definition(&abs(client, &file), position) {
                Ok(v) => ResponseLine::ok(id, json!({ "definition": client.enrich_locations(&v) })),
                Err(e) => ResponseLine { id, result: None, error: Some(e) },
            }
        }
        Op::Hover { file, position } => match client.hover(&abs(client, &file), position) {
            Ok(v) => ResponseLine::ok(id, json!({ "hover": v })),
            Err(e) => ResponseLine { id, result: None, error: Some(e) },
        },
    }
}

/// Resolve a client-supplied path against the workspace root so relative paths
/// (`crates/foo/src/lib.rs`) work the same as absolute ones.
fn abs(client: &Arc<RaClient>, file: &str) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        client.root().join(p)
    }
}

// --- file watcher --------------------------------------------------------

/// Watch the workspace for `.rs` changes; on each (debounced) change, push a
/// `didChange` to rust-analyzer and broadcast `lsp.changed` on the global
/// widget bus so open symbol panes re-resolve. Best-effort.
fn spawn_watcher(root: PathBuf, client: Arc<RaClient>) {
    std::thread::spawn(move || {
        use notify_debouncer_full::notify::RecursiveMode;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = match notify_debouncer_full::new_debouncer(
            Duration::from_millis(300),
            None,
            move |res| {
                let _ = tx.send(res);
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("jim-lsp: watcher init failed: {e}");
                return;
            }
        };
        if let Err(e) = debouncer.watch(&root, RecursiveMode::Recursive) {
            eprintln!("jim-lsp: watch {} failed: {e}", root.display());
            return;
        }

        for res in rx {
            let Ok(events) = res else { continue };
            // Collect distinct changed .rs files this batch.
            let mut changed: Vec<PathBuf> = Vec::new();
            for ev in events {
                for path in &ev.paths {
                    if path.extension().and_then(|s| s.to_str()) == Some("rs")
                        && !changed.contains(path)
                    {
                        changed.push(path.clone());
                    }
                }
            }
            for path in changed {
                let _ = client.did_change(&path);
                broadcast_changed(&path);
            }
        }
    });
}

/// Publish `lsp.changed { file }` on the global widget bus via `jimctl`.
fn broadcast_changed(file: &Path) {
    let jimctl = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("jimctl")))
        .unwrap_or_else(|| PathBuf::from("jimctl"));
    let payload = json!({ "file": file.to_string_lossy() }).to_string();
    let _ = std::process::Command::new(jimctl)
        .args([
            "msg",
            "emit",
            "--project",
            "global",
            "--topic",
            "lsp.changed",
            "--json",
            &payload,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

// --- singleton + daemonize + cleanup ------------------------------------

enum Singleton {
    Acquired,
    AlreadyRunning,
}

/// Create the pidfile atomically. If it already exists and the owner is alive
/// (or its socket still accepts), another daemon owns this root. Otherwise the
/// pidfile + socket are stale; reclaim them.
fn acquire_singleton(pid_path: &Path, socket_path: &Path) -> Singleton {
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(pid_path)
        {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                let _ = f.flush();
                return Singleton::Acquired;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner_alive = std::fs::read_to_string(pid_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .map(pid_alive)
                    .unwrap_or(false);
                let socket_live = UnixStream::connect(socket_path).is_ok();
                if owner_alive || socket_live {
                    return Singleton::AlreadyRunning;
                }
                // Stale — reclaim and retry.
                let _ = std::fs::remove_file(pid_path);
                let _ = std::fs::remove_file(socket_path);
            }
            Err(_) => return Singleton::AlreadyRunning,
        }
    }
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Standard double-fork + setsid detach. stdin/stdout → /dev/null; stderr →
/// `$JIM_LSP_LOG` if set, else /dev/null.
fn daemonize() -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid > 0 {
        std::process::exit(0);
    }
    if unsafe { libc::setsid() } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid > 0 {
        std::process::exit(0);
    }

    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let dn_fd = devnull.as_raw_fd();
    let err_fd = if let Some(log) = std::env::var_os("JIM_LSP_LOG") {
        match std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            Ok(f) => {
                let raw = f.as_raw_fd();
                std::mem::forget(f);
                raw
            }
            Err(_) => dn_fd,
        }
    } else {
        dn_fd
    };
    unsafe {
        libc::dup2(dn_fd, 0);
        libc::dup2(dn_fd, 1);
        libc::dup2(err_fd, 2);
    }
    Ok(())
}

struct Cleanup {
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.pid_path);
    }
}
