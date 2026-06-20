//! rust-analyzer client: LSP JSON-RPC over the child's stdio with
//! `Content-Length` framing.
//!
//! One reader thread owns rust-analyzer's stdout and demultiplexes three
//! message shapes: responses (matched to a waiting request by `id`),
//! server→client requests (answered with a canned result so RA never hangs),
//! and notifications (`$/progress`, `experimental/serverStatus`, … — used to
//! track indexing readiness; the rest are drained). Requests block on a
//! per-call channel until their `id` comes back.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{symbol_kind_name, ProtoError, Range, SymbolNode};

/// How long a single LSP request waits for its response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A connected, initialized rust-analyzer session for one workspace root.
pub struct RaClient {
    root: PathBuf,
    writer: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, Sender<Result<Value, ProtoError>>>>>,
    ready: Arc<(Mutex<bool>, Condvar)>,
    /// Files we've sent `didOpen` for → current version.
    opened: Mutex<HashMap<String, i64>>,
    /// Set once `workspace/symbol` has returned a non-empty result, proving
    /// rust-analyzer's symbol index is built. It lags the `quiescent` signal
    /// and builds lazily on first query, so before this flips we retry an
    /// empty/null result; after, an empty result is a genuine no-match.
    ws_warm: std::sync::atomic::AtomicBool,
    _child: Child,
}

impl RaClient {
    /// Spawn rust-analyzer for `root`, run the LSP handshake, and return a
    /// session ready to take queries. Indexing happens asynchronously after
    /// this returns — use [`RaClient::wait_ready`] before trusting results.
    pub fn start(exe: &Path, root: &Path) -> Result<Arc<Self>, String> {
        let mut child = Command::new(exe)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn rust-analyzer ({}): {e}", exe.display()))?;

        let stdin = child.stdin.take().ok_or("rust-analyzer: no stdin")?;
        let stdout = child.stdout.take().ok_or("rust-analyzer: no stdout")?;

        let writer = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<i64, Sender<Result<Value, ProtoError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ready = Arc::new((Mutex::new(false), Condvar::new()));

        // Reader thread.
        {
            let pending = pending.clone();
            let ready = ready.clone();
            let writer = writer.clone();
            std::thread::spawn(move || reader_loop(stdout, pending, ready, writer));
        }

        let client = Arc::new(Self {
            root: root.to_path_buf(),
            writer,
            next_id: AtomicI64::new(1),
            pending,
            ready,
            opened: Mutex::new(HashMap::new()),
            ws_warm: std::sync::atomic::AtomicBool::new(false),
            _child: child,
        });

        client.initialize()?;
        Ok(client)
    }

    fn initialize(&self) -> Result<(), String> {
        let root_uri = path_to_uri(&self.root);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{
                "uri": root_uri,
                "name": self.root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            }],
            "capabilities": {
                "textDocument": {
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "synchronization": { "didSave": true, "dynamicRegistration": false },
                },
                "window": { "workDoneProgress": true },
            },
            "initializationOptions": {
                // Index from the workspace; keep it quiet otherwise.
                "cachePriming": { "enable": true },
            },
        });
        self.request("initialize", params)
            .map_err(|e| format!("initialize failed: {} {}", e.code, e.message))?;
        self.notify("initialized", json!({}));
        Ok(())
    }

    /// Block until rust-analyzer reports it has finished its initial indexing
    /// (`experimental/serverStatus { quiescent: true }` or an indexing
    /// `$/progress` `end`), or `timeout` elapses. Returns whether it became
    /// ready.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &*self.ready;
        let done = lock.lock().unwrap();
        if *done {
            return true;
        }
        let res = cvar.wait_timeout_while(done, timeout, |d| !*d).unwrap();
        *res.0
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // --- structural ops --------------------------------------------------

    /// Hierarchical document symbols for `file`.
    ///
    /// rust-analyzer processes `didOpen` asynchronously, so the first
    /// `documentSymbol` right after opening a file can race ahead and return an
    /// empty list before analysis lands. We retry-on-empty with a short bounded
    /// backoff; a genuinely symbol-less file just costs that backoff once.
    pub fn symbols(&self, file: &Path) -> Result<Vec<SymbolNode>, ProtoError> {
        self.document_symbols(file, Duration::from_secs(4))
    }

    fn document_symbols(
        &self,
        file: &Path,
        max_retry: Duration,
    ) -> Result<Vec<SymbolNode>, ProtoError> {
        self.ensure_open(file)?;
        let uri = path_to_uri(file);
        let params = json!({ "textDocument": { "uri": uri } });
        let mut last = parse_symbols(&self.request("textDocument/documentSymbol", params.clone())?);
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(200);
        while last.is_empty() && waited < max_retry {
            std::thread::sleep(step);
            waited += step;
            last = parse_symbols(&self.request("textDocument/documentSymbol", params.clone())?);
        }
        Ok(last)
    }

    /// Whole-project outline: document symbols for every `.rs` file the user
    /// owns (walking the workspace, skipping `target/`, deps, hidden dirs),
    /// grouped by file. Returns `(relative_path, symbols)` pairs, files sorted.
    /// The index is warm by the time this runs (callers `ensure` first), so we
    /// use a short per-file retry rather than the full 4s.
    pub fn outline(&self) -> Result<Vec<(String, Vec<SymbolNode>)>, ProtoError> {
        let mut files = Vec::new();
        enumerate_rs_files(&self.root, &mut files, 0);
        files.sort();
        // Batch `didOpen` everything first so rust-analyzer analyzes the whole
        // set in parallel, then a short settle — far faster than paying the
        // open+analyze race serially per file.
        for f in &files {
            let _ = self.ensure_open(f);
        }
        std::thread::sleep(Duration::from_millis(400));
        let mut out = Vec::new();
        for f in &files {
            if let Ok(syms) = self.document_symbols(f, Duration::from_millis(400)) {
                if !syms.is_empty() {
                    let rel = f
                        .strip_prefix(&self.root)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| f.to_string_lossy().to_string());
                    out.push((rel, syms));
                }
            }
        }
        Ok(out)
    }

    /// Source text of `range` within `file` (read from disk, UTF-16 aware).
    pub fn source(&self, file: &Path, range: &Range) -> Result<String, ProtoError> {
        let text = std::fs::read_to_string(file).map_err(|e| ProtoError {
            code: "io".into(),
            message: format!("read {}: {e}", file.display()),
        })?;
        Ok(slice_range(&text, range))
    }

    /// Every `impl` block for a type, gathered across the workspace: inherent
    /// impls and trait impls, each with its methods. rust-analyzer names impl
    /// blocks `impl Foo` / `impl Trait for Foo` in documentSymbol, so we scan
    /// every file's top-level symbols for impl blocks whose target type matches
    /// `type_name` and return them with their children. Lets a struct pane show
    /// "everything implemented on me", even from other files.
    pub fn type_impls(&self, type_name: &str) -> Result<Vec<Value>, ProtoError> {
        let mut files = Vec::new();
        enumerate_rs_files(&self.root, &mut files, 0);
        files.sort();
        for f in &files {
            let _ = self.ensure_open(f);
        }
        std::thread::sleep(Duration::from_millis(200));
        let mut out = Vec::new();
        for f in &files {
            let syms = match self.document_symbols(f, Duration::from_millis(300)) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Absolute path so a symbol pane (which has no workspace root) can
            // open a method directly.
            let abs = f.to_string_lossy().to_string();
            collect_impls(&syms, type_name, &abs, &mut out);
        }
        Ok(out)
    }

    /// Project-wide "organized by type" view: every type (struct/enum/trait/
    /// union) with the methods implemented on it (inherent + trait, gathered
    /// across files), plus a bucket of free functions/consts. Returns
    /// `{ "types": [ {name,kind,file,sel_line,members:[…]} ], "free": [ … ] }`.
    pub fn type_outline(&self) -> Result<Value, ProtoError> {
        let mut files = Vec::new();
        enumerate_rs_files(&self.root, &mut files, 0);
        files.sort();
        for f in &files {
            let _ = self.ensure_open(f);
        }
        std::thread::sleep(Duration::from_millis(400));
        let mut types: std::collections::BTreeMap<String, TypeAcc> = std::collections::BTreeMap::new();
        let mut free: Vec<Value> = Vec::new();
        for f in &files {
            let syms = self
                .document_symbols(f, Duration::from_millis(300))
                .unwrap_or_default();
            gather_types(&syms, &f.to_string_lossy(), &mut types, &mut free);
        }
        let types_json: Vec<Value> = types
            .into_iter()
            .map(|(name, g)| {
                json!({
                    "name": name, "kind": g.kind, "file": g.file,
                    "sel_line": g.sel_line, "members": g.members,
                })
            })
            .collect();
        Ok(json!({ "types": types_json, "free": free }))
    }

    pub fn workspace_symbols(&self, query: &str) -> Result<Value, ProtoError> {
        use std::sync::atomic::Ordering;
        // rust-analyzer's workspace symbol index builds lazily (often only on
        // the first query) and lags `quiescent`, returning `null`/`[]` until
        // it's ready. We can't tell "index warming" from "genuine no-match" up
        // front, so: retry an empty/null result until the index FIRST yields
        // something (marking it warm), then trust `[]` as a real no-match.
        let params = json!({ "query": query });
        let step = Duration::from_millis(300);
        let mut waited = Duration::ZERO;
        loop {
            let last = self.request("workspace/symbol", params.clone())?;
            let nonempty = last.as_array().map(|a| !a.is_empty()).unwrap_or(false);
            if nonempty {
                self.ws_warm.store(true, Ordering::Release);
                return Ok(last);
            }
            // Empty/null: a genuine no-match once the index is proven warm.
            if self.ws_warm.load(Ordering::Acquire) || waited >= Duration::from_secs(30) {
                return Ok(last);
            }
            std::thread::sleep(step);
            waited += step;
        }
    }

    /// Turn raw LSP `Location[]` into entries tagged with the SYMBOL that
    /// encloses each one — so "used by" reads as functions, not file:line, and
    /// a click can open that symbol directly. documentSymbol per unique file
    /// (warm = fast), cached within the call.
    pub fn enrich_locations(&self, locs: &Value) -> Vec<Value> {
        let arr = match locs.as_array() {
            Some(a) => a,
            None => return Vec::new(),
        };
        let mut cache: HashMap<String, Vec<SymbolNode>> = HashMap::new();
        let mut out = Vec::new();
        for loc in arr {
            let uri = loc.get("uri").and_then(Value::as_str).unwrap_or("");
            let path = uri_to_path(uri);
            let line = loc
                .get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            if !cache.contains_key(&path) {
                let syms = self
                    .document_symbols(Path::new(&path), Duration::from_millis(400))
                    .unwrap_or_default();
                cache.insert(path.clone(), syms);
            }
            let mut best: Option<(String, String, String, u32)> = None;
            enclosing_symbol(&cache[&path], line, "", &mut best);
            let (symbol, kind, container, sym_line) =
                best.unwrap_or_else(|| (String::new(), String::new(), String::new(), line));
            out.push(json!({
                "file": path,
                "line": line,
                "symbol": symbol,
                "kind": kind,
                "container": container,
                "sym_line": sym_line,
            }));
        }
        out
    }

    pub fn references(&self, file: &Path, position: crate::Position) -> Result<Value, ProtoError> {
        self.ensure_open(file)?;
        let uri = path_to_uri(file);
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
                "context": { "includeDeclaration": true },
            }),
        )
    }

    pub fn definition(&self, file: &Path, position: crate::Position) -> Result<Value, ProtoError> {
        self.ensure_open(file)?;
        let uri = path_to_uri(file);
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
            }),
        )
    }

    pub fn hover(&self, file: &Path, position: crate::Position) -> Result<Value, ProtoError> {
        self.ensure_open(file)?;
        let uri = path_to_uri(file);
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
            }),
        )
    }

    /// Tell rust-analyzer a file changed on disk: full-document text sync with
    /// a bumped version. Safe to call for an unopened file (sends `didOpen`).
    pub fn did_change(&self, file: &Path) -> Result<(), ProtoError> {
        let text = std::fs::read_to_string(file).map_err(|e| ProtoError {
            code: "io".into(),
            message: format!("read {}: {e}", file.display()),
        })?;
        let key = file.to_string_lossy().to_string();
        let uri = path_to_uri(file);
        let mut opened = self.opened.lock().unwrap();
        match opened.get_mut(&key) {
            Some(version) => {
                *version += 1;
                let v = *version;
                drop(opened);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": v },
                        "contentChanges": [{ "text": text }],
                    }),
                );
            }
            None => {
                opened.insert(key, 1);
                drop(opened);
                self.send_did_open(&uri, &text, 1);
            }
        }
        Ok(())
    }

    // --- internals -------------------------------------------------------

    fn ensure_open(&self, file: &Path) -> Result<(), ProtoError> {
        let key = file.to_string_lossy().to_string();
        {
            let opened = self.opened.lock().unwrap();
            if opened.contains_key(&key) {
                return Ok(());
            }
        }
        let text = std::fs::read_to_string(file).map_err(|e| ProtoError {
            code: "io".into(),
            message: format!("read {}: {e}", file.display()),
        })?;
        let uri = path_to_uri(file);
        self.send_did_open(&uri, &text, 1);
        self.opened.lock().unwrap().insert(key, 1);
        Ok(())
    }

    fn send_did_open(&self, uri: &str, text: &str, version: i64) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": version,
                    "text": text,
                }
            }),
        );
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, ProtoError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);

        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write_framed(&msg) {
            self.pending.lock().unwrap().remove(&id);
            return Err(ProtoError { code: "transport".into(), message: e });
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(res) => res,
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(ProtoError {
                    code: "timeout".into(),
                    message: format!("rust-analyzer did not answer {method} within 60s"),
                })
            }
        }
    }

    fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.write_framed(&msg);
    }

    fn write_framed(&self, msg: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
        let mut w = self.writer.lock().unwrap();
        write!(w, "Content-Length: {}\r\n\r\n", body.len()).map_err(|e| e.to_string())?;
        w.write_all(&body).map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())
    }
}

/// Owns rust-analyzer's stdout; dispatches every framed message.
fn reader_loop(
    stdout: std::process::ChildStdout,
    pending: Arc<Mutex<HashMap<i64, Sender<Result<Value, ProtoError>>>>>,
    ready: Arc<(Mutex<bool>, Condvar)>,
    writer: Arc<Mutex<ChildStdin>>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let body = match read_message(&mut reader) {
            Ok(Some(b)) => b,
            Ok(None) => break, // EOF — rust-analyzer exited
            Err(_) => break,
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };

        let has_id = msg.get("id").map(|v| !v.is_null()).unwrap_or(false);
        let is_response = has_id && (msg.get("result").is_some() || msg.get("error").is_some());
        let method = msg.get("method").and_then(Value::as_str);

        if is_response {
            let id = msg.get("id").and_then(Value::as_i64);
            if let Some(id) = id {
                if let Some(tx) = pending.lock().unwrap().remove(&id) {
                    let payload = if let Some(err) = msg.get("error") {
                        Err(ProtoError {
                            code: err
                                .get("code")
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "lsp".into()),
                            message: err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("rust-analyzer error")
                                .to_string(),
                        })
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(payload);
                }
            }
        } else if let (Some(method), true) = (method, has_id) {
            // Server → client request: answer so RA never blocks.
            answer_server_request(&msg, method, &writer);
        } else if let Some(method) = method {
            handle_notification(method, &msg, &ready);
        }
    }
    // RA died: wake anything waiting on readiness, and fail pending requests.
    let (lock, cvar) = &*ready;
    *lock.lock().unwrap() = false;
    cvar.notify_all();
    let mut p = pending.lock().unwrap();
    for (_, tx) in p.drain() {
        let _ = tx.send(Err(ProtoError {
            code: "backend_died".into(),
            message: "rust-analyzer exited".into(),
        }));
    }
}

fn answer_server_request(msg: &Value, method: &str, writer: &Arc<Mutex<ChildStdin>>) {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let result = match method {
        // RA asks for our config for N items — reply one null per item so it
        // falls back to defaults.
        "workspace/configuration" => {
            let n = msg
                .get("params")
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(1);
            Value::Array(vec![Value::Null; n])
        }
        // create/register/unregister: a null result is the accept.
        _ => Value::Null,
    };
    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    if let Ok(body) = serde_json::to_vec(&reply) {
        if let Ok(mut w) = writer.lock() {
            let _ = write!(w, "Content-Length: {}\r\n\r\n", body.len());
            let _ = w.write_all(&body);
            let _ = w.flush();
        }
    }
}

fn handle_notification(method: &str, msg: &Value, ready: &Arc<(Mutex<bool>, Condvar)>) {
    let mut became_ready = false;
    match method {
        "experimental/serverStatus" => {
            let quiescent = msg
                .get("params")
                .and_then(|p| p.get("quiescent"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if quiescent {
                became_ready = true;
            }
        }
        "$/progress" => {
            // An indexing/cachePriming progress that ends → treat as ready.
            let params = msg.get("params");
            let token = params
                .and_then(|p| p.get("token"))
                .map(|t| t.to_string())
                .unwrap_or_default();
            let kind = params
                .and_then(|p| p.get("value"))
                .and_then(|v| v.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let indexing = token.contains("Indexing")
                || token.contains("cachePriming")
                || token.contains("roots scanned")
                || token.contains("Roots Scanned");
            if kind == "end" && indexing {
                became_ready = true;
            }
        }
        _ => {} // logMessage / publishDiagnostics / showMessage / … — drain.
    }
    if became_ready {
        let (lock, cvar) = &**ready;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

/// Read one `Content-Length`-framed message. `Ok(None)` on clean EOF.
fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut content_len: Option<usize> = None;
    loop {
        let mut line = Vec::new();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end();
        if trimmed.is_empty() {
            break; // blank line ends headers
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_len = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_len.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(buf))
}

// --- symbol parsing ------------------------------------------------------

#[derive(Deserialize)]
struct RaDocSymbol {
    name: String,
    #[serde(default)]
    detail: Option<String>,
    kind: u8,
    range: Range,
    #[serde(rename = "selectionRange")]
    selection_range: Range,
    #[serde(default)]
    children: Vec<RaDocSymbol>,
}

#[derive(Deserialize)]
struct RaSymInfo {
    name: String,
    kind: u8,
    location: RaLocation,
}

#[derive(Deserialize)]
struct RaLocation {
    range: Range,
}

fn parse_symbols(result: &Value) -> Vec<SymbolNode> {
    // Hierarchical DocumentSymbol[] (what we advertised). Fall back to the flat
    // SymbolInformation[] shape if RA gave us that instead.
    if let Ok(syms) = serde_json::from_value::<Vec<RaDocSymbol>>(result.clone()) {
        return syms.iter().map(doc_to_node).collect();
    }
    if let Ok(syms) = serde_json::from_value::<Vec<RaSymInfo>>(result.clone()) {
        return syms
            .into_iter()
            .map(|s| SymbolNode {
                name: s.name,
                kind: s.kind,
                kind_name: symbol_kind_name(s.kind).to_string(),
                detail: None,
                range: s.location.range,
                selection_range: s.location.range,
                children: Vec::new(),
            })
            .collect();
    }
    Vec::new()
}

fn doc_to_node(s: &RaDocSymbol) -> SymbolNode {
    SymbolNode {
        name: s.name.clone(),
        kind: s.kind,
        kind_name: symbol_kind_name(s.kind).to_string(),
        detail: s.detail.clone(),
        range: s.range,
        selection_range: s.selection_range,
        children: s.children.iter().map(doc_to_node).collect(),
    }
}

// --- path/uri + range slicing -------------------------------------------

/// Accumulator for one type group in [`RaClient::type_outline`].
#[derive(Default)]
struct TypeAcc {
    kind: String,
    file: String,
    sel_line: u32,
    members: Vec<Value>,
}

/// Walk a file's symbols, registering type defs, attaching impl methods to
/// their type, and bucketing free functions/consts. Recurses into modules.
fn gather_types(
    nodes: &[SymbolNode],
    file: &str,
    types: &mut std::collections::BTreeMap<String, TypeAcc>,
    free: &mut Vec<Value>,
) {
    for n in nodes {
        if n.name.starts_with("impl") {
            let ty = impl_target(&n.name);
            if ty.is_empty() {
                continue;
            }
            let tr = impl_trait(&n.name);
            let g = types.entry(ty).or_default();
            for c in &n.children {
                if c.kind_name == "method" || c.kind_name == "function" {
                    g.members.push(json!({
                        "name": c.name, "kind": c.kind_name, "file": file,
                        "sel_line": c.selection_range.start.line,
                        "trait": tr, "detail": c.detail,
                    }));
                }
            }
        } else if matches!(
            n.kind_name.as_str(),
            "struct" | "enum" | "interface" | "class" | "union"
        ) {
            let g = types.entry(n.name.clone()).or_default();
            g.kind = n.kind_name.clone();
            g.file = file.to_string();
            g.sel_line = n.selection_range.start.line;
        } else if n.kind_name == "function" || n.kind_name == "constant" {
            free.push(json!({
                "name": n.name, "kind": n.kind_name, "file": file,
                "sel_line": n.selection_range.start.line, "detail": n.detail,
            }));
        } else {
            gather_types(&n.children, file, types, free);
        }
    }
}

/// Recursively gather `impl` blocks targeting `type_name`, descending into
/// modules (so `mod foo { impl Bar {…} }` is found) but not into impl blocks
/// themselves (their children are the methods we want).
fn collect_impls(nodes: &[SymbolNode], type_name: &str, file: &str, out: &mut Vec<Value>) {
    for n in nodes {
        if n.name.starts_with("impl") {
            if impl_target(&n.name) == type_name {
                let methods: Vec<Value> = n
                    .children
                    .iter()
                    .map(|c| {
                        json!({
                            "name": c.name,
                            "kind_name": c.kind_name,
                            "detail": c.detail,
                            "sel_line": c.selection_range.start.line,
                        })
                    })
                    .collect();
                out.push(json!({
                    "file": file,
                    "label": n.name,
                    "trait": impl_trait(&n.name),
                    "sel_line": n.selection_range.start.line,
                    "methods": methods,
                }));
            }
        } else {
            collect_impls(&n.children, type_name, file, out);
        }
    }
}

/// The type an `impl` block targets, from its documentSymbol label. Handles all
/// the shapes rust-analyzer emits:
///   `impl Foo`, `impl Foo<T>`, `impl<T> Foo<T>`, `impl Trait for Foo`,
///   `impl Trait<T> for Foo<T> where …`, `impl Trait for &Foo` / `&mut Foo`,
///   `impl Trait for *const Foo`, `impl Trait for path::Foo`  → all `Foo`.
fn impl_target(label: &str) -> String {
    // The receiver type is after " for " (trait impl) or, for an inherent impl,
    // right after `impl` (skipping a leading `<generics>`).
    let body = match label.find(" for ") {
        Some(i) => &label[i + 5..],
        None => skip_angle_block(label.strip_prefix("impl").unwrap_or(label).trim_start()),
    };
    // Strip reference / pointer receivers, then take the leading type path token
    // up to a generic-arg `<`, whitespace (a where-clause), or end.
    let mut body = body.trim_start();
    loop {
        if let Some(r) = body.strip_prefix("&mut ") {
            body = r.trim_start();
        } else if let Some(r) = body.strip_prefix('&') {
            body = r.trim_start();
        } else if let Some(r) = body.strip_prefix("*const ").or_else(|| body.strip_prefix("*mut ")) {
            body = r.trim_start();
        } else {
            break;
        }
    }
    let end = body
        .find(|c: char| c == '<' || c.is_whitespace())
        .unwrap_or(body.len());
    let ty = &body[..end];
    ty.rsplit("::").next().unwrap_or(ty).to_string()
}

/// If `s` starts with a `<…>` generic-parameter list, return the rest after it.
fn skip_angle_block(s: &str) -> &str {
    if !s.starts_with('<') {
        return s;
    }
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return s[i + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    s
}

/// The trait an `impl` block implements, or "" for an inherent impl:
/// `impl Display for Foo` → `Display`, `impl Foo` → ``.
fn impl_trait(label: &str) -> String {
    let rest = label.strip_prefix("impl ").unwrap_or(label);
    match rest.find(" for ") {
        Some(i) => rest[..i].trim().to_string(),
        None => String::new(),
    }
}

/// Collect the user's own `.rs` files under `root`, skipping `target/`, hidden
/// dirs, and common vendor dirs (so dependencies are excluded). Capped to keep
/// the outline bounded on huge trees.
fn enumerate_rs_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    const CAP: usize = 600;
    if depth > 16 || out.len() >= CAP {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= CAP {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if matches!(name.as_ref(), "target" | "node_modules" | "vendor") {
                continue;
            }
            enumerate_rs_files(&path, out, depth + 1);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// Deepest symbol whose range contains `line` → (name, kind, container, sel_line).
fn enclosing_symbol(
    nodes: &[SymbolNode],
    line: u32,
    parent: &str,
    best: &mut Option<(String, String, String, u32)>,
) {
    for n in nodes {
        if line >= n.range.start.line && line <= n.range.end.line {
            *best = Some((
                n.name.clone(),
                n.kind_name.clone(),
                parent.to_string(),
                n.selection_range.start.line,
            ));
            enclosing_symbol(&n.children, line, &n.name, best);
        }
    }
}

/// Reverse of [`path_to_uri`]: `file://…` (percent-encoded) → filesystem path.
fn uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Slice `[range.start, range.end)` out of `text`, treating LSP positions as
/// 0-based line + 0-based UTF-16 column.
fn slice_range(text: &str, range: &Range) -> String {
    let start = offset_of(text, range.start);
    let end = offset_of(text, range.end);
    text.get(start..end).unwrap_or("").to_string()
}

fn offset_of(text: &str, pos: crate::Position) -> usize {
    // Byte offset of the target line's start.
    let mut line = 0u32;
    let mut idx = 0usize;
    let bytes = text.as_bytes();
    while line < pos.line && idx < bytes.len() {
        if bytes[idx] == b'\n' {
            line += 1;
        }
        idx += 1;
    }
    // Walk the line counting UTF-16 units up to `pos.character`.
    let line_str = &text[idx..];
    let mut utf16 = 0u32;
    for (byte_off, ch) in line_str.char_indices() {
        if ch == '\n' {
            return idx + byte_off;
        }
        if utf16 >= pos.character {
            return idx + byte_off;
        }
        utf16 += ch.len_utf16() as u32;
    }
    idx + line_str.len()
}
