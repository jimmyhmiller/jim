//! Headless widget snapshot tool.
//!
//! Spins up a minimal Bevy app with the same plugins the GUI uses, but
//! with a hidden window. Spawns a single widget pane running the given
//! command, waits enough frames for it to emit a frame + load any
//! background-image assets, then writes a screenshot to disk and exits.
//!
//! Usage:
//!   widget-snapshot --out atelier.png --cmd ./target/release/atelier
//!                   [--size 1100x1200] [--frames 180]
//!
//! Lets us iterate on widget visuals without popping windows or
//! capturing the desktop.

use std::path::PathBuf;
use std::process::ExitCode;

use bevy::prelude::*;
use bevy::app::AppExit;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::window::{ExitCondition, WindowPlugin, WindowResolution};
use jim_pane::{PanePlugin, PaneRect};
use jim_widget::WidgetPlugin;

#[derive(Resource)]
struct SnapshotConfig {
    cmd: String,
    args: Vec<String>,
    out_path: PathBuf,
    size: Vec2,
    title: String,
    wait_frames: u32,
    /// If set, force this `Select`'s dropdown open before snapshotting (so the
    /// floating overlay is visible in the static image).
    open_select: Option<String>,
    /// If true, force the first `Tooltip` shown before snapshotting.
    show_tooltip: bool,
    /// If set, force this `Popover` open before snapshotting.
    open_popover: Option<String>,
    /// If set, send a `HostEvent::Toggle { id, checked: true }` to the widget
    /// `toggle_lead` frames before the snapshot — captures a `transition`
    /// animation mid-flight.
    toggle: Option<String>,
    toggle_lead: u32,
}

/// The single widget pane the snapshot spawned (for forcing a select open).
#[derive(Resource)]
struct SnapshotPane(bevy::prelude::Entity);

#[derive(Resource, Default)]
struct SnapshotState {
    frames_seen: u32,
    fired: bool,
    written: bool,
}

fn parse_size(s: &str) -> Option<Vec2> {
    let (w, h) = s.split_once('x')?;
    Some(Vec2::new(w.parse().ok()?, h.parse().ok()?))
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut cmd: Option<String> = None;
    let mut out_path: Option<PathBuf> = None;
    let mut size = Vec2::new(1100.0, 1200.0);
    let mut title = "Widget".to_string();
    let mut wait_frames: u32 = 180;
    let mut subprocess_args: Vec<String> = Vec::new();
    let mut open_select: Option<String> = None;
    let mut show_tooltip = false;
    let mut open_popover: Option<String> = None;
    let mut toggle: Option<String> = None;
    let mut toggle_lead: u32 = 10;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out_path = args.next().map(PathBuf::from),
            "--cmd" => cmd = args.next(),
            "--open-select" => open_select = args.next(),
            "--open-popover" => open_popover = args.next(),
            "--show-tooltip" => show_tooltip = true,
            "--toggle" => toggle = args.next(),
            "--toggle-lead" => {
                if let Some(n) = args.next().and_then(|s| s.parse().ok()) {
                    toggle_lead = n;
                }
            }
            "--size" => {
                if let Some(s) = args.next().and_then(|s| parse_size(&s)) {
                    size = s;
                }
            }
            "--title" => {
                if let Some(t) = args.next() {
                    title = t;
                }
            }
            "--frames" => {
                if let Some(n) = args.next().and_then(|s| s.parse().ok()) {
                    wait_frames = n;
                }
            }
            "--" => {
                subprocess_args = args.collect();
                break;
            }
            other => {
                eprintln!("widget-snapshot: unknown flag {:?}", other);
                return ExitCode::from(2);
            }
        }
    }

    let Some(cmd) = cmd else {
        eprintln!(
            "widget-snapshot: --cmd <program> required\n\
             usage: widget-snapshot --out out.png --cmd ./target/release/atelier"
        );
        return ExitCode::from(2);
    };
    let out_path = out_path.unwrap_or_else(|| PathBuf::from("snapshot.png"));

    let config = SnapshotConfig {
        cmd,
        args: subprocess_args,
        out_path,
        size,
        title,
        wait_frames,
        open_select,
        show_tooltip,
        open_popover,
        toggle,
        toggle_lead,
    };

    let mut app = App::new();
    let win_w = (config.size.x + 60.0) as u32;
    let win_h = (config.size.y + 60.0) as u32;
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "widget-snapshot".into(),
            resolution: WindowResolution::new(win_w, win_h),
            visible: false,
            ..default()
        }),
        exit_condition: ExitCondition::DontExit,
        close_when_requested: true,
        ..default()
    }));

    // Editor-bevy's font setup pushes a PaneFont resource that pane-bevy
    // requires. It also initializes the FontRegistry idempotently.
    app.init_resource::<jim_style::Theme>()
        .init_resource::<jim_style::StyleErrors>()
        // ChromeThemePlugin's apply_per_project_chrome reads these; the main app
        // inits them via StylePlugin, which the snapshot tool doesn't add.
        .init_resource::<jim_style::ProjectThemes>()
        .init_resource::<jim_style::ProjectStyleState>()
        .init_resource::<jim_style::StylePresetRegistry>()
        .add_message::<jim_style::ThemeChanged>()
        .add_plugins(jim_style::theme::ThemePlugin)
        .add_plugins(jim_style::FontRegistryPlugin)
        .add_plugins(jim_style::chrome_theme::ChromeThemePlugin)
        .add_systems(Startup, jim_editor::setup_editor_font);

    // Widget plugin's `forward_claude_events` reads a ClaudeBusEvent
    // message; the snapshot tool doesn't connect to the bus but we
    // still need to register the message type so the reader doesn't
    // panic on first run.
    app.add_message::<claude_bus_bevy::ClaudeBusEvent>();

    // Reserve the overlay layer (32) so no pane camera claims it, matching how
    // terminal-bevy reserves MENU_OVERLAY_LAYER for floating content.
    app.add_plugins(PanePlugin {
        reserved_layers: vec![32],
    })
    .add_plugins(WidgetPlugin);

    app.insert_resource(config)
        .init_resource::<SnapshotState>()
        .add_systems(Startup, setup_camera_and_pane.after(jim_editor::setup_editor_font))
        .add_systems(
            Update,
            (
                take_snapshot_when_ready,
                force_open_select,
                force_show_tooltip,
                force_open_popover,
                force_toggle,
            ),
        );

    app.run();
    ExitCode::SUCCESS
}

fn setup_camera_and_pane(world: &mut World) {
    let (size, cmd, args, title) = {
        let c = world.resource::<SnapshotConfig>();
        (c.size, c.cmd.clone(), c.args.clone(), c.title.clone())
    };

    // 2D camera. spawn_pane derives initial position from window size,
    // so it'll center the pane in the window automatically.
    world.spawn(Camera2d);

    // Floating-overlay camera: renders only layer 32 above everything (matches
    // terminal-bevy's menu-overlay camera), so widget dropdowns are visible.
    world.spawn((
        Camera2d,
        bevy::camera::Camera {
            order: 100_000,
            clear_color: bevy::camera::ClearColorConfig::None,
            ..Default::default()
        },
        bevy::camera::visibility::RenderLayers::layer(32),
    ));

    // PaneFontMetrics — pane-bevy needs this resource for layout. Use
    // JetBrains Mono at 14 px (~8.4 px cell). jim_editor's setup_*
    // doesn't insert it; the GUI host does it directly.
    if world.get_resource::<jim_pane::PaneFontMetrics>().is_none() {
        world.insert_resource(jim_pane::PaneFontMetrics {
            cell_width: 8.4,
            font_size: 14.0,
        });
    }

    // Spawn the pane near the window center. position_panes uses
    // window size to clamp; pos here is roughly where we want the
    // top-left corner in window coords (top-left origin).
    let rect = PaneRect {
        pos: Vec2::new(30.0, 30.0),
        size,
        z: 0.5,
    };
    let spawned = jim_pane::spawn_pane(world, jim_widget::PANE_KIND, &title, rect, None);

    use jim_widget::*;
    let bundle = (
        Widget::new(cmd.clone(), args.clone(), None),
        WidgetRender::default(),
        WidgetTargets::default(),
        WidgetContentRoot(spawned.content_root),
        WidgetScroll::default(),
    );

    world.insert_resource(SnapshotPane(spawned.entity));

    // Spawn the widget subprocess and attach its IO components.
    match jim_widget::spawn_widget_process(&cmd, &args, None) {
        Ok((process, io)) => {
            world.entity_mut(spawned.entity).insert(bundle).insert((process, io));
        }
        Err(e) => {
            eprintln!("widget-snapshot: spawn_widget_process failed: {}", e);
            world.entity_mut(spawned.entity).insert(bundle);
        }
    }
}

/// Once the widget has rendered (so its `Select` target exists), force the
/// requested dropdown open so the floating overlay shows in the snapshot.
fn force_open_select(
    config: Res<SnapshotConfig>,
    state: Res<SnapshotState>,
    pane: Option<Res<SnapshotPane>>,
    mut open: ResMut<jim_widget::WidgetOpenSelect>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let (Some(id), Some(pane)) = (config.open_select.as_ref(), pane) else {
        return;
    };
    if state.frames_seen < 40 {
        return;
    }
    open.0 = Some(jim_widget::OpenSelect {
        pane: pane.0,
        id: id.clone(),
    });
    *done = true;
}

/// Force the requested `Popover` open before snapshotting.
fn force_open_popover(
    config: Res<SnapshotConfig>,
    state: Res<SnapshotState>,
    pane: Option<Res<SnapshotPane>>,
    mut open: ResMut<jim_widget::WidgetOpenPopover>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let (Some(id), Some(pane)) = (config.open_popover.as_ref(), pane) else {
        return;
    };
    if state.frames_seen < 40 {
        return;
    }
    open.0 = Some(jim_widget::OpenSelect {
        pane: pane.0,
        id: id.clone(),
    });
    *done = true;
}

/// Force the first rendered `Tooltip` shown (headless has no cursor to hover).
fn force_show_tooltip(
    config: Res<SnapshotConfig>,
    state: Res<SnapshotState>,
    pane: Option<Res<SnapshotPane>>,
    targets: Query<&jim_widget::WidgetTargets>,
    mut active: ResMut<jim_widget::ActiveTooltip>,
    mut done: Local<bool>,
) {
    if *done || !config.show_tooltip {
        return;
    }
    let Some(pane) = pane else {
        return;
    };
    if state.frames_seen < 40 {
        return;
    }
    if let Ok(t) = targets.get(pane.0) {
        if let Some(tip) = t.tooltips.first() {
            active.0 = Some(jim_widget::ActiveTip {
                pane: pane.0,
                anchor: tip.anchor,
                text: tip.text.clone(),
                style: tip.style.clone(),
            });
            *done = true;
        }
    }
}

/// Send a real `toggle` host event shortly before the capture so a Glaze
/// `transition checked …` is photographed mid-flight.
fn force_toggle(
    config: Res<SnapshotConfig>,
    state: Res<SnapshotState>,
    pane: Option<Res<SnapshotPane>>,
    io: Query<&jim_widget::WidgetIO>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let (Some(id), Some(pane)) = (config.toggle.as_ref(), pane) else {
        return;
    };
    if state.frames_seen + config.toggle_lead < config.wait_frames {
        return;
    }
    if let Ok(io) = io.get(pane.0) {
        let evt = serde_json::to_string(&jim_widget::protocol::HostEvent::Toggle {
            id: id.clone(),
            checked: true,
        })
        .expect("HostEvent::Toggle serializes");
        let _ = io.tx.send(evt);
        eprintln!("widget-snapshot: sent toggle {:?}", id);
        *done = true;
    }
}

fn take_snapshot_when_ready(
    mut commands: Commands,
    mut state: ResMut<SnapshotState>,
    config: Res<SnapshotConfig>,
    mut exit: MessageWriter<AppExit>,
) {
    state.frames_seen += 1;
    if state.fired {
        // Give the screenshot pipeline a few frames to actually write
        // the file before we exit.
        if state.frames_seen.saturating_sub(config.wait_frames) > 30 {
            exit.write(AppExit::Success);
        }
        return;
    }
    if state.frames_seen < config.wait_frames {
        return;
    }
    let out = config.out_path.clone();
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(out.clone()));
    eprintln!("widget-snapshot: saving {:?}", out);
    state.fired = true;
    state.written = true;
}
