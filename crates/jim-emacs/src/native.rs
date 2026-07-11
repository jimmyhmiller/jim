//! Native Emacs pane: run the forked GNU Emacs (`emacs-jim`, the `jim`
//! window system) and render ITS redisplay in a jim pane.
//!
//! Unlike the tty pane ([`crate`]'s `PANE_KIND`), this is the real
//! thing: Emacs's display engine computes glyph layout and picks glyph
//! ids from its own font, then serializes draw-ops (frame/clear/glyph
//! run/cursor) over a unix socket. jim replays them into a per-pane
//! RGBA framebuffer — clearing rects, alpha-blending each glyph
//! rasterized (by glyph id, from Emacs's own font file, via swash) at
//! the exact pixel position Emacs laid it out. Emacs owns every pixel
//! *position*; jim owns every *pixel*.
//!
//! The framebuffer is one Bevy `Image` shown as one `Sprite` under the
//! pane's content_root — no per-glyph entities, no redisplay churn.
//!
//! v1 is display-only (no keyboard/mouse yet — that needs the Coil
//! read_socket_hook to consume input events off the same socket).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use bevy::input::keyboard::Key;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::sprite::Anchor;

use jim_pane::{PaneKindMarker, PaneRect, PaneRegistry, MARGIN, TITLE_H};
use serde_json::Value;

use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::Format;
use swash::FontRef;

/// Stable identifier for native emacs panes.
pub const PANE_KIND: &str = "emacs-native";

/// Supersampling of the framebuffer over Emacs's logical pixels so text
/// stays crisp on retina. Emacs lays out at `px` (its "pixels"); we
/// rasterize/composite at `px * FB_SCALE` and show the sprite at the
/// logical size, letting the GPU downsample.
const FB_SCALE: i64 = 2;

// ---------- Op protocol (text lines from the Coil backend) ----------

#[derive(Clone, Debug)]
enum Op {
    FrameSize { w: i32, h: i32 },
    ClearFrame { bg: u32 },
    ClearArea { x: i32, y: i32, w: i32, h: i32, bg: u32 },
    Run {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        asc: i32,
        fg: u32,
        bg: u32,
        glyphs: Vec<u16>,
    },
    Cursor { x: i32, y: i32, w: i32, h: i32, kind: i32 },
    Font { path: String, px: i32, asc: i32, desc: i32 },
    /// Shift a framebuffer region vertically by `dy` (Emacs's scroll
    /// optimization): copy (x,y,w,h) to (x, y+dy, w, h).
    Scroll { x: i32, y: i32, w: i32, h: i32, dy: i32 },
    /// The frame's title (buffer name + mode) for the pane title bar.
    Title { text: String },
    Flush,
}

fn kv<'a>(fields: &'a [&'a str], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|f| f.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}
fn kvi(fields: &[&str], key: &str) -> i32 {
    kv(fields, key).and_then(|v| v.parse().ok()).unwrap_or(0)
}
fn kvhex(fields: &[&str], key: &str) -> u32 {
    kv(fields, key)
        .and_then(|v| u32::from_str_radix(v, 16).ok())
        .unwrap_or(0)
}

/// Parse an op line into (frame_id, Op). Every op carries `f=N`.
fn parse_op(line: &str) -> Option<(u32, Op)> {
    let mut it = line.split_whitespace();
    let tag = it.next()?;
    let fields: Vec<&str> = it.collect();
    let fid = kv(&fields, "f").and_then(|v| v.parse().ok()).unwrap_or(0);
    let op = match tag {
        "frame-size" | "frame-new" => Op::FrameSize {
            w: kvi(&fields, "w"),
            h: kvi(&fields, "h"),
        },
        "clear-frame" => Op::ClearFrame { bg: kvhex(&fields, "bg") },
        "clear-area" => Op::ClearArea {
            x: kvi(&fields, "x"),
            y: kvi(&fields, "y"),
            w: kvi(&fields, "w"),
            h: kvi(&fields, "h"),
            bg: kvhex(&fields, "bg"),
        },
        "run" => {
            // glyph ids are the trailing `g=,id,id,...` field.
            let glyphs = kv(&fields, "g")
                .map(|g| {
                    g.split(',')
                        .filter(|s| !s.is_empty())
                        .filter_map(|s| s.parse::<u16>().ok())
                        .collect()
                })
                .unwrap_or_default();
            Op::Run {
                x: kvi(&fields, "x"),
                y: kvi(&fields, "y"),
                w: kvi(&fields, "w"),
                h: kvi(&fields, "h"),
                asc: kvi(&fields, "asc"),
                fg: kvhex(&fields, "fg"),
                bg: kvhex(&fields, "bg"),
                glyphs,
            }
        }
        "cursor" => Op::Cursor {
            x: kvi(&fields, "x"),
            y: kvi(&fields, "y"),
            w: kvi(&fields, "w"),
            h: kvi(&fields, "h"),
            kind: kvi(&fields, "kind"),
        },
        "font" => Op::Font {
            // path is always the LAST field and may contain spaces
            // (e.g. "Andale Mono.ttf"), so take the whole remainder of
            // the line after "path=" rather than a whitespace token.
            path: line
                .find("path=")
                .map(|i| line[i + 5..].to_string())
                .unwrap_or_default(),
            px: kvi(&fields, "px"),
            asc: kvi(&fields, "asc"),
            desc: kvi(&fields, "desc"),
        },
        "scroll" => Op::Scroll {
            x: kvi(&fields, "x"),
            y: kvi(&fields, "y"),
            w: kvi(&fields, "w"),
            h: kvi(&fields, "h"),
            dy: kvi(&fields, "dy"),
        },
        "title" => Op::Title {
            // Everything after "title f=N " — may contain spaces.
            text: line.splitn(3, char::is_whitespace).nth(2).unwrap_or("").to_string(),
        },
        "flush" => Op::Flush,
        _ => return None, // frame-delete: ignored
    };
    Some((fid, op))
}

// ---------- Shared connection: one emacs, many frames (panes) ----------
//
// A single Emacs process holds all buffers; each jim pane is a frame on
// it, so the same buffer can appear in multiple panes. Draw-ops are
// routed to per-frame queues by frame id; input records carry the
// target frame id.

/// PID (== process-group id, since we spawn emacs with `process_group(0)`)
/// of the shared emacs child, or 0 when there is none. Recorded so the
/// async-signal-safe `handle_term_signal` can reap it when jim is killed
/// with SIGTERM/SIGINT/SIGHUP — the paths where `Drop`/`AppExit` never
/// run. A single shared emacs means one pid is enough.
static EMACS_CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Previous disposition of each signal we hook (indexed by
/// `prev_handler_slot`), captured at install time so we can CHAIN to it
/// rather than replace it. Bevy's `TerminalCtrlCHandlerPlugin` (via the
/// `ctrlc` crate) already owns SIGINT/SIGTERM and turns them into a
/// graceful `AppExit` — which is what runs `kill_emacs_on_app_exit`,
/// layout persistence, etc. Overriding it with SIG_DFL + re-raise would
/// trade the orphan bug for a broken graceful shutdown.
static PREV_SIG_HANDLERS: [AtomicUsize; 3] =
    [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)];

fn prev_handler_slot(sig: i32) -> Option<usize> {
    match sig {
        nix::libc::SIGTERM => Some(0),
        nix::libc::SIGINT => Some(1),
        nix::libc::SIGHUP => Some(2),
        _ => None,
    }
}

/// SIGTERM/SIGINT/SIGHUP handler: SIGTERM the emacs process group so it
/// (and any grandchildren) die instead of orphaning at 100% CPU, then
/// hand off to whatever handler was installed before us (bevy's ctrl-c →
/// graceful AppExit). If there was none, restore the default disposition
/// and re-raise so jim terminates as it normally would. This makes the
/// emacs kill unconditional (even if the graceful exit later wedges)
/// without stealing the graceful path. ASYNC-SIGNAL-SAFE: only `kill`,
/// `signal`, `raise`, atomic loads, and a call into the previous handler
/// (ctrlc's is a self-pipe write) — no allocation, no locks, no `wait`.
extern "C" fn handle_term_signal(sig: i32) {
    let pid = EMACS_CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // Negative pid → the whole process group (emacs is the group
        // leader). SIGTERM lets emacs auto-save via its own handler.
        unsafe {
            nix::libc::kill(-pid, nix::libc::SIGTERM);
        }
    }
    let prev = prev_handler_slot(sig)
        .map(|i| PREV_SIG_HANDLERS[i].load(Ordering::SeqCst))
        .unwrap_or(nix::libc::SIG_DFL);
    if prev == nix::libc::SIG_IGN {
        return;
    }
    if prev != nix::libc::SIG_DFL && prev != nix::libc::SIG_ERR {
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(prev) };
        f(sig);
        return;
    }
    unsafe {
        nix::libc::signal(sig, nix::libc::SIG_DFL);
        nix::libc::raise(sig);
    }
}

/// Install `handle_term_signal` for the fatal terminating signals, once
/// per process, capturing (and later chaining to) the handlers that were
/// there first — in practice bevy's ctrl-c handler, installed during
/// plugin build, well before the first emacs pane spawns. Kept
/// intentionally tiny.
fn install_term_signal_handlers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let h = handle_term_signal as *const () as nix::libc::sighandler_t;
        for sig in [nix::libc::SIGTERM, nix::libc::SIGINT, nix::libc::SIGHUP] {
            let prev = nix::libc::signal(sig, h);
            if let Some(i) = prev_handler_slot(sig) {
                PREV_SIG_HANDLERS[i].store(prev, Ordering::SeqCst);
            }
        }
    });
}

/// 24-byte input record: [type, mods, button, down, fid(4), code(4),
/// x(4), y(4), reserved(4)]. For resize, code/x hold new w/h.
struct SharedConn {
    writer: Arc<Mutex<Option<UnixStream>>>,
    frame_ops: Arc<Mutex<HashMap<u32, Vec<Op>>>>,
    /// Per-frame split direction from a `frame-new … split=N` (1=right,
    /// 2=below) — set by the worker, consumed by reconcile_frames to
    /// pick the dock edge.
    split_hints: Arc<Mutex<HashMap<u32, u8>>>,
    generation: Arc<AtomicU64>,
    child: std::process::Child,
    sock_path: PathBuf,
    /// Control channel: jim → emacs newline-delimited commands (e.g.
    /// `open <fid> <path>`). jim is the server; emacs connects as a client
    /// (see `jim--ctl` in jim-win.el) and the accepted stream lands here.
    ctl_writer: Arc<Mutex<Option<UnixStream>>>,
    ctl_sock_path: PathBuf,
    _thread: std::thread::JoinHandle<()>,
    _ctl_thread: std::thread::JoinHandle<()>,
}

impl SharedConn {
    fn rec(t: u8, fid: u32) -> [u8; 24] {
        let mut r = [0u8; 24];
        r[0] = t;
        r[4..8].copy_from_slice(&fid.to_le_bytes());
        r
    }
    fn send(&self, rec: [u8; 24]) -> bool {
        if let Ok(mut w) = self.writer.lock() {
            if let Some(stream) = w.as_mut() {
                use std::io::Write;
                return stream.write_all(&rec).is_ok();
            }
        }
        false
    }
    fn send_resize(&self, fid: u32, w: i32, h: i32) -> bool {
        let mut r = Self::rec(3, fid);
        r[8..12].copy_from_slice(&w.to_le_bytes());
        r[12..16].copy_from_slice(&h.to_le_bytes());
        self.send(r)
    }
    /// Ask emacs to `find-file` `path` in the frame `fid`. Newline-delimited
    /// on the control socket; `path` is the rest of the line (may contain
    /// spaces, must not contain `\n`). Returns false if emacs hasn't
    /// connected the control channel yet.
    fn send_open_file(&self, fid: u32, path: &str) -> bool {
        if path.contains('\n') {
            return false;
        }
        self.send_ctl(&format!("open {fid} {path}\n"))
    }
    /// Set the emacs default font size (points), applied to all frames.
    fn send_font(&self, size: i32) -> bool {
        self.send_ctl(&format!("font {size}\n"))
    }
    /// Write one newline-terminated command to the control channel.
    fn send_ctl(&self, line: &str) -> bool {
        if let Ok(mut w) = self.ctl_writer.lock() {
            if let Some(stream) = w.as_mut() {
                use std::io::Write;
                return stream.write_all(line.as_bytes()).is_ok();
            }
        }
        false
    }
    fn send_key(&self, fid: u32, code: u32, mods: u8) {
        let mut r = Self::rec(1, fid);
        r[1] = mods;
        r[8..12].copy_from_slice(&code.to_le_bytes());
        self.send(r);
    }
    /// A function key: `keysym` is an X keysym (0xff51 Left, …); byte2=1
    /// tells the port to emit a NON_ASCII_KEYSTROKE_EVENT.
    fn send_fkey(&self, fid: u32, keysym: u32, mods: u8) {
        let mut r = Self::rec(1, fid);
        r[1] = mods;
        r[2] = 1;
        r[8..12].copy_from_slice(&keysym.to_le_bytes());
        self.send(r);
    }
    /// Mouse wheel: direction (up) + frame-pixel position.
    fn send_wheel(&self, fid: u32, up: bool, x: i32, y: i32) {
        let mut r = Self::rec(7, fid);
        r[3] = up as u8;
        r[12..16].copy_from_slice(&x.to_le_bytes());
        r[16..20].copy_from_slice(&y.to_le_bytes());
        self.send(r);
    }
    fn send_mouse(&self, fid: u32, button: u8, down: bool, x: i32, y: i32, mods: u8) {
        let mut r = Self::rec(2, fid);
        r[1] = mods;
        r[2] = button;
        r[3] = down as u8;
        r[12..16].copy_from_slice(&x.to_le_bytes());
        r[16..20].copy_from_slice(&y.to_le_bytes());
        self.send(r);
    }
    fn send_motion(&self, fid: u32, x: i32, y: i32) {
        let mut r = Self::rec(4, fid);
        r[12..16].copy_from_slice(&x.to_le_bytes());
        r[16..20].copy_from_slice(&y.to_le_bytes());
        self.send(r);
    }
    fn send_create_frame(&self, fid: u32) -> bool {
        self.send(Self::rec(5, fid))
    }
    fn send_delete_frame(&self, fid: u32) {
        self.send(Self::rec(6, fid));
    }

    /// Terminate the shared emacs child. Idempotent: safe to call from
    /// both the `AppExit` system and `Drop`. SIGTERM the whole process
    /// group first (graceful — emacs auto-saves and grandchildren die),
    /// give it a brief moment, then guarantee the reap with SIGKILL.
    fn kill_child(&mut self) {
        let pid = self.child.id() as i32;
        // Clear the static so the signal handler won't also target a pid
        // we're already reaping.
        EMACS_CHILD_PID.store(0, Ordering::SeqCst);
        if pid > 0 {
            unsafe {
                nix::libc::kill(-pid, nix::libc::SIGTERM);
            }
            // ~500ms grace for emacs to auto-save (SIGTERM → Fkill_emacs)
            // and exit before we force it.
            for _ in 0..50 {
                match self.child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// jim theme colors handed to Emacs so its frame matches the native UI.
#[derive(Clone)]
pub struct EmacsTheme {
    pub bg: String,
    pub fg: String,
    pub cursor: String,
}

fn hex(c: bevy::color::LinearRgba) -> String {
    let s = Color::LinearRgba(c).to_srgba();
    format!(
        "#{:02x}{:02x}{:02x}",
        (s.red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (s.green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (s.blue.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

impl SharedConn {
    fn start(
        theme: EmacsTheme,
        wakeup: Option<bevy::winit::EventLoopProxy<bevy::winit::WinitUserEvent>>,
    ) -> std::io::Result<Self> {
        let sock_path = jim_pane_data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("emacs-shared.sock");
        let _ = std::fs::remove_file(&sock_path);
        if let Some(parent) = sock_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let listener = UnixListener::bind(&sock_path)?;

        // Control channel (jim → emacs commands). Separate socket so the
        // fixed-24-byte input protocol stays untouched; parsing lives in
        // elisp (a normal process filter), where variable-length strings
        // belong.
        let ctl_sock_path = jim_pane_data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("emacs-ctl.sock");
        let _ = std::fs::remove_file(&ctl_sock_path);
        let ctl_listener = UnixListener::bind(&ctl_sock_path)?;

        let emacs_bin = emacs_binary();
        // Load the user's real init (no `-Q`) so their completion
        // framework, keybindings, etc. are present. `--no-splash` keeps
        // the *scratch* buffer up front. Override the whole arg list
        // with JIM_EMACS_ARGS (space-separated) for a vanilla `-Q` run.
        let mut cmd = std::process::Command::new(&emacs_bin);
        match std::env::var("JIM_EMACS_ARGS") {
            Ok(args) if !args.trim().is_empty() => {
                cmd.args(args.split_whitespace());
            }
            _ => {
                // Seed the frame with jim's palette so an un-themed
                // Emacs blends into the native UI. `-bg/-fg/-cr` land in
                // the initial-frame-alist; a user emacs theme can still
                // override. JIM_DIVIDER is read by jim-win.el for the
                // window-divider face.
                cmd.arg("--no-splash")
                    .args(["-bg", &theme.bg, "-fg", &theme.fg, "-cr", &theme.cursor])
                    // -bg/-fg only theme the INITIAL frame (initial-frame-
                    // alist). Panes 2+ are new frames, so also hand the
                    // palette via env → jim-win.el puts it in
                    // default-frame-alist, which every frame inherits.
                    .env("JIM_BG", &theme.bg)
                    .env("JIM_FG", &theme.fg)
                    .env("JIM_CURSOR", &theme.cursor)
                    .env("JIM_DIVIDER", &theme.cursor);
            }
        }
        let child = cmd
            .env("JIM_DISPLAY", &sock_path)
            .env("JIM_CTL", &ctl_sock_path)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::null())
            // Own process group (pgid == child pid) so we can reap emacs
            // and any grandchildren as a unit, and so terminal signals to
            // jim's group don't hit emacs at the wrong time.
            .process_group(0)
            .spawn()?;

        // Record the pid + arm the signal handler so a SIGTERM/SIGINT/
        // SIGHUP to jim (where Drop/AppExit never run) still kills emacs
        // instead of orphaning it.
        EMACS_CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
        install_term_signal_handlers();

        let frame_ops: Arc<Mutex<HashMap<u32, Vec<Op>>>> = Arc::new(Mutex::new(HashMap::new()));
        let split_hints: Arc<Mutex<HashMap<u32, u8>>> = Arc::new(Mutex::new(HashMap::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let writer: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let fo_w = frame_ops.clone();
        let sh_w = split_hints.clone();
        let gen_w = generation.clone();
        let writer_w = writer.clone();
        let thread = std::thread::Builder::new()
            .name("emacs-native".into())
            .spawn(move || conn_loop(listener, fo_w, sh_w, gen_w, writer_w, wakeup))
            .expect("spawn emacs-native thread");

        let ctl_writer: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let ctl_writer_w = ctl_writer.clone();
        let ctl_thread = std::thread::Builder::new()
            .name("emacs-native-ctl".into())
            .spawn(move || ctl_accept_loop(ctl_listener, ctl_writer_w))
            .expect("spawn emacs-native-ctl thread");

        Ok(Self {
            writer,
            frame_ops,
            split_hints,
            generation,
            child,
            sock_path,
            ctl_writer,
            ctl_sock_path,
            _thread: thread,
            _ctl_thread: ctl_thread,
        })
    }
}

impl Drop for SharedConn {
    fn drop(&mut self) {
        self.kill_child();
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_file(&self.ctl_sock_path);
    }
}

/// Accept emacs's control-channel connection and stash the stream so
/// `send_open_file` can write to it. Keeps accepting so an emacs restart
/// re-establishes the channel.
fn ctl_accept_loop(listener: UnixListener, writer: Arc<Mutex<Option<UnixStream>>>) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Ok(mut w) = writer.lock() {
                    *w = Some(s);
                }
            }
            Err(_) => break,
        }
    }
}

/// Accept the emacs connection and route each op to its frame's queue,
/// waking the render loop.
fn conn_loop(
    listener: UnixListener,
    frame_ops: Arc<Mutex<HashMap<u32, Vec<Op>>>>,
    split_hints: Arc<Mutex<HashMap<u32, u8>>>,
    generation: Arc<AtomicU64>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    wakeup: Option<bevy::winit::EventLoopProxy<bevy::winit::WinitUserEvent>>,
) {
    let stream: UnixStream = match listener.accept() {
        Ok((s, _)) => s,
        Err(e) => {
            eprintln!("[emacs-native] accept failed: {e}");
            return;
        }
    };
    if let Ok(clone) = stream.try_clone() {
        *writer.lock().expect("writer lock") = Some(clone);
    }
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        // Capture the split-direction hint that rides on frame-new.
        if line.starts_with("frame-new") {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let fid = kv(&fields, "f").and_then(|v| v.parse().ok()).unwrap_or(0);
            let split = kv(&fields, "split").and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
            if fid != 0 && split != 0 {
                split_hints.lock().expect("split_hints").insert(fid, split);
            }
        }
        if let Some((fid, op)) = parse_op(&line) {
            frame_ops
                .lock()
                .expect("frame_ops lock")
                .entry(fid)
                .or_default()
                .push(op);
            generation.fetch_add(1, Ordering::Relaxed);
            if let Some(p) = wakeup.as_ref() {
                let _ = p.send_event(bevy::winit::WinitUserEvent::WakeUp);
            }
        }
    }
}

fn emacs_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("JIM_EMACS_BIN") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Documents/Code/emacs-jim/src/emacs")
}

fn jim_pane_data_dir() -> Option<PathBuf> {
    jim_daemon_data_dir()
}
fn jim_daemon_data_dir() -> Option<PathBuf> {
    // Reuse ~/.jim (same root the daemon/scrollback use).
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".jim"))
}

// ---------- Store + components ----------

#[derive(Default, Resource)]
pub struct EmacsNativeStore {
    /// The one shared Emacs process (started with the first pane).
    shared: Option<SharedConn>,
    /// Pane entity → its Emacs frame id.
    frame_of_pane: HashMap<Entity, u32>,
    /// Next frame id to hand out (the initial frame is id 1).
    next_id: u32,
    /// Frame ids whose create-frame command hasn't been delivered yet
    /// (the socket writer isn't ready until Emacs connects). Retried
    /// every frame until sent — otherwise a pane spawned before Emacs
    /// boots would silently never get its frame.
    pending_create: Vec<u32>,
}

impl EmacsNativeStore {
    /// Ask the emacs pane `pane` to open `path` via `find-file` in its own
    /// frame. Returns false if there's no live emacs, `pane` isn't a known
    /// emacs frame, or the control channel isn't connected yet.
    pub fn send_open_file(&self, pane: Entity, path: &str) -> bool {
        let (Some(conn), Some(&fid)) = (self.shared.as_ref(), self.frame_of_pane.get(&pane))
        else {
            return false;
        };
        conn.send_open_file(fid, path)
    }

    /// Set the emacs default font size (points) on all native panes. Font
    /// is a global face attribute, so no per-pane frame id is needed.
    pub fn send_font(&self, size: i32) -> bool {
        self.shared.as_ref().is_some_and(|c| c.send_font(size))
    }
}

/// Per-pane framebuffer + glyph rasterizer state.
#[derive(Component)]
pub struct EmacsFrame {
    /// This pane's Emacs frame id (ops with `f=<id>` route here).
    frame_id: u32,
    /// The RGBA framebuffer shown as the pane's content sprite.
    image: Handle<Image>,
    /// The sprite entity (child of content_root) whose custom_size we
    /// keep in sync with the logical frame size.
    sprite: Entity,
    /// Framebuffer dimensions in device pixels (emacs px * FB_SCALE).
    fb_w: u32,
    fb_h: u32,
    /// Working CPU framebuffer (RGBA). Ops draw into this; it's copied
    /// to the GPU `image` only on `flush`, so partial redisplays never
    /// present (no divider/text flicker).
    fb: Vec<u8>,
    /// jim theme background, for the pre-clear framebuffer fill.
    bg: [u8; 3],
    last_generation: u64,
    raster: GlyphRaster,
}

/// swash-backed rasterizer: one Emacs font, glyph bitmaps cached by id.
struct GlyphRaster {
    font_bytes: Option<&'static [u8]>,
    px: f32,
    ctx: ScaleContext,
    cache: HashMap<u16, Option<CachedGlyph>>,
}

#[derive(Clone)]
struct CachedGlyph {
    w: i32,
    h: i32,
    left: i32,
    top: i32,
    alpha: Vec<u8>,
}

impl GlyphRaster {
    fn new() -> Self {
        Self {
            font_bytes: None,
            px: 14.0,
            ctx: ScaleContext::new(),
            cache: HashMap::new(),
        }
    }

    fn set_font(&mut self, path: &str, px: i32) {
        self.px = px.max(1) as f32;
        self.cache.clear();
        self.font_bytes = std::fs::read(path)
            .ok()
            .map(|b| &*Box::leak(b.into_boxed_slice()));
        if self.font_bytes.is_none() && !path.is_empty() {
            eprintln!("[emacs-native] could not read font {path}");
        }
    }

    fn glyph(&mut self, id: u16) -> Option<&CachedGlyph> {
        if !self.cache.contains_key(&id) {
            let g = self.rasterize(id);
            self.cache.insert(id, g);
        }
        self.cache.get(&id).and_then(|o| o.as_ref())
    }

    fn rasterize(&mut self, id: u16) -> Option<CachedGlyph> {
        let font = FontRef::from_index(self.font_bytes?, 0)?;
        let mut scaler = self
            .ctx
            .builder(font)
            .size(self.px * FB_SCALE as f32)
            .hint(true)
            .build();
        let img = Render::new(&[Source::Outline])
            .format(Format::Alpha)
            .render(&mut scaler, id)?;
        Some(CachedGlyph {
            w: img.placement.width as i32,
            h: img.placement.height as i32,
            left: img.placement.left,
            top: img.placement.top,
            alpha: img.data,
        })
    }
}

// ---------- Framebuffer compositing ----------

fn unpack(rgb: u32) -> [u8; 3] {
    [
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    ]
}

/// Fill a rect with an opaque color.
fn fill_rect(px: &mut [u8], fb_w: u32, fb_h: u32, x: i32, y: i32, w: i32, h: i32, rgb: u32) {
    let c = unpack(rgb);
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + w).max(0) as u32).min(fb_w);
    let y1 = ((y + h).max(0) as u32).min(fb_h);
    for row in y0..y1 {
        let base = ((row * fb_w + x0) * 4) as usize;
        for col in 0..(x1.saturating_sub(x0)) {
            let i = base + (col * 4) as usize;
            px[i] = c[0];
            px[i + 1] = c[1];
            px[i + 2] = c[2];
            px[i + 3] = 255;
        }
    }
}

/// Alpha-blend one coverage bitmap (fg over whatever is in the buffer).
#[allow(clippy::too_many_arguments)]
fn blend_glyph(
    px: &mut [u8],
    fb_w: u32,
    fb_h: u32,
    gx: i32,
    gy: i32,
    gw: i32,
    gh: i32,
    alpha: &[u8],
    fg: [u8; 3],
) {
    for row in 0..gh {
        let py = gy + row;
        if py < 0 || py as u32 >= fb_h {
            continue;
        }
        for col in 0..gw {
            let pxx = gx + col;
            if pxx < 0 || pxx as u32 >= fb_w {
                continue;
            }
            let a = alpha[(row * gw + col) as usize] as u32;
            if a == 0 {
                continue;
            }
            let i = ((py as u32 * fb_w + pxx as u32) * 4) as usize;
            for ch in 0..3 {
                let bg = px[i + ch] as u32;
                px[i + ch] = ((fg[ch] as u32 * a + bg * (255 - a)) / 255) as u8;
            }
            px[i + 3] = 255;
        }
    }
}

// ---------- Plugin / systems ----------

pub struct EmacsNativePlugin;

impl Plugin for EmacsNativePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EmacsNativeStore>()
            .add_systems(Startup, register_native_kind)
            .add_systems(
                Update,
                (
                    flush_pending_creates,
                    sync_native_resize,
                    sync_emacs_frames,
                    handle_native_keyboard,
                    handle_native_mouse,
                    handle_native_wheel,
                )
                    .chain(),
            )
            // Exclusive (needs &mut World to spawn panes) — runs after
            // the op queues are populated.
            .add_systems(Update, reconcile_frames.after(sync_emacs_frames))
            // Belt-and-suspenders: kill the shared emacs child on a clean
            // AppExit (the Coil read_socket_hook EOF path + the signal
            // handler cover the crash/force-quit paths).
            .add_systems(Last, kill_emacs_on_app_exit);
    }
}

/// On `AppExit`, terminate the shared emacs child so a normal jim quit
/// never leaves it running. Complements `Drop for SharedConn` (which may
/// not run on every teardown ordering) and the signal handler.
fn kill_emacs_on_app_exit(
    mut exit: MessageReader<AppExit>,
    mut store: ResMut<EmacsNativeStore>,
) {
    if exit.read().next().is_none() {
        return;
    }
    if let Some(conn) = store.shared.as_mut() {
        conn.kill_child();
    }
}

/// When Emacs creates a frame we didn't ask for (a `C-x 3`/`C-x 2`
/// split, rebound to `make-frame`, or a pop-up frame), it shows up as a
/// frame id with ops but no pane. Spawn a jim pane that ADOPTS that
/// frame, placed beside the source pane — so an Emacs split becomes a
/// real, draggable jim pane on the same shared buffer.
fn reconcile_frames(world: &mut World) {
    let orphans: Vec<u32> = {
        let store = world.resource::<EmacsNativeStore>();
        let Some(conn) = store.shared.as_ref() else {
            return;
        };
        let mapped: std::collections::HashSet<u32> =
            store.frame_of_pane.values().copied().collect();
        let mut fo = conn.frame_ops.lock().expect("frame_ops");
        // The reserved initial frame (id 1) is never deleted on close, so it
        // stays alive and keeps repainting after its pane is gone. Nothing
        // drains its ops (only pane-backed frames are drained), so discard
        // them here to avoid unbounded growth. It must NOT be re-adopted —
        // that would make the initial pane respawn on every close.
        if !mapped.contains(&1) {
            fo.remove(&1);
        }
        fo.keys()
            .copied()
            // id 0 is the sentinel; id 1 is the reserved initial frame handled
            // above. Genuine Emacs-initiated splits (which we DO adopt into
            // panes) always get ids >= 2.
            .filter(|id| *id > 1 && !mapped.contains(id))
            .collect()
    };
    if orphans.is_empty() {
        return;
    }

    // The pane the split was issued from (the split's anchor).
    let source = world.resource::<jim_pane::FocusedPane>().0;
    let (base_rect, project) = match source
        .and_then(|f| world.get::<PaneRect>(f).copied().map(|r| (f, r)))
    {
        Some((f, r)) => (r, world.get::<jim_pane::PaneProject>(f).map(|p| p.0)),
        None => (
            PaneRect {
                pos: Vec2::new(80.0, 80.0),
                size: Vec2::new(820.0, 560.0),
                z: 1.0,
            },
            None,
        ),
    };

    for (i, id) in orphans.into_iter().enumerate() {
        // Split direction hint (1=right, 2=below); 0/none → floating.
        let hint = world
            .resource::<EmacsNativeStore>()
            .shared
            .as_ref()
            .and_then(|c| c.split_hints.lock().ok().and_then(|mut h| h.remove(&id)))
            .unwrap_or(0);

        // Spawn the adopting pane somewhere sane; docking repositions it.
        let off = 24.0 * i as f32;
        let rect = PaneRect {
            pos: base_rect.pos + Vec2::new(base_rect.size.x + 20.0 + off, off),
            size: base_rect.size,
            z: base_rect.z + 1.0,
        };
        let cfg = serde_json::json!({ "adopt_frame_id": id });
        let Some(new_pane) =
            jim_pane::spawn_pane_from_registry(world, PANE_KIND, "emacs", rect, project, &cfg)
        else {
            continue;
        };

        // Dock it onto the source pane's edge → a real tiled split.
        if let (Some(src), Some(edge)) = (
            source,
            match hint {
                1 => Some(jim_pane::dock::DropEdge::Right),
                2 => Some(jim_pane::dock::DropEdge::Bottom),
                _ => None,
            },
        ) {
            jim_pane::dock::dock_split(world, src, new_pane, edge);
        }
    }
}

/// Deliver queued create-frame commands once the Emacs socket writer is
/// up. `send_create_frame` returns false until Emacs connects, so we
/// keep any id that didn't go through and retry next frame.
fn flush_pending_creates(mut store: ResMut<EmacsNativeStore>) {
    if store.pending_create.is_empty() {
        return;
    }
    let store = &mut *store;
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    store.pending_create.retain(|&id| !conn.send_create_frame(id));
}

/// Keep each Emacs frame sized to its pane's content area. Sends a
/// resize whenever the content pixel size changes (the initial fit once
/// emacs connects, and every drag-resize after). Content size in
/// logical px == Emacs frame px (the sprite renders 1:1 logical).
fn sync_native_resize(
    store: Res<EmacsNativeStore>,
    panes: Query<(Entity, &PaneRect, &PaneKindMarker, Option<&jim_pane::PaneChromeOverride>)>,
    mut last: Local<std::collections::HashMap<Entity, (i32, i32)>>,
) {
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    for (entity, rect, kind, chrome_ov) in &panes {
        if kind.0 != PANE_KIND {
            continue;
        }
        let Some(&fid) = store.frame_of_pane.get(&entity) else {
            continue;
        };
        // Docked panes have a slim header — size the frame to the reclaimed
        // content area so emacs fills the cell below it.
        let title_h = jim_pane::override_title_h(chrome_ov);
        let cw = (rect.size.x - 2.0 * MARGIN).max(32.0) as i32;
        let ch = (rect.size.y - title_h - 2.0 * MARGIN).max(32.0) as i32;
        if last.get(&entity) == Some(&(cw, ch)) {
            continue;
        }
        // Retry until emacs is connected (also delivers the initial fit).
        if conn.send_resize(fid, cw, ch) {
            last.insert(entity, (cw, ch));
        }
    }
}

/// Mouse wheel over a native pane → Emacs WHEEL_EVENTs (which
/// mouse-wheel-mode turns into scrolls). Trackpad pixel-deltas are
/// accumulated into whole "notches" so scrolling isn't absurdly fast.
fn handle_native_wheel(
    mut wheel: MessageReader<bevy::input::mouse::MouseWheel>,
    windows: Query<&Window>,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<EmacsNativeStore>,
    panes: Query<(Entity, &PaneRect, &PaneKindMarker, Option<&jim_pane::PaneChromeOverride>)>,
    mut accum: Local<f32>,
) {
    use bevy::input::mouse::MouseScrollUnit;
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    let mut notches = 0.0f32;
    for ev in wheel.read() {
        notches += match ev.unit {
            MouseScrollUnit::Line => ev.y,
            MouseScrollUnit::Pixel => ev.y / 40.0,
        };
    }
    if notches == 0.0 {
        return;
    }
    *accum += notches;
    let steps = accum.trunc() as i32;
    if steps == 0 {
        return;
    }
    *accum -= steps as f32;

    // Route to the native pane under the cursor.
    let Ok(win) = windows.single() else { return };
    let Some(cur) = win.cursor_position() else { return };
    let canvas = viewport.window_to_canvas(cur);
    let visible: Vec<(Entity, PaneRect)> = panes
        .iter()
        .filter(|(_, _, k, _)| k.0 == PANE_KIND)
        .map(|(e, r, _, _)| (e, r.clone()))
        .collect();
    let Some(pane) = jim_pane::topmost_pane_at(canvas, &visible) else {
        return;
    };
    let Ok((rect, ov)) = panes.get(pane).map(|(_, r, _, ov)| (r, ov)) else { return };
    let Some(&fid) = store.frame_of_pane.get(&pane) else {
        return;
    };
    let local = jim_pane::pt_to_content_local_th(canvas, rect, jim_pane::override_title_h(ov));
    let up = steps > 0;
    for _ in 0..steps.abs() {
        conn.send_wheel(fid, up, local.x as i32, local.y as i32);
    }
}

/// Left-click on a native pane → a mouse press+release pair at the
/// content-local pixel (which equals the Emacs frame pixel, since the
/// sprite renders at logical = frame size). Emacs pairs them into a
/// `mouse-1` click that sets point.
fn handle_native_mouse(
    mut presses: MessageReader<jim_pane::PaneContentPressed>,
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window>,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<EmacsNativeStore>,
    rects: Query<(&PaneRect, Option<&jim_pane::PaneChromeOverride>)>,
    kinds: Query<&PaneKindMarker>,
    mut pressed: Local<Option<(Entity, i32, i32)>>,
) {
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    let modbits = (ctrl as u8) | ((alt as u8) << 1) | ((shift as u8) << 2);

    let Some(conn) = store.shared.as_ref() else {
        return;
    };

    for ev in presses.read() {
        if !matches!(kinds.get(ev.pane), Ok(k) if k.0 == PANE_KIND) {
            continue;
        }
        let x = ev.local_pt.x as i32;
        let y = ev.local_pt.y as i32;
        if let Some(&fid) = store.frame_of_pane.get(&ev.pane) {
            conn.send_mouse(fid, 0, true, x, y, modbits);
            *pressed = Some((ev.pane, x, y));
        }
    }

    // While the button is held after pressing a native pane, stream
    // motion at the current content-local pixel (drives divider drag
    // and region selection in Emacs).
    if let Some((pane, lx, ly)) = *pressed {
        if buttons.pressed(MouseButton::Left) {
            if let (Ok(win), Ok((rect, ov)), Some(&fid)) =
                (windows.single(), rects.get(pane), store.frame_of_pane.get(&pane))
            {
                if let Some(cur) = win.cursor_position() {
                    let canvas = viewport.window_to_canvas(cur);
                    let local =
                        jim_pane::pt_to_content_local_th(canvas, rect, jim_pane::override_title_h(ov));
                    let (x, y) = (local.x as i32, local.y as i32);
                    if (x, y) != (lx, ly) {
                        conn.send_motion(fid, x, y);
                        *pressed = Some((pane, x, y));
                    }
                }
            }
        }
    }

    if buttons.just_released(MouseButton::Left) {
        if let Some((pane, x, y)) = *pressed {
            if let Some(&fid) = store.frame_of_pane.get(&pane) {
                conn.send_mouse(fid, 0, false, x, y, modbits);
            }
            *pressed = None;
        }
    }
}

/// Send keystrokes to the focused native-emacs pane over its socket.
/// Encodes each key as (codepoint, modifier-bits); Emacs's Coil
/// read_socket_hook turns them into input events.
fn handle_native_keyboard(
    mut events: MessageReader<bevy::input::keyboard::KeyboardInput>,
    mods: Res<ButtonInput<KeyCode>>,
    focused: Res<jim_pane::FocusedPane>,
    owner: Res<jim_pane::KeyboardOwner>,
    store: Res<EmacsNativeStore>,
    kinds: Query<&PaneKindMarker>,
) {
    let buffered: Vec<bevy::input::keyboard::KeyboardInput> = events.read().cloned().collect();

    // Only when the focused pane is a native-emacs pane and nothing
    // modal owns the keyboard.
    let Some(target) = focused.0 else { return };
    if !matches!(kinds.get(target), Ok(k) if k.0 == PANE_KIND) {
        return;
    }
    if !owner.allows_pane(target) {
        return;
    }
    let (Some(conn), Some(&fid)) = (store.shared.as_ref(), store.frame_of_pane.get(&target))
    else {
        return;
    };

    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);
    let alt = mods.pressed(KeyCode::AltLeft) || mods.pressed(KeyCode::AltRight);
    let cmd = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);
    if cmd {
        // Mac clipboard muscle memory: translate Cmd+C/X/V into the
        // equivalent Emacs kill-ring chords so the pbcopy/pbpaste bridge
        // (interprogram-cut/paste-function in jim-win.el) round-trips them
        // to the macOS pasteboard. modbits layout: ctrl=1, alt(meta)=2.
        //   Cmd+C -> M-w (kill-ring-save)  Cmd+X -> C-w (kill-region)
        //   Cmd+V -> C-y (yank)
        // Everything else under Cmd is still jim's and is dropped below.
        for ev in &buffered {
            if !ev.state.is_pressed() {
                continue;
            }
            let chord: Option<(u32, u8)> = match ev.key_code {
                KeyCode::KeyC => Some(('w' as u32, 0b010)), // M-w
                KeyCode::KeyX => Some(('w' as u32, 0b001)), // C-w
                KeyCode::KeyV => Some(('y' as u32, 0b001)), // C-y
                _ => None,
            };
            if let Some((code, m)) = chord {
                conn.send_key(fid, code, m);
            }
        }
        return; // Cmd is jim's; don't forward.
    }

    let modbits = (ctrl as u8) | ((alt as u8) << 1) | ((shift as u8) << 2);

    for ev in &buffered {
        if !ev.state.is_pressed() {
            continue;
        }
        // Function/navigation keys → X keysyms (NON_ASCII_KEYSTROKE).
        let fkey: Option<u32> = match ev.key_code {
            KeyCode::ArrowLeft => Some(0xff51),
            KeyCode::ArrowUp => Some(0xff52),
            KeyCode::ArrowRight => Some(0xff53),
            KeyCode::ArrowDown => Some(0xff54),
            KeyCode::Home => Some(0xff50),
            KeyCode::End => Some(0xff57),
            KeyCode::PageUp => Some(0xff55),
            KeyCode::PageDown => Some(0xff56),
            KeyCode::Delete => Some(0xffff), // XK_Delete
            _ => None,
        };
        if let Some(ks) = fkey {
            conn.send_fkey(fid, ks, modbits);
            continue;
        }

        // Named keys that are plain ASCII control chars.
        let named: Option<u32> = match ev.key_code {
            KeyCode::Enter | KeyCode::NumpadEnter => Some(13),
            KeyCode::Tab => Some(9),
            KeyCode::Backspace => Some(127),
            KeyCode::Escape => Some(27),
            KeyCode::Space => Some(32),
            _ => None,
        };
        if let Some(code) = named {
            // For a plain space, drop shift so it doesn't read as S-SPC.
            let m = if code == 32 { modbits & !0b100 } else { modbits };
            conn.send_key(fid, code, m);
            continue;
        }

        // Ctrl or Meta chord: send the BASE character + modifier bits so
        // Emacs canonicalises (C-a, M-x). macOS composes Option+key into
        // accented glyphs, so we re-derive the base char from the
        // physical key rather than trusting the composed logical key.
        if ctrl || alt {
            if let Some(ch) = crate::base_char(ev.key_code, shift) {
                // Keep the base lowercase for chords (C-a not C-A) unless
                // shift is explicitly held.
                conn.send_key(fid, ch as u32, modbits);
            }
            continue;
        }

        // Plain printable text: send the composed character as-is.
        if let Key::Character(s) = &ev.logical_key {
            if let Some(ch) = s.chars().next() {
                conn.send_key(fid, ch as u32, 0);
            }
        }
    }
}

fn register_native_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(jim_pane::PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Emacs (native)",
        radial_icon: Some("E"),
        default_size: Vec2::new(820.0, 560.0),
        spawn: native_spawn_from_config,
        snapshot: native_snapshot,
        on_close: Some(native_on_close),
    });
}

fn native_spawn_from_config(world: &mut World, entity: Entity, content_root: Entity, config: &Value) {
    let session_id = config
        .get("session_id")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        });
    // Set by reconcile_frames when Emacs itself created the frame (a
    // split/pop-up); the pane adopts that id instead of allocating one.
    let adopt = config.get("adopt_frame_id").and_then(|v| v.as_u64()).map(|v| v as u32);
    populate_native_pane(world, entity, content_root, session_id, adopt);
}

fn native_snapshot(world: &World, entity: Entity) -> Value {
    let sid = world
        .get::<jim_terminal::TerminalSession>(entity)
        .map(|s| s.0)
        .unwrap_or(0);
    serde_json::json!({ "session_id": sid })
}

fn native_on_close(world: &mut World, entity: Entity) {
    if let Some(mut store) = world.get_resource_mut::<EmacsNativeStore>() {
        if let Some(fid) = store.frame_of_pane.remove(&entity) {
            // Delete the frame (id 1 is the initial frame — deleting the
            // sole frame would kill Emacs, so keep it; the pane just
            // detaches). The shared Emacs stays alive so buffers persist.
            if fid != 1 {
                if let Some(conn) = store.shared.as_ref() {
                    conn.send_delete_frame(fid);
                }
            }
            if let Some(conn) = store.shared.as_ref() {
                conn.frame_ops.lock().expect("frame_ops").remove(&fid);
            }
        }
    }
}

pub fn populate_native_pane(
    world: &mut World,
    entity: Entity,
    content_root: Entity,
    session_id: u64,
    adopt: Option<u32>,
) {
    // jim theme → the emacs frame palette (per-pane project theme if
    // known, else the global active theme).
    let theme = {
        let global = world.resource::<jim_style::Theme>();
        let proj = world
            .get::<jim_pane::PaneProject>(entity)
            .and_then(|p| world.resource::<jim_style::ProjectThemes>().get(p.0));
        let t = proj.unwrap_or(global);
        EmacsTheme {
            bg: hex(t.color(jim_style::tokens::BG)),
            fg: hex(t.color(jim_style::tokens::FG)),
            cursor: hex(t.color(jim_style::tokens::ACCENT)),
        }
    };
    let bg_bytes = {
        let s = Color::LinearRgba(world.resource::<jim_style::Theme>().color(jim_style::tokens::BG))
            .to_srgba();
        [
            (s.red.clamp(0.0, 1.0) * 255.0) as u8,
            (s.green.clamp(0.0, 1.0) * 255.0) as u8,
            (s.blue.clamp(0.0, 1.0) * 255.0) as u8,
        ]
    };

    // Initial framebuffer — resized on the first frame-size op.
    let fb_w = 64u32;
    let fb_h = 64u32;
    let image = world
        .resource_mut::<Assets<Image>>()
        .add(blank_image_rgb(fb_w, fb_h, bg_bytes));

    let sprite = world
        .spawn((
            ChildOf(content_root),
            Sprite {
                image: image.clone(),
                custom_size: Some(Vec2::new(
                    fb_w as f32 / FB_SCALE as f32,
                    fb_h as f32 / FB_SCALE as f32,
                )),
                ..default()
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(0.0, 0.0, 0.0),
            Visibility::Inherited,
        ))
        .id();

    // Ensure the shared Emacs is running, then claim a frame id. The
    // very first pane adopts Emacs's initial frame (id 1); later panes
    // ask Emacs to make a new frame.
    let wakeup = world
        .get_resource::<bevy::winit::EventLoopProxyWrapper>()
        .map(|w| bevy::winit::EventLoopProxy::clone(w));
    let mut store = world.resource_mut::<EmacsNativeStore>();
    if store.shared.is_none() {
        match SharedConn::start(theme, wakeup) {
            Ok(conn) => store.shared = Some(conn),
            Err(e) => eprintln!("[emacs-native] failed to start emacs: {e}"),
        }
        store.next_id = 1;
    }
    let frame_id = match adopt {
        // Emacs-initiated frame (a split or pop-up): adopt its id, don't
        // send create-frame (it already exists), keep the counter ahead.
        Some(m) => {
            if store.next_id <= m {
                store.next_id = m + 1;
            }
            store.frame_of_pane.insert(entity, m);
            m
        }
        None => {
            // Stay above any id Emacs auto-allocated for a split.
            let seen = store.frame_of_pane.values().copied().max().unwrap_or(0);
            if store.next_id <= seen {
                store.next_id = seen + 1;
            }
            store.next_id += 1;
            let id = store.next_id - 1; // first pane → 1, then 2, 3, …
            if id != 1 {
                // Delivered by flush_pending_creates once Emacs connects.
                store.pending_create.push(id);
            }
            store.frame_of_pane.insert(entity, id);
            id
        }
    };

    world.entity_mut(entity).insert((
        EmacsFrame {
            frame_id,
            image,
            sprite,
            fb_w,
            fb_h,
            fb: rgba_filled(fb_w, fb_h, bg_bytes),
            bg: bg_bytes,
            last_generation: 0,
            raster: GlyphRaster::new(),
        },
        jim_terminal::TerminalSession(session_id),
    ));
}

/// Vertically shift a framebuffer region by `dy` pixels (Emacs's
/// scroll optimization). Copies rows in the safe order for the overlap
/// so the shift doesn't clobber not-yet-copied source rows.
fn scroll_rect(px: &mut [u8], fb_w: u32, fb_h: u32, x: i32, y: i32, w: i32, h: i32, dy: i32) {
    if w <= 0 || h <= 0 || dy == 0 {
        return;
    }
    let (fbw, fbh) = (fb_w as i32, fb_h as i32);
    let x0 = x.max(0);
    let x1 = (x + w).min(fbw);
    if x1 <= x0 {
        return;
    }
    let row_bytes = ((x1 - x0) * 4) as usize;
    // Shift up (dy<0): copy top→bottom. Shift down: bottom→top.
    let mut rows: Vec<i32> = (0..h).collect();
    if dy > 0 {
        rows.reverse();
    }
    for i in rows {
        let (sy, ty) = (y + i, y + dy + i);
        if sy < 0 || sy >= fbh || ty < 0 || ty >= fbh {
            continue;
        }
        let s = ((sy * fbw + x0) * 4) as usize;
        let d = ((ty * fbw + x0) * 4) as usize;
        px.copy_within(s..s + row_bytes, d);
    }
}

/// An RGBA buffer of (w*h) pixels filled with an opaque color.
fn rgba_filled(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut data = vec![0u8; (w.max(1) * h.max(1) * 4) as usize];
    for px in data.chunks_exact_mut(4) {
        px[0] = rgb[0];
        px[1] = rgb[1];
        px[2] = rgb[2];
        px[3] = 255;
    }
    data
}

fn blank_image_rgb(w: u32, h: u32, rgb: [u8; 3]) -> Image {
    let data = rgba_filled(w, h, rgb);
    let mut img = Image::new(
        Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    img.sampler = bevy::image::ImageSampler::linear();
    img
}

/// Drain each pane's op queue into its framebuffer and re-upload.
fn sync_emacs_frames(
    store: Res<EmacsNativeStore>,
    mut images: ResMut<Assets<Image>>,
    mut frames: Query<(Entity, &mut EmacsFrame, &PaneKindMarker)>,
    mut sprites: Query<&mut Sprite>,
    mut commands: Commands,
) {
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    let cur_gen = conn.generation.load(Ordering::Relaxed);
    for (entity, mut frame, kind) in &mut frames {
        if kind.0 != PANE_KIND {
            continue;
        }
        if cur_gen == frame.last_generation {
            continue;
        }
        frame.last_generation = cur_gen;

        // Take this frame's pending ops from the shared queue.
        let ops: Vec<Op> = {
            let mut guard = conn.frame_ops.lock().expect("frame_ops lock");
            guard.get_mut(&frame.frame_id).map(std::mem::take).unwrap_or_default()
        };
        if ops.is_empty() {
            continue;
        }
        let _ = entity;

        // Handle a resize first if present (rebuild the image + sprite).
        let mut new_dims: Option<(u32, u32)> = None;
        for op in &ops {
            if let Op::FrameSize { w, h } = op {
                let nw = (*w as i64 * FB_SCALE).max(1) as u32;
                let nh = (*h as i64 * FB_SCALE).max(1) as u32;
                if nw != frame.fb_w || nh != frame.fb_h {
                    new_dims = Some((nw, nh));
                }
            }
        }
        if let Some((nw, nh)) = new_dims {
            frame.fb_w = nw;
            frame.fb_h = nh;
            frame.fb = rgba_filled(nw, nh, frame.bg);
            if let Some(mut img) = images.get_mut(&frame.image) {
                *img = blank_image_rgb(nw, nh, frame.bg);
            }
            // Resize the content sprite to the logical frame size.
            if let Ok(mut sprite) = sprites.get_mut(frame.sprite) {
                sprite.custom_size = Some(Vec2::new(
                    nw as f32 / FB_SCALE as f32,
                    nh as f32 / FB_SCALE as f32,
                ));
            }
        }

        let (fb_w, fb_h) = (frame.fb_w, frame.fb_h);
        let image = frame.image.clone();
        // Split-borrow the working buffer and the rasterizer (both need
        // &mut at once). Draw into `fb`, not the GPU image.
        let EmacsFrame { fb, raster, .. } = &mut *frame;
        let px = fb.as_mut_slice();
        let mut present = false;
        let mut new_title: Option<String> = None;

        for op in ops {
            match op {
                Op::Flush => present = true,
                Op::Title { text } => new_title = Some(text),
                Op::Font { path, px: fpx, .. } => raster.set_font(&path, fpx),
                Op::FrameSize { .. } => {}
                Op::ClearFrame { bg } => {
                    fill_rect(px, fb_w, fb_h, 0, 0, fb_w as i32, fb_h as i32, bg)
                }
                Op::ClearArea { x, y, w, h, bg } => fill_rect(
                    px,
                    fb_w,
                    fb_h,
                    x * FB_SCALE as i32,
                    y * FB_SCALE as i32,
                    w * FB_SCALE as i32,
                    h * FB_SCALE as i32,
                    bg,
                ),
                Op::Run {
                    x, y, w, h, asc, fg, bg, glyphs,
                } => {
                    // Background box for the run first (Emacs's own run
                    // height, so the block cursor fills the whole cell).
                    fill_rect(
                        px,
                        fb_w,
                        fb_h,
                        x * FB_SCALE as i32,
                        y * FB_SCALE as i32,
                        w * FB_SCALE as i32,
                        h * FB_SCALE as i32,
                        bg,
                    );
                    let fgc = unpack(fg);
                    let baseline = (y + asc) * FB_SCALE as i32;
                    let advance = if glyphs.is_empty() {
                        0
                    } else {
                        (w * FB_SCALE as i32) / glyphs.len() as i32
                    };
                    let x0 = x * FB_SCALE as i32;
                    for (i, gid) in glyphs.iter().enumerate() {
                        let pen_x = x0 + advance * i as i32;
                        if let Some(g) = raster.glyph(*gid) {
                            let gx = pen_x + g.left;
                            let gy = baseline - g.top;
                            blend_glyph(px, fb_w, fb_h, gx, gy, g.w, g.h, &g.alpha, fgc);
                        }
                    }
                }
                Op::Scroll { x, y, w, h, dy } => scroll_rect(
                    px,
                    fb_w,
                    fb_h,
                    x * FB_SCALE as i32,
                    y * FB_SCALE as i32,
                    w * FB_SCALE as i32,
                    h * FB_SCALE as i32,
                    dy * FB_SCALE as i32,
                ),
                Op::Cursor { .. } => {} // cursor is now an inverted glyph run
            }
        }

        // Present the completed frame atomically on flush.
        if present {
            if let Some(mut img) = images.get_mut(&image) {
                if let Some(data) = img.data.as_mut() {
                    if data.len() == fb.len() {
                        data.copy_from_slice(fb);
                    }
                }
            }
        }

        // Live buffer identity in the pane title bar.
        if let Some(title) = new_title {
            commands
                .entity(entity)
                .insert(jim_pane::PaneTitle(title));
        }
    }
    let _ = (MARGIN, TITLE_H);
}

