//! IPC between the running `terminal-bevy` app and external CLIs
//! (`tbopen`, `tbwidget`).
//!
//! Wire format: one JSON object per connection, terminated by EOF —
//! keeps both ends trivial (no framing, no length prefix). The app
//! reads to EOF, parses, dispatches; the CLI sends one request and
//! shuts down its half of the socket.
//!
//! Requests are tagged via the `action` field. Unknown actions are
//! logged and dropped so adding new ones never breaks older daemons.
//!
//! Socket lives at `<data_dir()>/socket`. We unlink the path on
//! startup so a stale socket from a previous crashed run doesn't
//! block bind. Stale-while-running detection (e.g. another instance
//! actually listening) isn't handled — first run wins, subsequent
//! ones fail to bind and log.

use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::winit::{EventLoopProxy, WinitUserEvent};
use serde::{Deserialize, Serialize};

use crate::data_dir;

/// Tagged wire format for external CLI → app. Each variant maps onto a
/// `PendingActions` entry on the next frame.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum IpcRequest {
    /// `tbopen <file> [--project NAME]` — open a file in an editor pane.
    OpenFile {
        path: PathBuf,
        #[serde(default)]
        project: Option<String>,
    },
    /// `tbwidget [--title T] [--cwd D] [--project P] -- <cmd> [args...]` —
    /// spawn a new widget pane running `cmd`. When `args` is non-empty
    /// the command runs directly (no shell); otherwise `cmd` is fed to
    /// `sh -c`.
    ///
    /// `position` is an optional window-space top-left `[x, y]` for the
    /// new pane; `size` is an optional `[w, h]`. Both default to the
    /// project's normal cascade / widget kind's default size.
    SpawnWidget {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        position: Option<[f32; 2]>,
        #[serde(default)]
        size: Option<[f32; 2]>,
        /// Optional widget kind override. Default is the subprocess
        /// widget kind (`"widget"`). Pass `"script_widget"` to spawn a
        /// funct-scripted in-process widget — `command` is then
        /// interpreted as the script filename under
        /// `~/.jim/widgets/` (no shell invocation).
        #[serde(default)]
        kind: Option<String>,
        /// Per-instance params for a `script_widget`, forwarded to the
        /// funct global `params`. Lets one generic primitive (`http.ft`,
        /// `bar.ft`) be configured per spawn — URL, columns, in/out
        /// topics — instead of a file per endpoint.
        #[serde(default)]
        params: Option<serde_json::Value>,
    },
    /// `widget projects` — list known projects so external tools can
    /// pick one by name. Response is written back over the same socket
    /// as a single JSON object then EOF: `{"projects":[{"id":N,
    /// "name":"…","active":bool},…]}`.
    ListProjects,
    /// `{"action":"activate_project","project":"Recursion"}` — make a
    /// project the active (viewed) one, as if clicking it in the sidebar.
    /// Lets dev tooling bring freshly-spawned panes into view. Fire-and-
    /// forget; no response body.
    ActivateProject { project: String },
    /// Toggle the 3D project-prism ("cube") overview on/off. Unit
    /// variant, so the wire form is the bare JSON string `"ToggleCube"`.
    /// Primarily a dev/scripting hook mirroring the Cmd+Shift+C keybind.
    ToggleCube,
    /// `tbinbox --project NAME --sender X --body "..."` — append a
    /// message to a project's inbox. The receiver writes the message
    /// to `~/.jim/inbox/<id>.jsonl`; the running app's
    /// `InboxPane` picks it up on its next disk poll. Fire-and-forget;
    /// no response body.
    SendInbox {
        /// Resolved on the GUI side against current `Projects`. May be
        /// a name (`"editor-idea"`) or `"active"` for the current one.
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        sender: Option<String>,
        #[serde(default)]
        subject: Option<String>,
        body: String,
    },
    /// `tbproject set-cwd [--project NAME] <path>` — write a project's
    /// `default_cwd`. `project` accepts a name or `"active"` (default).
    /// `cwd = None` clears the override so new terminals fall back to
    /// `$HOME`.
    SetProjectDefaultCwd {
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// `tbsuggest [--kind K] [--title T] [--command CMD] [--cwd D]
    /// [--reason R] [--config JSON] [--project P]` — park a *suggested*
    /// pane in the drawer (the Quake-style dropdown) rather than
    /// spawning it on the canvas. The AI uses this when it infers a
    /// pane might be useful (e.g. it just ran a command in a side
    /// terminal) but doesn't want to clutter the canvas: the user pulls
    /// it down later and picks it.
    ///
    /// The stored item is a generic `PaneSnapshot`-shaped record: any
    /// registered pane `kind` plus its JSON `config`. As a convenience
    /// for the common "command pane" case, passing `command` with no
    /// explicit `kind`/`config` builds a `run-button` config
    /// (`{title, command, cwd}`) automatically.
    ///
    /// `project` is a *hint*; it's resolved against the live project
    /// list only when the user materializes the suggestion. Fire-and-
    /// forget; no response body.
    SuggestPane {
        /// Registered pane kind. Defaults to `"run-button"` when
        /// `command` is given and no kind is specified.
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        title: Option<String>,
        /// Convenience: shell command for the default `run-button`
        /// kind. Ignored if an explicit `config` is supplied.
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// One-line "why this might be useful", shown under the title
        /// in the drawer.
        #[serde(default)]
        reason: Option<String>,
        /// Explicit kind-specific config blob. When present it's stored
        /// verbatim and `command`/`cwd` are ignored.
        #[serde(default)]
        config: Option<serde_json::Value>,
        #[serde(default)]
        project: Option<String>,
        /// Invocation cwd of the CLI. When `project` is unset, the app
        /// maps this to the owning project (by `default_cwd`) so the
        /// suggestion is scoped to the terminal's project rather than
        /// the GUI's active one. Falls back to unscoped (global) if no
        /// project owns the dir.
        #[serde(default)]
        from_cwd: Option<PathBuf>,
    },
    /// Capture the primary window to a PNG at `path`, rendered by the app
    /// itself (Bevy's `Screenshot`), so it works without macOS screen-
    /// recording permission and never steals focus from the user. Wire
    /// form: `{"action":"screenshot","path":"/tmp/x.png","reason":"why"}`.
    ///
    /// Capture is **gated by a consent toast** ([`crate::screenshot_consent`]):
    /// the request shows an on-screen prompt (with `reason`) the user can
    /// tap to capture immediately or dismiss; if untouched it auto-captures
    /// after a short countdown. So a request issued while the user is
    /// working never grabs a frame out from under them. The file appears
    /// once the capture actually fires.
    Screenshot {
        path: PathBuf,
        /// Short description of what the requester wants to see — shown in
        /// the consent toast so the user knows why.
        #[serde(default)]
        reason: Option<String>,
    },
    /// `tbclose --project P [--kind K]` — close (despawn) panes in a
    /// project, optionally filtered to a pane `kind` (e.g. `script_widget`).
    /// Routes through the normal pane-close path (`on_close` + despawn),
    /// so it's the scriptable equivalent of clicking each close button.
    /// Fire-and-forget; no response body.
    CloseProjectPanes {
        #[serde(default)]
        project: Option<String>,
        /// Pane kind to close (e.g. `"script_widget"`, `"widget"`). None =
        /// every pane in the project.
        #[serde(default)]
        kind: Option<String>,
        /// Close only panes whose title exactly matches one of these. Lets
        /// callers remove a SINGLE pane (e.g. a duplicate "timeline")
        /// without nuking the rest. None = no title filter.
        #[serde(default)]
        titles: Option<Vec<String>>,
    },
    /// `tbmsg emit --project P --topic T [--json '{...}'] [--retain]` —
    /// publish a message onto the widget↔widget bus from the shell (or a
    /// `proc_spawn`ed child). Delivered to every widget in project `P` as
    /// `on_message` / `HostEvent::Message` (default `sender = "tbmsg"`).
    /// Fire-and-forget; no response body.
    WidgetMessage {
        /// Resolved on the GUI side against current `Projects`. A name
        /// (`"datalog-db"`) or `"active"` for the current one. The special
        /// value `"global"` (or `"*"`) targets the GLOBAL channel, which is
        /// delivered to EVERY widget regardless of project — this is what
        /// the cross-project `agent.*` bus rides on (see CHANNELS.md).
        #[serde(default)]
        project: Option<String>,
        topic: String,
        /// Parsed JSON payload (object/array/scalar). Defaults to null.
        #[serde(default)]
        payload: serde_json::Value,
        /// Retain as the topic's last value for late-joining widgets.
        #[serde(default)]
        retain: bool,
        /// Publisher id stamped onto the message (so `on_message`'s
        /// `sender` and reply-by-sender work). Defaults to `"tbmsg"`.
        #[serde(default)]
        sender: Option<String>,
    },
    /// `tbissue --title "…" [--body "…"] [--project NAME]` — file an
    /// issue into a project's Issues pane from the shell. The app appends
    /// it to `~/.jim/issues/<id>.json` (single-writer, no clobber) and
    /// any open Issues pane for that project shows it live. When
    /// `project` is unset the app maps the caller's `from_cwd` to its
    /// owning project, falling back to the active one.
    AddIssue {
        title: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        from_cwd: Option<PathBuf>,
    },
    /// Open the command palette overlay, optionally pre-filling the search
    /// query. Used for scripting / verification and (later) as the entry
    /// point for the DeepSeek "Ask" flow. Fire-and-forget.
    OpenPalette {
        #[serde(default)]
        query: Option<String>,
        /// Immediately route the query to DeepSeek (the "Ask" flow)
        /// instead of just opening the action search.
        #[serde(default)]
        ask: bool,
    },
}

/// One accepted IPC connection: the parsed request plus the open socket,
/// kept around so request-response variants (e.g. `ListProjects`) can
/// write a reply back from the main thread. For fire-and-forget variants
/// the receiver simply drops the stream.
pub struct IpcMessage {
    pub req: IpcRequest,
    pub stream: UnixStream,
}

/// Path of the IPC socket. `None` if `$HOME` isn't set.
pub fn socket_path() -> Option<PathBuf> {
    Some(data_dir()?.join("socket"))
}

/// Dispatch a request to *this* app's own IPC socket — i.e. drive the
/// app the same way the `tb*` CLIs do, over the same wire path. Used by
/// the DeepSeek tool executor so its actions go through the identical
/// `listener → drain_ipc_open_requests` path rather than a parallel
/// in-process dispatch that could drift. Blocking, but the write is a
/// tiny local unix-socket send; the request lands on the next frame.
pub fn dispatch_local(req: &IpcRequest) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = socket_path()
        .ok_or_else(|| std::io::Error::other("no socket path ($HOME unset)"))?;
    let mut stream = UnixStream::connect(path)?;
    let bytes = serde_json::to_vec(req)
        .map_err(|e| std::io::Error::other(format!("serialize ipc request: {e}")))?;
    stream.write_all(&bytes)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

// ---------- Listener: bounded worker pool + read timeouts + metrics ----------
//
// The old listener was a single-threaded accept loop that `read_to_string`'d
// each connection inline. A client that opened a connection and never closed
// its write half parked that read forever, wedging *all* IPC (every new
// connect got ECONNREFUSED once the backlog filled). Now a dedicated acceptor
// hands each connection to a fixed pool of worker threads, each of which reads
// with a timeout — so one slow/stuck client can at most occupy one worker for
// `IPC_READ_TIMEOUT`, never the whole listener.

/// Worker threads handling accepted connections concurrently.
const IPC_WORKERS: usize = 8;
/// Per-connection read budget. A client that doesn't send a full request +
/// EOF within this window is dropped (its worker freed) instead of parking.
const IPC_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How many recent requests to keep for the introspection feed.
const RECENT_CAP: usize = 32;

/// One observed IPC request, for the live activity feed.
#[derive(Clone, Debug)]
pub struct IpcEvent {
    /// Unix-epoch milliseconds when the request finished being read.
    pub ts_ms: u64,
    /// The `action` tag (or `"?"` if unparseable / no payload).
    pub action: String,
    /// `"ok"` | `"parse_error"` | `"timeout"` | `"read_error"`.
    pub outcome: &'static str,
    /// Read+parse duration in milliseconds.
    pub dur_ms: u64,
}

/// Shared, thread-safe view of the listener's health and activity. Updated by
/// the acceptor + worker threads; read by the app to publish to a monitor
/// pane (see `crate::ipc_stats`).
#[derive(Default)]
pub struct IpcMetrics {
    /// Fixed worker-pool size.
    pub workers: usize,
    /// Connections accepted (lifetime).
    pub accepted: AtomicU64,
    /// Requests parsed + forwarded to the app (lifetime).
    pub completed: AtomicU64,
    /// Requests that arrived but failed to parse (lifetime).
    pub parse_errors: AtomicU64,
    /// Connections dropped for exceeding the read timeout (lifetime).
    pub timeouts: AtomicU64,
    /// Connections dropped on a non-timeout read error (lifetime).
    pub read_errors: AtomicU64,
    /// Workers currently handling a connection.
    pub busy: AtomicU64,
    /// Accepted connections waiting for a free worker.
    pub queued: AtomicU64,
    /// Most-recent requests, newest first (capped at [`RECENT_CAP`]).
    pub recent: Mutex<VecDeque<IpcEvent>>,
}

/// Shared handle to [`IpcMetrics`].
pub type IpcMetricsHandle = Arc<IpcMetrics>;

impl IpcMetrics {
    fn record(&self, action: String, outcome: &'static str, dur_ms: u64) {
        if let Ok(mut q) = self.recent.lock() {
            q.push_front(IpcEvent { ts_ms: now_ms(), action, outcome, dur_ms });
            while q.len() > RECENT_CAP {
                q.pop_back();
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Extract the `action` tag from a raw request body without committing to the
/// full typed parse (so we can label even malformed requests).
fn action_tag(buf: &str) -> String {
    #[derive(Deserialize)]
    struct Tag {
        action: String,
    }
    serde_json::from_str::<Tag>(buf)
        .map(|t| t.action)
        .unwrap_or_else(|_| "?".to_string())
}

/// Spawn the IPC listener: an acceptor thread plus a fixed worker pool.
/// Returns the receiver half of an mpsc channel that fires once per parsed
/// request, and a shared [`IpcMetricsHandle`] for introspection. `None` if we
/// can't open the socket — the app keeps running, just without IPC.
///
/// The optional `wakeup` is winit's event-loop proxy; without it, IPC
/// requests sit in the channel until the next reactive-mode tick (up to 5s),
/// which feels broken. With it, each parsed request immediately wakes the main
/// loop so the drain system runs that frame.
pub fn spawn_listener(
    wakeup: Option<EventLoopProxy<WinitUserEvent>>,
) -> Option<(Receiver<IpcMessage>, IpcMetricsHandle)> {
    let path = socket_path()?;
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("[ipc] mkdir {}: {}", parent.display(), e);
            return None;
        }
    }
    // Best-effort unlink of stale socket. If another instance is actually
    // listening, the bind below will still succeed (we just steal the path) —
    // first instance to bind after the unlink wins.
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[ipc] bind {}: {}", path.display(), e);
            return None;
        }
    };

    let (tx, rx) = channel::<IpcMessage>();
    let metrics: IpcMetricsHandle = Arc::new(IpcMetrics {
        workers: IPC_WORKERS,
        ..Default::default()
    });

    // Accepted connections fan out to the worker pool via this queue. Each
    // worker `recv`s under the mutex but only long enough to take a stream —
    // the slow read happens after releasing it, so the pool is genuinely
    // concurrent (one worker stays armed in `recv` while the others process).
    let (jobs_tx, jobs_rx) = channel::<UnixStream>();
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));

    for i in 0..IPC_WORKERS {
        let jobs_rx = jobs_rx.clone();
        let tx = tx.clone();
        let metrics = metrics.clone();
        let wakeup = wakeup.clone();
        if let Err(e) = thread::Builder::new()
            .name(format!("tb-ipc-w{i}"))
            .spawn(move || ipc_worker(jobs_rx, tx, metrics, wakeup))
        {
            eprintln!("[ipc] spawn worker {i}: {e}");
        }
    }

    let acc_metrics = metrics.clone();
    thread::Builder::new()
        .name("tb-ipc-accept".into())
        .spawn(move || accept_loop(listener, jobs_tx, acc_metrics))
        .ok()?;

    Some((rx, metrics))
}

/// Accept connections forever, handing each to the worker pool. Never blocks
/// on a client — the read happens on a worker.
fn accept_loop(listener: UnixListener, jobs_tx: Sender<UnixStream>, metrics: IpcMetricsHandle) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                metrics.accepted.fetch_add(1, Ordering::Relaxed);
                metrics.queued.fetch_add(1, Ordering::Relaxed);
                if jobs_tx.send(stream).is_err() {
                    // All workers gone — app shutting down.
                    metrics.queued.fetch_sub(1, Ordering::Relaxed);
                    return;
                }
            }
            Err(e) => eprintln!("[ipc] accept: {e}"),
        }
    }
}

/// Pull connections off the shared queue and handle them one at a time.
fn ipc_worker(
    jobs_rx: Arc<Mutex<Receiver<UnixStream>>>,
    tx: Sender<IpcMessage>,
    metrics: IpcMetricsHandle,
    wakeup: Option<EventLoopProxy<WinitUserEvent>>,
) {
    loop {
        let stream = {
            let guard = match jobs_rx.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            match guard.recv() {
                Ok(s) => s,
                Err(_) => return, // acceptor dropped — shutdown
            }
        };
        metrics.queued.fetch_sub(1, Ordering::Relaxed);
        metrics.busy.fetch_add(1, Ordering::Relaxed);
        handle_conn(stream, &tx, &metrics, &wakeup);
        metrics.busy.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Read one request from a connection (with a timeout), parse it, and forward
/// it to the app. Records the outcome for introspection.
fn handle_conn(
    mut stream: UnixStream,
    tx: &Sender<IpcMessage>,
    metrics: &IpcMetricsHandle,
    wakeup: &Option<EventLoopProxy<WinitUserEvent>>,
) {
    let start = Instant::now();
    let _ = stream.set_read_timeout(Some(IPC_READ_TIMEOUT));
    let mut buf = String::new();
    match stream.read_to_string(&mut buf) {
        Ok(_) => {
            let action = action_tag(&buf);
            let dur = start.elapsed().as_millis() as u64;
            match serde_json::from_str::<IpcRequest>(&buf) {
                Ok(req) => {
                    metrics.completed.fetch_add(1, Ordering::Relaxed);
                    metrics.record(action, "ok", dur);
                    if tx.send(IpcMessage { req, stream }).is_err() {
                        return; // receiver dropped — shutting down
                    }
                    if let Some(p) = wakeup {
                        let _ = p.send_event(WinitUserEvent::WakeUp);
                    }
                }
                Err(e) => {
                    metrics.parse_errors.fetch_add(1, Ordering::Relaxed);
                    metrics.record(action, "parse_error", dur);
                    eprintln!("[ipc] parse: {e} (raw: {buf:?})");
                }
            }
        }
        Err(e) => {
            let dur = start.elapsed().as_millis() as u64;
            let timed_out = matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            );
            if timed_out {
                metrics.timeouts.fetch_add(1, Ordering::Relaxed);
                metrics.record("?".to_string(), "timeout", dur);
            } else {
                metrics.read_errors.fetch_add(1, Ordering::Relaxed);
                metrics.record("?".to_string(), "read_error", dur);
                eprintln!("[ipc] read: {e}");
            }
        }
    }
}
