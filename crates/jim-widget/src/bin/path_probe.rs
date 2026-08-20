//! Headless probe for `CanvasItem::Path` rendering through a per-pane camera.
//!
//! Bevy 0.19 silently drops blend-mode `ColorMaterial` `Mesh2d` on the
//! per-pane cameras (it renders fine on the global overlay cameras) — the
//! jim-whiteboard toolbar bug. `vector.rs` therefore flattens alpha and draws
//! opaque, and this probe is how we know that actually reaches the screen
//! rather than assuming it does.
//!
//! Spawns a real widget pane, pushes Path items through the SAME
//! `render_canvas_items` entry the subprocess canvas path uses, screenshots
//! the window, and prints the color found at a few known-inside-the-shape
//! sample points so the result is checkable without eyeballing a PNG:
//!
//!   cargo run --release -p jim_widget --bin path_probe -- --out /tmp/path.png
//!
//! The window is created VISIBLE but UNFOCUSED: an invisible window presents
//! nothing on macOS, so the screenshot comes back solid black and proves
//! nothing — but a visible one must not steal focus from whatever the user is
//! doing. It closes itself after the capture.

use std::path::PathBuf;
use std::process::ExitCode;

use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy::window::{ExitCondition, WindowPlugin, WindowResolution};
use jim_pane::{PanePlugin, PaneRect};
use jim_widget::WidgetPlugin;
use jim_widget::protocol::{CanvasItem, PathCap, PathJoin};

/// Pane content box, and where the pane sits in the window.
const PANE_POS: Vec2 = Vec2::new(40.0, 40.0);
const PANE_SIZE: Vec2 = Vec2::new(520.0, 320.0);

/// Sample points in CANVAS coordinates plus the color each must come back as.
/// Kept well inside each shape so a half-pixel of antialiasing can't flip the
/// result.
struct Sample {
    what: &'static str,
    /// Canvas coordinates — the probe converts to window pixels itself.
    at: Vec2,
    expect: Expect,
}

enum Expect {
    /// Exactly this color (within a tolerance for antialiasing).
    Color([u8; 3]),
    /// This color at this alpha, composited over whatever the `Background`
    /// sample reads — which is also the check that `flatten_alpha` agrees with
    /// what real blending would have produced.
    FlattenedOverBackground([u8; 3], u8),
    /// Whatever the pane paints when nothing is drawn on it.
    Background,
}

const SAMPLES: &[Sample] = &[
    Sample {
        what: "filled triangle interior",
        at: Vec2::new(60.0, 60.0),
        expect: Expect::Color([0xE0, 0x5A, 0x3C]),
    },
    Sample {
        what: "stroked polyline",
        at: Vec2::new(300.0, 40.0),
        expect: Expect::Color([0x4C, 0x8F, 0xD8]),
    },
    Sample {
        what: "donut arc band",
        at: Vec2::new(432.0, 62.0),
        expect: Expect::Color([0x6F, 0xB0, 0x7A]),
    },
    Sample {
        what: "area fill, 25% alpha flattened over the pane background",
        at: Vec2::new(210.0, 145.0),
        expect: Expect::FlattenedOverBackground([0x3C, 0x7A, 0xE0], 0x40),
    },
    // The control: bare pane, no path. Also the background the sample above
    // is expected to have been composited against.
    Sample {
        what: "pane background (control)",
        at: Vec2::new(210.0, 250.0),
        expect: Expect::Background,
    },
];

#[derive(Resource)]
struct ProbeConfig {
    out_path: PathBuf,
    wait_frames: u32,
}

#[derive(Resource, Default)]
struct ProbeState {
    frames: u32,
    fired: bool,
}

fn items() -> Vec<CanvasItem> {
    let path =
        |id: &str, d: &str, fill: Option<&str>, stroke: Option<&str>, w: f32| CanvasItem::Path {
            id: id.to_string(),
            d: d.to_string(),
            fill: fill.map(str::to_string),
            stroke: stroke.map(str::to_string),
            stroke_width: w,
            cap: PathCap::Round,
            join: PathJoin::Round,
            bg: None,
            z: 0.0,
        };
    vec![
        // A plain closed fill.
        path(
            "tri",
            "M 20 20 L 110 20 L 65 110 Z",
            Some("#e05a3c"),
            None,
            0.0,
        ),
        // An area fill with alpha — the case that forced opaque materials.
        path(
            "area",
            "M 150 190 L 150 120 C 190 90 230 170 270 110 L 270 190 Z",
            Some("#3c7ae040"),
            None,
            0.0,
        ),
        // An open stroked polyline: what df_view_line.ft fakes with rects.
        path(
            "line",
            "M 290 30 L 330 70 L 370 20 L 410 60",
            None,
            Some("#4c8fd8"),
            18.0,
        ),
        // Arc commands, i.e. a donut band.
        path(
            "donut",
            "M 400 90 A 60 60 0 0 1 460 30 L 460 60 A 30 30 0 0 0 430 90 Z",
            Some("#6fb07a"),
            None,
            0.0,
        ),
        // Malformed data must be reported, not drawn — watch stderr for it.
        path("broken", "M 0 0 X 9", Some("#ffffff"), None, 0.0),
    ]
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut out_path = PathBuf::from("/tmp/path_probe.png");
    let mut wait_frames = 30u32;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => {
                if let Some(p) = args.next() {
                    out_path = PathBuf::from(p);
                }
            }
            "--frames" => {
                if let Some(n) = args.next().and_then(|s| s.parse().ok()) {
                    wait_frames = n;
                }
            }
            other => {
                eprintln!("[path-probe] unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }

    let mut app = App::new();
    let win_w = (PANE_SIZE.x + 2.0 * PANE_POS.x) as u32;
    let win_h = (PANE_SIZE.y + 2.0 * PANE_POS.y) as u32;
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "path-probe".into(),
            resolution: WindowResolution::new(win_w, win_h),
            // See the module docs: visible (or the capture is black) but
            // never focused (or it steals the user's keyboard).
            visible: true,
            focused: false,
            position: bevy::window::WindowPosition::At(IVec2::new(60, 60)),
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
    // The widget host reads the embedded-editor messages (Element::Editor), so
    // those message types have to exist even when no editor is on screen —
    // without them a widget system fails param validation and the app dies
    // before the capture frame.
    app.add_message::<jim_editor::EmbeddedEditorPress>()
        .add_message::<jim_editor::EmbeddedEditorDrag>()
        .add_message::<jim_editor::EmbeddedEditorRelease>()
        .add_message::<jim_editor::EmbeddedEditorScroll>()
        .add_message::<jim_editor::EmbeddedEditorSubmit>();
    app.add_plugins(PanePlugin {
        reserved_layers: vec![32],
    })
    .add_plugins(WidgetPlugin);

    app.insert_resource(ProbeConfig {
        out_path,
        wait_frames,
    })
    .init_resource::<ProbeState>()
    .add_systems(Startup, setup.after(jim_editor::setup_editor_font))
    .add_systems(Update, capture);

    app.run();
    ExitCode::SUCCESS
}

fn setup(world: &mut World) {
    world.spawn(Camera2d);
    if world.get_resource::<jim_pane::PaneFontMetrics>().is_none() {
        world.insert_resource(jim_pane::PaneFontMetrics {
            cell_width: 8.4,
            font_size: 14.0,
        });
    }

    let rect = PaneRect {
        pos: PANE_POS,
        size: PANE_SIZE,
        z: 0.5,
    };
    let spawned = jim_pane::spawn_pane(world, jim_widget::PANE_KIND, "path_probe", rect, None);
    let root = spawned.content_root;

    let surface = Color::from(
        world
            .resource::<jim_style::Theme>()
            .color(jim_style::tokens::PANE_BG),
    );
    let font = world.resource::<jim_pane::PaneFont>().0.clone();
    let fonts = world.resource::<jim_style::FontRegistry>().clone();
    let items = items();

    // `resource_scope` lifts `Assets<Image>` out so `render_canvas_items` can
    // take it by &mut while the command queue still borrows the world.
    world.resource_scope(|world, mut images: Mut<Assets<Image>>| {
        let mut queue = bevy::ecs::world::CommandQueue::default();
        {
            let mut commands = Commands::new(&mut queue, world);
            let mut cache = jim_widget::WidgetImageCache::default();
            jim_widget::render_canvas_items(
                &mut commands,
                &mut images,
                &mut cache,
                root,
                &items,
                Vec2::ZERO,
                0.5,
                &font,
                &fonts,
                surface,
            );
        }
        queue.apply(world);
    });

    eprintln!(
        "[path-probe] pane at {:?} size {:?}, {} items",
        PANE_POS,
        PANE_SIZE,
        items.len()
    );
}

fn capture(mut commands: Commands, mut state: ResMut<ProbeState>, config: Res<ProbeConfig>) {
    state.frames += 1;
    if state.fired || state.frames < config.wait_frames {
        return;
    }
    state.fired = true;
    let out = config.out_path.clone();
    commands.spawn(Screenshot::primary_window()).observe(
        move |captured: On<ScreenshotCaptured>, mut exit: MessageWriter<AppExit>| {
            let img = match captured.image.clone().try_into_dynamic() {
                Ok(d) => d.to_rgb8(),
                Err(e) => {
                    eprintln!("[path-probe] could not decode the screenshot: {e}");
                    exit.write(AppExit::error());
                    return;
                }
            };
            if let Err(e) = img.save(&out) {
                eprintln!("[path-probe] could not write {}: {e}", out.display());
            }
            // The framebuffer is in physical pixels; samples are in logical
            // ones (retina doubles them).
            let scale = img.width() as f32 / (PANE_SIZE.x + 2.0 * PANE_POS.x);
            let sample = |at: Vec2| -> [u8; 3] {
                let win = PANE_POS
                    + Vec2::new(jim_pane::MARGIN, jim_pane::TITLE_H + jim_pane::MARGIN)
                    + at;
                let px = (win * scale).as_uvec2();
                img.get_pixel(px.x.min(img.width() - 1), px.y.min(img.height() - 1))
                    .0
            };
            let background = SAMPLES
                .iter()
                .find(|s| matches!(s.expect, Expect::Background))
                .map(|s| sample(s.at))
                .unwrap_or([0, 0, 0]);

            let mut failed = 0;
            for s in SAMPLES {
                let got = sample(s.at);
                let want = match s.expect {
                    Expect::Color(c) => c,
                    Expect::Background => background,
                    Expect::FlattenedOverBackground(c, a) => {
                        let src = Color::srgb_u8(c[0], c[1], c[2]).with_alpha(a as f32 / 255.0);
                        let dst = Color::srgb_u8(background[0], background[1], background[2]);
                        let flat = jim_widget::flatten_alpha_for_probe(src, dst).to_srgba();
                        [
                            (flat.red * 255.0).round() as u8,
                            (flat.green * 255.0).round() as u8,
                            (flat.blue * 255.0).round() as u8,
                        ]
                    }
                };
                // Tolerance covers antialiasing and the 8-bit round trip.
                let ok = got
                    .iter()
                    .zip(want.iter())
                    .all(|(g, w)| (*g as i16 - *w as i16).abs() <= 8);
                if !ok {
                    failed += 1;
                }
                eprintln!(
                    "  {:<6} {:<52} got #{:02x}{:02x}{:02x} want #{:02x}{:02x}{:02x}",
                    if ok { "ok" } else { "FAILED" },
                    s.what,
                    got[0],
                    got[1],
                    got[2],
                    want[0],
                    want[1],
                    want[2]
                );
            }
            if failed == 0 {
                eprintln!("[path-probe] all {} samples matched", SAMPLES.len());
                exit.write(AppExit::Success);
            } else {
                eprintln!("[path-probe] {failed} sample(s) did not render as expected");
                exit.write(AppExit::error());
            }
        },
    );
}
