//! Exercise the exact libghostty mouse path the worker uses in
//! `WorkerMsg::Mouse`: detect the child's tracking mode from DECSET
//! sequences, then encode a normalized event into wire bytes. This is the
//! part of terminal mouse support that can't be driven by a real click in
//! a headless test, so we pin the escape-sequence contract here.

use libghostty_vt::{
    key, mouse,
    terminal::Mode,
    Terminal, TerminalOptions,
};

const CELL_W: u32 = 8;
const CELL_H: u32 = 16;
const COLS: u16 = 80;
const ROWS: u16 = 24;

fn new_terminal() -> Terminal<'static, 'static> {
    Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: 0,
    })
    .expect("Terminal::new")
}

fn encoder_for(term: &Terminal<'static, 'static>, any_button: bool) -> mouse::Encoder<'static> {
    let mut enc = mouse::Encoder::new().expect("encoder");
    enc.set_options_from_terminal(term);
    enc.set_size(mouse::EncoderSize {
        screen_width: COLS as u32 * CELL_W,
        screen_height: ROWS as u32 * CELL_H,
        cell_width: CELL_W,
        cell_height: CELL_H,
        padding_top: 0,
        padding_bottom: 0,
        padding_right: 0,
        padding_left: 0,
    });
    enc.set_any_button_pressed(any_button);
    enc
}

/// Pixel position at the center of the 0-based cell `(col, row)`, matching
/// how the main thread hands content-local pixels to the worker.
fn cell_center(col: u16, row: u16) -> mouse::Position {
    mouse::Position {
        x: col as f32 * CELL_W as f32 + 1.0,
        y: row as f32 * CELL_H as f32 + 1.0,
    }
}

fn event(action: mouse::Action, button: Option<mouse::Button>, pos: mouse::Position) -> mouse::Event<'static> {
    let mut ev = mouse::Event::new().expect("event");
    ev.set_action(action);
    ev.set_button(button);
    ev.set_mods(key::Mods::empty());
    ev.set_position(pos);
    ev
}

#[test]
fn tracking_flag_follows_decset() {
    let mut term = new_terminal();
    assert!(!term.is_mouse_tracking().unwrap(), "no tracking by default");

    // Enable normal (1000) tracking + SGR (1006) format.
    term.vt_write(b"\x1b[?1000h\x1b[?1006h");
    assert!(term.is_mouse_tracking().unwrap(), "1000h turns tracking on");
    // 1000 is press/release only — no motion tracking.
    assert!(!term.mode(Mode::BUTTON_MOUSE).unwrap());
    assert!(!term.mode(Mode::ANY_MOUSE).unwrap());

    // Disabling returns to no tracking (mirrors the snapshot flag flipping
    // back so the main thread resumes local selection).
    term.vt_write(b"\x1b[?1000l");
    assert!(!term.is_mouse_tracking().unwrap(), "1000l turns tracking off");
}

#[test]
fn button_motion_mode_detected() {
    let mut term = new_terminal();
    term.vt_write(b"\x1b[?1002h\x1b[?1006h");
    assert!(term.is_mouse_tracking().unwrap());
    assert!(term.mode(Mode::BUTTON_MOUSE).unwrap(), "1002 = drag reporting");
}

#[test]
fn sgr_left_press_and_release_bytes() {
    let mut term = new_terminal();
    term.vt_write(b"\x1b[?1000h\x1b[?1006h");

    // Press left at cell (col=2, row=1). SGR is 1-based: 3;2. 'M' = press.
    let mut enc = encoder_for(&term, true);
    let mut out = Vec::new();
    enc.encode_to_vec(
        &event(mouse::Action::Press, Some(mouse::Button::Left), cell_center(2, 1)),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, b"\x1b[<0;3;2M", "SGR left press");

    // Release at the same cell — lowercase 'm' terminator in SGR.
    let mut out = Vec::new();
    enc.set_any_button_pressed(false);
    enc.encode_to_vec(
        &event(mouse::Action::Release, Some(mouse::Button::Left), cell_center(2, 1)),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, b"\x1b[<0;3;2m", "SGR left release");
}

#[test]
fn press_release_only_mode_drops_motion() {
    // In press/release-only tracking (1000), bare motion must not be
    // reported — the encoder yields nothing, matching the main thread's
    // decision to only ship motion when 1002/1003 is active.
    let mut term = new_terminal();
    term.vt_write(b"\x1b[?1000h\x1b[?1006h");

    let mut enc = encoder_for(&term, false);
    let mut out = Vec::new();
    enc.encode_to_vec(
        &event(mouse::Action::Motion, None, cell_center(5, 5)),
        &mut out,
    )
    .unwrap();
    assert!(out.is_empty(), "no motion report in 1000 mode, got {out:?}");
}
