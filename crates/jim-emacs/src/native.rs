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

use std::collections::{HashMap, VecDeque};
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

use jim_pane::{MARGIN, PaneKindMarker, PaneRect, PaneRegistry, TITLE_H};
use serde_json::Value;

use swash::FontRef;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::Format;

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
    FrameSize {
        w: i32,
        h: i32,
    },
    ClearFrame {
        bg: u32,
    },
    ClearArea {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        bg: u32,
    },
    Run {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        asc: i32,
        /// Which announced font the glyph ids belong to (see `Op::Font`).
        font: u32,
        fg: u32,
        bg: u32,
        /// The rect xdisp.c says this run must be clipped to. Emacs draws
        /// a partially-visible last row whenever a window's text area is
        /// not a whole number of lines, and expects the window system to
        /// clip it — an X port sets it on the GC. Nothing here clips, so
        /// without honouring this the half-row is painted at full height
        /// and lands on top of the mode line.
        clip: Rect2,
        glyphs: Vec<u16>,
    },
    Cursor {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        kind: i32,
    },
    Font {
        /// Backend-assigned font id; runs reference it by this. Ids are
        /// 1-based, 0 means "unknown" (fall back to the frame default).
        id: u32,
        /// Emacs's numeric weight/slant (regular 80, bold/italic 200).
        /// One `path` can hold several faces — Menlo.ttc has regular,
        /// bold, italic and bold-italic — so these pick which one.
        weight: i32,
        slant: i32,
        path: String,
        px: i32,
        asc: i32,
        desc: i32,
    },
    /// Shift a framebuffer region vertically by `dy` (Emacs's scroll
    /// optimization): copy (x,y,w,h) to (x, y+dy, w, h).
    Scroll {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        dy: i32,
    },
    /// The frame's title (buffer name + mode) for the pane title bar.
    Title {
        text: String,
    },
    Flush,
}

/// Emacs's numeric style values for a plain face (font.c's
/// `font_style_table`): regular weight is 80, roman slant 100; bold and
/// italic are both 200. Anything at or above this threshold counts.
const NORMAL_WEIGHT: i32 = 80;
const NORMAL_SLANT: i32 = 100;
const EMPHASIS_THRESHOLD: i32 = 150;

/// An integer rect in Emacs's logical pixels.
#[derive(Clone, Copy, Debug, Default)]
struct Rect2 {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect2 {
    /// Scaled into framebuffer device pixels, or `None` when the rect is
    /// empty — an empty clip means "draw nothing".
    fn scaled(self, scale: i32) -> Option<Self> {
        if self.w <= 0 || self.h <= 0 {
            return None;
        }
        Some(Self {
            x: self.x * scale,
            y: self.y * scale,
            w: self.w * scale,
            h: self.h * scale,
        })
    }
    fn right(self) -> i32 {
        self.x + self.w
    }
    fn bottom(self) -> i32 {
        self.y + self.h
    }
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
        "clear-frame" => Op::ClearFrame {
            bg: kvhex(&fields, "bg"),
        },
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
                font: kv(&fields, "font")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                fg: kvhex(&fields, "fg"),
                bg: kvhex(&fields, "bg"),
                clip: Rect2 {
                    x: kvi(&fields, "cx"),
                    y: kvi(&fields, "cy"),
                    w: kvi(&fields, "cw"),
                    h: kvi(&fields, "ch"),
                },
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
            id: kv(&fields, "id").and_then(|v| v.parse().ok()).unwrap_or(0),
            weight: kv(&fields, "wt")
                .and_then(|v| v.parse().ok())
                .unwrap_or(NORMAL_WEIGHT),
            slant: kv(&fields, "sl")
                .and_then(|v| v.parse().ok())
                .unwrap_or(NORMAL_SLANT),
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
            text: line
                .splitn(3, char::is_whitespace)
                .nth(2)
                .unwrap_or("")
                .to_string(),
        },
        "flush" => Op::Flush,
        _ => return None, // frame-delete: ignored
    };
    Some((fid, op))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameLifecycle {
    New { fid: u32, split: u8 },
    Delete { fid: u32 },
}

fn parse_frame_lifecycle(line: &str) -> Option<FrameLifecycle> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let fid = kv(&fields, "f")?.parse().ok()?;
    if fid == 0 {
        return None;
    }
    match fields.first().copied()? {
        "frame-new" => Some(FrameLifecycle::New {
            fid,
            split: kv(&fields, "split")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }),
        "frame-delete" => Some(FrameLifecycle::Delete { fid }),
        _ => None,
    }
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
static PREV_SIG_HANDLERS: [AtomicUsize; 3] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

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
    /// Frame ids Emacs deleted on its own. Jim-originated deletes are
    /// harmless here because their pane mapping is removed first.
    deleted_frames: Arc<Mutex<Vec<u32>>>,
    generation: Arc<AtomicU64>,
    child: std::process::Child,
    sock_path: PathBuf,
    /// Control channel: jim → emacs newline-delimited commands (e.g.
    /// `open <fid> <path>`). jim is the server; emacs connects as a client
    /// (see `jim--ctl` in jim-win.el) and the accepted stream lands here.
    ctl_writer: Arc<Mutex<Option<UnixStream>>>,
    /// …and the same socket the other way: `state <fid> <json>` lines
    /// Emacs pushes whenever the selected window's buffer, point, or
    /// scroll extent changes. Drained by `drain_emacs_events`.
    ctl_inbox: Arc<Mutex<Vec<String>>>,
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
    /// Set the default font size in PIXELS (jim's `font_size` token),
    /// which is what Emacs text has to match to look like part of the
    /// app. Points would be a third too big — the port reports 96dpi.
    fn send_font_pixels(&self, px: i32) -> bool {
        self.send_ctl(&format!("font-px {px}\n"))
    }
    /// Hand jim's whole design-token palette to Emacs (see
    /// [`EmacsPalette`] and `jim--apply-theme`).
    fn send_theme(&self, palette: &EmacsPalette) -> bool {
        self.send_ctl(&format!("theme {}\n", palette.to_json()))
    }
    /// Scroll by `dy` PIXELS at frame pixel (x, y). Positive `dy` moves
    /// toward the beginning of the buffer (a two-finger-down gesture).
    /// Emacs runs this through `pixel-scroll-precision-scroll-*`, i.e.
    /// real sub-line vscroll rather than the 2-line notches
    /// `mouse-wheel-mode` produces from a WHEEL_EVENT.
    fn send_scroll(&self, fid: u32, x: i32, y: i32, dy: i32) -> bool {
        self.send_ctl(&format!("scroll {fid} {x} {y} {dy}\n"))
    }
    /// Multi-click selection. The 24-byte input record carries no
    /// timestamp, so `make_lispy_event` can never pair clicks into a
    /// double-click itself (keyboard.c wants a non-zero
    /// `button_down_time`); jim counts the clicks and asks for the
    /// word/line selection here instead.
    fn send_click(&self, fid: u32, x: i32, y: i32, count: u32) -> bool {
        self.send_ctl(&format!("click {fid} {x} {y} {count}\n"))
    }
    /// Run an interactive command in `fid`'s selected window.
    fn send_cmd(&self, fid: u32, command: &str) -> bool {
        if command.contains('\n') {
            return false;
        }
        self.send_ctl(&format!("cmd {fid} {command}\n"))
    }
    /// Make `fid` Emacs's selected frame — jim owns focus, so Emacs's
    /// notion of it has to follow.
    fn send_focus(&self, fid: u32) -> bool {
        self.send_ctl(&format!("focus {fid}\n"))
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
        r[20..24].copy_from_slice(&event_time_ms().to_le_bytes());
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

/// The framebuffer contents a bar caret is sitting on top of.
struct CaretUnder {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    pixels: Vec<u8>,
}

/// Put back what the previous caret covered — but ONLY if those pixels
/// are still the caret's own colour.
///
/// A bar caret, unlike a block, is a rect drawn *over* the glyph rather
/// than being the glyph, so nothing erases it as a side effect.
/// `display_and_set_cursor` erases by calling `erase_phys_cursor`
/// directly (it never goes through the port's `draw_window_cursor`
/// hook), and that erases only by redrawing the character underneath —
/// with several paths that skip even that (`goto mark_cursor_off`:
/// hpos past the end of the row, zero visible height, a cursor in the
/// fringe). Those are the ghost carets left behind at a line's edge.
///
/// The colour test is what makes this safe. If Emacs DID repaint that
/// area, the pixels are no longer caret-coloured and we leave them
/// alone — restoring stale pixels would be its own bug, e.g. dragging
/// an `hl-line` highlight along behind the caret. If they are still
/// caret-coloured, nothing repainted them and the save-under is exactly
/// what belongs there.
fn restore_caret(px: &mut [u8], fb_w: u32, fb_h: u32, under: &CaretUnder, caret: [u8; 3]) {
    let row_bytes = (under.w.max(0) as usize) * 4;
    if row_bytes == 0 {
        return;
    }
    let still_caret = (0..under.h).all(|row| {
        let py = under.y + row;
        if py < 0 || py as u32 >= fb_h {
            return true;
        }
        (0..under.w).all(|col| {
            let pxx = under.x + col;
            if pxx < 0 || pxx as u32 >= fb_w {
                return true;
            }
            let i = ((py as u32 * fb_w + pxx as u32) * 4) as usize;
            px[i] == caret[0] && px[i + 1] == caret[1] && px[i + 2] == caret[2]
        })
    });
    if !still_caret {
        return;
    }
    for row in 0..under.h {
        let py = under.y + row;
        if py < 0 || py as u32 >= fb_h {
            continue;
        }
        let src = (row as usize) * row_bytes;
        for col in 0..under.w {
            let pxx = under.x + col;
            if pxx < 0 || pxx as u32 >= fb_w {
                continue;
            }
            let d = ((py as u32 * fb_w + pxx as u32) * 4) as usize;
            let sidx = src + (col as usize) * 4;
            px[d..d + 4].copy_from_slice(&under.pixels[sidx..sidx + 4]);
        }
    }
}

/// Snapshot the framebuffer under a rect, for `restore_caret`.
fn save_under(px: &[u8], fb_w: u32, fb_h: u32, x: i32, y: i32, w: i32, h: i32) -> CaretUnder {
    let mut pixels = vec![0u8; (w.max(0) as usize) * (h.max(0) as usize) * 4];
    for row in 0..h {
        let py = y + row;
        for col in 0..w {
            let pxx = x + col;
            let d = ((row as usize) * (w as usize) + col as usize) * 4;
            if py < 0 || py as u32 >= fb_h || pxx < 0 || pxx as u32 >= fb_w {
                continue;
            }
            let sidx = ((py as u32 * fb_w + pxx as u32) * 4) as usize;
            pixels[d..d + 4].copy_from_slice(&px[sidx..sidx + 4]);
        }
    }
    CaretUnder { x, y, w, h, pixels }
}

/// Milliseconds since jim started, for the input record's timestamp.
///
/// Emacs compares event timestamps against `double-click-time` to pair
/// clicks, and `make_lispy_event` refuses to promote anything while
/// `button_down_time` is still zero — so a record with no timestamp can
/// never produce a double-click. Wrapping at u32 (~49 days) at worst
/// costs one missed pairing, which is what X11 does too.
fn event_time_ms() -> u32 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u32
}

/// jim's whole design-token palette, flattened to `name -> "#rrggbb"`.
///
/// Every *color* token the active [`jim_style::Theme`] knows about is in
/// here, not just bg/fg/cursor: `jim-integration.el` maps them onto the
/// full Emacs face set (syntax, mode line, region, dividers, …), so an
/// Emacs pane and a jim editor pane colour the same code the same way.
/// Tokens with alpha are composited over `bg` first — Emacs faces are
/// opaque.
#[derive(Clone, Default, PartialEq)]
pub struct EmacsPalette {
    entries: Vec<(String, String)>,
}

impl EmacsPalette {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn color_or(&self, name: &str, fallback: &str) -> String {
        self.get(name).unwrap_or(fallback).to_string()
    }

    /// One JSON object line for the `theme` control command.
    fn to_json(&self) -> String {
        let map: serde_json::Map<String, Value> = self
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        Value::Object(map).to_string()
    }

    /// Flatten a theme into the palette. `bg` is resolved first so the
    /// alpha compositing of every other token has a backdrop.
    pub fn from_theme(theme: &jim_style::Theme) -> Self {
        let bg = theme.color(jim_style::tokens::BG);
        let mut entries: Vec<(String, String)> = Vec::new();
        let mut names = theme.token_names();
        names.sort();
        for name in names {
            if let Some(jim_style::TokenValue::Color(c)) = theme.get_by_name(&name) {
                entries.push((name, hex_over(c, bg)));
            }
        }
        Self { entries }
    }
}

fn srgba(c: bevy::color::LinearRgba) -> bevy::color::Srgba {
    Color::LinearRgba(c).to_srgba()
}

fn hex(c: bevy::color::LinearRgba) -> String {
    let s = srgba(c);
    format!(
        "#{:02x}{:02x}{:02x}",
        (s.red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (s.green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (s.blue.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// `c` composited over `backdrop` (Emacs has no translucent faces).
fn hex_over(c: bevy::color::LinearRgba, backdrop: bevy::color::LinearRgba) -> String {
    let a = c.alpha.clamp(0.0, 1.0);
    if a >= 0.999 {
        return hex(c);
    }
    hex(bevy::color::LinearRgba::new(
        c.red * a + backdrop.red * (1.0 - a),
        c.green * a + backdrop.green * (1.0 - a),
        c.blue * a + backdrop.blue * (1.0 - a),
        1.0,
    ))
}

fn rgb_bytes(c: bevy::color::LinearRgba) -> [u8; 3] {
    let s = srgba(c);
    [
        (s.red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (s.green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (s.blue.clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

impl SharedConn {
    fn start(
        palette: EmacsPalette,
        font_px: i32,
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
        let bg = palette.color_or("bg", "#101418");
        let fg = palette.color_or("fg", "#e6e6e6");
        let cursor = palette.color_or("caret", &palette.color_or("accent", &fg));
        let divider = palette.color_or("chrome_divider", &cursor);
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
                    .args(["-bg", &bg, "-fg", &fg, "-cr", &cursor])
                    // -bg/-fg only theme the INITIAL frame (initial-frame-
                    // alist). Panes 2+ are new frames, so also hand the
                    // palette via env → jim-win.el puts it in
                    // default-frame-alist, which every frame inherits.
                    .env("JIM_BG", &bg)
                    .env("JIM_FG", &fg)
                    .env("JIM_CURSOR", &cursor)
                    .env("JIM_DIVIDER", &divider);
            }
        }
        // jim-win.el sizes the initial frame from JIM_EMACS_FONT_SIZE
        // (points) before our control channel exists, so seed it from the
        // theme rather than letting it default to 15pt — otherwise the
        // first paint is visibly oversized and only settles once the
        // `font-px` command lands. The port reports 96dpi ⇒ pt = px * 3/4.
        if std::env::var_os("JIM_EMACS_FONT_SIZE").is_none() {
            cmd.env(
                "JIM_EMACS_FONT_SIZE",
                ((font_px as f32 * 0.75).round() as i32).max(1).to_string(),
            );
        }

        // The integration layer (theme/scroll/multi-click/state) is
        // ordinary runtime lisp loaded LAST, after the user's init: the
        // fork's own `lisp/term/jim-win.el` is preloaded into the dump,
        // so keeping this out of it means editing it costs a pane
        // restart rather than a re-dump. `-l` files load inside
        // `command-line-1`, i.e. before `window-setup-hook` — which is
        // exactly where it needs to be to hook that.
        match install_integration_el() {
            Ok(path) if std::env::var_os("JIM_EMACS_NO_INTEGRATION").is_none() => {
                cmd.arg("-l").arg(path);
            }
            Ok(_) => {}
            Err(e) => eprintln!("[emacs-native] could not install jim-integration.el: {e}"),
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
        let deleted_frames: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let writer: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let fo_w = frame_ops.clone();
        let sh_w = split_hints.clone();
        let df_w = deleted_frames.clone();
        let gen_w = generation.clone();
        let writer_w = writer.clone();
        let wakeup2 = wakeup.clone();
        let thread = std::thread::Builder::new()
            .name("emacs-native".into())
            .spawn(move || conn_loop(listener, fo_w, sh_w, df_w, gen_w, writer_w, wakeup))
            .expect("spawn emacs-native thread");

        let ctl_writer: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let ctl_inbox: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ctl_writer_w = ctl_writer.clone();
        let ctl_inbox_w = ctl_inbox.clone();
        let ctl_wakeup = wakeup2;
        let ctl_thread = std::thread::Builder::new()
            .name("emacs-native-ctl".into())
            .spawn(move || ctl_accept_loop(ctl_listener, ctl_writer_w, ctl_inbox_w, ctl_wakeup))
            .expect("spawn emacs-native-ctl thread");

        Ok(Self {
            writer,
            frame_ops,
            split_hints,
            deleted_frames,
            generation,
            child,
            sock_path,
            ctl_writer,
            ctl_inbox,
            ctl_sock_path,
            _thread: thread,
            _ctl_thread: ctl_thread,
        })
    }
}

impl SharedConn {
    /// Whether the Emacs child is still running.
    ///
    /// One `waitpid(WNOHANG)`; also reaps, so the dead child does not sit
    /// as a zombie. `store.shared.is_some()` used to be the only liveness
    /// test anywhere, and it stays true forever once set — see
    /// `respawn_emacs_if_dead` for what that cost.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for SharedConn {
    fn drop(&mut self) {
        self.kill_child();
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_file(&self.ctl_sock_path);
    }
}

/// The elisp integration layer, shipped in the binary and materialised
/// next to the rest of jim's per-user state. Rewritten on every launch
/// so a jim upgrade can never leave a stale copy behind.
fn install_integration_el() -> std::io::Result<PathBuf> {
    const SOURCE: &str = include_str!("../elisp/jim-integration.el");
    let dir = jim_pane_data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("emacs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("jim-integration.el");
    std::fs::write(&path, SOURCE)?;
    Ok(path)
}

/// Accept emacs's control-channel connection, stash the stream so
/// `send_open_file` and friends can write to it, and read the other
/// direction — `state <fid> <json>` lines — into `inbox`. Keeps
/// accepting so an emacs restart re-establishes the channel.
fn ctl_accept_loop(
    listener: UnixListener,
    writer: Arc<Mutex<Option<UnixStream>>>,
    inbox: Arc<Mutex<Vec<String>>>,
    wakeup: Option<bevy::winit::EventLoopProxy<bevy::winit::WinitUserEvent>>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { break };
        let Ok(read_half) = stream.try_clone() else {
            continue;
        };
        if let Ok(mut w) = writer.lock() {
            *w = Some(stream);
        }
        // One reader per accepted connection: emacs only ever holds one
        // at a time, and this returns at EOF so a reconnect gets a fresh
        // reader rather than two racing on the inbox.
        let reader = BufReader::new(read_half);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                continue;
            }
            if let Ok(mut q) = inbox.lock() {
                // A stalled main loop must not let this grow without
                // bound; the newest state is the only one that matters.
                if q.len() > 512 {
                    q.drain(..256);
                }
                q.push(line);
            }
            if let Some(p) = wakeup.as_ref() {
                let _ = p.send_event(bevy::winit::WinitUserEvent::WakeUp);
            }
        }
    }
}

/// Accept the emacs connection and route each op to its frame's queue,
/// waking the render loop.
fn conn_loop(
    listener: UnixListener,
    frame_ops: Arc<Mutex<HashMap<u32, Vec<Op>>>>,
    split_hints: Arc<Mutex<HashMap<u32, u8>>>,
    deleted_frames: Arc<Mutex<Vec<u32>>>,
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
        match parse_frame_lifecycle(&line) {
            Some(FrameLifecycle::New { fid, split }) if split != 0 => {
                split_hints.lock().expect("split_hints").insert(fid, split);
            }
            Some(FrameLifecycle::Delete { fid }) => {
                deleted_frames.lock().expect("deleted_frames").push(fid);
                if let Some(p) = wakeup.as_ref() {
                    let _ = p.send_event(bevy::winit::WinitUserEvent::WakeUp);
                }
                continue;
            }
            _ => {}
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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
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

/// What Emacs reports back about one pane's selected window. Fed by
/// `state <fid> <json>` lines on the control channel (see
/// `jim--report-state`); consumed by the scroll indicator, the pane
/// title, and — republished on the `emacs.state` bus topic by jim-app —
/// the file tree widget.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmacsFrameState {
    pub buffer: String,
    /// Absolute path of the visited file, if the buffer visits one.
    pub path: Option<String>,
    pub dir: Option<String>,
    pub modified: bool,
    pub read_only: bool,
    pub mode: String,
    pub line: u32,
    pub column: u32,
    /// Fraction of the buffer above the viewport, and below its bottom
    /// edge — i.e. the scroll thumb's extent, in 0.0..=1.0.
    pub top: f32,
    pub bottom: f32,
}

impl EmacsFrameState {
    fn from_json(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        Self {
            buffer: s("buffer").unwrap_or_default(),
            path: s("path"),
            dir: s("dir"),
            modified: v.get("modified").and_then(|x| x.as_bool()).unwrap_or(false),
            read_only: v.get("readonly").and_then(|x| x.as_bool()).unwrap_or(false),
            mode: s("mode").unwrap_or_default(),
            line: v.get("line").and_then(|x| x.as_u64()).unwrap_or(1) as u32,
            column: v.get("column").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            top: v.get("top").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
            bottom: v.get("bottom").and_then(|x| x.as_f64()).unwrap_or(1.0) as f32,
        }
    }
}

/// Set when jim itself changed the font size — the only thing that
/// changes Emacs's row height. Consumed by `sync_emacs_frames`, which
/// then re-learns each frame's `line_h` from the next run it sees.
///
/// It deliberately does NOT key off the `font` op: those arrive whenever
/// Emacs realizes a face, many of them at the same size. Re-learning on
/// each one meant a batch carrying a font op but no runs left `line_h`
/// at 0, and the next resize pass then sent an UNROUNDED height — which
/// undid the whole-line fit and put a half-drawn row back under the mode
/// line.
#[derive(Default, Resource)]
pub struct EmacsRelearnLineH(bool);

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
    /// Jim-originated split requests waiting for the corresponding
    /// Emacs-created frame to appear on the draw-op stream.
    pending_splits: VecDeque<PendingNativeSplit>,
    /// Latest state reported per pane. See [`EmacsFrameState`].
    state_of_pane: HashMap<Entity, EmacsFrameState>,
    /// Panes whose state changed since the last drain, for jim-app to
    /// republish on the bus.
    pub(crate) state_dirty: Vec<Entity>,
    /// Palette last delivered to Emacs, so a theme.ft hot-reload that
    /// doesn't actually change a colour costs nothing (applying a theme
    /// forces a full `redraw-display`).
    sent_palette: Option<EmacsPalette>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSplitDirection {
    Right,
    Below,
}

impl NativeSplitDirection {
    fn hint(self) -> u8 {
        match self {
            Self::Right => 1,
            Self::Below => 2,
        }
    }

    fn command(self) -> &'static str {
        match self {
            Self::Right => "jim-split-window-right",
            Self::Below => "jim-split-window-below",
        }
    }

    fn edge(self) -> jim_pane::dock::DropEdge {
        match self {
            Self::Right => jim_pane::dock::DropEdge::Right,
            Self::Below => jim_pane::dock::DropEdge::Bottom,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingNativeSplit {
    source: Entity,
    source_fid: u32,
    direction: NativeSplitDirection,
}

fn take_matching_split(
    pending: &mut VecDeque<PendingNativeSplit>,
    frames: &HashMap<Entity, u32>,
    hint: u8,
) -> Option<PendingNativeSplit> {
    pending.retain(|p| frames.get(&p.source) == Some(&p.source_fid));
    let pos = pending.iter().position(|p| p.direction.hint() == hint);
    pos.and_then(|p| pending.remove(p))
}

impl EmacsNativeStore {
    fn request_split(&mut self, source: Entity, direction: NativeSplitDirection) -> bool {
        let (Some(conn), Some(&source_fid)) =
            (self.shared.as_ref(), self.frame_of_pane.get(&source))
        else {
            return false;
        };
        self.pending_splits.push_back(PendingNativeSplit {
            source,
            source_fid,
            direction,
        });
        if conn.send_cmd(source_fid, direction.command()) {
            true
        } else {
            self.pending_splits.pop_back();
            false
        }
    }

    /// Ask the emacs pane `pane` to open `path` via `find-file` in its own
    /// frame. Returns false if there's no live emacs, `pane` isn't a known
    /// emacs frame, or the control channel isn't connected yet.
    pub fn send_open_file(&self, pane: Entity, path: &str) -> bool {
        let (Some(conn), Some(&fid)) = (self.shared.as_ref(), self.frame_of_pane.get(&pane)) else {
            return false;
        };
        conn.send_open_file(fid, path)
    }

    /// Set the emacs default font size (points) on all native panes. Font
    /// is a global face attribute, so no per-pane frame id is needed.
    pub fn send_font(&self, size: i32) -> bool {
        self.shared.as_ref().is_some_and(|c| c.send_font(size))
    }

    /// Run an interactive Emacs command in `pane`'s frame (e.g.
    /// `"save-buffer"`).
    pub fn send_command(&self, pane: Entity, command: &str) -> bool {
        let (Some(conn), Some(&fid)) = (self.shared.as_ref(), self.frame_of_pane.get(&pane)) else {
            return false;
        };
        conn.send_cmd(fid, command)
    }

    /// The last state Emacs reported for `pane`.
    pub fn state(&self, pane: Entity) -> Option<&EmacsFrameState> {
        self.state_of_pane.get(&pane)
    }

    /// True once a shared Emacs process exists.
    pub fn is_running(&self) -> bool {
        self.shared.is_some()
    }

    /// Drain the panes whose state changed since the last call.
    pub fn take_state_changes(&mut self) -> Vec<(Entity, EmacsFrameState)> {
        let dirty = std::mem::take(&mut self.state_dirty);
        dirty
            .into_iter()
            .filter_map(|e| self.state_of_pane.get(&e).map(|s| (e, s.clone())))
            .collect()
    }
}

/// Split `source` into a second native Emacs pane. The new Emacs frame
/// shows the same buffer; when its first draw operations arrive,
/// [`reconcile_frames`] adopts it and docks it beside this exact pane.
pub fn request_native_split(
    world: &mut World,
    source: Entity,
    direction: NativeSplitDirection,
) -> bool {
    if !matches!(world.get::<PaneKindMarker>(source), Some(k) if k.0 == PANE_KIND) {
        return false;
    }
    world
        .resource_mut::<EmacsNativeStore>()
        .request_split(source, direction)
}

/// Rasterizers for every font Emacs has announced, keyed by the id the
/// port gave it.
///
/// This is a RESOURCE, not per-pane state, because the port's font
/// registry is process-wide: each distinct `struct font *` is announced
/// ONCE, as `font id=N …`, and from then on every `run` in every frame
/// just names that id. A per-pane cache only ever holds the fonts that
/// happened to be announced while that pane was being redisplayed, so a
/// pane opened later draws runs naming ids it has never seen, finds no
/// rasterizer, and renders no glyphs at all — a pane that sizes itself
/// correctly, takes ops, counts flushes and shows nothing but its
/// background, mode line and caret.
#[derive(Default, Resource)]
struct EmacsFonts {
    rasters: HashMap<u32, GlyphRaster>,
    /// Id of the most recent `font` op, for runs that carry none (id 0).
    default_font: u32,
    /// Font ids already reported as missing, so the warning does not
    /// repeat every frame.
    warned: std::collections::HashSet<u32>,
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
    /// Whether this Emacs process has supplied real dimensions.  The size
    /// and the first flush are allowed to arrive in separate batches.
    sized: bool,
    /// The bootstrap image is deliberately hidden until Emacs has sent a
    /// real frame size and completed a flush.  Exposing the 64x64 transport
    /// placeholder is the tiny square bug.
    ready: bool,
    /// Working CPU framebuffer (RGBA). Ops draw into this; it's copied
    /// to the GPU `image` only on `flush`, so partial redisplays never
    /// present (no divider/text flicker).
    fb: Vec<u8>,
    /// jim theme background, for the pre-clear framebuffer fill.
    bg: [u8; 3],
    /// jim's `caret` token. The port emits a bar cursor as a bare rect
    /// and leaves the colour to us, so the caret follows the theme live
    /// instead of being baked in at Emacs startup.
    caret: [u8; 3],
    /// Save-under for the bar caret: the last caret's device-pixel rect
    /// and the framebuffer contents it covered. See `restore_caret`.
    caret_under: Option<CaretUnder>,
    last_generation: u64,
    /// Set when `line_h` changed, so the fitted frame size is recomputed
    /// and re-sent even though the pane itself did not move.
    resize_dirty: bool,
    /// Overlay sprite: the macOS-style scroll indicator, shown while the
    /// view moves and faded out once it settles.
    scrollbar: Entity,
    /// Seconds remaining on the indicator's fade.
    scrollbar_fade: f32,
    /// Emacs's line height in logical px (ascent + descent of the frame
    /// font), learned from the `font` op. Used to size the frame to a
    /// WHOLE number of lines — see `sync_native_resize`.
    line_h: i32,
}

/// swash-backed rasterizer: one Emacs font, glyph bitmaps cached by id.
struct GlyphRaster {
    font_bytes: Option<&'static [u8]>,
    /// Which face inside the file. Only ever non-zero for a font
    /// COLLECTION (.ttc), where bold/italic live alongside regular.
    index: usize,
    px: f32,
    ctx: ScaleContext,
    cache: HashMap<u16, Option<CachedGlyph>>,
}

/// The face in `data` whose weight and slant match what Emacs asked for.
///
/// A .ttc holds several faces behind one path — Menlo.ttc has regular,
/// bold, italic and bold-italic — and Emacs only ever tells us the
/// filename. Rasterising face 0 regardless is what made every bold and
/// italic face render as regular. Scores each face on whether its
/// boldness and slant agree with the request and takes the best; falls
/// back to face 0, which is right for the single-face .ttf case.
fn face_index_for(data: &[u8], weight: i32, slant: i32) -> usize {
    let want_bold = weight >= EMPHASIS_THRESHOLD;
    let want_italic = slant >= EMPHASIS_THRESHOLD;
    // A plain face is ALWAYS index 0. Emacs gives us a filename, not a
    // face, and index 0 is the one it means; searching for a "matching"
    // face here can land on a different family in the same collection
    // (a .ttc may hold several), whose glyph ids mean something else
    // entirely — that renders as text with colliding, mis-advanced
    // glyphs. Only an emphasized face is worth searching for.
    if !want_bold && !want_italic {
        return 0;
    }
    let Some(collection) = swash::FontDataRef::new(data) else {
        return 0;
    };
    if collection.len() <= 1 {
        return 0;
    }
    // Stay inside the family index 0 belongs to, for the same reason.
    let family_of = |i: usize| -> Option<String> {
        let font = collection.get(i)?;
        font.localized_strings()
            .find_by_id(swash::StringId::Family, None)
            .map(|s| s.to_string())
    };
    let base_family = family_of(0);
    let mut best = (0usize, -1i32);
    for i in 0..collection.len() {
        let Some(font) = collection.get(i) else {
            continue;
        };
        if base_family.is_some() && family_of(i) != base_family {
            continue;
        }
        let attrs = font.attributes();
        // swash reports the CSS scale: 400 regular, 700 bold.
        let is_bold = attrs.weight().0 >= 600;
        let is_italic = !matches!(attrs.style(), swash::Style::Normal);
        let score = (is_bold == want_bold) as i32 + (is_italic == want_italic) as i32;
        if score > best.1 {
            best = (i, score);
        }
    }
    // Nothing matched both traits: index 0 beats a wrong face.
    if best.1 < 2 { 0 } else { best.0 }
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
            index: 0,
            px: 14.0,
            ctx: ScaleContext::new(),
            cache: HashMap::new(),
        }
    }

    fn set_font(&mut self, path: &str, px: i32, weight: i32, slant: i32) {
        self.px = px.max(1) as f32;
        self.cache.clear();
        self.font_bytes = std::fs::read(path)
            .ok()
            .map(|b| &*Box::leak(b.into_boxed_slice()));
        if self.font_bytes.is_none() && !path.is_empty() {
            eprintln!("[emacs-native] could not read font {path}");
        }
        self.index = self
            .font_bytes
            .map(|b| face_index_for(b, weight, slant))
            .unwrap_or(0);
    }

    fn glyph(&mut self, id: u16) -> Option<&CachedGlyph> {
        if !self.cache.contains_key(&id) {
            let g = self.rasterize(id);
            self.cache.insert(id, g);
        }
        self.cache.get(&id).and_then(|o| o.as_ref())
    }

    fn rasterize(&mut self, id: u16) -> Option<CachedGlyph> {
        let font = FontRef::from_index(self.font_bytes?, self.index)?;
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

/// `fill_rect`, restricted to `clip`. An empty clip paints nothing.
#[allow(clippy::too_many_arguments)]
fn fill_rect_clipped(
    px: &mut [u8],
    fb_w: u32,
    fb_h: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rgb: u32,
    clip: Option<Rect2>,
) {
    let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + w, y + h);
    if let Some(c) = clip {
        x0 = x0.max(c.x);
        y0 = y0.max(c.y);
        x1 = x1.min(c.right());
        y1 = y1.min(c.bottom());
    }
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    fill_rect(px, fb_w, fb_h, x0, y0, x1 - x0, y1 - y0, rgb);
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
    clip: Option<Rect2>,
) {
    for row in 0..gh {
        let py = gy + row;
        if py < 0 || py as u32 >= fb_h {
            continue;
        }
        if let Some(c) = clip
            && (py < c.y || py >= c.bottom())
        {
            continue;
        }
        for col in 0..gw {
            let pxx = gx + col;
            if pxx < 0 || pxx as u32 >= fb_w {
                continue;
            }
            if let Some(c) = clip
                && (pxx < c.x || pxx >= c.right())
            {
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
            .init_resource::<EmacsFonts>()
            .init_resource::<EmacsRelearnLineH>()
            .add_systems(Startup, register_native_kind)
            .add_systems(
                Update,
                (
                    respawn_emacs_if_dead,
                    flush_pending_creates,
                    sync_emacs_theme,
                    sync_emacs_font,
                    sync_emacs_focus,
                    sync_native_resize,
                    sync_emacs_frames,
                    drain_emacs_events,
                    sync_scroll_indicator,
                    handle_native_keyboard,
                    handle_native_mouse,
                    handle_native_wheel,
                )
                    .chain(),
            )
            // Exclusive (needs &mut World to spawn panes) — runs after
            // the op queues are populated.
            .add_systems(
                Update,
                (reconcile_frame_deletes, reconcile_frames)
                    .chain()
                    .after(sync_emacs_frames),
            )
            // Belt-and-suspenders: kill the shared emacs child on a clean
            // AppExit (the Coil read_socket_hook EOF path + the signal
            // handler cover the crash/force-quit paths).
            .add_systems(Last, kill_emacs_on_app_exit);
    }
}

/// On `AppExit`, terminate the shared emacs child so a normal jim quit
/// never leaves it running. Complements `Drop for SharedConn` (which may
/// not run on every teardown ordering) and the signal handler.
fn kill_emacs_on_app_exit(mut exit: MessageReader<AppExit>, mut store: ResMut<EmacsNativeStore>) {
    if exit.read().next().is_none() {
        return;
    }
    if let Some(conn) = store.shared.as_mut() {
        conn.kill_child();
    }
}

/// Close the Jim pane when Emacs itself deletes its backing frame.
/// Jim-originated close events find no mapping here, which prevents a loop.
fn reconcile_frame_deletes(world: &mut World) {
    let deleted = {
        let store = world.resource::<EmacsNativeStore>();
        let Some(conn) = store.shared.as_ref() else {
            return;
        };
        std::mem::take(&mut *conn.deleted_frames.lock().expect("deleted_frames"))
    };
    if deleted.is_empty() {
        return;
    }

    let panes: Vec<Entity> = {
        let store = world.resource::<EmacsNativeStore>();
        deleted
            .into_iter()
            .filter_map(|fid| {
                store
                    .frame_of_pane
                    .iter()
                    .find_map(|(&pane, &mapped)| (mapped == fid).then_some(pane))
            })
            .collect()
    };
    world
        .resource_mut::<jim_pane::PendingPaneActions>()
        .close
        .extend(panes);
}

/// When Emacs creates a frame we didn't ask for (a `C-x 3`/`C-x 2`
/// split, rebound to `make-frame`, or a pop-up frame), it shows up as a
/// frame id with ops but no pane. Spawn a jim pane that ADOPTS that
/// frame, placed beside the source pane — so an Emacs split becomes a
/// real, draggable jim pane on the same shared buffer.
fn reconcile_frames(world: &mut World) {
    let mut orphans: Vec<u32> = {
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
    orphans.sort_unstable();
    if orphans.is_empty() {
        return;
    }

    // Project membership is a hard invariant for panes (jim-app asserts
    // it and panics). Resolve it per orphan because explicit split requests
    // may come from different panes before either new frame is realized.
    for (i, id) in orphans.iter().copied().enumerate() {
        // Split direction hint (1=right, 2=below); 0/none → floating.
        let hint = world
            .resource::<EmacsNativeStore>()
            .shared
            .as_ref()
            .and_then(|c| c.split_hints.lock().ok().and_then(|mut h| h.remove(&id)))
            .unwrap_or(0);

        // A Jim menu/palette split records its exact source before asking
        // Emacs to create the frame. Pair it with the matching directional
        // frame-new event. Keyboard-originated splits have no pending record
        // and retain the focused-pane fallback.
        let pending = {
            let mut store = world.resource_mut::<EmacsNativeStore>();
            let store = &mut *store;
            take_matching_split(&mut store.pending_splits, &store.frame_of_pane, hint)
        };
        let source = pending
            .map(|p| p.source)
            .or_else(|| world.resource::<jim_pane::FocusedPane>().0)
            .filter(|e| world.get_entity(*e).is_ok());
        let base_rect = source
            .and_then(|e| world.get::<PaneRect>(e).copied())
            .unwrap_or(PaneRect {
                pos: Vec2::new(80.0, 80.0),
                size: Vec2::new(820.0, 560.0),
                z: 1.0,
            });
        let project = source
            .and_then(|e| world.get::<jim_pane::PaneProject>(e).map(|p| p.0))
            .or_else(|| {
                let panes: Vec<Entity> = world
                    .resource::<EmacsNativeStore>()
                    .frame_of_pane
                    .keys()
                    .copied()
                    .collect();
                panes
                    .into_iter()
                    .find_map(|e| world.get::<jim_pane::PaneProject>(e).map(|p| p.0))
            })
            .or_else(|| {
                let mut q =
                    world.query_filtered::<&jim_pane::PaneProject, With<jim_pane::PaneTag>>();
                q.iter(world).next().map(|p| p.0)
            });
        let Some(project) = project else {
            let store = world.resource::<EmacsNativeStore>();
            if let Some(conn) = store.shared.as_ref()
                && let Ok(mut fo) = conn.frame_ops.lock()
            {
                fo.remove(&id);
            }
            eprintln!(
                "[emacs-native] emacs made frame {id} with no pane to inherit a project from; \
                 not adopting it"
            );
            continue;
        };

        // Spawn the adopting pane somewhere sane; docking repositions it.
        let off = 24.0 * i as f32;
        let rect = PaneRect {
            pos: base_rect.pos + Vec2::new(base_rect.size.x + 20.0 + off, off),
            size: base_rect.size,
            z: base_rect.z + 1.0,
        };
        let cfg = serde_json::json!({ "adopt_frame_id": id });
        let Some(new_pane) = jim_pane::spawn_pane_from_registry(
            world,
            PANE_KIND,
            "emacs",
            rect,
            Some(project),
            &cfg,
        ) else {
            continue;
        };

        // Dock it onto the source pane's edge → a real tiled split.
        let edge = pending.map(|p| p.direction.edge()).or_else(|| match hint {
            1 => Some(jim_pane::dock::DropEdge::Right),
            2 => Some(jim_pane::dock::DropEdge::Bottom),
            _ => None,
        });
        if let (Some(src), Some(edge)) = (source, edge) {
            jim_pane::dock::dock_split(world, src, new_pane, edge);
            // A split is a navigation action: the newly-created editor is
            // where subsequent typing and Emacs minibuffer commands belong.
            world.resource_mut::<jim_pane::FocusedPane>().0 = Some(new_pane);
        }
    }
}

/// Deliver queued create-frame commands once the Emacs socket writer is
/// up. `send_create_frame` returns false until Emacs connects, so we
/// keep any id that didn't go through and retry next frame.
/// Whether to trace frame creation, sizing and framebuffer growth.
///
/// Set by `JIM_EMACS_DEBUG` or by touching `~/.jim/emacs-debug` — the file
/// matters because `dev-restart.sh` launches the GUI under `env -i`, so an
/// exported variable never reaches it. Read once; these sites are per-frame.
fn emacs_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("JIM_EMACS_DEBUG").is_some()
            || jim_pane_data_dir().is_some_and(|d| d.join("emacs-debug").exists())
    })
}

/// Notice that the shared Emacs has died, and put every native pane back
/// onto a fresh one.
///
/// One Emacs serves every native pane, so losing it loses them all at
/// once — and until this existed, nothing noticed. `store.shared` is set
/// on the first pane and never cleared, and `populate_native_pane` gates
/// the spawn on `shared.is_none()`, so after a crash every pane stayed
/// wired to a socket with nobody on the other end: commands silently
/// dropped, `is_running()` still answering yes, and a newly opened pane
/// sitting at the 64x64 framebuffer it spawns with, because the frame-size
/// op that grows it never arrives. That is the "tiny square", and it
/// persisted until jim itself was restarted.
///
/// Recovery is a full reset rather than a reconnect: the new process has
/// none of the old frames, so every pane claims a new id and re-fits.
/// Buffers die with the process — that is the cost of the crash, not of
/// the recovery.
fn respawn_emacs_if_dead(
    mut store: ResMut<EmacsNativeStore>,
    mut fonts: ResMut<EmacsFonts>,
    theme: Res<jim_style::Theme>,
    wakeup: Option<Res<bevy::winit::EventLoopProxyWrapper>>,
    mut panes: Query<(Entity, &PaneKindMarker, &mut EmacsFrame)>,
    mut visibility: Query<&mut Visibility>,
) {
    if !store.shared.as_mut().is_some_and(|c| !c.is_alive()) {
        return;
    }
    eprintln!("[emacs-native] the shared emacs exited; restarting it and re-attaching every pane");

    // Dropping the connection reaps the child and unlinks both sockets,
    // which `SharedConn::start` needs before it can bind them again.
    store.shared = None;
    store.frame_of_pane.clear();
    store.pending_create.clear();
    store.pending_splits.clear();
    store.state_of_pane.clear();
    store.state_dirty.clear();
    // Force the palette out again — the new process has never seen it.
    store.sent_palette = None;
    store.next_id = 1;
    // Font ids belong to the process that announced them; the new Emacs
    // will announce its own from scratch.
    *fonts = EmacsFonts::default();

    let proxy = wakeup.map(|w| bevy::winit::EventLoopProxy::clone(&w));
    match SharedConn::start(
        EmacsPalette::from_theme(&theme),
        theme_font_px(&theme),
        proxy,
    ) {
        Ok(conn) => store.shared = Some(conn),
        Err(e) => {
            // Leave `shared` None so the next pane spawn tries again
            // rather than wiring itself to nothing.
            eprintln!("[emacs-native] could not restart emacs: {e}");
            return;
        }
    }

    let store = &mut *store;
    for (entity, kind, mut frame) in &mut panes {
        if kind.0 != PANE_KIND {
            continue;
        }
        // Same allocation the startup path uses: the first pane adopts
        // Emacs's own initial frame (id 1), the rest ask for one.
        let id = store.next_id;
        store.next_id += 1;
        if id != 1 {
            store.pending_create.push(id);
        }
        store.frame_of_pane.insert(entity, id);
        frame.frame_id = id;
        // The pane has not moved, so `sync_native_resize` would find its
        // memoized size unchanged and send nothing — leaving the new frame
        // at no size at all. `resize_dirty` drops that memo. It is cleared
        // on the first pass whether or not the send happens, which is fine:
        // the memo is only written on a successful send, so a pane still
        // waiting on its create-frame retries on the next frame.
        frame.resize_dirty = true;
        // Everything learned from the old process is stale: the font ids a
        // run names, the line height the fit rounds to, and the generation
        // counter the op drain compares against.
        frame.line_h = 0;
        frame.last_generation = 0;
        frame.caret_under = None;
        frame.sized = false;
        frame.ready = false;
        if let Ok(mut vis) = visibility.get_mut(frame.sprite) {
            *vis = Visibility::Hidden;
        }
    }
}

fn flush_pending_creates(mut store: ResMut<EmacsNativeStore>) {
    if store.pending_create.is_empty() {
        return;
    }
    let store = &mut *store;
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    let dbg = emacs_dbg();
    store.pending_create.retain(|&id| {
        let sent = conn.send_create_frame(id);
        if dbg && sent {
            eprintln!("[emacs-dbg] create-frame {id} delivered");
        }
        !sent
    });
}

/// Keep each Emacs frame sized to its pane's content area. Sends a
/// resize whenever the content pixel size changes (the initial fit once
/// emacs connects, and every drag-resize after). Content size in
/// logical px == Emacs frame px (the sprite renders 1:1 logical).
#[allow(clippy::type_complexity)]
fn sync_native_resize(
    store: Res<EmacsNativeStore>,
    mut panes: Query<(
        Entity,
        &PaneRect,
        &PaneKindMarker,
        Option<&jim_pane::PaneChromeOverride>,
        Option<&mut EmacsFrame>,
    )>,
    mut last: Local<std::collections::HashMap<Entity, (i32, i32)>>,
) {
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    for (entity, rect, kind, chrome_ov, mut frame) in &mut panes {
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
        let mut ch = (rect.size.y - title_h - 2.0 * MARGIN).max(32.0) as i32;
        // Round DOWN to a whole number of text lines. A frame height that
        // leaves a partial row is normally harmless — the window system
        // clips it — but `draw_glyph_string` here carries no clip rect, so
        // jim blits that half-row wherever Emacs put it and it lands on
        // top of the mode line. Not producing the partial row is the fix.
        let line_h = frame.as_ref().map(|f| f.line_h).unwrap_or(0);
        if (4..=64).contains(&line_h) {
            let whole = (ch / line_h) * line_h;
            if whole >= line_h {
                ch = whole;
            }
        }
        // A newly-learned line height means the size we already sent was
        // computed against the wrong multiple; drop the memo so the
        // corrected one goes out.
        let forced = frame.as_ref().is_some_and(|f| f.resize_dirty);
        if forced {
            if let Some(f) = frame.as_mut() {
                f.resize_dirty = false;
            }
            last.remove(&entity);
        }
        let dbg = emacs_dbg();
        if last.get(&entity) == Some(&(cw, ch)) {
            continue;
        }
        if dbg {
            eprintln!(
                "[emacs-dbg] frame {fid} wants {cw}x{ch} (line_h={line_h}, pending_create={})",
                store.pending_create.contains(&fid)
            );
        }
        // Retry until emacs is connected (also delivers the initial fit).
        // A frame whose create-frame command hasn't landed yet must NOT be
        // resized: `store-event` in the port falls back to the LAST frame
        // for an unknown id, so an early resize would silently resize a
        // different pane's frame and then never be re-sent.
        if store.pending_create.contains(&fid) {
            continue;
        }
        if conn.send_resize(fid, cw, ch) {
            if line_h > 0 && ch % line_h != 0 {
                eprintln!(
                    "[emacs-native] frame {fid} fitted to {cw}x{ch} which is NOT a \
                     whole number of {line_h}px rows — expect a half-drawn bottom row"
                );
            }
            last.insert(entity, (cw, ch));
            if dbg {
                eprintln!("[emacs-dbg] frame {fid} resize {cw}x{ch} SENT");
            }
        } else if dbg {
            eprintln!("[emacs-dbg] frame {fid} resize {cw}x{ch} NOT SENT (writer not ready)");
        }
    }
}

/// Mouse wheel / trackpad over a native pane → a pixel-precision scroll.
///
/// The pixels the trackpad reports are forwarded verbatim over the
/// control channel, where `jim--scroll` runs them through
/// `pixel-scroll-precision-scroll-*`: sub-line `window-vscroll`, so a
/// slow two-finger drag moves the text by the distance your fingers
/// moved instead of snapping two whole lines at a time. macOS delivers
/// its own momentum tail as more pixel events, so flings coast.
///
/// If the control channel isn't up yet (Emacs still booting), fall back
/// to the WHEEL_EVENT record — `mouse-wheel-mode`'s line scrolling, but
/// better than dropping the gesture.
fn handle_native_wheel(
    mut wheel: MessageReader<bevy::input::mouse::MouseWheel>,
    windows: Query<&Window>,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<EmacsNativeStore>,
    panes: Query<(
        Entity,
        &PaneRect,
        &PaneKindMarker,
        Option<&jim_pane::PaneChromeOverride>,
    )>,
    mut accum: Local<f32>,
    mut last_target: Local<Option<Entity>>,
) {
    use bevy::input::mouse::MouseScrollUnit;
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    // A wheel "line" is one detent on a real mouse; 40px matches what
    // the notch-based encoder used to treat as one detent.
    const PIXELS_PER_LINE: f32 = 40.0;
    let mut dy = 0.0f32;
    for ev in wheel.read() {
        dy += match ev.unit {
            MouseScrollUnit::Line => ev.y * PIXELS_PER_LINE,
            MouseScrollUnit::Pixel => ev.y,
        };
    }
    if dy == 0.0 {
        return;
    }

    // Route to the native pane under the cursor.
    let Ok(win) = windows.single() else { return };
    let Some(cur) = win.cursor_position() else {
        return;
    };
    let canvas = viewport.window_to_canvas(cur);
    let visible: Vec<(Entity, PaneRect)> = panes
        .iter()
        .filter(|(_, _, k, _)| k.0 == PANE_KIND)
        .map(|(e, r, _, _)| (e, r.clone()))
        .collect();
    let Some(pane) = jim_pane::topmost_pane_at(canvas, &visible) else {
        return;
    };
    let Ok((rect, ov)) = panes.get(pane).map(|(_, r, _, ov)| (r, ov)) else {
        return;
    };
    let Some(&fid) = store.frame_of_pane.get(&pane) else {
        return;
    };

    // Sub-pixel remainder carries over so a slow drag still accumulates
    // instead of rounding to zero every frame — but only within one
    // pane, or leftover motion would leak across a hover change.
    if *last_target != Some(pane) {
        *accum = 0.0;
        *last_target = Some(pane);
    }
    *accum += dy;
    let pixels = accum.trunc() as i32;
    if pixels == 0 {
        return;
    }
    *accum -= pixels as f32;

    let local = jim_pane::pt_to_content_local_th(canvas, rect, jim_pane::override_title_h(ov));
    if !conn.send_scroll(fid, local.x as i32, local.y as i32, pixels) {
        let notches = pixels / PIXELS_PER_LINE as i32;
        for _ in 0..notches.abs() {
            conn.send_wheel(fid, notches > 0, local.x as i32, local.y as i32);
        }
    }
}

/// How long after a click a second one still counts as a double-click,
/// and how far it may drift. Mirrors macOS's own defaults closely
/// enough that muscle memory transfers.
const MULTI_CLICK_SECS: f32 = 0.45;
const MULTI_CLICK_SLOP: i32 = 4;

/// Click-streak bookkeeping for `handle_native_mouse`.
struct MultiClick {
    pane: Entity,
    x: i32,
    y: i32,
    at: f32,
    count: u32,
}

/// Left-click on a native pane → a mouse press+release pair at the
/// content-local pixel (which equals the Emacs frame pixel, since the
/// sprite renders at logical = frame size). Emacs pairs them into a
/// `mouse-1` click that sets point.
///
/// Multi-clicks are counted HERE rather than in Emacs: the port's input
/// record has no timestamp field, so `make_lispy_event` leaves
/// `button_down_time` at zero and can never promote a click to a
/// double-click. jim counts them and asks `jim--click` for the word /
/// line selection, which is what makes double-click-to-select-a-word
/// and triple-click-to-select-a-line work at all.
#[allow(clippy::type_complexity)]
fn handle_native_mouse(
    mut presses: MessageReader<jim_pane::PaneContentPressed>,
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    windows: Query<&Window>,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<EmacsNativeStore>,
    rects: Query<(&PaneRect, Option<&jim_pane::PaneChromeOverride>)>,
    kinds: Query<&PaneKindMarker>,
    mut pressed: Local<Option<(Entity, i32, i32)>>,
    mut clicks: Local<Option<MultiClick>>,
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
            let now = time.elapsed_secs();
            let count = match clicks.as_ref() {
                Some(prev)
                    if prev.pane == ev.pane
                        && now - prev.at <= MULTI_CLICK_SECS
                        && (prev.x - x).abs() <= MULTI_CLICK_SLOP
                        && (prev.y - y).abs() <= MULTI_CLICK_SLOP =>
                {
                    prev.count + 1
                }
                _ => 1,
            };
            *clicks = Some(MultiClick {
                pane: ev.pane,
                x,
                y,
                at: now,
                count,
            });

            conn.send_mouse(fid, 0, true, x, y, modbits);
            *pressed = Some((ev.pane, x, y));
            if count >= 2 {
                // The press above already put point under the cursor;
                // this widens it to the word (2) or the line (3+).
                conn.send_click(fid, x, y, count.min(3));
            }
        }
    }

    // While the button is held after pressing a native pane, stream
    // motion at the current content-local pixel (drives divider drag
    // and region selection in Emacs).
    if let Some((pane, lx, ly)) = *pressed {
        if buttons.pressed(MouseButton::Left) {
            if let (Ok(win), Ok((rect, ov)), Some(&fid)) = (
                windows.single(),
                rects.get(pane),
                store.frame_of_pane.get(&pane),
            ) {
                if let Some(cur) = win.cursor_position() {
                    let canvas = viewport.window_to_canvas(cur);
                    let local = jim_pane::pt_to_content_local_th(
                        canvas,
                        rect,
                        jim_pane::override_title_h(ov),
                    );
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
    let (Some(conn), Some(&fid)) = (store.shared.as_ref(), store.frame_of_pane.get(&target)) else {
        return;
    };

    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);
    let alt = mods.pressed(KeyCode::AltLeft) || mods.pressed(KeyCode::AltRight);
    let cmd = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);
    if cmd {
        // Mac muscle memory. Two flavours:
        //
        //  * clipboard chords translate to the equivalent Emacs kill-ring
        //    chords, so the pbcopy/pbpaste bridge (interprogram-cut/
        //    paste-function in jim-win.el) round-trips them to the macOS
        //    pasteboard. modbits layout: ctrl=1, alt(meta)=2.
        //      Cmd+C -> M-w (kill-ring-save)  Cmd+X -> C-w (kill-region)
        //      Cmd+V -> C-y (yank)
        //  * the rest run a named command over the control channel. These
        //    are the shortcuts every other Mac editor has and Emacs
        //    doesn't; none of them collide with a jim binding (jim owns
        //    Cmd+O/T/W/K and the Cmd+Shift set).
        //
        // Anything else under Cmd stays jim's and is dropped below.
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
                continue;
            }
            let command: Option<&str> = match (ev.key_code, shift) {
                (KeyCode::KeyS, false) => Some("save-buffer"),
                (KeyCode::KeyZ, false) => Some("undo"),
                (KeyCode::KeyZ, true) => Some("undo-redo"),
                (KeyCode::KeyA, false) => Some("mark-whole-buffer"),
                (KeyCode::KeyF, false) => Some("isearch-forward"),
                (KeyCode::KeyL, false) => Some("goto-line"),
                (KeyCode::BracketLeft, false) => Some("beginning-of-buffer"),
                (KeyCode::BracketRight, false) => Some("end-of-buffer"),
                _ => None,
            };
            if let Some(command) = command {
                conn.send_cmd(fid, command);
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
            let m = if code == 32 {
                modbits & !0b100
            } else {
                modbits
            };
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

// ---------- Theme, state, and the scroll indicator ----------

/// Width of the overlay scroll indicator, and how long it lingers after
/// the view stops moving.
const SCROLLBAR_W: f32 = 4.0;
const SCROLLBAR_INSET: f32 = 3.0;
const SCROLLBAR_MIN_H: f32 = 24.0;
const SCROLLBAR_HOLD_SECS: f32 = 0.9;
const SCROLLBAR_ALPHA: f32 = 0.45;

/// Keep Emacs's selected frame in step with jim's focused pane.
///
/// Without this, `selected-frame` is whatever Emacs last chose for
/// itself, so `M-x`, the minibuffer, and every state report could land
/// on a different pane than the one you are typing into.
fn sync_emacs_focus(
    store: Res<EmacsNativeStore>,
    focused: Res<jim_pane::FocusedPane>,
    kinds: Query<&PaneKindMarker>,
    mut last: Local<Option<u32>>,
) {
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    let Some(pane) = focused.0 else { return };
    if !matches!(kinds.get(pane), Ok(k) if k.0 == PANE_KIND) {
        return;
    }
    let Some(&fid) = store.frame_of_pane.get(&pane) else {
        return;
    };
    if *last == Some(fid) {
        return;
    }
    if conn.send_focus(fid) {
        *last = Some(fid);
    }
}

/// jim's UI font size in pixels, which Emacs text is matched to.
fn theme_font_px(theme: &jim_style::Theme) -> i32 {
    (theme.f32(jim_style::tokens::FONT_SIZE).round() as i32).clamp(6, 72)
}

/// Keep Emacs's font size on jim's `font_size` token.
///
/// Skipped entirely when JIM_EMACS_FONT_SIZE is set: that is the user
/// saying what they want, and the theme does not get to override it.
fn sync_emacs_font(
    store: Res<EmacsNativeStore>,
    theme: Res<jim_style::Theme>,
    mut changed: MessageReader<jim_style::ThemeChanged>,
    mut sent: Local<Option<i32>>,
    mut relearn: ResMut<EmacsRelearnLineH>,
) {
    if std::env::var_os("JIM_EMACS_FONT_SIZE").is_some() {
        return;
    }
    let theme_moved = changed.read().count() > 0 || theme.is_changed();
    if !theme_moved && sent.is_some() {
        return;
    }
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    let px = theme_font_px(&theme);
    if *sent == Some(px) {
        return;
    }
    if conn.send_font_pixels(px) {
        *sent = Some(px);
        relearn.0 = true;
    }
}

/// Push jim's palette to Emacs whenever the theme changes — and once as
/// soon as the control channel comes up, since a pane can exist before
/// Emacs has finished booting.
///
/// Emacs applies a palette with a full `redraw-display`, so this is
/// deduplicated against the last palette actually delivered: theme.ft
/// hot-reload fires on every keystroke in that file, and most of those
/// edits don't move a colour.
fn sync_emacs_theme(
    mut store: ResMut<EmacsNativeStore>,
    theme: Res<jim_style::Theme>,
    mut changed: MessageReader<jim_style::ThemeChanged>,
) {
    let theme_moved = changed.read().count() > 0 || theme.is_changed();
    let store = &mut *store;
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    // Nothing to do unless the theme moved or we still owe Emacs its
    // first palette (the control channel isn't up the instant we spawn).
    if !theme_moved && store.sent_palette.is_some() {
        return;
    }
    let palette = EmacsPalette::from_theme(&theme);
    if store.sent_palette.as_ref() == Some(&palette) {
        return;
    }
    if conn.send_theme(&palette) {
        store.sent_palette = Some(palette);
    }
}

/// Drain `state <fid> <json>` lines Emacs pushed on the control channel
/// into per-pane [`EmacsFrameState`], and wake the scroll indicator.
fn drain_emacs_events(
    mut store: ResMut<EmacsNativeStore>,
    mut frames: Query<(Entity, &mut EmacsFrame, &PaneKindMarker)>,
) {
    let lines: Vec<String> = {
        let Some(conn) = store.shared.as_ref() else {
            return;
        };
        let Ok(mut inbox) = conn.ctl_inbox.lock() else {
            return;
        };
        if inbox.is_empty() {
            return;
        }
        std::mem::take(&mut *inbox)
    };

    // frame id → pane, so a `state` line can find its pane.
    let pane_of_frame: HashMap<u32, Entity> =
        store.frame_of_pane.iter().map(|(&e, &f)| (f, e)).collect();

    for line in lines {
        let Some(rest) = line.strip_prefix("state ") else {
            continue;
        };
        let Some((fid_s, json)) = rest.split_once(' ') else {
            continue;
        };
        let Ok(fid) = fid_s.parse::<u32>() else {
            continue;
        };
        let Some(&pane) = pane_of_frame.get(&fid) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let next = EmacsFrameState::from_json(&value);
        let prev = store.state_of_pane.get(&pane);
        if prev == Some(&next) {
            continue;
        }
        let scrolled = prev.is_none_or(|p| p.top != next.top || p.bottom != next.bottom);
        // Point moves on every keystroke; the bus does not need to hear
        // about that. Only a change of IDENTITY — which buffer, which
        // file, saved or not — is worth waking every widget in the
        // project for.
        let identity_changed = prev.is_none_or(|p| {
            p.buffer != next.buffer
                || p.path != next.path
                || p.modified != next.modified
                || p.read_only != next.read_only
                || p.mode != next.mode
        });
        store.state_of_pane.insert(pane, next);
        if identity_changed && !store.state_dirty.contains(&pane) {
            store.state_dirty.push(pane);
        }
        if scrolled {
            if let Ok((_, mut frame, _)) = frames.get_mut(pane) {
                frame.scrollbar_fade = SCROLLBAR_HOLD_SECS;
            }
        }
    }
}

/// Size, place, and fade the overlay scroll indicator from the extent
/// Emacs reported. Nothing else in jim has a scrollbar, so this one
/// behaves like the macOS overlay kind: it appears while the view moves
/// and gets out of the way once it settles.
fn sync_scroll_indicator(
    time: Res<Time>,
    store: Res<EmacsNativeStore>,
    theme: Res<jim_style::Theme>,
    mut frames: Query<(
        Entity,
        &mut EmacsFrame,
        &PaneKindMarker,
        &PaneRect,
        Option<&jim_pane::PaneChromeOverride>,
    )>,
    mut sprites: Query<(&mut Sprite, &mut Transform, &mut Visibility)>,
) {
    let accent = theme.color(jim_style::tokens::ACCENT);
    for (pane, mut frame, kind, rect, chrome_ov) in &mut frames {
        if kind.0 != PANE_KIND {
            continue;
        }
        let Ok((mut sprite, mut transform, mut vis)) = sprites.get_mut(frame.scrollbar) else {
            continue;
        };
        let state = store.state_of_pane.get(&pane);
        let (top, bottom) = match state {
            Some(s) => (s.top.clamp(0.0, 1.0), s.bottom.clamp(0.0, 1.0)),
            None => (0.0, 1.0),
        };
        let visible_frac = (bottom - top).clamp(0.0, 1.0);

        if frame.scrollbar_fade > 0.0 {
            frame.scrollbar_fade = (frame.scrollbar_fade - time.delta_secs()).max(0.0);
        }
        // A buffer that fits entirely in the window has nothing to
        // indicate — showing a full-height thumb there is noise.
        if visible_frac >= 0.999 || frame.scrollbar_fade <= 0.0 {
            if *vis != Visibility::Hidden {
                *vis = Visibility::Hidden;
            }
            continue;
        }

        let title_h = jim_pane::override_title_h(chrome_ov);
        let content_w = (rect.size.x - 2.0 * MARGIN).max(1.0);
        let content_h = (rect.size.y - title_h - 2.0 * MARGIN).max(1.0);
        let thumb_h = (visible_frac * content_h)
            .max(SCROLLBAR_MIN_H)
            .min(content_h);
        // `top` is a fraction of the whole buffer; the thumb travels the
        // track minus its own height, so the bottom of the buffer parks
        // the thumb flush with the bottom of the track.
        let travel = (content_h - thumb_h).max(0.0);
        let y = -(top / (1.0 - visible_frac).max(1e-4) * travel).clamp(0.0, travel);

        // Ease the last 40% of the hold into a fade-out.
        let alpha = SCROLLBAR_ALPHA * (frame.scrollbar_fade / (SCROLLBAR_HOLD_SECS * 0.4)).min(1.0);
        sprite.color = Color::LinearRgba(accent).with_alpha(alpha);
        sprite.custom_size = Some(Vec2::new(SCROLLBAR_W, thumb_h));
        transform.translation.x = content_w - SCROLLBAR_W - SCROLLBAR_INSET;
        transform.translation.y = y;
        transform.translation.z = 2.0;
        if *vis != Visibility::Inherited {
            *vis = Visibility::Inherited;
        }
    }
}

fn register_native_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(jim_pane::PaneKindSpec {
        kind: PANE_KIND,
        // The workspace action ("Emacs", ⌘K E) is the way in: an editor
        // with no file tree beside it is rarely what anyone wants, so
        // this bare kind stays out of the radial and is named for what
        // it is. It still has to be registered — layout restore and
        // Emacs-initiated splits both spawn through it.
        display_name: "Emacs Pane (no sidebar)",
        radial_icon: None,
        default_size: Vec2::new(820.0, 560.0),
        spawn: native_spawn_from_config,
        snapshot: native_snapshot,
        on_close: Some(native_on_close),
    });
}

fn native_spawn_from_config(
    world: &mut World,
    entity: Entity,
    content_root: Entity,
    config: &Value,
) {
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
    let adopt = config
        .get("adopt_frame_id")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
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
        store.state_of_pane.remove(&entity);
        store.state_dirty.retain(|&e| e != entity);
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
    let palette = {
        let global = world.resource::<jim_style::Theme>();
        let proj = world
            .get::<jim_pane::PaneProject>(entity)
            .and_then(|p| world.resource::<jim_style::ProjectThemes>().get(p.0));
        EmacsPalette::from_theme(proj.unwrap_or(global))
    };
    let bg_bytes = rgb_bytes(
        world
            .resource::<jim_style::Theme>()
            .color(jim_style::tokens::BG),
    );
    let font_px = theme_font_px(world.resource::<jim_style::Theme>());
    let caret_bytes = rgb_bytes(
        world
            .resource::<jim_style::Theme>()
            .color(jim_style::tokens::CARET),
    );

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
            // The native transport starts every frame at 64x64.  Keep that
            // implementation detail invisible until the first complete,
            // correctly-sized frame has been presented.
            Visibility::Hidden,
        ))
        .id();

    // The scroll indicator rides above the framebuffer sprite. Starts
    // hidden; `sync_scroll_indicator` sizes and fades it from the state
    // Emacs reports.
    let accent = world
        .resource::<jim_style::Theme>()
        .color(jim_style::tokens::ACCENT);
    let scrollbar = world
        .spawn((
            ChildOf(content_root),
            Sprite {
                color: Color::LinearRgba(accent).with_alpha(0.0),
                custom_size: Some(Vec2::new(SCROLLBAR_W, 0.0)),
                ..default()
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(0.0, 0.0, 2.0),
            Visibility::Hidden,
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
        match SharedConn::start(palette, font_px, wakeup) {
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
            if emacs_dbg() {
                eprintln!(
                    "[emacs-dbg] pane {entity:?} claimed frame {id} (create queued: {})",
                    id != 1
                );
            }
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
            sized: false,
            ready: false,
            fb: rgba_filled(fb_w, fb_h, bg_bytes),
            bg: bg_bytes,
            caret: caret_bytes,
            caret_under: None,
            last_generation: 0,
            resize_dirty: false,
            scrollbar,
            scrollbar_fade: 0.0,
            line_h: 0,
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

fn native_frame_should_reveal(sized: bool, ready: bool, uploaded: bool) -> bool {
    !ready && sized && uploaded
}

/// Drain each pane's op queue into its framebuffer and re-upload.
fn sync_emacs_frames(
    store: Res<EmacsNativeStore>,
    mut fonts: ResMut<EmacsFonts>,
    mut relearn: ResMut<EmacsRelearnLineH>,
    mut images: ResMut<Assets<Image>>,
    mut frames: Query<(Entity, &mut EmacsFrame, &PaneKindMarker)>,
    mut sprites: Query<(&mut Sprite, &mut Visibility)>,
    mut commands: Commands,
) {
    let Some(conn) = store.shared.as_ref() else {
        return;
    };
    let cur_gen = conn.generation.load(Ordering::Relaxed);
    let relearn = std::mem::take(&mut relearn.0);
    for (entity, mut frame, kind) in &mut frames {
        if kind.0 != PANE_KIND {
            continue;
        }
        if relearn {
            frame.line_h = 0;
        }
        if cur_gen == frame.last_generation {
            continue;
        }
        frame.last_generation = cur_gen;

        // Take this frame's pending ops from the shared queue.
        let ops: Vec<Op> = {
            let mut guard = conn.frame_ops.lock().expect("frame_ops lock");
            guard
                .get_mut(&frame.frame_id)
                .map(std::mem::take)
                .unwrap_or_default()
        };
        if ops.is_empty() {
            continue;
        }
        if emacs_dbg() {
            let runs = ops.iter().filter(|o| matches!(o, Op::Run { .. })).count();
            let flushes = ops.iter().filter(|o| matches!(o, Op::Flush)).count();
            let colours: Vec<String> = ops
                .iter()
                .filter_map(|o| match o {
                    Op::Run { fg, bg, font, .. } => Some(format!("{fg:06x}/{bg:06x}@f{font}")),
                    _ => None,
                })
                .take(3)
                .collect();
            eprintln!(
                "[emacs-dbg] frame {} batch: {} ops, {runs} runs, {flushes} flush  fg/bg {}",
                frame.frame_id,
                ops.len(),
                colours.join(" ")
            );
        }
        let _ = entity;

        // Handle a resize first if present (rebuild the image + sprite).
        if ops.iter().any(|op| matches!(op, Op::FrameSize { .. })) {
            frame.sized = true;
        }
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
            if emacs_dbg() {
                eprintln!(
                    "[emacs-dbg] frame {} framebuffer {}x{} -> {nw}x{nh}",
                    frame.frame_id, frame.fb_w, frame.fb_h
                );
            }
            frame.fb_w = nw;
            frame.fb_h = nh;
            // The framebuffer is about to be replaced; a save-under into
            // the old one would restore garbage at the wrong place.
            frame.caret_under = None;
            frame.fb = rgba_filled(nw, nh, frame.bg);
            // If this fails the CPU and GPU buffers disagree from here on
            // and every present is dropped, so say so rather than leaving
            // a blank pane to be discovered by eye.
            if let Some(mut img) = images.get_mut(&frame.image) {
                *img = blank_image_rgb(nw, nh, frame.bg);
            } else {
                eprintln!(
                    "[emacs-native] frame {}: could not resize the image asset to {nw}x{nh}",
                    frame.frame_id
                );
            }
            // Resize the content sprite to the logical frame size.
            if let Ok((mut sprite, _)) = sprites.get_mut(frame.sprite) {
                sprite.custom_size = Some(Vec2::new(
                    nw as f32 / FB_SCALE as f32,
                    nh as f32 / FB_SCALE as f32,
                ));
            }
        }

        let (fb_w, fb_h) = (frame.fb_w, frame.fb_h);
        let fid = frame.frame_id;
        let image = frame.image.clone();
        let should_reveal = native_frame_should_reveal(frame.sized, frame.ready, true);
        let sprite_entity = frame.sprite;
        // Split-borrow the working buffer and the rasterizers (both need
        // &mut at once). Draw into `fb`, not the GPU image.
        let EmacsFrame {
            fb,
            caret,
            caret_under,
            ..
        } = &mut *frame;
        let caret = *caret;
        let EmacsFonts {
            rasters,
            default_font,
            warned,
        } = &mut *fonts;
        let px = fb.as_mut_slice();
        let mut present = false;
        let mut reveal = false;
        let mut new_title: Option<String> = None;
        let mut new_line_h: Option<i32> = None;

        for op in ops {
            match op {
                Op::FrameSize { .. } => {}
                Op::Flush => present = true,
                Op::Title { text } => new_title = Some(text),
                Op::Font {
                    id,
                    weight,
                    slant,
                    path,
                    px: fpx,
                    asc,
                    desc,
                } => {
                    if emacs_dbg() {
                        eprintln!(
                            "[emacs-dbg] frame {fid} font id={id} px={fpx} wt={weight} sl={slant} {path}"
                        );
                    }
                    rasters
                        .entry(id)
                        .or_insert_with(GlyphRaster::new)
                        .set_font(&path, fpx, weight, slant);
                    *default_font = id;
                    let _ = (asc, desc);
                }
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
                    x,
                    y,
                    w,
                    h,
                    asc,
                    font,
                    fg,
                    bg,
                    clip,
                    glyphs,
                } => {
                    // A run's height IS Emacs's line height, which is
                    // what the frame must be a whole multiple of. The
                    // font op cannot give it: `asc + desc` leaves out
                    // line-spacing (14px font, ascent 13 + descent 3,
                    // but rows are 22px), and rounding to the wrong
                    // multiple is the same as not rounding at all.
                    // Learn it ONCE per font. Runs are not all the same
                    // height (a smaller face, an image, the echo area),
                    // and letting it change on every batch means
                    // `resize_dirty` re-fits the frame constantly — a
                    // resize storm the pane never settles out of.
                    if h > 0 && !glyphs.is_empty() && new_line_h.is_none() {
                        new_line_h = Some(h);
                    }
                    let clip = clip.scaled(FB_SCALE as i32);
                    // Background box for the run first (Emacs's own run
                    // height, so the block cursor fills the whole cell).
                    fill_rect_clipped(
                        px,
                        fb_w,
                        fb_h,
                        x * FB_SCALE as i32,
                        y * FB_SCALE as i32,
                        w * FB_SCALE as i32,
                        h * FB_SCALE as i32,
                        bg,
                        clip,
                    );
                    let fgc = unpack(fg);
                    let baseline = (y + asc) * FB_SCALE as i32;
                    let advance = if glyphs.is_empty() {
                        0
                    } else {
                        (w * FB_SCALE as i32) / glyphs.len() as i32
                    };
                    let x0 = x * FB_SCALE as i32;
                    // Glyph ids are indices into THIS run's font, so a
                    // run must be rasterized by its own font's raster.
                    let key = if rasters.contains_key(&font) {
                        font
                    } else {
                        *default_font
                    };
                    let Some(raster) = rasters.get_mut(&key) else {
                        // Nothing to draw with — and drawing nothing is
                        // indistinguishable from an empty buffer, so say so
                        // once rather than leaving a silently textless pane.
                        if warned.insert(font) {
                            eprintln!(
                                "[emacs-native] frame {fid}: run names font {font}, which was \
                                 never announced — those glyphs will not render"
                            );
                        }
                        continue;
                    };
                    for (i, gid) in glyphs.iter().enumerate() {
                        let pen_x = x0 + advance * i as i32;
                        if let Some(g) = raster.glyph(*gid) {
                            let gx = pen_x + g.left;
                            let gy = baseline - g.top;
                            blend_glyph(px, fb_w, fb_h, gx, gy, g.w, g.h, &g.alpha, fgc, clip);
                        }
                    }
                }
                Op::Scroll { x, y, w, h, dy } => {
                    // The blit moves pixels out from under the saved
                    // rect, so its coordinates no longer mean anything.
                    *caret_under = None;
                    scroll_rect(
                        px,
                        fb_w,
                        fb_h,
                        x * FB_SCALE as i32,
                        y * FB_SCALE as i32,
                        w * FB_SCALE as i32,
                        h * FB_SCALE as i32,
                        dy * FB_SCALE as i32,
                    );
                }
                // A bar / hbar cursor: the port sends the rect and we
                // paint it in jim's own caret colour. A box cursor never
                // reaches here — the port draws that as an inverted
                // glyph run so it fills the whole cell.
                Op::Cursor { x, y, w, h, .. } => {
                    // Lift the previous caret first — see `restore_caret`
                    // for why Emacs cannot be relied on to erase a bar.
                    if let Some(under) = caret_under.take() {
                        restore_caret(px, fb_w, fb_h, &under, caret);
                    }
                    let (dx, dy, dw, dh) = (
                        x * FB_SCALE as i32,
                        y * FB_SCALE as i32,
                        w * FB_SCALE as i32,
                        h * FB_SCALE as i32,
                    );
                    *caret_under = Some(save_under(px, fb_w, fb_h, dx, dy, dw, dh));
                    fill_rect(
                        px,
                        fb_w,
                        fb_h,
                        dx,
                        dy,
                        dw,
                        dh,
                        u32::from_be_bytes([0, caret[0], caret[1], caret[2]]),
                    );
                }
            }
        }

        // Present the completed frame atomically on flush.
        //
        // Every bail-out here is a pane that draws nothing while looking
        // perfectly healthy from the outside — ops arriving, runs
        // rasterized, flushes counted — so none of them stay silent. A
        // size mismatch in particular is not self-correcting: `fb` is
        // resized in the block above whether or not the GPU image could
        // be resized with it, and once the two disagree every later
        // present is skipped for the life of the pane.
        if present {
            match images.get_mut(&image) {
                None => {
                    eprintln!("[emacs-native] frame {fid}: no image asset — pane will stay blank")
                }
                Some(mut img) => match img.data.as_mut() {
                    None => eprintln!(
                        "[emacs-native] frame {fid}: image has no CPU data — pane will stay blank"
                    ),
                    Some(data) if data.len() != fb.len() => eprintln!(
                        "[emacs-native] frame {fid}: image is {} bytes but the framebuffer is {} \
                         ({fb_w}x{fb_h}) — pane will stay blank until they agree",
                        data.len(),
                        fb.len()
                    ),
                    Some(data) => {
                        data.copy_from_slice(fb);
                        // Reveal only after the correctly-sized image was
                        // successfully uploaded.  A failed upload remains a
                        // normal empty pane, never a transport-sized square.
                        reveal = should_reveal;
                        // Diagnostic escape hatch: touch ~/.jim/emacs-dump to
                        // write the next presented framebuffer of every frame
                        // to a PPM, so what jim actually rasterized can be
                        // looked at directly rather than inferred from op
                        // counts. Consumed on use.
                        if let Some(dir) = jim_pane_data_dir()
                            && dir.join("emacs-dump").exists()
                        {
                            let mut out = format!("P6\n{fb_w} {fb_h}\n255\n").into_bytes();
                            out.extend(fb.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
                            let _ = std::fs::write(dir.join(format!("emacs-frame-{fid}.ppm")), out);
                        }
                    }
                },
            }
        }

        if reveal {
            frame.ready = true;
            if let Ok((_, mut vis)) = sprites.get_mut(sprite_entity) {
                *vis = Visibility::Inherited;
            }
        }

        if let Some(h) = new_line_h
            && frame.line_h == 0
        {
            // The pane's fitted size was computed against the old (or
            // unknown) line height, and `sync_native_resize` memoizes
            // what it sent — so without this the frame keeps whatever
            // unrounded height it was given at spawn, leaving a partial
            // bottom row that draws over the mode line.
            frame.line_h = h;
            frame.resize_dirty = true;
        }

        // Live buffer identity in the pane title bar.
        if let Some(title) = new_title {
            commands.entity(entity).insert(jim_pane::PaneTitle(title));
        }
    }
    let _ = (MARGIN, TITLE_H);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_frame_lifecycle_events() {
        assert_eq!(
            parse_frame_lifecycle("frame-new f=17 w=900 h=600 split=2"),
            Some(FrameLifecycle::New { fid: 17, split: 2 })
        );
        assert_eq!(
            parse_frame_lifecycle("frame-delete f=17"),
            Some(FrameLifecycle::Delete { fid: 17 })
        );
        assert_eq!(parse_frame_lifecycle("frame-delete f=0"), None);
        assert_eq!(parse_frame_lifecycle("flush f=17"), None);
    }

    #[test]
    fn bootstrap_framebuffer_is_never_revealed() {
        assert!(!native_frame_should_reveal(false, false, true));
        assert!(!native_frame_should_reveal(true, false, false));
        assert!(native_frame_should_reveal(true, false, true));
        assert!(!native_frame_should_reveal(true, true, true));
    }

    #[test]
    fn split_completion_keeps_its_explicit_source() {
        let right = Entity::from_raw_u32(7).expect("valid entity");
        let below = Entity::from_raw_u32(8).expect("valid entity");
        let stale = Entity::from_raw_u32(9).expect("valid entity");
        let mut frames = HashMap::from([(right, 3), (below, 4), (stale, 99)]);
        let mut pending = VecDeque::from([
            PendingNativeSplit {
                source: right,
                source_fid: 3,
                direction: NativeSplitDirection::Right,
            },
            PendingNativeSplit {
                source: stale,
                source_fid: 5,
                direction: NativeSplitDirection::Right,
            },
            PendingNativeSplit {
                source: below,
                source_fid: 4,
                direction: NativeSplitDirection::Below,
            },
        ]);

        let matched = take_matching_split(&mut pending, &frames, 2).expect("below split");
        assert_eq!(matched.source, below);
        assert_eq!(pending.len(), 1);

        // A request whose pane was rebound to another frame is stale and
        // must not steal a later frame-new event with the same direction.
        frames.insert(right, 30);
        assert!(take_matching_split(&mut pending, &frames, 1).is_none());
    }

    /// The whole point of `face_index_for`: one path, four faces.
    /// Without it every bold and italic face rasterised as index 0 —
    /// i.e. looked exactly like regular text.
    #[test]
    fn picks_the_bold_and_italic_faces_out_of_a_collection() {
        let Ok(data) = std::fs::read("/System/Library/Fonts/Menlo.ttc") else {
            return; // not a mac, or the font moved — nothing to assert
        };
        let regular = face_index_for(&data, NORMAL_WEIGHT, NORMAL_SLANT);
        let bold = face_index_for(&data, 200, NORMAL_SLANT);
        let italic = face_index_for(&data, NORMAL_WEIGHT, 200);
        let bold_italic = face_index_for(&data, 200, 200);

        let faces = [regular, bold, italic, bold_italic];
        assert_eq!(
            faces.iter().collect::<std::collections::HashSet<_>>().len(),
            4,
            "regular/bold/italic/bold-italic must resolve to four distinct \
             faces, got {faces:?}"
        );

        // And each one really carries the attributes we asked for.
        let collection = swash::FontDataRef::new(&data).expect("Menlo.ttc parses");
        let attrs = |i: usize| collection.get(i).expect("face exists").attributes();
        assert!(attrs(bold).weight().0 >= 600, "bold face is not bold");
        assert!(attrs(regular).weight().0 < 600, "regular face is bold");
        assert!(
            !matches!(attrs(italic).style(), swash::Style::Normal),
            "italic face is not slanted"
        );
        assert!(
            matches!(attrs(regular).style(), swash::Style::Normal),
            "regular face is slanted"
        );
    }
}
