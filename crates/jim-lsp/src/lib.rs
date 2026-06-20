//! `jim-lsp` — headless rust-analyzer sidecar for Jim's structural code
//! explorer.
//!
//! A long-lived daemon owns ONE rust-analyzer child per *workspace root* and
//! answers structural queries (document symbols, source slices, references,
//! definition, hover) over a Unix socket using line-delimited JSON (NDJSON).
//! It outlives the GUI so rust-analyzer's expensive index survives editor
//! restarts — exactly the reason `jim-daemon` is its own process.
//!
//! This module holds the wire protocol (shared with the `jimctl lsp` client),
//! the on-disk path scheme, workspace-root detection, rust-analyzer discovery,
//! and the connect-or-spawn helper.

pub mod daemon;
pub mod lsp;

use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Wire protocol (NDJSON over the daemon's Unix socket)
//
// Each request is ONE JSON object per line: an optional correlation `id`
// (echoed back, used by `jimctl lsp rpc`) flattened with the tagged op. Each
// response is ONE JSON object per line: the echoed `id` plus exactly one of
// `result` / `error`. Errors are always structured — never a silent stub.
// ---------------------------------------------------------------------------

/// A request line: `{ "id": <any?>, "op": "...", ...fields }`.
#[derive(Debug, Deserialize)]
pub struct RequestLine {
    /// Optional correlation token, echoed verbatim on the matching response.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    #[serde(flatten)]
    pub op: Op,
}

/// The structural operations the daemon understands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Start (if needed) rust-analyzer and block until it has finished its
    /// initial indexing. Returns `{ "ready": true, "root": "..." }`.
    Ensure,
    /// Hierarchical document symbols for one file.
    Symbols { file: String },
    /// Whole-project outline: every owned `.rs` file's symbols, grouped by
    /// file. Returns `{ "files": [ { "file", "symbols" } ] }`.
    Outline,
    /// Every `impl` block (inherent + trait) for a type, with its methods,
    /// gathered across the workspace. Returns `{ "impls": [...] }`.
    Impls { name: String },
    /// Whole project organized by owning type: each type with its impl methods,
    /// plus free functions. Returns `{ "types": [...], "free": [...] }`.
    TypeOutline,
    /// The source text of a range within a file (whole-item slice).
    Source { file: String, range: Range },
    /// Workspace-wide fuzzy symbol search.
    WorkspaceSymbols { query: String },
    /// References to the symbol at a position (raw LSP `Location[]`).
    References { file: String, position: Position },
    /// Definition of the symbol at a position (raw LSP result).
    Definition { file: String, position: Position },
    /// Hover info at a position (raw LSP `Hover`).
    Hover { file: String, position: Position },
}

/// A response line.
#[derive(Debug, Serialize, Deserialize)]
pub struct ResponseLine {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtoError>,
}

impl ResponseLine {
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self { id, result: Some(result), error: None }
    }
    pub fn err(id: Option<serde_json::Value>, code: &str, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(ProtoError { code: code.to_string(), message: message.into() }),
        }
    }
}

/// A structured error. `code` is a stable machine tag; `message` is human text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoError {
    pub code: String,
    pub message: String,
}

/// LSP position: 0-based line, 0-based UTF-16 column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// LSP range (half-open `[start, end)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A node in the document-symbol tree returned to clients. Mirrors LSP
/// `DocumentSymbol` but with a resolved `kind_name` so funct widgets don't
/// need the numeric `SymbolKind` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolNode {
    pub name: String,
    /// LSP `SymbolKind` integer (1-based).
    pub kind: u8,
    /// Lower-case kind name (`"function"`, `"struct"`, `"method"`, …).
    pub kind_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whole-item span (includes doc comments / attributes).
    pub range: Range,
    /// The identifier span — used as a stable-ish anchor for re-resolution.
    pub selection_range: Range,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SymbolNode>,
}

/// Map an LSP `SymbolKind` integer to a lower-case name.
pub fn symbol_kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// On-disk paths
// ---------------------------------------------------------------------------

/// Runtime directory for short-lived ephemera (sockets, pidfiles) that must
/// respect macOS's 104-byte `sockaddr_un.sun_path` limit. We co-locate with
/// the FROZEN terminal-daemon runtime dir (`/tmp/.terminal-bevy-<uid>/`); our
/// filenames are namespaced (`jim-lsp-*`) so nothing collides. Overridable via
/// `TERMINAL_BEVY_RUNTIME_DIR` (kept identical to `jim_daemon` for test
/// isolation).
pub fn runtime_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("TERMINAL_BEVY_RUNTIME_DIR") {
        return PathBuf::from(p);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/.terminal-bevy-{}", uid))
}

/// Short stable hash of a canonical workspace root, for socket/pid filenames.
pub fn workspace_hash(root: &Path) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.to_string_lossy().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Unix socket for the daemon owning `root`.
pub fn socket_path_for(root: &Path) -> PathBuf {
    runtime_dir().join(format!("jim-lsp-{}.sock", workspace_hash(root)))
}

/// PID file for the daemon owning `root`.
pub fn pid_path_for(root: &Path) -> PathBuf {
    runtime_dir().join(format!("jim-lsp-{}.pid", workspace_hash(root)))
}

// ---------------------------------------------------------------------------
// Workspace-root detection
// ---------------------------------------------------------------------------

/// Resolve the *Cargo workspace root* for `start`. Walks up looking for the
/// `Cargo.toml` that declares `[workspace]`; failing that, the topmost
/// directory that has any `Cargo.toml`. Returns `None` when `start` is not
/// inside a Cargo project at all (the caller surfaces a clear error).
pub fn workspace_root(start: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut dir: PathBuf = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start
    };
    let mut topmost_cargo: Option<PathBuf> = None;
    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file() {
            if let Ok(s) = std::fs::read_to_string(&cargo) {
                if s.lines().any(|l| l.trim_start().starts_with("[workspace]")) {
                    return Some(dir);
                }
            }
            topmost_cargo = Some(dir.clone());
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    topmost_cargo
}

// ---------------------------------------------------------------------------
// rust-analyzer discovery
// ---------------------------------------------------------------------------

/// Locate the `rust-analyzer` executable. Order: `RUST_ANALYZER` env →
/// `rustup which rust-analyzer` → `~/.cargo/bin/rust-analyzer` → PATH. Returns
/// a clear error (not a stub) when none is found.
pub fn find_rust_analyzer() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("RUST_ANALYZER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(out) = std::process::Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
    {
        if out.status.success() {
            let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = Path::new(&home).join(".cargo/bin/rust-analyzer");
        if p.is_file() {
            return Ok(p);
        }
    }
    // Bare name — let exec resolve it via PATH. Probe with --version so we fail
    // here (with a clear message) rather than deep inside the daemon.
    if std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(PathBuf::from("rust-analyzer"));
    }
    Err("rust-analyzer not found. Install it with `rustup component add rust-analyzer` \
         (or set $RUST_ANALYZER to its path)."
        .to_string())
}

// ---------------------------------------------------------------------------
// Connect-or-spawn
// ---------------------------------------------------------------------------

/// Path to the `jim-lsp` daemon binary, assumed co-located with the running
/// executable (the app bundle / target dir ships them side by side).
fn daemon_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("jim-lsp")))
        .unwrap_or_else(|| PathBuf::from("jim-lsp"))
}

/// Connect to the daemon for `root`, spawning it if not already running. The
/// daemon double-forks and detaches; we poll the socket until it binds.
/// `root` must already be a resolved workspace root.
pub fn connect_or_spawn(root: &Path) -> Result<UnixStream, String> {
    let sock = socket_path_for(root);
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    // Not running — launch the daemon. It owns its own lifecycle from here.
    let bin = daemon_binary();
    std::process::Command::new(&bin)
        .arg(root)
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;

    // Poll for the socket to appear and accept. The daemon binds only after
    // rust-analyzer's (synchronous) `initialize` returns; on a large workspace
    // the preceding `cargo metadata` can take a while, so allow generous
    // headroom. Indexing happens AFTER bind — `Ensure` waits for that.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "jim-lsp daemon did not come up at {} within 20s",
                sock.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(75));
    }
}

/// Send one request and read exactly one response line. Convenience for the
/// one-shot `jimctl lsp <op>` path. The stream is consumed.
pub fn request_once(mut stream: UnixStream, req: &RequestLine) -> Result<ResponseLine, String> {
    // RequestLine isn't Serialize (it owns a flattened enum + Value); build the
    // wire object from its parts.
    let mut obj = serde_json::to_value(&req.op).map_err(|e| e.to_string())?;
    if let (Some(id), serde_json::Value::Object(map)) = (&req.id, &mut obj) {
        map.insert("id".into(), id.clone());
    }
    let mut line = serde_json::to_string(&obj).map_err(|e| e.to_string())?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    stream.flush().ok();
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut buf = String::new();
    stream.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    let first = buf.lines().next().unwrap_or("");
    if first.is_empty() {
        return Err("daemon closed the connection without a response".to_string());
    }
    serde_json::from_str(first).map_err(|e| format!("bad response: {e}: {first}"))
}
