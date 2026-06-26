//! Persist + restore the OS-window size and position across runs.
//!
//! On startup, [`load`] reads `~/.jim/window.json` (if any)
//! and returns the saved geometry so `main.rs` can seed the initial
//! `Window` resolution + position.
//!
//! At runtime, [`save_on_change`] listens for `WindowResized` and
//! `WindowMoved` events; when one fires it writes the current geometry
//! back to disk. Writes are debounced so a continuous drag doesn't
//! hammer the filesystem.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::{
    Monitor, PrimaryMonitor, PrimaryWindow, WindowCloseRequested, WindowMoved, WindowResized,
};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "window.json";
const WRITE_DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// The geometry loaded from disk at startup, captured into a resource so
/// [`fit_window_to_monitor`] can re-apply it once the real OS scale
/// factor is known — *before* [`save_on_change`] can overwrite the file
/// with the (wrongly-clamped) creation-time size. `main.rs` inserts this.
#[derive(Resource, Default, Clone, Copy)]
pub struct RestoredGeometry(pub Option<WindowGeometry>);

fn path() -> Option<std::path::PathBuf> {
    crate::data_dir().map(|d| d.join(FILE_NAME))
}

/// Read the saved geometry, if any. Returns `None` on first run, on
/// IO error, or if the file is malformed (so the caller falls back to
/// hard-coded defaults).
pub fn load() -> Option<WindowGeometry> {
    let p = path()?;
    let body = std::fs::read_to_string(&p).ok()?;
    let g: WindowGeometry = serde_json::from_str(&body).ok()?;
    // Drop obviously-degenerate values (window minimized to 0, etc.)
    // — falling back to defaults is friendlier than restoring a
    // window the user can't see.
    if g.w < 200 || g.h < 150 {
        return None;
    }
    Some(g)
}

fn write(g: &WindowGeometry) {
    let Some(p) = path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string(g) {
        let _ = std::fs::write(&p, body);
    }
}

/// Constrain a saved window rect to a monitor (all in physical pixels,
/// winit's top-left origin). Size is preserved whenever it fits — the
/// window is *shifted* to stay fully on-screen rather than shrunk; it's
/// only clamped smaller when genuinely larger than the usable area. This
/// is what fixes "resized all the way to the right, size not kept after
/// restart": the saved position then put the right edge off-screen, and
/// macOS clamped the window *narrower* on restore. `top_inset` reserves
/// space for the menu bar so the window lands below it.
fn fit_to_monitor(
    g: WindowGeometry,
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
    top_inset: u32,
) -> WindowGeometry {
    let avail_h = mon_h.saturating_sub(top_inset);
    let w = g.w.min(mon_w);
    let h = g.h.min(avail_h);
    let min_x = mon_x;
    let max_x = (mon_x + mon_w as i32 - w as i32).max(min_x);
    let min_y = mon_y + top_inset as i32;
    let max_y = (mon_y + mon_h as i32 - h as i32).max(min_y);
    WindowGeometry {
        x: g.x.clamp(min_x, max_x),
        y: g.y.clamp(min_y, max_y),
        w,
        h,
    }
}

/// Once the primary monitor is known, pull the (restored) window fully
/// on-screen at its saved size. Runs once: monitors aren't populated
/// until a frame or two after startup, so it waits for one to appear.
/// Only acts on windows positioned with `WindowPosition::At` (i.e.
/// restored from disk) — a first-run auto-placed window is left alone.
pub fn fit_window_to_monitor(
    mut done: Local<bool>,
    restored: Res<RestoredGeometry>,
    monitors: Query<(&Monitor, Has<PrimaryMonitor>)>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    if *done {
        return;
    }
    let Some(saved) = restored.0 else {
        *done = true; // first run / no saved geometry — let the OS place it
        return;
    };
    // Prefer the primary monitor; fall back to the first one enumerated.
    // `single()` would silently bail when winit reports 0 or 2+ monitors.
    let monitor = monitors
        .iter()
        .find(|(_, primary)| *primary)
        .or_else(|| monitors.iter().next())
        .map(|(m, _)| m);
    let Some(monitor) = monitor else {
        return; // monitors not enumerated yet — try again next frame
    };
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    *done = true;

    // Re-apply the SAVED geometry now that the real scale factor is known.
    // At window-creation the scale defaulted to 1.0, so winit requested
    // the size in logical units at the wrong scale and macOS clamped the
    // window to `monitor - position` — that's why a window resized toward
    // the right always came back at one fixed (narrower) width. Setting
    // physical size + physical position here, post-scale, restores it.
    let top_inset = (monitor.scale_factor * 38.0) as u32;
    let fitted = fit_to_monitor(
        saved,
        monitor.physical_position.x,
        monitor.physical_position.y,
        monitor.physical_width,
        monitor.physical_height,
        top_inset,
    );
    eprintln!("[window-geom] restored window to {:?} (monitor {}x{})", fitted, monitor.physical_width, monitor.physical_height);
    window
        .resolution
        .set_physical_resolution(fitted.w, fitted.h);
    window.position = WindowPosition::At(IVec2::new(fitted.x, fitted.y));
}

/// Local state for the debounce. `pending` carries the latest geometry
/// the user has nudged toward; `last_write_at` gates how often we
/// actually flush to disk.
#[derive(Default)]
pub struct SaveState {
    pending: Option<WindowGeometry>,
    last_write_at: Option<Instant>,
}

pub fn save_on_change(
    mut resized: MessageReader<WindowResized>,
    mut moved: MessageReader<WindowMoved>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut state: Local<SaveState>,
) {
    let mut dirty = false;
    for _ in resized.read() {
        dirty = true;
    }
    for _ in moved.read() {
        dirty = true;
    }

    if dirty {
        if let Ok(window) = windows.single() {
            let pos = match window.position {
                WindowPosition::At(p) => p,
                _ => IVec2::ZERO,
            };
            // Save PHYSICAL pixels — Bevy 0.18's
            // `WindowResolution::from((u32, u32))` calls
            // `WindowResolution::new(physical_width, physical_height)`,
            // so the restore path treats these as physical.
            state.pending = Some(WindowGeometry {
                x: pos.x,
                y: pos.y,
                w: window.resolution.physical_width(),
                h: window.resolution.physical_height(),
            });
        }
    }

    let Some(g) = state.pending else { return };
    let now = Instant::now();
    let should_write = state
        .last_write_at
        .is_none_or(|t| now.duration_since(t) >= WRITE_DEBOUNCE);
    if !should_write {
        return;
    }
    write(&g);
    state.pending = None;
    state.last_write_at = Some(now);
}

/// Respawn the primary window the instant it's despawned, so closing the
/// laptop lid / sleeping the display doesn't leave the app windowless.
///
/// On macOS, sleeping the display removes the monitor; bevy_winit despawns
/// the `Monitor` entity, and because `Monitor` is the `linked_spawn`
/// target of the window's `OnMonitor` relationship, that despawn cascades
/// and takes the primary window down with it (confirmed by backtrace).
/// Paired with `ExitCondition::DontExit` (set in `main.rs`) the app no
/// longer quits when that happens — but it would be left with no window.
///
/// This observer fires synchronously, *before* the entity is fully gone,
/// the moment a `PrimaryWindow` loses its `Window` component, and queues a
/// fresh primary window in the same command flush. So the `Window` entity
/// is recreated before the next schedule run — no system ever observes
/// zero windows (avoids `single()` panics) — and bevy_winit creates the OS
/// window again as soon as a display is back. The new window carries no
/// `OnMonitor` yet, so the in-flight monitor-despawn cascade can't reach
/// it. Cameras render to `WindowRef::Primary`, so they re-bind to it
/// automatically. Size/position are restored from the last saved geometry.
pub fn respawn_primary_window_on_loss(
    removed: On<Remove, Window>,
    mut commands: Commands,
    primaries: Query<(), With<PrimaryWindow>>,
) {
    // Only react to the *primary* window going away.
    if !primaries.contains(removed.event().entity) {
        return;
    }
    let saved = load();
    let (w, h) = saved.map(|g| (g.w, g.h)).unwrap_or((1200, 760));
    let position = saved
        .map(|g| WindowPosition::At(IVec2::new(g.x, g.y)))
        .unwrap_or_default();
    commands.spawn((
        Window {
            title: "Jim".into(),
            resolution: (w, h).into(),
            position,
            ..default()
        },
        PrimaryWindow,
    ));
    crate::diagnostics::append_log("[window] primary window lost (display sleep?) — respawned");
}

/// Make the window's red close button (and Cmd-W / Window→Close) actually
/// quit the app — while a display sleep / lid close still does not.
///
/// `main.rs` sets `close_when_requested: false` + `ExitCondition::DontExit`
/// so the lid-close monitor-despawn cascade can't take the app down (see
/// [`respawn_primary_window_on_loss`]). The side effect was that an explicit
/// user close request became inert — the red button did nothing.
///
/// The two cases are cleanly separable by the signal each produces:
/// - **Red button / Cmd-W**: winit emits [`WindowCloseRequested`] for the
///   primary window. That's a genuine "the user wants to quit" request.
/// - **Display sleep / lid close**: the monitor is removed and the `Window`
///   is despawned through the `OnMonitor` `linked_spawn` relationship. That
///   path emits **no** `WindowCloseRequested` at all — the old sleep-quit
///   came from `ExitCondition::OnAllClosed` auto-generating `AppExit` once
///   zero windows remained, which is exactly what `DontExit` now prevents.
///
/// So honoring a `WindowCloseRequested` for the primary window here restores
/// the close button without resurrecting the sleep-quit bug. We send
/// [`AppExit`] directly, which exits regardless of `ExitCondition::DontExit`
/// (that condition only governs the *automatic* exit-on-window-loss path,
/// not an explicit `AppExit`).
pub fn quit_on_close_request(
    mut requested: MessageReader<WindowCloseRequested>,
    primaries: Query<(), With<PrimaryWindow>>,
    mut exit: MessageWriter<AppExit>,
) {
    // Only the primary window's close button quits; a stray close request
    // for any other (transient) window must not take the whole app down.
    if requested
        .read()
        .any(|ev| primaries.contains(ev.window))
    {
        exit.write(AppExit::Success);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real numbers from the reported bug: 3456x2234 display, window saved
    // wider-than-fits at the saved x so its right edge ran off-screen.
    #[test]
    fn right_resized_window_keeps_width_by_shifting_left() {
        let g = WindowGeometry { x: 442, y: 78, w: 3228, h: 2328 };
        let f = fit_to_monitor(g, 0, 0, 3456, 2234, 0);
        // Width fits (3228 <= 3456) → preserved, not shrunk.
        assert_eq!(f.w, 3228, "width preserved");
        // Shifted left so the right edge sits exactly at the screen edge.
        assert_eq!(f.x, 3456 - 3228);
        assert_eq!(f.x + f.w as i32, 3456);
        // Height exceeded the screen → clamped to fit.
        assert_eq!(f.h, 2234);
        assert_eq!(f.y, 0);
    }

    #[test]
    fn already_fitting_window_is_unchanged() {
        let g = WindowGeometry { x: 100, y: 100, w: 1200, h: 800 };
        assert_eq!(fit_to_monitor(g, 0, 0, 3456, 2234, 0), g);
    }

    #[test]
    fn top_inset_pushes_window_below_menu_bar() {
        let g = WindowGeometry { x: 0, y: 0, w: 1000, h: 3000 };
        let f = fit_to_monitor(g, 0, 0, 3456, 2234, 76);
        assert_eq!(f.y, 76, "y clamped below the reserved menu-bar inset");
        assert_eq!(f.h, 2234 - 76, "height clamped to the usable area");
    }

    #[test]
    fn respects_nonzero_monitor_origin() {
        // Secondary monitor to the right of the primary.
        let g = WindowGeometry { x: 5000, y: 50, w: 800, h: 600 };
        let f = fit_to_monitor(g, 3456, 0, 1920, 1080, 0);
        assert!(f.x >= 3456 && f.x + f.w as i32 <= 3456 + 1920);
    }
}
