//! Emacs pane: hosts a REAL GNU Emacs in a floating pane.
//!
//! Phase 1 of the "emacs with our own renderer" plan: a long-lived
//! `emacs --daemon=jim` holds all editor state (buffers, packages, the
//! user's config); each pane is a tty frame attached to it via
//! `emacsclient -t` running on a jim-daemon PTY session. The VT stream
//! is parsed by the shared jim-terminal worker (libghostty-vt) and
//! rendered through the shared grid pipeline (`TermGrid` + `sync_grid`).
//!
//! What makes this a distinct pane kind rather than "a terminal running
//! emacs":
//! - its own lifecycle: closing the pane kills only the client frame —
//!   the emacs daemon (and every buffer) survives; a new pane reattaches.
//! - an emacs-tuned keyboard handler: `C-SPC` (set-mark), `C-/` (undo),
//!   `C-M-…` chords, `C-[ C-] C-\ C-^ C-_`, and xterm-style modified
//!   arrows/nav keys — none of which the terminal pane's v0 encoder emits.
//! - truecolor faces: the client is launched with `COLORTERM=truecolor`,
//!   which Emacs ≥28 detects for 24-bit direct color on a tty.
//!
//! Phase 2 (a `jim` window-system backend inside Emacs itself, emitting
//! draw ops over a socket) replaces the grid transport; the pane kind,
//! daemon lifecycle, and input routing here carry over.

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::sprite::Anchor;
use serde_json::Value;

use jim_pane::{FocusedPane, PaneKindMarker, PaneRect, PaneRegistry};
use jim_terminal::pty::PtySize;
use jim_terminal::worker::{WorkerHandle, WorkerMsg};
use jim_terminal::{
    build_term_grid, grid_size_for_rect, TerminalCursor, TerminalData, TerminalSession,
    TerminalStore, LINE_HEIGHT,
};

pub mod native;

/// Stable identifier for emacs panes (see `PaneKindMarker`).
pub const PANE_KIND: &str = "emacs";

/// Socket name of the shared per-user Emacs daemon. All emacs panes are
/// frames on this one daemon, so buffers/state are shared between panes
/// and survive any individual pane (or the whole GUI) going away.
const EMACS_SOCKET: &str = "jim";

/// Scrollback kept by the VT layer. Emacs owns its own display and
/// repaints on demand — the pty stream has no meaningful history to
/// scroll back into, so keep this token-sized.
const SCROLLBACK_LINES: usize = 1_000;

pub struct EmacsPlugin;

impl Plugin for EmacsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, register_emacs_kind)
            .add_systems(Update, handle_emacs_keyboard)
            // The native pane kind ("emacs-native"): real Emacs GUI
            // redisplay rendered from draw-ops. The tty kind ("emacs")
            // above stays as the fallback transport.
            .add_plugins(native::EmacsNativePlugin);
    }
}

fn register_emacs_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(jim_pane::PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Emacs",
        radial_icon: Some("Mx"),
        default_size: Vec2::new(820.0, 560.0),
        spawn: emacs_spawn_from_config,
        snapshot: emacs_snapshot,
        on_close: Some(emacs_on_close),
    });
}

/// The command the jim-daemon PTY session runs. Through a login shell so
/// a Dock-launched Jim (minimal launchd PATH) still finds a Homebrew
/// `emacsclient`; `exec` keeps the client as the session leader so
/// killing the daemon session kills exactly the frame.
///
/// `--alternate-editor=''` is the auto-start contract: if the `jim`
/// daemon isn't running, emacsclient starts `emacs --daemon=jim` and
/// retries (verified against Emacs 30.1).
fn emacs_command() -> Vec<String> {
    vec![
        "/bin/zsh".to_string(),
        "-lc".to_string(),
        format!(
            "export COLORTERM=truecolor; exec emacsclient -t -s {EMACS_SOCKET} --alternate-editor=''"
        ),
    ]
}

fn emacs_spawn_from_config(
    world: &mut World,
    entity: Entity,
    content_root: Entity,
    config: &Value,
) {
    let session_id = config
        .get("session_id")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| alloc_session_id(world));
    let initial_cwd = initial_cwd_for(world, entity);
    populate_emacs_pane(world, entity, content_root, session_id, initial_cwd);
}

fn emacs_snapshot(world: &World, entity: Entity) -> Value {
    let session_id = world
        .get::<TerminalSession>(entity)
        .map(|s| s.0)
        .unwrap_or(0);
    serde_json::json!({ "session_id": session_id })
}

/// Closing the pane shuts down the pty daemon session, which kills the
/// `emacsclient` process — Emacs deletes that frame and carries on. The
/// emacs daemon itself is NEVER killed from here; it's the state holder.
fn emacs_on_close(world: &mut World, entity: Entity) {
    if let Some(store) = world.get_resource::<TerminalStore>()
        && let Some(data) = store.map.get(&entity)
    {
        data.worker.send(WorkerMsg::Shutdown);
    }
    if let Some(mut store) = world.get_resource_mut::<TerminalStore>() {
        store.map.remove(&entity);
    }
}

/// Insert emacs-specific components on an already-spawned pane and start
/// its worker. Mirrors `populate_terminal_pane`, minus scrollback
/// logging/replay (emacs repaints its frame; replaying a stale VT stream
/// into a fresh grid buys nothing).
pub fn populate_emacs_pane(
    world: &mut World,
    pane_entity: Entity,
    content_root: Entity,
    session_id: u64,
    initial_cwd: Option<String>,
) {
    let cell_width = world.resource::<jim_terminal::MonoMetrics>().cell_width;
    let rect = *world
        .get::<PaneRect>(pane_entity)
        .expect("pane entity must already have PaneRect");
    let (cols, rows) = grid_size_for_rect(rect.size, cell_width);

    let wakeup = world
        .get_resource::<bevy::winit::EventLoopProxyWrapper>()
        .map(|w| bevy::winit::EventLoopProxy::clone(w));
    let worker = WorkerHandle::spawn(
        session_id,
        emacs_command(),
        initial_cwd,
        PtySize {
            cols,
            rows,
            cell_width_px: cell_width as u16,
            cell_height_px: LINE_HEIGHT as u16,
        },
        SCROLLBACK_LINES,
        None,
        None,
        wakeup,
    )
    .expect("WorkerHandle::spawn (emacs)");

    let cursor = world
        .spawn((
            ChildOf(content_root),
            Sprite {
                color: Color::srgba(0.55, 0.75, 0.95, 0.50),
                custom_size: Some(Vec2::new(cell_width, LINE_HEIGHT)),
                ..default()
            },
            Anchor::TOP_LEFT,
            Transform::from_xyz(0.0, 0.0, 1.0),
        ))
        .id();

    let term_grid = build_term_grid(world, content_root, cell_width, cols, rows);

    world.entity_mut(pane_entity).insert((
        term_grid,
        TerminalSession(session_id),
        TerminalCursor(cursor),
    ));

    world
        .get_resource_mut::<TerminalStore>()
        .expect("TerminalStore resource (did setup_terminal_font run?)")
        .map
        .insert(pane_entity, TerminalData { worker });
}

/// Session ids come from the same allocator the shell installs for
/// terminals (`TerminalIdAllocator`) so pty daemon sockets never collide
/// across pane kinds.
fn alloc_session_id(world: &mut World) -> u64 {
    if let Some(alloc) = world.remove_resource::<jim_terminal::TerminalIdAllocator>() {
        let id = (alloc.0)(world);
        world.insert_resource(alloc);
        id
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

fn initial_cwd_for(world: &World, entity: Entity) -> Option<String> {
    let resolver = world.get_resource::<jim_terminal::TerminalInitialCwd>()?;
    (resolver.0)(world, entity)
}

// ---------- Keyboard ----------

/// Translate Bevy key events into the bytes tty Emacs expects. Superset
/// of the terminal pane's v0 encoder: full C-<punctuation> coverage,
/// C-SPC, C-M- chords, and xterm modifier-encoded navigation keys.
///
/// Cmd-modified keys are left for app-level handlers (Cmd+V paste works
/// through the shared clipboard system; Emacs users live on Meta=Option).
fn handle_emacs_keyboard(
    mut events: MessageReader<KeyboardInput>,
    mods: Res<ButtonInput<KeyCode>>,
    focused: Res<FocusedPane>,
    owner: Res<jim_pane::KeyboardOwner>,
    store: Res<TerminalStore>,
    kinds: Query<&PaneKindMarker>,
) {
    let buffered: Vec<KeyboardInput> = events.read().cloned().collect();

    let target_kind = focused.0.and_then(|e| kinds.get(e).ok());
    if !matches!(target_kind, Some(k) if k.0 == PANE_KIND) {
        return;
    }
    if matches!(focused.0, Some(t) if !owner.allows_pane(t)) {
        return;
    }
    let Some(target) = focused.0 else { return };
    let Some(data) = store.map.get(&target) else {
        return;
    };
    let child_alive = {
        let g = data.worker.snapshot.lock().expect("snapshot lock");
        g.child_alive
    };
    if !child_alive {
        return;
    }

    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);
    let alt = mods.pressed(KeyCode::AltLeft) || mods.pressed(KeyCode::AltRight);
    let cmd = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);
    if cmd {
        return;
    }

    let mut out: Vec<u8> = Vec::with_capacity(16);

    for ev in &buffered {
        if !ev.state.is_pressed() {
            continue;
        }

        // Control chords (with or without Meta on top). `C-M-x` is
        // ESC + the control byte — the standard tty encoding Emacs reads.
        if ctrl {
            if let Some(b) = ctrl_byte(ev.key_code, shift) {
                if alt {
                    out.push(0x1b);
                }
                out.push(b);
                continue;
            }
        }

        // Modifier-encoded navigation keys (xterm CSI 1;m / n;m~ forms).
        // Emacs decodes these under TERM=xterm-* into S-<up>, C-<right>,
        // M-<prior>, etc. Plain (unmodified) forms fall through to the
        // simple encodings below.
        if (shift || ctrl || alt)
            && let Some(bytes) = modified_nav_key(ev.key_code, shift, ctrl, alt)
        {
            out.extend_from_slice(&bytes);
            continue;
        }

        // Shift+Tab → backtab.
        if shift && matches!(ev.key_code, KeyCode::Tab) {
            out.extend_from_slice(b"\x1b[Z");
            continue;
        }

        if let Some(bytes) = plain_nav_key(ev.key_code) {
            // Meta'd Enter/Backspace keep the ESC-prefix convention.
            if alt
                && matches!(
                    ev.key_code,
                    KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Backspace
                )
            {
                out.push(0x1b);
            }
            out.extend_from_slice(bytes);
            continue;
        }

        // Printable text; Option acts as Meta. macOS composes Option+key
        // into accented glyphs (Option+x → ≈), so meta sequences re-derive
        // the base char from the physical key — same fix as the terminal
        // pane's encoder.
        match &ev.logical_key {
            Key::Character(s) => {
                if alt && !ctrl {
                    out.push(0x1b);
                    if let Some(ch) = base_char(ev.key_code, shift) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    } else {
                        out.extend_from_slice(s.as_str().as_bytes());
                    }
                } else {
                    out.extend_from_slice(s.as_str().as_bytes());
                }
            }
            Key::Space => {
                if alt && !ctrl {
                    out.push(0x1b);
                }
                out.push(b' ');
            }
            _ => {}
        }
    }

    if !out.is_empty() {
        data.worker.send(WorkerMsg::Input(out));
    }
}

/// Control-chord byte for a physical key, or None if the key has no tty
/// control encoding. Covers the full emacs-critical set the terminal
/// pane's letters-only map misses: C-SPC/C-@ (0x00, set-mark), C-[ (ESC),
/// C-\ (0x1c), C-] (0x1d), C-^ (0x1e), C-_ / C-/ (0x1f, undo). C-- also
/// maps to 0x1f, matching Terminal.app/iTerm2 so undo-on-C-- works.
fn ctrl_byte(code: KeyCode, shift: bool) -> Option<u8> {
    Some(match code {
        KeyCode::KeyA => 1,
        KeyCode::KeyB => 2,
        KeyCode::KeyC => 3,
        KeyCode::KeyD => 4,
        KeyCode::KeyE => 5,
        KeyCode::KeyF => 6,
        KeyCode::KeyG => 7,
        KeyCode::KeyH => 8,
        KeyCode::KeyI => 9,
        KeyCode::KeyJ => 10,
        KeyCode::KeyK => 11,
        KeyCode::KeyL => 12,
        KeyCode::KeyM => 13,
        KeyCode::KeyN => 14,
        KeyCode::KeyO => 15,
        KeyCode::KeyP => 16,
        KeyCode::KeyQ => 17,
        KeyCode::KeyR => 18,
        KeyCode::KeyS => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyU => 21,
        KeyCode::KeyV => 22,
        KeyCode::KeyW => 23,
        KeyCode::KeyX => 24,
        KeyCode::KeyY => 25,
        KeyCode::KeyZ => 26,
        KeyCode::Space | KeyCode::Digit2 => 0x00,
        KeyCode::BracketLeft => 0x1b,
        KeyCode::Backslash => 0x1c,
        KeyCode::BracketRight => 0x1d,
        KeyCode::Digit6 if shift => 0x1e,
        KeyCode::Slash | KeyCode::Minus => 0x1f,
        _ => return None,
    })
}

/// xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn xterm_mod(shift: bool, ctrl: bool, alt: bool) -> u8 {
    1 + (shift as u8) + ((alt as u8) << 1) + ((ctrl as u8) << 2)
}

/// CSI-encoded navigation key carrying modifier state, e.g. C-<right> →
/// `ESC [ 1 ; 5 C`, S-<prior> → `ESC [ 5 ; 2 ~`.
fn modified_nav_key(code: KeyCode, shift: bool, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let m = xterm_mod(shift, ctrl, alt);
    let seq = match code {
        KeyCode::ArrowUp => format!("\x1b[1;{m}A"),
        KeyCode::ArrowDown => format!("\x1b[1;{m}B"),
        KeyCode::ArrowRight => format!("\x1b[1;{m}C"),
        KeyCode::ArrowLeft => format!("\x1b[1;{m}D"),
        KeyCode::Home => format!("\x1b[1;{m}H"),
        KeyCode::End => format!("\x1b[1;{m}F"),
        KeyCode::Insert => format!("\x1b[2;{m}~"),
        KeyCode::Delete => format!("\x1b[3;{m}~"),
        KeyCode::PageUp => format!("\x1b[5;{m}~"),
        KeyCode::PageDown => format!("\x1b[6;{m}~"),
        _ => return None,
    };
    Some(seq.into_bytes())
}

fn plain_nav_key(code: KeyCode) -> Option<&'static [u8]> {
    Some(match code {
        KeyCode::Enter | KeyCode::NumpadEnter => b"\r",
        KeyCode::Tab => b"\t",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Escape => b"\x1b",
        KeyCode::Delete => b"\x1b[3~",
        KeyCode::Insert => b"\x1b[2~",
        KeyCode::PageUp => b"\x1b[5~",
        KeyCode::PageDown => b"\x1b[6~",
        KeyCode::ArrowUp => b"\x1b[A",
        KeyCode::ArrowDown => b"\x1b[B",
        KeyCode::ArrowRight => b"\x1b[C",
        KeyCode::ArrowLeft => b"\x1b[D",
        KeyCode::Home => b"\x1b[H",
        KeyCode::End => b"\x1b[F",
        _ => return None,
    })
}

/// Base US-QWERTY character for a physical key (see the Meta path above).
pub(crate) fn base_char(code: KeyCode, shift: bool) -> Option<char> {
    let ch = match code {
        KeyCode::KeyA => if shift { 'A' } else { 'a' },
        KeyCode::KeyB => if shift { 'B' } else { 'b' },
        KeyCode::KeyC => if shift { 'C' } else { 'c' },
        KeyCode::KeyD => if shift { 'D' } else { 'd' },
        KeyCode::KeyE => if shift { 'E' } else { 'e' },
        KeyCode::KeyF => if shift { 'F' } else { 'f' },
        KeyCode::KeyG => if shift { 'G' } else { 'g' },
        KeyCode::KeyH => if shift { 'H' } else { 'h' },
        KeyCode::KeyI => if shift { 'I' } else { 'i' },
        KeyCode::KeyJ => if shift { 'J' } else { 'j' },
        KeyCode::KeyK => if shift { 'K' } else { 'k' },
        KeyCode::KeyL => if shift { 'L' } else { 'l' },
        KeyCode::KeyM => if shift { 'M' } else { 'm' },
        KeyCode::KeyN => if shift { 'N' } else { 'n' },
        KeyCode::KeyO => if shift { 'O' } else { 'o' },
        KeyCode::KeyP => if shift { 'P' } else { 'p' },
        KeyCode::KeyQ => if shift { 'Q' } else { 'q' },
        KeyCode::KeyR => if shift { 'R' } else { 'r' },
        KeyCode::KeyS => if shift { 'S' } else { 's' },
        KeyCode::KeyT => if shift { 'T' } else { 't' },
        KeyCode::KeyU => if shift { 'U' } else { 'u' },
        KeyCode::KeyV => if shift { 'V' } else { 'v' },
        KeyCode::KeyW => if shift { 'W' } else { 'w' },
        KeyCode::KeyX => if shift { 'X' } else { 'x' },
        KeyCode::KeyY => if shift { 'Y' } else { 'y' },
        KeyCode::KeyZ => if shift { 'Z' } else { 'z' },
        KeyCode::Digit1 => if shift { '!' } else { '1' },
        KeyCode::Digit2 => if shift { '@' } else { '2' },
        KeyCode::Digit3 => if shift { '#' } else { '3' },
        KeyCode::Digit4 => if shift { '$' } else { '4' },
        KeyCode::Digit5 => if shift { '%' } else { '5' },
        KeyCode::Digit6 => if shift { '^' } else { '6' },
        KeyCode::Digit7 => if shift { '&' } else { '7' },
        KeyCode::Digit8 => if shift { '*' } else { '8' },
        KeyCode::Digit9 => if shift { '(' } else { '9' },
        KeyCode::Digit0 => if shift { ')' } else { '0' },
        KeyCode::Minus => if shift { '_' } else { '-' },
        KeyCode::Equal => if shift { '+' } else { '=' },
        KeyCode::BracketLeft => if shift { '{' } else { '[' },
        KeyCode::BracketRight => if shift { '}' } else { ']' },
        KeyCode::Backslash => if shift { '|' } else { '\\' },
        KeyCode::Semicolon => if shift { ':' } else { ';' },
        KeyCode::Quote => if shift { '"' } else { '\'' },
        KeyCode::Comma => if shift { '<' } else { ',' },
        KeyCode::Period => if shift { '>' } else { '.' },
        KeyCode::Slash => if shift { '?' } else { '/' },
        KeyCode::Backquote => if shift { '~' } else { '`' },
        _ => return None,
    };
    Some(ch)
}
