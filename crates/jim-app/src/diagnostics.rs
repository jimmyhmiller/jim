//! Memory post-mortem instrumentation.
//!
//! The GUI process has twice ballooned to ~180 GB and tripped macOS's
//! "your system has run out of application memory" guard. Static reading
//! didn't pin the culprit (the render hot-paths — snapshots, the cells
//! texture, the glyph atlas — are all bounded/in-place), so this module
//! exists to leave a definitive breadcrumb trail the *next* time it runs
//! away.
//!
//! Two independent recorders, both appending to
//! `~/.jim/diagnostics.log`:
//!
//! 1. A **background heartbeat thread** that logs only the process
//!    physical footprint every few seconds. It does not touch the Bevy
//!    `World`, so it keeps recording even if the main loop wedges (e.g.
//!    stuck holding the snapshot mutex) — giving us the memory-over-time
//!    curve no matter what.
//! 2. A **Bevy sampler system** that, on the same cadence, dumps the
//!    structural breakdown: entity count, every `Assets<T>` count, total
//!    `Image` bytes (+ the largest few), and per-terminal snapshot sizes.
//!    This is what tells us *which* collection grew.
//!
//! As the footprint crosses escalating thresholds (2 GiB, 4, 8, 16, …)
//! we emit a loud `WARN` line with the full breakdown and the top image
//! assets, so the log ends with an unambiguous "here's what was huge"
//! record right before the OOM.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::sprite::Anchor;

use jim_terminal::term_material::TermMaterial;
use jim_terminal::worker::SnapCell;
use jim_terminal::{MonoFont, TerminalStore, FONT_SIZE};
use crate::MENU_OVERLAY_LAYER;

/// How often both recorders sample. 5 s keeps the log small (a full day
/// is a few MB) while still catching a runaway that doubles in minutes.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// First footprint threshold that triggers a loud `WARN` dump; doubles
/// each time it's crossed (2 → 4 → 8 → … GiB). A healthy session sits
/// well under 2 GiB, so the first WARN already means something is wrong.
const FIRST_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Footprint at which the on-screen warning overlay appears (yellow). A
/// healthy session with several terminals sits around 1 GiB, so 4 GiB is
/// already clearly abnormal but leaves enormous headroom before the
/// ~180 GiB OOM — plenty of time to react.
const OVERLAY_WARN_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Footprint at which the overlay turns red and grows.
const OVERLAY_CRIT_BYTES: u64 = 16 * 1024 * 1024 * 1024;

const OVERLAY_MARGIN: f32 = 8.0;
/// Same layer/Z band as the FPS meter so it floats above panes + menus.
const OVERLAY_Z: f32 = 951.0;

/// How long the app may sit in winit `Continuous` (60fps, ~1.5 cores) for
/// a *transient* reason before the yellow "GPU pinned" bar appears. This
/// is a regression canary for the class of bug where a widget holds
/// `set_animating(true)` (or a Glaze transition / drawer never finishes)
/// and pins the whole app at full framerate forever. Real animations are
/// brief: garden growth is 8s, transitions <1s, even a busy back-to-back
/// agent session rarely keeps it continuous for a solid minute — so a full
/// minute of *uninterrupted* continuous means something is genuinely
/// stuck. Intentionally-sustained modes (an animated theme, an open
/// palette, the 3D prism) are excluded from the timer in
/// `maintain_winit_mode_for_animation`, so they never trip it.
/// See [[project_widget_slow_tick_decouple]].
const CONTINUOUS_WARN_SECS: f32 = 60.0;
/// Height of the warning bar across the top edge.
const PIN_BAR_HEIGHT: f32 = 26.0;
/// Clicking the bar dismisses it and suppresses it for this long, even if
/// the app stays pinned. After it elapses, a still-stuck pin re-raises the
/// bar (so a dismissal is a snooze, not a permanent mute).
const PIN_MUTE_SECS: f32 = 15.0 * 60.0;

pub struct DiagnosticsPlugin;

impl Plugin for DiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        spawn_footprint_heartbeat();
        install_panic_breadcrumb();
        append_log("[mem] ---- diagnostics started ----");
        app.init_resource::<MemReadout>()
            .init_resource::<ContinuousWatch>()
            .add_systems(
                Update,
                (
                    sample_memory,
                    update_warning_overlay,
                    dismiss_continuous_pin_on_click,
                    update_continuous_pin_overlay,
                )
                    .chain(),
            );
    }
}

/// How long (seconds) the app has been continuously in winit `Continuous`
/// for a transient reason, plus a human-readable reason string. Written by
/// `maintain_winit_mode_for_animation` each frame; read by
/// `update_continuous_pin_overlay`. `held_secs` resets to 0 the moment the
/// app drops back to reactive (or the only reason is an intentionally-
/// sustained one), so the bar self-clears.
#[derive(Resource, Default)]
pub struct ContinuousWatch {
    pub held_secs: f32,
    pub reason: String,
    /// Seconds remaining on the post-dismissal snooze. `> 0` hides the bar
    /// regardless of `held_secs`; counts down in
    /// `maintain_winit_mode_for_animation`. Set to `PIN_MUTE_SECS` when the
    /// user clicks the bar.
    pub mute_remaining_secs: f32,
}

/// Latest sample, published by `sample_memory` for the on-screen overlay
/// (and anything else that wants the current footprint without a syscall).
#[derive(Resource, Default)]
pub struct MemReadout {
    pub footprint: Option<u64>,
    pub rate_mib_per_min: f32,
}

/// Physical footprint in bytes — the number macOS Activity Monitor shows
/// as "Memory" and the one the OOM guard trips on. `None` if the syscall
/// fails or we're not on macOS.
#[cfg(target_os = "macos")]
pub fn phys_footprint_bytes() -> Option<u64> {
    // SAFETY: `proc_pid_rusage` with `RUSAGE_INFO_V2` fills a
    // `rusage_info_v2`. We hand it a zeroed buffer of exactly that type
    // (cast to the C `void**` the signature wants) and only read it back
    // on success.
    unsafe {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        let ret = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr() as *mut libc::rusage_info_t,
        );
        if ret != 0 {
            return None;
        }
        Some(info.assume_init().ri_phys_footprint)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn phys_footprint_bytes() -> Option<u64> {
    None
}

/// Cumulative CPU time (user + system) this process has consumed, in
/// nanoseconds. Differencing two readings over wall-clock time gives a
/// CPU% that matches Activity Monitor / `ps` (and can exceed 100% across
/// cores) — unlike Bevy's `SystemInformationDiagnosticsPlugin`, whose
/// per-process figure reads a flat 0 on this macOS setup.
#[cfg(target_os = "macos")]
pub fn process_cpu_time_ns() -> Option<u64> {
    // SAFETY: identical pattern to `phys_footprint_bytes` — fill a zeroed
    // `rusage_info_v2` and read it back only on success. `ri_user_time` /
    // `ri_system_time` are nanosecond CPU-time counters.
    unsafe {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        let ret = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr() as *mut libc::rusage_info_t,
        );
        if ret != 0 {
            return None;
        }
        let info = info.assume_init();
        Some(info.ri_user_time.wrapping_add(info.ri_system_time))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn process_cpu_time_ns() -> Option<u64> {
    None
}

/// Background thread: footprint-only, World-independent, wedge-proof.
fn spawn_footprint_heartbeat() {
    let _ = std::thread::Builder::new()
        .name("mem-heartbeat".into())
        .spawn(|| loop {
            std::thread::sleep(SAMPLE_INTERVAL);
            if let Some(fp) = phys_footprint_bytes() {
                append_log(&format!(
                    "[mem-hb] footprint={}B ({:.2} GiB)",
                    fp,
                    fp as f64 / GIB
                ));
            }
        });
}

#[derive(Default)]
pub struct DiagState {
    last_sample: Option<Instant>,
    prev_footprint: Option<u64>,
    prev_at: Option<Instant>,
    /// Next footprint (bytes) that triggers a loud WARN dump. 0 until
    /// initialized to `FIRST_WARN_BYTES`.
    next_warn: u64,
}

/// Exclusive sampler — needs broad read access to resources + entity
/// count, which is cleanest via `&mut World`.
fn sample_memory(world: &mut World, mut st: Local<DiagState>) {
    let now = Instant::now();
    if let Some(last) = st.last_sample {
        if now.duration_since(last) < SAMPLE_INTERVAL {
            return;
        }
    }
    st.last_sample = Some(now);
    // Exclusive system (&mut World) → runs ALONE, stalling every other thread
    // while it walks the entire world + all asset stores. Span the real work
    // (after the throttle early-return) so this periodic stall is visible.
    let _t_prof = jim_pane::prof::sys_span("sample_memory");
    if st.next_warn == 0 {
        st.next_warn = FIRST_WARN_BYTES;
    }

    let footprint = phys_footprint_bytes();

    // --- entity count (catch-all for leaked panes / sprites) ---
    let entity_count = world.query::<Entity>().iter(world).count();

    // --- asset stores ---
    let (img_count, img_bytes, top_imgs) = world
        .get_resource::<Assets<Image>>()
        .map(|images| {
            let mut total: u64 = 0;
            let mut sizes: Vec<(String, u64)> = Vec::new();
            for (id, img) in images.iter() {
                let bytes = img.data.as_ref().map_or(0, |d| d.len()) as u64;
                total += bytes;
                sizes.push((format!("{:?}", id), bytes));
            }
            sizes.sort_by(|a, b| b.1.cmp(&a.1));
            sizes.truncate(5);
            (images.len(), total, sizes)
        })
        .unwrap_or((0, 0, Vec::new()));

    let mesh_count = world.get_resource::<Assets<Mesh>>().map_or(0, |a| a.len());
    let colormat_count = world
        .get_resource::<Assets<ColorMaterial>>()
        .map_or(0, |a| a.len());
    let termmat_count = world
        .get_resource::<Assets<TermMaterial>>()
        .map_or(0, |a| a.len());
    let font_count = world.get_resource::<Assets<Font>>().map_or(0, |a| a.len());
    let layout_count = world
        .get_resource::<Assets<TextureAtlasLayout>>()
        .map_or(0, |a| a.len());

    // --- per-terminal snapshots ---
    let (term_count, snap_cells, snap_bytes) = world
        .get_resource::<TerminalStore>()
        .map(|store| {
            let mut cells: u64 = 0;
            for data in store.map.values() {
                // try_lock: never block the sampler on the worker, and
                // never deadlock if a worker is mid-publish.
                if let Ok(g) = data.worker.snapshot.try_lock() {
                    cells += g.cells.len() as u64;
                }
            }
            let bytes = cells * std::mem::size_of::<SnapCell>() as u64;
            (store.map.len(), cells, bytes)
        })
        .unwrap_or((0, 0, 0));

    // --- growth rate since last sample ---
    let mut rate_mib_min = 0.0f64;
    let rate_str = match (footprint, st.prev_footprint, st.prev_at) {
        (Some(fp), Some(prev), Some(prev_at)) => {
            let dt = now.duration_since(prev_at).as_secs_f64().max(0.001);
            let dmib = (fp as f64 - prev as f64) / MIB;
            rate_mib_min = dmib / dt * 60.0;
            format!(" d={:+.1}MiB/{:.0}s ({:+.2}MiB/min)", dmib, dt, rate_mib_min)
        }
        _ => String::new(),
    };

    // Publish for the on-screen overlay.
    if let Some(mut readout) = world.get_resource_mut::<MemReadout>() {
        readout.footprint = footprint;
        readout.rate_mib_per_min = rate_mib_min as f32;
    }

    // On-disk scrollback logs: a proxy for how much each terminal has
    // streamed. The GUI worker replays these into a libghostty Terminal
    // whose scrollback lives in plain (non-Bevy, non-GPU) heap we can't
    // size directly — so a ballooning footprint that tracks the total
    // scrollback bytes points the finger at libghostty.
    let (sb_count, sb_bytes) = scrollback_dir_size();

    let fp_str = footprint
        .map(|fp| format!("{:.2}GiB", fp as f64 / GIB))
        .unwrap_or_else(|| "n/a".into());

    // The headline number for post-mortem: footprint minus everything we
    // can actually account for. A large/growing `unaccounted` means the
    // leak is in native heap (libghostty workers) or GPU memory, NOT in
    // Bevy assets/entities — which is what the live data already hints.
    let accounted = img_bytes + snap_bytes;
    let unaccounted = footprint.map(|fp| fp.saturating_sub(accounted));
    let unacc_str = unaccounted
        .map(|u| format!("{:.2}GiB", u as f64 / GIB))
        .unwrap_or_else(|| "n/a".into());

    append_log(&format!(
        "[mem-detail] footprint={}{} unaccounted={} entities={} \
         images={}/{:.1}MiB meshes={} colormats={} termmats={} fonts={} \
         atlas_layouts={} terminals={} snap_cells={} snap={:.1}MiB \
         scrollback_logs={}/{:.1}MiB",
        fp_str,
        rate_str,
        unacc_str,
        entity_count,
        img_count,
        img_bytes as f64 / MIB,
        mesh_count,
        colormat_count,
        termmat_count,
        font_count,
        layout_count,
        term_count,
        snap_cells,
        snap_bytes as f64 / MIB,
        sb_count,
        sb_bytes as f64 / MIB,
    ));

    // --- escalating WARN with the smoking-gun breakdown ---
    if let Some(fp) = footprint {
        if fp >= st.next_warn {
            let top = top_imgs
                .iter()
                .map(|(id, b)| format!("{}={:.1}MiB", id, *b as f64 / MIB))
                .collect::<Vec<_>>()
                .join(", ");
            append_log(&format!(
                "[mem-WARN] footprint crossed {:.1}GiB! entities={} \
                 images={}/{:.1}MiB meshes={} colormats={} termmats={} \
                 terminals={} snap={:.1}MiB | largest images: [{}]",
                fp as f64 / GIB,
                entity_count,
                img_count,
                img_bytes as f64 / MIB,
                mesh_count,
                colormat_count,
                termmat_count,
                term_count,
                snap_bytes as f64 / MIB,
                top,
            ));
            // Advance past every threshold we've already blown through
            // so a sudden jump doesn't spam, but still re-arms higher.
            while fp >= st.next_warn {
                st.next_warn = st.next_warn.saturating_mul(2);
            }
        }
    }

    st.prev_footprint = footprint;
    st.prev_at = Some(now);
}

#[derive(Component)]
struct MemWarnOverlay;

/// Threshold-gated on-screen warning. Unlike the FPS meter (manual
/// toggle), this appears on its own once the footprint crosses
/// `OVERLAY_WARN_BYTES`, turns red + larger past `OVERLAY_CRIT_BYTES`,
/// and disappears again if memory recovers. Gives a heads-up long before
/// the OOM guard fires.
fn update_warning_overlay(
    mut commands: Commands,
    readout: Res<MemReadout>,
    font: Option<Res<MonoFont>>,
    windows: Query<&Window>,
    mut existing: Query<
        (
            Entity,
            &mut Text2d,
            &mut TextColor,
            &mut TextFont,
            &mut Transform,
        ),
        With<MemWarnOverlay>,
    >,
    mut was_showing: Local<bool>,
) {
    let show = readout
        .footprint
        .map(|fp| fp >= OVERLAY_WARN_BYTES)
        .unwrap_or(false);

    if show != *was_showing {
        *was_showing = show;
        let fp = readout.footprint.unwrap_or(0);
        append_log(&format!(
            "[mem-overlay] warning {} at {:.2}GiB",
            if show { "SHOWN" } else { "cleared" },
            fp as f64 / GIB,
        ));
    }

    if !show {
        for (e, ..) in &existing {
            commands.entity(e).despawn();
        }
        return;
    }

    let fp = readout.footprint.unwrap_or(0);
    let crit = fp >= OVERLAY_CRIT_BYTES;
    let color = if crit {
        Color::srgb(1.0, 0.25, 0.2)
    } else {
        Color::srgb(1.0, 0.8, 0.2)
    };
    let size = if crit { FONT_SIZE * 1.6 } else { FONT_SIZE * 1.2 };
    // ASCII only — the mono font has no warning glyph (would render tofu).
    let text = format!(
        "MEMORY HIGH: {:.1} GiB ({:+.0} MiB/min)",
        fp as f64 / GIB,
        readout.rate_mib_per_min,
    );

    let Ok(window) = windows.single() else {
        return;
    };
    // Top-left, so it doesn't collide with the top-right FPS meter.
    let x = -window.width() * 0.5 + OVERLAY_MARGIN;
    let y = window.height() * 0.5 - OVERLAY_MARGIN;

    if let Ok((_, mut t, mut tcolor, mut tfont, mut tx)) = existing.single_mut() {
        t.0 = text;
        tcolor.0 = color;
        tfont.font_size = FontSize::Px(size);
        tx.translation.x = x;
        tx.translation.y = y;
    } else {
        let Some(font) = font else { return };
        commands.spawn((
            MemWarnOverlay,
            Text2d::new(text),
            TextFont {
                font: (font.0.clone()).into(),
                font_size: FontSize::Px(size),
                ..default()
            },
            TextColor(color),
            Anchor::TOP_LEFT,
            Transform::from_xyz(x, y, OVERLAY_Z),
            RenderLayers::layer(MENU_OVERLAY_LAYER),
        ));
    }
}

#[derive(Component)]
struct ContinuousPinBar;
#[derive(Component)]
struct ContinuousPinText;

/// A left-click anywhere on the warning bar (the top `PIN_BAR_HEIGHT`px
/// strip, full width) dismisses it and arms the `PIN_MUTE_SECS` snooze.
/// Only fires while the bar is actually visible, so a normal click in the
/// top edge of the canvas isn't swallowed when there's no warning up.
fn dismiss_continuous_pin_on_click(
    mut watch: ResMut<ContinuousWatch>,
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    // Bar must be on screen to be clickable.
    if !(watch.held_secs > CONTINUOUS_WARN_SECS && watch.mute_remaining_secs <= 0.0) {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    // cursor_position: origin top-left, y grows downward — the bar occupies
    // the top strip, so y within [0, PIN_BAR_HEIGHT] is a hit.
    if cursor.y <= PIN_BAR_HEIGHT {
        watch.mute_remaining_secs = PIN_MUTE_SECS;
        append_log(&format!(
            "[pin-overlay] dismissed by click after {:.0}s — snoozing {:.0}min",
            watch.held_secs,
            PIN_MUTE_SECS / 60.0,
        ));
    }
}

/// The regression canary: a yellow bar across the top edge that appears
/// once the app has been pinned in `Continuous` (60fps) for a transient
/// reason longer than `CONTINUOUS_WARN_SECS`, and names the culprit (e.g.
/// `widget:garden`). Self-clears when the app drops back to reactive, and
/// a click dismisses it for `PIN_MUTE_SECS`. Mirrors the memory-warning
/// overlay's spawn/despawn-on-threshold shape.
fn update_continuous_pin_overlay(
    mut commands: Commands,
    watch: Res<ContinuousWatch>,
    font: Option<Res<MonoFont>>,
    windows: Query<&Window>,
    mut bars: Query<
        (Entity, &mut Sprite, &mut Transform),
        (With<ContinuousPinBar>, Without<ContinuousPinText>),
    >,
    mut texts: Query<
        (Entity, &mut Text2d, &mut Transform),
        (With<ContinuousPinText>, Without<ContinuousPinBar>),
    >,
    mut was_showing: Local<bool>,
) {
    let show = watch.held_secs > CONTINUOUS_WARN_SECS && watch.mute_remaining_secs <= 0.0;

    if show != *was_showing {
        *was_showing = show;
        append_log(&format!(
            "[pin-overlay] {} after {:.1}s continuous — reason: {}",
            if show { "SHOWN" } else { "cleared" },
            watch.held_secs,
            watch.reason,
        ));
    }

    if !show {
        for (e, ..) in &bars {
            commands.entity(e).despawn();
        }
        for (e, ..) in &texts {
            commands.entity(e).despawn();
        }
        return;
    }

    let Ok(window) = windows.single() else {
        return;
    };
    let w = window.width();
    let bar_y = window.height() * 0.5 - PIN_BAR_HEIGHT * 0.5;
    let yellow = Color::srgb(0.96, 0.75, 0.1);
    // ASCII only — the mono font has no warning glyph (would render tofu).
    let text = format!(
        "GPU PINNED CONTINUOUS {:.0}s  -  {}",
        watch.held_secs, watch.reason,
    );

    if let Ok((_, mut sprite, mut tx)) = bars.single_mut() {
        sprite.custom_size = Some(Vec2::new(w, PIN_BAR_HEIGHT));
        tx.translation.y = bar_y;
    } else {
        commands.spawn((
            ContinuousPinBar,
            Sprite {
                color: yellow,
                custom_size: Some(Vec2::new(w, PIN_BAR_HEIGHT)),
                ..default()
            },
            // Just behind the label, same overlay band as the FPS meter.
            Transform::from_xyz(0.0, bar_y, OVERLAY_Z - 0.1),
            RenderLayers::layer(MENU_OVERLAY_LAYER),
        ));
    }

    if let Ok((_, mut t, mut tx)) = texts.single_mut() {
        t.0 = text;
        tx.translation.y = bar_y;
    } else {
        let Some(font) = font else { return };
        commands.spawn((
            ContinuousPinText,
            Text2d::new(text),
            TextFont {
                font: (font.0.clone()).into(),
                font_size: FontSize::Px(FONT_SIZE),
                ..default()
            },
            // Dark text reads on yellow.
            TextColor(Color::srgb(0.12, 0.06, 0.0)),
            Anchor::CENTER,
            Transform::from_xyz(0.0, bar_y, OVERLAY_Z),
            RenderLayers::layer(MENU_OVERLAY_LAYER),
        ));
    }
}

/// Total size (and file count) of the per-terminal scrollback logs under
/// `~/.jim/scrollback/`. Cheap: directory metadata only.
fn scrollback_dir_size() -> (usize, u64) {
    let Some(dir) = crate::data_dir().map(|d| d.join("scrollback")) else {
        return (0, 0);
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (0, 0);
    };
    let mut count = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                count += 1;
                bytes += meta.len();
            }
        }
    }
    (count, bytes)
}

fn diag_log_path() -> Option<PathBuf> {
    crate::data_dir().map(|d| d.join("diagnostics.log"))
}

/// Append one timestamped line to the diagnostics log and mirror it to
/// stderr. Each line is written with a single `write_all`, so concurrent
/// writes from the heartbeat thread and the sampler stay line-atomic
/// under `O_APPEND`.
pub(crate) fn append_log(line: &str) {
    let epoch_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let full = format!("{} {}\n", epoch_ms, line);
    if let Some(p) = diag_log_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
        {
            let _ = f.write_all(full.as_bytes());
        }
    }
    eprint!("{}", full);
}

/// Route any unwinding panic into the diagnostics log before the default
/// hook runs. On a Dock launch stderr goes nowhere, so a panic on the main
/// loop or a background thread would otherwise leave no trace. Chains the
/// previous hook so normal panic output (and any abort) is unchanged.
fn install_panic_breadcrumb() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        append_log(&format!("[panic] {info}"));
        prev(info);
    }));
}
