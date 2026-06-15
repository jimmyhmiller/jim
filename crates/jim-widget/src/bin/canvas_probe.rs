//! Headless probe for canvas sprite anchoring + pane-camera clipping.
//!
//! Spawns a real pane (so the per-pane viewport-clip camera is in play)
//! and drops raw colored sprites as children of its content_root — the
//! same way `script_widget::diff_render` emits canvas Rects — to isolate
//! how `Anchor::BOTTOM_LEFT` vs `Anchor::TOP_LEFT` tall rects render and
//! clip, with NO funct/scroll/extent code involved.
//!
//! Two "bars" of identical geometry (canvas rows 34..baseline):
//!   - LEFT  : Anchor::BOTTOM_LEFT at y = baseline      (grows up)
//!   - RIGHT : Anchor::TOP_LEFT    at y = baseline - h  (grows down to baseline)
//! If they render differently, the bug is in sprite/anchor/clip, not funct.
//!
//!   cargo run --release -p jim_app --bin canvas_probe -- --out /tmp/probe.png

use std::path::PathBuf;
use std::process::ExitCode;

use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::sprite::Anchor;
use bevy::window::{ExitCondition, WindowPlugin, WindowResolution};
use jim_pane::{PanePlugin, PaneRect};
use jim_widget::WidgetPlugin;

#[derive(Resource)]
struct ProbeConfig {
    out_path: PathBuf,
    size: Vec2,
    wait_frames: u32,
}

#[derive(Resource, Default)]
struct ProbeState {
    frames: u32,
    fired: bool,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut out_path = PathBuf::from("/tmp/probe.png");
    let mut size = Vec2::new(560.0, 300.0);
    let mut wait_frames = 60u32;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => {
                if let Some(p) = args.next() {
                    out_path = PathBuf::from(p);
                }
            }
            "--size" => {
                if let Some((w, h)) = args.next().and_then(|s| {
                    let (a, b) = s.split_once('x')?;
                    Some((a.parse().ok()?, b.parse().ok()?))
                }) {
                    size = Vec2::new(w, h);
                }
            }
            "--frames" => {
                if let Some(n) = args.next().and_then(|s| s.parse().ok()) {
                    wait_frames = n;
                }
            }
            _ => {}
        }
    }

    let mut app = App::new();
    let win_w = (size.x + 80.0) as u32;
    let win_h = (size.y + 80.0) as u32;
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "canvas-probe".into(),
            resolution: WindowResolution::new(win_w, win_h),
            visible: false,
            ..default()
        }),
        exit_condition: ExitCondition::DontExit,
        close_when_requested: true,
        ..default()
    }));

    app.init_resource::<jim_style::Theme>()
        .init_resource::<jim_style::StyleErrors>()
        .init_resource::<jim_style::ProjectThemes>()
        .init_resource::<jim_style::ProjectStyleState>()
        .init_resource::<jim_style::StylePresetRegistry>()
        .add_message::<jim_style::ThemeChanged>()
        .add_plugins(jim_style::theme::ThemePlugin)
        .add_plugins(jim_style::FontRegistryPlugin)
        .add_plugins(jim_style::chrome_theme::ChromeThemePlugin)
        .add_systems(Startup, jim_editor::setup_editor_font);
    app.add_message::<claude_bus_bevy::ClaudeBusEvent>();
    app.add_plugins(PanePlugin {
        reserved_layers: vec![32],
    })
    .add_plugins(WidgetPlugin);

    app.insert_resource(ProbeConfig {
        out_path,
        size,
        wait_frames,
    })
    .init_resource::<ProbeState>()
    .add_systems(
        Startup,
        setup.after(jim_editor::setup_editor_font),
    )
    .add_systems(Update, capture);

    app.run();
    ExitCode::SUCCESS
}

fn setup(world: &mut World) {
    let size = world.resource::<ProbeConfig>().size;
    world.spawn(Camera2d);
    if world.get_resource::<jim_pane::PaneFontMetrics>().is_none() {
        world.insert_resource(jim_pane::PaneFontMetrics {
            cell_width: 8.4,
            font_size: 14.0,
        });
    }

    let rect = PaneRect {
        pos: Vec2::new(40.0, 40.0),
        size,
        z: 0.5,
    };
    let spawned = jim_pane::spawn_pane(world, jim_widget::PANE_KIND, "canvas_probe", rect, None);
    let root = spawned.content_root;

    // OVERFLOW PROBE: a full-pane magenta sprite parented to the PANE (not
    // content_root, so clip_widget_sprites doesn't shrink it) at z just
    // below the cover (0.25). It would paint the entire pane incl. all
    // margins; the content-cover should now mask it down to the content
    // rect, leaving the title strip + margin ring as chrome. If magenta
    // bleeds into the bottom/side margins, the cover fix failed.
    world.spawn((
        ChildOf(spawned.entity),
        Sprite {
            color: Color::srgba(0.9, 0.2, 0.6, 0.9),
            custom_size: Some(size),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(0.0, 0.0, 0.2),
        Visibility::Inherited,
    ));

    // Geometry matching df_view_vbars: pad_t=34, pad_b=26.
    let pad_t = 34.0f32;
    let pad_b = 26.0f32;
    let content_h = size.y - jim_pane::TITLE_H - 2.0 * jim_pane::MARGIN;
    let plot_h = content_h - pad_t - pad_b;
    let baseline = pad_t + plot_h;
    let bar_w = 40.0f32;

    // Bottom-left bars (the natural vbars form) at increasing heights, all
    // on the baseline. With the clip_widget_sprites anchor fix these should
    // render at their true heights (50,100,150,200), growing UP.
    let mut x = 20.0;
    for (i, hh) in [50.0f32, 100.0, 150.0, 200.0].iter().enumerate() {
        world.spawn((
            ChildOf(root),
            Sprite {
                color: Color::srgb(0.85, 0.7, 0.3),
                custom_size: Some(Vec2::new(bar_w, *hh)),
                ..default()
            },
            Anchor::BOTTOM_LEFT,
            Transform::from_xyz(x, -baseline, 1.0),
            Visibility::Inherited,
        ));
        eprintln!("[probe] bottom-left bar {i}: x={x} h={hh} (expect renders at true height)");
        x += 60.0;
    }

    // Bottom-center, full plot_h, to compare (garden plants use this).
    world.spawn((
        ChildOf(root),
        Sprite {
            color: Color::srgb(0.5, 0.85, 0.4),
            custom_size: Some(Vec2::new(bar_w, plot_h)),
            ..default()
        },
        Anchor::BOTTOM_CENTER,
        Transform::from_xyz(x + bar_w * 0.5, -baseline, 1.0),
        Visibility::Inherited,
    ));
    x += 80.0;

    // TOP-LEFT at baseline-h, full plot_h (working form, reference).
    world.spawn((
        ChildOf(root),
        Sprite {
            color: Color::srgb(0.3, 0.7, 0.85),
            custom_size: Some(Vec2::new(bar_w, plot_h)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(x, -(baseline - plot_h), 1.0),
        Visibility::Inherited,
    ));

    // CONTROL: a bottom-left bar as a WORLD-ROOT entity (NOT under
    // content_root, no pane camera) — rendered by the main Camera2d with
    // no pane viewport clip. If THIS renders full height, the clamp comes
    // from the pane context; if short, it's intrinsic to the sprite.
    world.spawn((
        Sprite {
            color: Color::srgb(0.9, 0.4, 0.6),
            custom_size: Some(Vec2::new(bar_w, plot_h)),
            ..default()
        },
        Anchor::BOTTOM_LEFT,
        Transform::from_xyz(-300.0, -120.0, 1.0),
        Visibility::Visible,
    ));

    // A thin baseline marker + a top marker so we can see the plot bounds.
    world.spawn((
        ChildOf(root),
        Sprite {
            color: Color::srgb(0.5, 0.5, 0.5),
            custom_size: Some(Vec2::new(content_h.max(size.x), 2.0)),
            ..default()
        },
        Anchor::CENTER_LEFT,
        Transform::from_xyz(0.0, -baseline, 0.5),
        Visibility::Inherited,
    ));

    eprintln!(
        "[probe] content_h={content_h:.1} plot_h={plot_h:.1} baseline={baseline:.1} \
         LEFT=bottom-left@y={baseline:.1} RIGHT=top-left@y={:.1} h={plot_h:.1}",
        baseline - plot_h
    );
}

fn capture(
    mut commands: Commands,
    mut state: ResMut<ProbeState>,
    config: Res<ProbeConfig>,
    mut exit: MessageWriter<AppExit>,
) {
    state.frames += 1;
    if state.fired {
        if state.frames.saturating_sub(config.wait_frames) > 20 {
            exit.write(AppExit::Success);
        }
        return;
    }
    if state.frames < config.wait_frames {
        return;
    }
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(config.out_path.clone()));
    eprintln!("[probe] saving {:?}", config.out_path);
    state.fired = true;
}
