//! Application shell for the Jim editor: the floating-pane canvas that
//! hosts the terminal emulator (now extracted to the `jim_terminal`
//! crate), the text editor, widgets, projects sidebar, cube overview,
//! radial menu, drawer, and IPC.
//!
//! The terminal widget itself lives in `jim_terminal`; this crate adds
//! `jim_terminal::TerminalPlugin` and keeps only the shell integration
//! glue that couples the terminal to project state (scroll into the
//! active project, bell / Claude-notification badge pulses).

use std::path::PathBuf;

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;

use jim_pane::{
    AnimatedChromePane, ChromeAnimates, PaneKindMarker, PanePlugin, PaneRect, PaneTag,
};
use serde_json::Value;

use jim_terminal::worker::WorkerMsg;
use jim_terminal::{
    pt_to_cell, BellPulse, MonoMetrics, TerminalSession, TerminalStore, LINE_HEIGHT,
    PANE_KIND,
};

pub mod actions;
pub mod agent;
pub mod canvas;
pub mod canvas_pane;
pub mod command_palette;
pub mod claude_events_pane;
pub mod context_menu;
pub mod cube;
pub mod debug_bar;
pub mod diagnostics;
pub mod drawer;
pub mod expose;
pub mod fps;
pub mod graph_view;
pub mod inference_dispatch;
pub mod inbox;
pub mod inferences_pane;
pub mod ipc_stats;
pub mod issues_pane;
/// Re-export of the daemon protocol from the headless crate so existing
/// callers can continue to write `jim_app::daemon_proto::*`.
pub use jim_daemon::proto as daemon_proto;
pub mod ipc;
pub mod projects;
pub mod radial;
pub mod run_button;
pub mod pane_annotation;
pub mod render_trace;
pub mod screenshot_consent;
pub mod tools;
pub mod whiteboard_bg;
pub mod window_geometry;
pub mod workflow_graph;
use projects::{
    NewPaneRequest, OpenFileRequest, OpenProjectTarget, PendingActions, ProjectMembership,
    Projects, Sidebar,
};

/// Root for all on-disk persistence (projects + per-terminal scrollback).
/// `~/.jim/` on every supported platform.
///
/// Delegates to `jim_daemon::data_dir` so the daemon process and
/// the editor process agree on the location of socket / pid files.
pub fn data_dir() -> Option<PathBuf> {
    jim_daemon::data_dir()
}

/// Directory holding the running `jim` executable. Both `cargo build` and
/// the `.app` bundle co-locate our sibling binaries (`jimctl`, `glaze_ui`,
/// …) next to `jim` — `target/release/` in dev, `Contents/MacOS/` in the
/// bundle — so this is the one place to resolve them without baking the
/// builder's absolute paths into the binary (which break on another Mac).
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// Dedicated RenderLayer for menu overlays (radial menu, per-pane
/// context menu) so they draw on top of every per-pane camera. Pane
/// cameras have order `(rect.z * 100) + 1`, which can climb past 600
/// as panes are focused — anything drawn on layer 0 ends up *under*
/// those pane cameras inside their viewports, which made the radial
/// vanish behind panes. The overlay camera (see [`setup_camera`])
/// runs at order [`MENU_OVERLAY_CAMERA_ORDER`] (well above any pane)
/// and renders only this layer, so menu items are guaranteed on top.
pub const MENU_OVERLAY_LAYER: usize = 32;
/// Camera order for the menu-overlay camera. Sized so it stays above
/// any plausible pane-camera order: pane cameras max out around
/// `(MAX_PANE_Z * 100) + 1` ≈ 50_001, so 100_000 leaves headroom.
pub const MENU_OVERLAY_CAMERA_ORDER: isize = 100_000;

/// Dedicated RenderLayer for the canvas whiteboard drawing. The drawing is an
/// overlay you paint ON TOP of panes, so its geometry lives here and is drawn
/// by [`WHITEBOARD_OVERLAY_CAMERA_ORDER`] — above every per-pane camera, but
/// below the toolbar (pinned higher, see `pin_toolbar_z`) and the menu overlay.
pub const WHITEBOARD_OVERLAY_LAYER: usize = 31;
/// Camera order for the whiteboard overlay camera. Above any pane
/// (~50_001) and below the toolbar (~85_001) + menu overlay (100_000).
pub const WHITEBOARD_OVERLAY_CAMERA_ORDER: isize = 80_000;

/// Whether our OS window currently has keyboard focus. Mirrors the
/// `WindowFocused` events winit dispatches; we maintain it ourselves
/// rather than polling `Window::focused` because (at least on
/// macOS / Bevy 0.18) the field doesn't always reflect app-level
/// activation changes when the user Cmd+Tabs to another app.
///
/// Defaults to true — first frame the user is presumably looking at
/// us; a `WindowFocused(false)` will arrive if not.
#[derive(Resource)]
pub struct AppFocused(pub bool);

impl Default for AppFocused {
    fn default() -> Self {
        Self(true)
    }
}

// ---------- Plugin ----------

/// The app-shell plugin. Adds `jim_terminal::TerminalPlugin` for the
/// terminal widget, then registers every shell plugin (pane chrome,
/// projects, canvas, cube, radial, drawer, …), the shell camera setup,
/// global actions, and the shell-coupled glue systems.
pub struct AppShellPlugin;

impl Plugin for AppShellPlugin {
    fn build(&self, app: &mut App) {
        // Terminal widget crate: GPU material, selection, font/atlas
        // startup, terminal pane kind + per-frame terminal systems.
        app.add_plugins(jim_terminal::TerminalPlugin);
        // Emacs pane kind: tty frames on a shared `emacs --daemon=jim`,
        // rendered through the terminal crate's VT-grid pipeline.
        app.add_plugins(jim_emacs::EmacsPlugin);
        // Install the shell-coupling seams the terminal spawn/restore
        // path calls through (session-id allocator, initial cwd, dirty
        // hook) so jim_terminal stays free of a jim_app dependency.
        app.insert_resource(jim_terminal::TerminalIdAllocator(Box::new(|world| {
            world.resource_mut::<Projects>().allocate_terminal_id()
        })));
        app.insert_resource(jim_terminal::TerminalInitialCwd(Box::new(|world, entity| {
            world
                .get::<ProjectMembership>(entity)
                .map(|m| m.0)
                .and_then(|pid| {
                    world
                        .get_resource::<Projects>()
                        .and_then(|p| p.default_cwd_of(pid).map(str::to_string))
                })
        })));
        app.insert_resource(jim_terminal::TerminalDirtyHook(Box::new(|world| {
            world.resource_mut::<Projects>().terminals_dirty = true;
        })));
        // Flame-graph widget crate: registers the "flame" pane kind and the
        // generic flame-bevy offscreen-render + CPU readback systems. The
        // rendered trace is shown as a Sprite filling the pane content.
        app.add_plugins(jim_flame::FlamePlugin);
        // Render-thread phase timing into jim's trace (extract/prepare/queue/
        // passes), so render cost stops being an anonymous "untracked gap".
        app.add_plugins(render_trace::RenderTracePlugin);
        // Channel the async (non-modal) Open dialog hands chosen paths
        // back on. `action_open_file` clones the sender and `await`s the
        // sheet off the main thread; `drain_file_picks` routes results
        // into `PendingActions`. NonSend because the mpsc ends are `!Sync`
        // and both touchpoints run on the main thread.
        {
            let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
            app.insert_non_send_resource(FilePickChannel { tx, rx });
        }
        app.insert_resource(ClearColor(Color::srgb(0.072, 0.075, 0.085)))
            .insert_resource(AppFocused::default())
            .insert_resource(bevy::winit::WinitSettings {
                focused_mode: bevy::winit::UpdateMode::reactive(
                    std::time::Duration::from_secs(5),
                ),
                unfocused_mode: bevy::winit::UpdateMode::reactive_low_power(
                    std::time::Duration::from_secs(60),
                ),
            })
            // Pane-bevy owns chrome (drag, resize, close, focus,
            // hit-test). Terminal-specific systems below register the
            // "terminal" kind, render the grid, and handle keyboard +
            // mouse-driven selection inside the content area.
            // Reserve EVERY layer that a non-pane, non-project-scoped
            // camera renders, so no pane is ever allocated one. A collision
            // draws that pane's content across every project (and over the
            // cube), because that global camera isn't gated by project:
            //   - MENU_OVERLAY_LAYER (32): menus / FPS / status bar.
            //   - WHITEBOARD_OVERLAY_LAYER (31): the canvas drawing overlay.
            //   - cube::CUBE_LAYER (4096): the prism's structural geometry.
            //   - jim_style::dynamic::OVERLAY_LAYER (30): the dust/shader
            //     canvas overlay, drawn at order 1_000_001 above everything.
            // This is the single registry of global layers; anyone adding a
            // global overlay camera MUST add its layer here. See
            // `PaneLayerAllocator`.
            .add_plugins(PanePlugin {
                reserved_layers: vec![
                    MENU_OVERLAY_LAYER,
                    WHITEBOARD_OVERLAY_LAYER,
                    cube::CUBE_LAYER,
                    jim_style::dynamic::OVERLAY_LAYER,
                    jim_pane::dock::DOCK_OVERLAY_LAYER,
                ],
            })
            .add_plugins(jim_pane::DockPlugin)
            .add_plugins(diagnostics::DiagnosticsPlugin)
            .add_plugins(projects::ProjectsPlugin)
            .add_plugins(actions::ActionsPlugin)
            .add_plugins(canvas::CanvasPlugin)
            .add_plugins(canvas_pane::CanvasPanePlugin)
            .add_plugins(context_menu::ContextMenuPlugin)
            .add_plugins(cube::CubePlugin)
            .add_plugins(expose::ExposePlugin)
            .add_plugins(radial::RadialPlugin)
            .add_plugins(command_palette::CommandPalettePlugin)
            .add_plugins(screenshot_consent::ScreenshotConsentPlugin)
            .add_plugins(drawer::DrawerPlugin)
            .add_plugins(run_button::RunButtonPlugin)
            .add_plugins(jim_whiteboard::WhiteboardPlugin)
            .add_plugins(whiteboard_bg::WhiteboardBackgroundPlugin)
            // DISABLED: the per-pane annotation surface was a THIRD input
            // authority. For a draw tool it claimed every press (it "paints
            // anywhere" and set InputConsumed) WITHOUT excluding the toolbar,
            // so toolbar clicks were eaten whenever a draw tool was active
            // (couldn't switch tools). It also made ink stick to / move with
            // individual panes, which contradicts the fixed drawing-layer model
            // (ink must not move with panes). The canvas board
            // (WhiteboardBackgroundPlugin) is the single drawing layer; it
            // already renders over panes via the overlay camera.
            // .add_plugins(pane_annotation::PaneAnnotationPlugin)
            .add_plugins(workflow_graph::WorkflowGraphPlugin)
            .add_plugins(fps::FpsOverlayPlugin)
            .add_plugins(debug_bar::DebugBarPlugin)
            .add_plugins(claude_events_pane::ClaudeEventsPanePlugin)
            .add_plugins(inferences_pane::InferencesPanePlugin)
            .add_plugins(issues_pane::IssuesPanePlugin)
            .add_plugins(inbox::InboxPanePlugin)
            .add_plugins(inference_dispatch::InferenceDispatchPlugin)
            .add_plugins(jim_widget::WidgetPlugin)
            .add_plugins(jim_widget::script_widget::ScriptWidgetPlugin)
            .add_plugins(jim_editor::EditorEmbedPlugin)
            .add_plugins(jim_style::StylePlugin)
            .add_plugins(jim_style::state::ProjectStatePlugin);
        // Bespoke (non-pane) actions. Pane-spawn actions are auto-
        // generated from the `PaneRegistry` at PostStartup; these are the
        // capabilities that aren't "spawn a pane kind". Each was formerly
        // a hand-rolled keyboard-shortcut system.
        use actions::{Action, ActionRun, AppActionsExt, KeyChord};
        app.add_action(Action {
            id: "file.open",
            title: "Open File…",
            category: "File",
            keywords: &["edit", "buffer"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd(KeyCode::KeyO)] },
            run: ActionRun::Custom(action_open_file),
        })
        .add_action(Action {
            id: "view.dev_panel",
            title: "Style Dev Panel",
            category: "View",
            keywords: &["debug", "tokens"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd_shift(KeyCode::KeyD)] },
            run: ActionRun::Custom(action_open_dev_panel),
        })
        .add_action(Action {
            id: "view.theme_editor",
            title: "Theme Editor",
            category: "View",
            keywords: &["color", "oklch", "palette"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd_shift(KeyCode::KeyT)] },
            run: ActionRun::Custom(action_open_theme_editor),
        })
        .add_action(Action {
            id: "view.style_picker",
            title: "Styles",
            category: "View",
            keywords: &["preset", "theme", "skin"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd_shift(KeyCode::KeyS)] },
            run: ActionRun::Custom(action_open_style_picker),
        })
        .add_action(Action {
            id: "view.chess",
            title: "Chess",
            category: "View",
            keywords: &["game", "stockfish", "board", "uci"],
            radial_icon: None,
            default_keys: &[],
            run: ActionRun::Custom(action_open_chess),
        })
        .add_action(Action {
            id: "view.toggle_cube",
            title: "Toggle Project Cube",
            category: "View",
            keywords: &["prism", "3d", "overview", "switch"],
            // Eligible for the radial ring — proof the ring now hosts
            // non-pane actions. `cube.rs` keeps its own ⌘⇧\ single-chord
            // toggle; here we add a *sequence* binding (⌘K then C) — a
            // different chord, so no double-toggle — to exercise the
            // chord-sequence matcher in-tree.
            radial_icon: Some("◧"),
            default_keys: const { &[KeyChord::cmd(KeyCode::KeyK), KeyChord::plain(KeyCode::KeyC)] },
            run: ActionRun::Custom(action_toggle_cube),
        })
        // ----- Pane / view control (formerly nothing — new global chords) -----
        .add_action(Action {
            id: "style.glaze_ui_showcase",
            title: "Glaze UI Showcase",
            category: "Style",
            keywords: &["design", "components", "gallery", "atelier"],
            radial_icon: None,
            default_keys: const { &[] },
            run: ActionRun::Custom(action_open_glaze_ui),
        })
        .add_action(Action {
            id: "canvas.cycle_pan_preset",
            title: "Cycle Canvas Pan Preset",
            category: "View",
            keywords: &["trackpad", "scroll", "drag", "gesture", "pan"],
            radial_icon: None,
            // Deliberately unbound: its old Cmd+Shift+P chord collided
            // with the palette-open key, so every palette open silently
            // advanced the preset (eventually onto one with
            // trackpad_scroll off — killing cmd+scroll).
            default_keys: const { &[] },
            run: ActionRun::Custom(action_cycle_pan_preset),
        })
        .add_action(Action {
            id: "pane.close_focused",
            title: "Close Focused Pane",
            category: "Pane",
            keywords: &["kill", "dismiss"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd(KeyCode::KeyW)] },
            run: ActionRun::Custom(action_close_focused),
        })
        .add_action(Action {
            id: "pane.focus_next",
            title: "Focus Next Pane",
            category: "Pane",
            keywords: &["cycle", "switch"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd(KeyCode::BracketRight)] },
            run: ActionRun::Custom(action_focus_next),
        })
        .add_action(Action {
            id: "pane.focus_prev",
            title: "Focus Previous Pane",
            category: "Pane",
            keywords: &["cycle", "switch"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd(KeyCode::BracketLeft)] },
            run: ActionRun::Custom(action_focus_prev),
        })
        .add_action(Action {
            id: "view.zoom_in",
            title: "Zoom In",
            category: "View",
            keywords: &["scale", "magnify"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd(KeyCode::Equal)] },
            run: ActionRun::Custom(|ctx| canvas::zoom_active(ctx.world, 1.1)),
        })
        .add_action(Action {
            id: "view.zoom_out",
            title: "Zoom Out",
            category: "View",
            keywords: &["scale", "shrink"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd(KeyCode::Minus)] },
            run: ActionRun::Custom(|ctx| canvas::zoom_active(ctx.world, 1.0 / 1.1)),
        })
        .add_action(Action {
            id: "view.zoom_reset",
            title: "Reset Zoom",
            category: "View",
            keywords: &["scale", "100%", "actual size"],
            radial_icon: None,
            default_keys: const { &[KeyChord::cmd(KeyCode::Digit0)] },
            run: ActionRun::Custom(|ctx| canvas::zoom_reset_active(ctx.world)),
        })
        .add_action(Action {
            id: "keybinds.reload",
            title: "Reload Keybindings",
            category: "View",
            keywords: &["hotkey", "shortcut", "rebind", "config"],
            radial_icon: None,
            default_keys: &[],
            run: ActionRun::Custom(|ctx| actions::rebuild_keymap(ctx.world)),
        })
        .add_action(Action {
            id: "project.set_cwd_from_terminal",
            title: "Set Project Default Dir from Focused Terminal",
            category: "Project",
            keywords: &["cwd", "directory", "default", "terminal", "working"],
            radial_icon: None,
            default_keys: &[],
            run: ActionRun::Custom(action_set_project_cwd_from_focused),
        });
        app
            .add_systems(
                Startup,
                (
                    setup_camera,
                    // Runs after the terminal crate's `setup_terminal_font`
                    // so its `PaneFont` / `PaneFontMetrics` (the themed
                    // JetBrains mono used by every cosmic-text pane)
                    // deterministically replace the terminal's SF Mono
                    // defaults as a matched pair. Without the ordering, only
                    // one of the two resources might win and the caret grid
                    // would drift from the rendered text.
                    jim_editor::setup_editor_font.after(jim_terminal::setup_terminal_font),
                    setup_ipc_listener,
                    request_microphone_access,
                    // Register NSWorkspace activation observers so
                    // `track_app_focus` reads a cached atomic instead of
                    // making a synchronous XPC `frontmostApplication` call
                    // every frame. See `install_app_focus_observers`.
                    install_app_focus_observers,
                ),
            )
            .add_systems(
                Update,
                (
                    mirror_active_project_to_style,
                    maintain_project_themes,
                    mirror_focus_to_style,
                    maintain_winit_mode_for_animation,
                    sync_canvas_clear_color,
                    window_geometry::fit_window_to_monitor,
                    window_geometry::save_on_change,
                    sync_wake_throttle_to_power,
                ),
            )
            // Survive lid-close / display-sleep: when the monitor goes away
            // the primary window is despawned by a `linked_spawn` cascade;
            // paired with `ExitCondition::DontExit` (main.rs) we respawn it
            // immediately so the app stays alive and visible on wake.
            // See `window_geometry::respawn_primary_window_on_loss`.
            .add_observer(window_geometry::respawn_primary_window_on_loss)
            // …but a deliberate close request (red button / Cmd-W) should
            // still quit. That request never fires on the lid-close cascade,
            // so honoring it here doesn't undo the survival behavior above.
            .add_systems(Update, window_geometry::quit_on_close_request)
            .add_systems(PostStartup, release_os_focus)
            // Single keyboard-ownership authority, before every Update
            // consumer reads it.
            .add_systems(PreUpdate, compute_keyboard_owner)
            .add_systems(Update, debug_fps_log)
            .add_systems(Update, debug_layer_cameras)
            .add_systems(Update, ipc_stats::publish_ipc_stats)
            .add_systems(
                Update,
                (
                    drain_ipc_open_requests,
                    drain_file_picks,
                    dispatch_bus_actions,
                    route_emacs_open_requests,
                ),
            )
            .add_systems(
                Update,
                (
                    // Focus-state + modifier reconciliation run before the
                    // terminal crate's `handle_keyboard` (in
                    // `jim_terminal::TerminalPlugin`) so a stuck Cmd (e.g. a
                    // swallowed Cmd-up from a system shortcut) doesn't drop
                    // this frame's keys. The reconciliation self-heals each
                    // frame, so cross-plugin ordering being best-effort is
                    // fine.
                    track_app_focus,
                    reconcile_macos_modifiers,
                    handle_scroll,
                    apply_bell_pulse,
                    apply_claude_notification_pulse,
                    clear_active_unread,
                    sync_dock_badge,
                )
                    .chain(),
            )
            .init_resource::<TerminalCwds>()
            .add_systems(Update, track_terminal_cwds);
    }
}

/// Shell camera setup: the main 2D camera (layer 0) and the menu-overlay
/// camera. Split out of the old `setup_camera_and_font`; the terminal's
/// font/atlas half now lives in `jim_terminal::setup_terminal_font`.
fn setup_camera(mut commands: Commands) {
    // Main camera explicitly on layer 0 — pane-bevy reserves layer 0
    // for pane chrome + non-pane scene content, and uses layers 1.. for
    // each per-pane camera. Making the main camera's layer explicit
    // matches the contract documented in `pane-bevy/src/camera.rs`.
    commands.spawn((
        Camera2d,
        bevy::camera::visibility::RenderLayers::layer(0),
    ));

    // Whiteboard overlay camera — renders only `WHITEBOARD_OVERLAY_LAYER` at
    // an order above every per-pane camera, so the canvas drawing paints ON
    // TOP of panes. Shares the default (identity) transform with the main
    // camera, so the drawing's baked canvas pan/zoom still lands aligned.
    // `clear_color: None` keeps panes visible wherever there's no drawing.
    commands.spawn((
        Camera2d,
        bevy::camera::Camera {
            order: WHITEBOARD_OVERLAY_CAMERA_ORDER,
            clear_color: bevy::camera::ClearColorConfig::None,
            ..default()
        },
        bevy::camera::visibility::RenderLayers::layer(WHITEBOARD_OVERLAY_LAYER),
    ));

    // Menu overlay camera — renders only `MENU_OVERLAY_LAYER` at a
    // camera order far above any per-pane camera, so radial / context
    // menus draw on top of every pane even when many panes are
    // focused. `clear_color: None` keeps the underlying scene visible
    // wherever the overlay has no geometry.
    commands.spawn((
        Camera2d,
        bevy::camera::Camera {
            order: MENU_OVERLAY_CAMERA_ORDER,
            clear_color: bevy::camera::ClearColorConfig::None,
            ..default()
        },
        bevy::camera::visibility::RenderLayers::layer(MENU_OVERLAY_LAYER),
    ));
}

#[cfg(target_os = "macos")]
fn release_os_focus() {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        unsafe { app.deactivate() };
    }
}

#[cfg(not(target_os = "macos"))]
fn release_os_focus() {}

/// Trigger the macOS microphone permission prompt at startup.
///
/// Why this is needed: `claude` (and any other CLI a user runs) records
/// audio through whichever process the OS deems *responsible* for it.
/// Our shells run under a launchd-detached daemon (double-fork +
/// `setsid`, PPID 1), so that responsible process is this app's code
/// identity — but a headless background daemon can't present the TCC
/// permission dialog. The foreground GUI can. Calling
/// `requestAccessForMediaType:` here pops the prompt once while we're
/// frontmost; the resulting grant is keyed on our code identity, which
/// the daemon shares (same signed binary), so `claude`'s voice dictation
/// can capture audio. Requires `NSMicrophoneUsageDescription` in
/// Info.plist (added by scripts/make-bundle.sh) — without it the request is
/// denied outright. Already-granted launches resolve to a no-op.
#[cfg(target_os = "macos")]
fn request_microphone_access() {
    use objc2::runtime::Bool;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    // winit/Bevy don't load AVFoundation, so the AVCaptureDevice class
    // wouldn't resolve at runtime. This empty extern forces a framework
    // load command into the binary.
    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    // AVMediaTypeAudio's documented constant value is the FourCC "soun";
    // using the literal avoids linking the Obj-C string symbol.
    let media_type = NSString::from_str("soun");
    // Heap block: AVFoundation invokes the completion handler
    // asynchronously, after this function returns, so it must outlive the
    // stack frame.
    let handler = block2::RcBlock::new(|granted: Bool| {
        eprintln!(
            "[mic] microphone access request resolved: granted={}",
            granted.as_bool()
        );
    });
    let cls = class!(AVCaptureDevice);
    unsafe {
        let _: () = msg_send![
            cls,
            requestAccessForMediaType: &*media_type,
            completionHandler: &*handler,
        ];
    }
}

#[cfg(not(target_os = "macos"))]
fn request_microphone_access() {}

/// True if the machine is currently running on AC (wall) power. On
/// battery this returns false, and the wake throttle relaxes to ~30 Hz.
///
/// Uses IOKit's power-source API directly via a minimal FFI block rather
/// than pulling in an `iokit`/`core-foundation` crate: the whole surface
/// is three C calls, and `jim-app` already links frameworks by hand for
/// the mic permission (see `request_microphone_access`). The empty
/// `#[link]` attributes force the IOKit + CoreFoundation load commands
/// into the binary so the symbols resolve at runtime — winit/Bevy don't
/// pull either framework in on their own.
///
/// Conservative on any uncertainty (null snapshot, unreadable string, a
/// value that's neither AC nor Battery such as "Off Line" / a UPS): we
/// report AC, i.e. prefer display responsiveness over the battery saving.
#[cfg(target_os = "macos")]
fn power_on_ac() -> bool {
    use std::ffi::{c_char, c_void, CStr};

    // CFTypeRef / CFStringRef are opaque pointers. `IOPSCopyPowerSourcesInfo`
    // returns a +1 CFDictionary we must `CFRelease`; the string from
    // `IOPSGetProvidingPowerSourceType` is borrowed from it (do NOT release).
    #[link(name = "IOKit", kind = "framework")]
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> *const c_void;
        fn IOPSGetProvidingPowerSourceType(blob: *const c_void) -> *const c_void;
        fn CFStringGetCString(
            s: *const c_void,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: *const c_void);
    }
    // kCFStringEncodingUTF8.
    const UTF8: u32 = 0x0800_0100;
    // kIOPSBatteryPowerValue. Everything else (kIOPSACPowerValue "AC Power",
    // kIOPSOffLineValue, unknown) is treated as "on AC".
    const BATTERY: &[u8] = b"Battery Power";

    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return true;
        }
        let src = IOPSGetProvidingPowerSourceType(blob);
        let mut buf = [0i8; 64];
        let ok = !src.is_null()
            && CFStringGetCString(src, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0;
        CFRelease(blob);
        if !ok {
            return true;
        }
        CStr::from_ptr(buf.as_ptr()).to_bytes() != BATTERY
    }
}

#[cfg(not(target_os = "macos"))]
fn power_on_ac() -> bool {
    true
}

/// Poll the power source on a slow timer and keep the terminal workers'
/// wake throttle in step: 16 ms (~60 Hz) on AC, 33 ms (~30 Hz) on
/// battery. The battery interval halves the worst-case winit wake rate
/// for background terminals streaming output, the dominant active-use
/// idle-ish CPU/GPU draw. `power_on_ac` is a cheap in-process IOKit call,
/// but the source flips rarely, so a 30 s cadence keeps the response to
/// (un)plugging snappy without adding measurable idle cost. Only calls
/// the setter when the value actually changes.
fn sync_wake_throttle_to_power(
    time: Res<Time>,
    mut timer: Local<Option<Timer>>,
    mut last_on_ac: Local<Option<bool>>,
) {
    let t = timer.get_or_insert_with(|| Timer::from_seconds(30.0, TimerMode::Repeating));
    let ticked = t.tick(time.delta()).just_finished();
    // First frame (`last_on_ac` still None) runs regardless of the timer
    // so the throttle is correct from startup, not 30 s in.
    if last_on_ac.is_some() && !ticked {
        return;
    }
    let on_ac = power_on_ac();
    if *last_on_ac != Some(on_ac) {
        *last_on_ac = Some(on_ac);
        jim_terminal::worker::set_wake_throttle_ms(if on_ac { 16 } else { 33 });
    }
}

/// Holds the receiver half of the IPC channel. `mpsc::Receiver` is
/// `Send` but `!Sync`, so we install it as a `NonSend` resource and
/// drain it from a system that always runs on the main thread.
pub struct IpcInbox(pub std::sync::mpsc::Receiver<ipc::IpcMessage>);

fn setup_ipc_listener(world: &mut World) {
    let wakeup = world
        .get_resource::<bevy::winit::EventLoopProxyWrapper>()
        .map(|w| bevy::winit::EventLoopProxy::clone(w));
    // Let widget worker threads wake the reactive main loop (so
    // `set_animating(true)`, async frame publishes, and bus emits aren't
    // stalled until the next input / ~5s timeout). widget-bevy doesn't
    // depend on winit, so we hand it a closure over the proxy.
    if let Some(proxy) = wakeup.clone() {
        jim_widget::set_wakeup_hook(move || {
            let _ = proxy.send_event(bevy::winit::WinitUserEvent::WakeUp);
        });
    }
    if let Some((rx, metrics)) = ipc::spawn_listener(wakeup) {
        world.insert_non_send_resource(IpcInbox(rx));
        world.insert_resource(ipc_stats::IpcMetricsRes(metrics));
    }
}

/// Drain any IPC requests received this frame and queue them as
/// entries in `PendingActions`. The actual world-mutating work
/// (file-read + editor spawn, widget spawn) happens in
/// `apply_pending_actions` next frame, so the IPC thread never touches
/// the World.
fn drain_ipc_open_requests(
    inbox: Option<NonSend<IpcInbox>>,
    mut pending: ResMut<PendingActions>,
    mut projects: ResMut<Projects>,
    mut drawer: ResMut<drawer::Drawer>,
    mut prism: ResMut<cube::Prism>,
    mut expose: ResMut<expose::Expose>,
    mut msg_bus: ResMut<jim_widget::WidgetMsgBus>,
    mut palette_open: ResMut<command_palette::PaletteOpenRequest>,
    mut issues: ResMut<issues_pane::IssuesStore>,
    mut screenshot_consent: ResMut<screenshot_consent::ScreenshotConsent>,
    mut commands: Commands,
) {
    let Some(inbox) = inbox else { return };
    while let Ok(msg) = inbox.0.try_recv() {
        let ipc::IpcMessage {
            req,
            stream: mut _stream,
        } = msg;
        match req {
            ipc::IpcRequest::OpenFile { path, project } => {
                let target = match project {
                    Some(name) => OpenProjectTarget::ByName(name),
                    None => OpenProjectTarget::Active,
                };
                pending.open_files.push(OpenFileRequest {
                    path,
                    project: target,
                    origin: None,
                });
            }
            ipc::IpcRequest::SpawnWidget {
                command,
                args,
                title,
                cwd,
                project,
                position,
                size,
                kind,
                params,
            } => {
                let target = match project {
                    Some(name) => OpenProjectTarget::ByName(name),
                    None => OpenProjectTarget::Active,
                };
                let Some(project_id) = projects::resolve_project(&target, &projects) else {
                    eprintln!("[ipc] spawn_widget: no matching project");
                    continue;
                };

                // Route by `kind`. funct widgets get a different config
                // shape (`script` field, not `command`) and a different
                // pane kind in the registry.
                let pane_kind = kind.as_deref().unwrap_or(jim_widget::PANE_KIND);
                let mut cfg = serde_json::Map::new();
                if pane_kind == jim_widget::script_widget::PANE_KIND {
                    // For script_widget, `command` is the script filename.
                    cfg.insert("script".into(), Value::String(command));
                    if let Some(t) = title {
                        cfg.insert("title".into(), Value::String(t));
                    }
                    if let Some(p) = params {
                        cfg.insert("params".into(), p);
                    }
                } else {
                    cfg.insert("command".into(), Value::String(command));
                    if !args.is_empty() {
                        cfg.insert(
                            "args".into(),
                            Value::Array(args.into_iter().map(Value::String).collect()),
                        );
                    }
                    if let Some(t) = title {
                        cfg.insert("title".into(), Value::String(t));
                    }
                    if let Some(p) = cwd {
                        cfg.insert(
                            "cwd".into(),
                            Value::String(p.to_string_lossy().into_owned()),
                        );
                    }
                }
                let kind_static: &'static str = match pane_kind {
                    "widget" => jim_widget::PANE_KIND,
                    "script_widget" => jim_widget::script_widget::PANE_KIND,
                    other => Box::leak(other.to_string().into_boxed_str()),
                };
                pending.new_panes.push(NewPaneRequest {
                    kind: kind_static,
                    project_id,
                    origin: position.map(|[x, y]| Vec2::new(x, y)),
                    size: size.map(|[w, h]| Vec2::new(w, h)),
                    config: Value::Object(cfg),
                });
            }
            ipc::IpcRequest::SpawnPane {
                kind,
                project,
                position,
                size,
                config,
            } => {
                let target = match project {
                    Some(name) => OpenProjectTarget::ByName(name),
                    None => OpenProjectTarget::Active,
                };
                let Some(project_id) = projects::resolve_project(&target, &projects) else {
                    eprintln!("[ipc] spawn_pane: no matching project");
                    continue;
                };
                let kind_static: &'static str = Box::leak(kind.into_boxed_str());
                pending.new_panes.push(NewPaneRequest {
                    kind: kind_static,
                    project_id,
                    origin: position.map(|[x, y]| Vec2::new(x, y)),
                    size: size.map(|[w, h]| Vec2::new(w, h)),
                    config: config.unwrap_or(Value::Null),
                });
            }
            ipc::IpcRequest::ToggleCube => {
                prism.pending_toggle = true;
            }
            ipc::IpcRequest::ToggleExpose => {
                expose.pending_toggle = true;
            }
            ipc::IpcRequest::ActivateProject { project } => {
                match projects::resolve_project(
                    &OpenProjectTarget::ByName(project),
                    &projects,
                ) {
                    Some(id) => projects.set_active(id),
                    None => eprintln!("[ipc] activate_project: no matching project"),
                }
            }
            ipc::IpcRequest::OpenPalette { query, ask } => {
                palette_open.requested = true;
                palette_open.seed = query;
                palette_open.ask = ask;
            }
            ipc::IpcRequest::AddIssue {
                title,
                body,
                project,
                from_cwd,
            } => {
                // Scope like SuggestPane: explicit name wins; else map the
                // caller's cwd to its owning project; else the active one.
                let project_id = match &project {
                    // An explicit name must resolve — don't silently fall
                    // back to the active project on a typo.
                    Some(name) => projects::resolve_project(
                        &OpenProjectTarget::ByName(name.clone()),
                        &projects,
                    ),
                    None => from_cwd
                        .as_deref()
                        .and_then(|c| projects::project_for_cwd(c, &projects))
                        .or(projects.active),
                };
                let Some(project_id) = project_id else {
                    match &project {
                        Some(name) => eprintln!("[ipc] add_issue: no project named {name:?}"),
                        None => eprintln!("[ipc] add_issue: no project owns cwd and none active"),
                    }
                    continue;
                };
                let id = issues.add_issue(project_id, title, body.unwrap_or_default());
                eprintln!("[ipc] add_issue: filed #{id} into project {project_id}");
            }
            ipc::IpcRequest::ListProjects => {
                use std::io::Write as _;
                let active = projects.active;
                let entries: Vec<Value> = projects
                    .list
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "id": p.id,
                            "name": p.name,
                            "active": Some(p.id) == active,
                        })
                    })
                    .collect();
                let body = serde_json::json!({ "projects": entries });
                let bytes = match serde_json::to_vec(&body) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("[ipc] list_projects: serialize: {}", e);
                        continue;
                    }
                };
                if let Err(e) = _stream.write_all(&bytes) {
                    eprintln!("[ipc] list_projects: write: {}", e);
                }
                let _ = _stream.shutdown(std::net::Shutdown::Write);
            }
            ipc::IpcRequest::SetProjectDefaultCwd { project, cwd } => {
                let target = match project.as_deref() {
                    Some("active") | None => OpenProjectTarget::Active,
                    Some(name) => OpenProjectTarget::ByName(name.to_string()),
                };
                let Some(project_id) = projects::resolve_project(&target, &projects) else {
                    eprintln!("[ipc] set_project_default_cwd: no matching project");
                    continue;
                };
                let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());
                let changed = projects.set_default_cwd(project_id, cwd_str.clone());
                eprintln!(
                    "[ipc] set_project_default_cwd: project={} cwd={:?} changed={}",
                    project_id, cwd_str, changed
                );
            }
            ipc::IpcRequest::SendInbox {
                project,
                sender,
                subject,
                body,
            } => {
                // Resolve project: name → id, or "active" / None → active.
                let target = match project.as_deref() {
                    Some("active") | None => OpenProjectTarget::Active,
                    Some(name) => OpenProjectTarget::ByName(name.to_string()),
                };
                let Some(project_id) = projects::resolve_project(&target, &projects) else {
                    eprintln!("[ipc] send_inbox: no matching project");
                    continue;
                };
                let sender = sender.unwrap_or_else(|| "external".to_string());
                if let Err(e) = inbox::append_message(project_id, sender, subject, body) {
                    eprintln!("[ipc] send_inbox: append: {}", e);
                }
            }
            ipc::IpcRequest::SuggestPane {
                kind,
                title,
                command,
                cwd,
                reason,
                config,
                project,
                from_cwd,
            } => {
                // Resolve the pane kind. Explicit `kind` wins; otherwise
                // a bare `command` implies the run-button "command pane".
                let kind = match kind {
                    Some(k) => k,
                    None if command.is_some() => "run-button".to_string(),
                    None => {
                        eprintln!(
                            "[ipc] suggest_pane: need --kind or --command; dropping"
                        );
                        continue;
                    }
                };

                // Build the config blob. Explicit `config` is stored
                // verbatim; otherwise synthesize a run-button config from
                // command/title/cwd (matching `run_button_snapshot`).
                let config = match config {
                    Some(c) => c,
                    None => {
                        let mut cfg = serde_json::Map::new();
                        if let Some(cmd) = &command {
                            cfg.insert("command".into(), Value::String(cmd.clone()));
                        }
                        if let Some(t) = &title {
                            cfg.insert("title".into(), Value::String(t.clone()));
                        }
                        if let Some(p) = &cwd {
                            cfg.insert(
                                "cwd".into(),
                                Value::String(p.to_string_lossy().into_owned()),
                            );
                        }
                        Value::Object(cfg)
                    }
                };

                // Row title: explicit, else the command, else the kind.
                let row_title = title
                    .or_else(|| command.clone())
                    .unwrap_or_else(|| kind.clone());

                // Scope the suggestion to a project at arrival: an
                // explicit name wins; otherwise map the caller's cwd to
                // its owning project; otherwise leave it unscoped
                // (global — shows in every project's drawer).
                let project_id = match &project {
                    Some(name) => {
                        projects::resolve_project(
                            &OpenProjectTarget::ByName(name.clone()),
                            &projects,
                        )
                    }
                    None => from_cwd
                        .as_deref()
                        .and_then(|c| projects::project_for_cwd(c, &projects)),
                };

                drawer.push(kind, row_title, reason, config, project_id);
            }
            ipc::IpcRequest::Screenshot { path, reason } => {
                // Don't capture immediately: enqueue for the consent toast,
                // which captures on user tap or after a short countdown. This
                // keeps an automated requester from grabbing a frame while
                // the user is mid-task.
                screenshot_consent.request(path, reason);
            }
            ipc::IpcRequest::CloseProjectPanes { project, kind, titles } => {
                let target = match project.as_deref() {
                    Some("active") | None => OpenProjectTarget::Active,
                    Some(name) => OpenProjectTarget::ByName(name.to_string()),
                };
                let Some(project_id) = projects::resolve_project(&target, &projects) else {
                    eprintln!("[ipc] close_project_panes: no matching project");
                    continue;
                };
                pending.close_panes.push((project_id, kind, titles));
            }
            ipc::IpcRequest::DockPanes { project, titles, template, empty, slots } => {
                let target = match project.as_deref() {
                    Some("active") | None => OpenProjectTarget::Active,
                    Some(name) => OpenProjectTarget::ByName(name.to_string()),
                };
                let Some(project_id) = projects::resolve_project(&target, &projects) else {
                    eprintln!("[ipc] dock_panes: no matching project");
                    continue;
                };
                pending
                    .dock_panes
                    .push((project_id, titles, template, empty, slots));
            }
            ipc::IpcRequest::WidgetMessage { project, topic, payload, retain, sender } => {
                // `"global"`/`"*"` → the global channel (`None`), delivered
                // to every widget; otherwise resolve a project id.
                let project = match project.as_deref() {
                    Some("global") | Some("*") => None,
                    other => {
                        let target = match other {
                            Some("active") | None => OpenProjectTarget::Active,
                            Some(name) => OpenProjectTarget::ByName(name.to_string()),
                        };
                        let Some(project_id) = projects::resolve_project(&target, &projects)
                        else {
                            eprintln!("[ipc] widget_message: no matching project");
                            continue;
                        };
                        Some(project_id)
                    }
                };
                msg_bus.push_external(jim_widget::PendingMsg {
                    project,
                    topic,
                    payload,
                    sender: sender.unwrap_or_else(|| "tbmsg".to_string()),
                    retain,
                });
            }
            ipc::IpcRequest::TraceControl { arm, ms } => {
                use std::io::Write as _;
                if let Some(on) = arm {
                    jim_pane::trace::set_enabled(on);
                }
                if let Some(ms) = ms {
                    fps::set_trace_threshold_ms(ms);
                }
                let armed = jim_pane::trace::enabled();
                let threshold_ms = fps::trace_threshold_ms();
                crate::diagnostics::append_log(&format!(
                    "[trace] ipc: capture {} (threshold {:.0}ms active)",
                    if armed { "ARMED" } else { "off" },
                    threshold_ms,
                ));
                let body = serde_json::json!({
                    "armed": armed,
                    "threshold_ms": threshold_ms,
                });
                if let Ok(bytes) = serde_json::to_vec(&body) {
                    let _ = _stream.write_all(&bytes);
                }
                let _ = _stream.shutdown(std::net::Shutdown::Write);
            }
        }
    }
}

/// Consume `jim.action` bus messages and re-dispatch their payload as a
/// local `IpcRequest`, so ANY bus participant — a Claude session via the
/// channel bridge, or a funct widget calling `emit("jim.action", …)` — can
/// drive the editor through the same action surface the `tb*`/`jimctl`
/// CLIs use. See CHANNELS.md. The payload IS an `IpcRequest` (internally
/// `action`-tagged), e.g. `{ "action": "open_file", "path": "…" }`.
///
/// It re-injects via `dispatch_local` (the socket) rather than touching
/// world state here, so the action lands on the next frame through the
/// identical `drain_ipc_open_requests` path — no parallel dispatch to
/// drift. An inner action is never another `jim.action`, so there's no
/// loop.
fn dispatch_bus_actions(mut events: MessageReader<jim_widget::BusMessageObserved>) {
    for ev in events.read() {
        if ev.topic != "jim.action" {
            continue;
        }
        let tag = ev
            .payload
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        match serde_json::from_value::<ipc::IpcRequest>(ev.payload.clone()) {
            Ok(req) => {
                eprintln!("[jim.action] {tag} (from {})", ev.sender);
                if let Err(e) = ipc::dispatch_local(&req) {
                    eprintln!("[jim.action] dispatch '{tag}' failed: {e}");
                }
            }
            Err(e) => eprintln!("[jim.action] bad payload from {} ('{tag}'): {e}", ev.sender),
        }
    }
}

/// Decode a funct/subprocess widget bus id (`rw<hex>` / `sw<hex>`, where
/// `<hex>` is `Entity::to_bits`) back into the sender's pane `Entity`.
fn widget_id_to_entity(id: &str) -> Option<Entity> {
    let hex = id.strip_prefix("rw").or_else(|| id.strip_prefix("sw"))?;
    let bits = u64::from_str_radix(hex, 16).ok()?;
    Entity::try_from_bits(bits)
}

/// Route `emacs.open_file` bus messages (payload `{ "path": "…" }`) to an
/// emacs pane. Prefers a native-emacs pane docked with the sender (so a
/// file widget in a dock drives its dock's editor — the "mini editor"),
/// falling back to the focused pane if it's a native-emacs pane. This is
/// the one consumer of the dock's co-member relationship: the sender's
/// pane entity is recovered from `BusMessageObserved.sender`, then its
/// dock siblings are searched for an emacs pane.
#[allow(clippy::type_complexity)]
fn route_emacs_open_requests(
    mut events: MessageReader<jim_widget::BusMessageObserved>,
    store: Res<jim_emacs::native::EmacsNativeStore>,
    focused: Res<jim_pane::FocusedPane>,
    members: Query<&jim_pane::dock::DockMember>,
    docks: Query<&jim_pane::dock::Dock>,
    kinds: Query<&jim_pane::PaneKindMarker>,
) {
    let is_emacs = |e: Entity| matches!(kinds.get(e), Ok(k) if k.0 == jim_emacs::native::PANE_KIND);
    for ev in events.read() {
        // Global font-size command: `{ "size": N }`. Font is a face
        // attribute shared by all emacs frames, so no pane target needed.
        if ev.topic == "emacs.font" {
            if let Some(size) = ev.payload.get("size").and_then(|v| v.as_i64()) {
                if !store.send_font(size as i32) {
                    eprintln!("[emacs.font] size {size}: emacs not ready");
                }
            }
            continue;
        }
        if ev.topic != "emacs.open_file" {
            continue;
        }
        let Some(path) = ev.payload.get("path").and_then(|v| v.as_str()) else {
            eprintln!("[emacs.open_file] missing 'path' from {}", ev.sender);
            continue;
        };
        // Prefer a native-emacs pane docked with the sender widget.
        let target = widget_id_to_entity(&ev.sender)
            .and_then(|sender| members.get(sender).ok().map(|dm| (sender, dm.dock)))
            .and_then(|(sender, dock)| {
                docks.get(dock).ok().and_then(|d| {
                    d.member_entities()
                        .into_iter()
                        .find(|&m| m != sender && is_emacs(m))
                })
            })
            // Fallback: the focused pane, if it's a native-emacs pane.
            .or_else(|| focused.0.filter(|&e| is_emacs(e)));

        match target {
            Some(pane) => {
                if !store.send_open_file(pane, path) {
                    eprintln!("[emacs.open_file] '{path}': emacs not ready / no frame");
                }
            }
            None => eprintln!(
                "[emacs.open_file] '{path}' from {}: no docked or focused emacs pane",
                ev.sender
            ),
        }
    }
}

/// Channel the async Open dialog delivers chosen paths back on.
/// `action_open_file` clones [`FilePickChannel::tx`] into the off-thread
/// task that awaits the sheet; [`drain_file_picks`] reads
/// [`FilePickChannel::rx`] each frame. Stored NonSend (both mpsc ends are
/// `!Sync`); both touchpoints run on the main thread.
struct FilePickChannel {
    tx: std::sync::mpsc::Sender<PathBuf>,
    rx: std::sync::mpsc::Receiver<PathBuf>,
}

/// `file.open` action (Cmd+O). Opens the native Open dialog and routes the
/// chosen file to an editor pane in the active project.
///
/// Crucially this uses `rfd::AsyncFileDialog`, **not** the blocking
/// `FileDialog::pick_file`. The blocking variant runs `-[NSOpenPanel
/// runModal]`, a nested modal run loop on the main thread; while it spins,
/// AppKit still pumps re-entrant native events (e.g. a drag-and-drop in
/// progress), which can drive a winit/rfd `block2` closure that panics —
/// and a Rust panic cannot unwind out through an Objective-C block, so the
/// process `abort()`s. The async variant begins a *sheet* modal attached
/// to the main window (`beginSheetModalForWindow:completionHandler:`),
/// which integrates with the existing run loop instead of nesting one, so
/// there is no re-entrancy to crash on.
///
/// `AsyncFileDialog::pick_file()` builds the panel eagerly and must touch
/// AppKit on the main thread; this action runs there (exclusive dispatcher
/// + Cmd+O swallowed by the keybind matcher). We then `await` the (Send)
/// future on the IO task pool so the main thread stays live, and hand the
/// path back over [`FilePickChannel`].
fn action_open_file(ctx: &mut actions::ActionCtx) {
    let Some(tx) = ctx
        .world
        .get_non_send_resource::<FilePickChannel>()
        .map(|c| c.tx.clone())
    else {
        return;
    };
    // Build the dialog (and begin the sheet) on the main thread.
    let dialog = rfd::AsyncFileDialog::new()
        .set_directory(std::env::current_dir().unwrap_or_else(|_| ".".into()))
        .set_title("Open file");
    let fut = dialog.pick_file();
    // Await off the main thread; the sheet's completion handler wakes it.
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            if let Some(handle) = fut.await {
                let _ = tx.send(handle.path().to_path_buf());
            }
        })
        .detach();
}

/// Drains paths chosen by the async Open dialog into `PendingActions`,
/// where `apply_pending_actions` turns each into an editor pane in the
/// active project. NonSend pins this to the main thread (mpsc `Receiver`
/// is `!Sync`).
fn drain_file_picks(
    channel: Option<NonSend<FilePickChannel>>,
    mut pending: ResMut<PendingActions>,
) {
    let Some(channel) = channel else { return };
    while let Ok(path) = channel.rx.try_recv() {
        pending.open_files.push(OpenFileRequest {
            path,
            project: OpenProjectTarget::Active,
            origin: None,
        });
    }
}


/// Mouse-wheel scrolls the terminal under the cursor (in the active
/// project). Pixel-mode events (trackpads) accumulate a fractional line
/// counter so small swipes still register.
fn handle_scroll(
    mut wheel: MessageReader<MouseWheel>,
    mut accum: Local<f32>,
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    viewport: Res<jim_pane::PaneViewport>,
    projects: Res<Projects>,
    store: Res<TerminalStore>,
    metrics: Res<MonoMetrics>,
    keys: Res<ButtonInput<KeyCode>>,
    all_panes: Query<(Entity, &PaneRect, Option<&Visibility>), With<PaneTag>>,
    terminals: Query<
        (Entity, Option<&ProjectMembership>, &PaneKindMarker),
        With<PaneTag>,
    >,
) {
    // Cmd+scroll is reserved for canvas pan (see canvas.rs). Drain the
    // events so they don't accumulate, but don't act on them.
    if keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight) {
        wheel.clear();
        *accum = 0.0;
        return;
    }
    let mut delta_lines: f32 = 0.0;
    for ev in wheel.read() {
        let lines = match ev.unit {
            MouseScrollUnit::Line => ev.y,
            MouseScrollUnit::Pixel => ev.y / LINE_HEIGHT,
        };
        delta_lines += lines;
    }
    if delta_lines == 0.0 {
        return;
    }
    *accum += delta_lines;
    let whole_lines = accum.trunc() as isize;
    if whole_lines == 0 {
        return;
    }
    *accum -= whole_lines as f32;

    let Ok(window) = windows.single() else {
        return;
    };
    let Some(pt) = window.cursor_position() else {
        return;
    };
    if pt.x < sidebar.width {
        return;
    }

    // Topmost pane of ANY kind under the cursor. If something is
    // sitting over the terminal (e.g. a widget pane), the wheel
    // belongs to that pane — don't steal it for the terminal
    // underneath.
    let all_rects: Vec<(Entity, PaneRect)> = all_panes
        .iter()
        .filter(|(_, _, vis)| !matches!(vis, Some(Visibility::Hidden)))
        .map(|(e, r, _)| (e, *r))
        .collect();
    let Some(target) = jim_pane::topmost_pane_at(viewport.window_to_canvas(pt), &all_rects)
    else {
        return;
    };
    // Only consume the wheel if that topmost pane is a VT-grid pane
    // (terminal or emacs) in the active project.
    let Ok((_, membership, kind)) = terminals.get(target) else {
        return;
    };
    if kind.0 != PANE_KIND && kind.0 != jim_emacs::PANE_KIND {
        return;
    }
    let in_active_project = match (projects.active, membership) {
        (Some(a), Some(p)) => a == p.0,
        _ => false,
    };
    if !in_active_project {
        return;
    }
    let Some(data) = store.map.get(&target) else {
        return;
    };

    // Cell under the cursor (viewport-relative, 0-based) so the worker
    // can fill in a mouse report when the child enabled mouse tracking.
    // Clamp the lower bound here; the worker clamps the upper bound
    // against the live grid size.
    let (col, row) = match all_rects.iter().find(|(e, _)| *e == target) {
        Some((_, rect)) => {
            let (c, r) = pt_to_cell(viewport.window_to_canvas(pt), rect, metrics.cell_width);
            (c.max(0) as u16, r.max(0) as u16)
        }
        None => (0, 0),
    };

    // Bevy: wheel.y > 0 = scroll-up gesture = reveal older content. The
    // worker decides whether that becomes a mouse report, an arrow key,
    // or a local scrollback move based on the VT's current modes.
    data.worker.send(WorkerMsg::Wheel {
        lines: whole_lines,
        col,
        row,
    });
}

// ---------- Rendering ----------

/// Render the visible grid into per-cell sprites that sample glyphs
/// from a shared atlas. The atlas pre-rasterized printable ASCII at
/// startup; non-ASCII chars get rasterized lazily on first sight.
///
/// Pool sizes (`bg`, `fg`) are exactly `cols * rows` and only change
/// on grid resize — every other frame just mutates `Sprite.color` and
/// `TextureAtlas.index` on the dirty rows. No cosmic-text, no Text2d,
/// no spawn/despawn churn.
/// Maintain `AppFocused` from app-level activation state, NOT winit's
/// `WindowFocused` events: on macOS those fire on per-window key focus
/// transitions and have been observed flipping back to `true`
/// spuriously even while the app is backgrounded. Polling
/// `NSApplication.isActive` each frame matches what the user actually
/// perceives as "looking at us". Logs every transition while diagnosing.
fn debug_fps_log(
    time: Res<Time>,
    mut frames: Local<u64>,
    mut last: Local<f64>,
) {
    if std::env::var("FPS_LOG").is_err() {
        return;
    }
    *frames += 1;
    let now = time.elapsed_secs_f64();
    if *last == 0.0 {
        *last = now;
        *frames = 0;
        return;
    }
    if now - *last >= 1.0 {
        eprintln!("[fps] {:.1}", *frames as f64 / (now - *last));
        *frames = 0;
        *last = now;
    }
}

/// Debug (`JIM_LAYER_DBG`): for every editor pane, report which render
/// layer it owns and EVERY camera whose mask includes that layer, with
/// each camera's order + world scale. If an editor's content is drawn by
/// more than one camera (or by cameras at different scales), that's the
/// double/overlapping render. Logs only when the picture changes.
fn debug_layer_cameras(
    panes: Query<(
        Entity,
        &jim_pane::PaneLayer,
        &jim_pane::PaneKindMarker,
        Option<&jim_pane::PaneCanvas>,
    )>,
    cameras: Query<(
        Entity,
        &Camera,
        &bevy::camera::visibility::RenderLayers,
        &GlobalTransform,
    )>,
    mut last: Local<Option<String>>,
) {
    if std::env::var("JIM_LAYER_DBG").is_err() {
        return;
    }
    let mut report = String::new();
    for (e, layer, kind, canvas) in &panes {
        if kind.0 != "editor" {
            continue;
        }
        let mut cams: Vec<String> = Vec::new();
        for (ce, cam, rl, gt) in &cameras {
            if rl.intersects(&bevy::camera::visibility::RenderLayers::from_layers(&[layer.0])) {
                let s = gt.compute_transform().scale;
                cams.push(format!(
                    "{ce:?}(order={},scale={:.3})",
                    cam.order, s.x
                ));
            }
        }
        report.push_str(&format!(
            "editor {e:?} layer={} canvas={:?} cameras=[{}]\n",
            layer.0,
            canvas.map(|c| c.0),
            cams.join(", "),
        ));
    }
    if last.as_deref() != Some(report.as_str()) {
        eprintln!("[cam-dbg]\n{report}");
        *last = Some(report);
    }
}

fn track_app_focus(
    mut focused: ResMut<AppFocused>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
) {
    // Read the cached focus state maintained by the NSWorkspace activation
    // observers (`install_app_focus_observers`). This used to call
    // `current_app_active()` directly — a synchronous XPC round-trip to
    // LaunchServices — every frame; at 60fps during a terminal stream that
    // showed up as `_LSCopyApplicationInformation` inside this ECS system in
    // process samples. The observers push activation changes into the atomic
    // instead, so the per-frame cost is now a plain atomic load.
    let now = app_focused_snapshot();
    if focused.0 != now {
        eprintln!("[focus] {} → {}", focused.0, now);
        focused.0 = now;
        // Cmd+Tab (and any other modal app switch) eats the key-release
        // events for whatever was held — most commonly Cmd itself. Without
        // this reset, ButtonInput<KeyCode> stays "pressed" on Super* and
        // every subsequent keystroke gets dropped by handle_keyboard's
        // `if cmd { return; }` gate.
        keys.release_all();
    }
}

/// Reconcile Bevy's modifier state with the OS's real-time view each
/// frame. The focus-transition reset in `track_app_focus` catches the
/// common Cmd+Tab case, but system shortcuts (Spotlight, Mission
/// Control, screenshots) can swallow a Cmd-up without changing
/// `frontmostApplication`, leaving `ButtonInput<KeyCode>::pressed(Super*)`
/// stuck true. Polling NSEvent.modifierFlags is the authoritative
/// signal: if Bevy thinks a modifier is held but the OS says it isn't,
/// release it — otherwise every terminal keystroke after the stuck
/// modifier gets silently dropped by handle_keyboard's gate.
#[cfg(target_os = "macos")]
fn reconcile_macos_modifiers(mut keys: ResMut<ButtonInput<KeyCode>>) {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags};

    let flags = unsafe { NSEvent::modifierFlags_class() };
    let want = |mask: NSEventModifierFlags| flags.contains(mask);

    let cmd = want(NSEventModifierFlags::NSEventModifierFlagCommand);
    let shift = want(NSEventModifierFlags::NSEventModifierFlagShift);
    let ctrl = want(NSEventModifierFlags::NSEventModifierFlagControl);
    let alt = want(NSEventModifierFlags::NSEventModifierFlagOption);

    let pairs = [
        (cmd, KeyCode::SuperLeft),
        (cmd, KeyCode::SuperRight),
        (shift, KeyCode::ShiftLeft),
        (shift, KeyCode::ShiftRight),
        (ctrl, KeyCode::ControlLeft),
        (ctrl, KeyCode::ControlRight),
        (alt, KeyCode::AltLeft),
        (alt, KeyCode::AltRight),
    ];
    for (os_held, code) in pairs {
        if !os_held && keys.pressed(code) {
            keys.release(code);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn reconcile_macos_modifiers() {}

/// Cached whole-app focus state, maintained by the NSWorkspace activation
/// observers on macOS (`install_app_focus_observers`). `track_app_focus`
/// reads it every frame; the observers write it only on an actual app
/// switch. Seeded `true` for the same reason `AppFocused` defaults true: on
/// the first frame the user is presumably looking at us.
#[cfg(target_os = "macos")]
static APP_FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Cheap in-process read of the cached app-focus state. On macOS this is the
/// `APP_FOCUSED` atomic kept current by the activation observers (no XPC).
/// Off macOS we're always considered focused.
fn app_focused_snapshot() -> bool {
    #[cfg(target_os = "macos")]
    {
        APP_FOCUSED.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Install NSWorkspace activation observers that keep `APP_FOCUSED` current,
/// replacing the per-frame `frontmostApplication` poll that used to live in
/// `track_app_focus`.
///
/// Why observers instead of polling: `frontmostApplication` is a synchronous
/// XPC round-trip to LaunchServices. Calling it 60×/sec (e.g. while a
/// terminal streams) is pure waste — the frontmost app changes only on an
/// actual app switch. NSWorkspace stays the authoritative signal
/// (`NSApplication.isActive` is unreliable under winit — see
/// `current_app_active`), but its notification center *pushes* activation
/// changes, so we pull the XPC value only when one actually happens.
///
/// `NSWorkspaceDidActivateApplicationNotification` posts *after* the
/// frontmost app has already changed, so re-reading `current_app_active()`
/// inside the block yields the correct new value while preserving the exact
/// pid-compare the old poll used. We also observe DidDeactivate so a
/// deactivation with no paired activation still refreshes the cache. The
/// observer tokens must outlive the process for the observers to stay live,
/// so they are intentionally leaked.
///
/// `NonSendMarker` forces this Startup system onto the main thread, which
/// AppKit requires for NSWorkspace notification-center registration.
#[cfg(target_os = "macos")]
fn install_app_focus_observers(_main_thread: bevy::ecs::system::NonSendMarker) {
    use block2::RcBlock;
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidActivateApplicationNotification,
        NSWorkspaceDidDeactivateApplicationNotification,
    };
    use objc2_foundation::NSNotification;
    use std::ptr::NonNull;
    use std::sync::atomic::Ordering;

    // Seed with one authoritative read so the first frames are correct even
    // before any activation event arrives.
    APP_FOCUSED.store(current_app_active(), Ordering::Relaxed);

    let workspace = unsafe { NSWorkspace::sharedWorkspace() };
    let center = unsafe { workspace.notificationCenter() };

    // Both notifications resolve focus the same way: ask who's frontmost now.
    let handler = RcBlock::new(|_notif: NonNull<NSNotification>| {
        APP_FOCUSED.store(current_app_active(), Ordering::Relaxed);
    });

    for name in [
        unsafe { NSWorkspaceDidActivateApplicationNotification },
        unsafe { NSWorkspaceDidDeactivateApplicationNotification },
    ] {
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                None,
                // nil queue → the block runs synchronously on the posting
                // (main) thread, so `current_app_active`'s AppKit calls are
                // main-thread-safe.
                None,
                &handler,
            )
        };
        // The observer stays registered only while its token is retained;
        // this observer lives for the whole process, so leak the token.
        std::mem::forget(token);
    }
}

#[cfg(not(target_os = "macos"))]
fn install_app_focus_observers() {}

#[cfg(target_os = "macos")]
fn current_app_active() -> bool {
    // `NSApplication.isActive` doesn't reliably flip for our app under
    // winit / Bevy on macOS — we've observed it staying `true` even
    // when the user has Cmd+Tab'd to another app. The authoritative
    // signal is "are we the frontmost app, system-wide": ask
    // NSWorkspace and compare its `frontmostApplication.pid` to ours.
    use objc2_app_kit::NSWorkspace;
    let workspace = unsafe { NSWorkspace::sharedWorkspace() };
    let Some(front) = (unsafe { workspace.frontmostApplication() }) else {
        return true;
    };
    let front_pid = unsafe { front.processIdentifier() };
    let our_pid = unsafe { nix::libc::getpid() };
    front_pid == our_pid
}

#[cfg(not(target_os = "macos"))]
fn current_app_active() -> bool {
    true
}

/// Bell counter. Polls each terminal's worker `bell_count` and bumps
/// the per-project unread counter for every fresh BEL the user can't
/// currently see (window unfocused, or its project not active). No
/// in-pane visual — only the sidebar badge + dock-tile badge react.
fn apply_bell_pulse(
    store: Res<TerminalStore>,
    app_focused: Res<AppFocused>,
    mut projects: ResMut<Projects>,
    mut terms: Query<(Entity, Option<&ProjectMembership>, &mut BellPulse)>,
) {
    let window_focused = app_focused.0;
    let active_project = projects.active;
    for (entity, membership, mut pulse) in &mut terms {
        let Some(data) = store.map.get(&entity) else {
            continue;
        };
        let cur = data
            .worker
            .bell_count
            .load(std::sync::atomic::Ordering::Relaxed);
        if cur <= pulse.last_seen {
            continue;
        }
        let new_bells = cur - pulse.last_seen;
        pulse.last_seen = cur;
        let Some(membership) = membership else {
            eprintln!(
                "[bell] new={} on terminal {:?} but no ProjectMembership — skipping",
                new_bells, entity
            );
            continue;
        };
        let pid = membership.0;
        let visible = window_focused && active_project == Some(pid);
        eprintln!(
            "[bell] new={} pid={} window_focused={} active={:?} visible={}",
            new_bells, pid, window_focused, active_project, visible
        );
        if visible {
            continue;
        }
        for _ in 0..new_bells {
            projects.bump_unread(pid);
        }
        eprintln!(
            "[bell] bumped pid={} → {} (total {})",
            pid,
            projects.unread_bells.get(&pid).copied().unwrap_or(0),
            projects.unread_total()
        );
    }
}

/// Bumps the per-project unread counter when Claude's Notification hook
/// fires ("Claude is waiting for your input" / "needs your permission").
///
/// This is the *authoritative* "Claude wants attention" signal. It
/// arrives on the bus (via `claude-event-logger notification`) every
/// time, independent of whether Claude emits a terminal BEL — recent
/// Claude builds frequently don't, which is why the BEL-only
/// `apply_bell_pulse` path stopped lighting up project badges. We route
/// the event to a project by `terminal_session_id` → the pane's
/// `TerminalSession` → its `ProjectMembership`, then apply the same
/// visibility gate as the bell path: skip when the user is already
/// looking at that project.
fn apply_claude_notification_pulse(
    mut events: MessageReader<claude_bus_bevy::ClaudeBusEvent>,
    app_focused: Res<AppFocused>,
    mut projects: ResMut<Projects>,
    panes: Query<(&TerminalSession, Option<&ProjectMembership>)>,
) {
    let window_focused = app_focused.0;
    let active_project = projects.active;
    for ev in events.read() {
        if ev.kind != "notification" {
            continue;
        }
        // Standalone Claude sessions (not running inside one of our
        // panes) carry an empty / non-numeric session id — ignore them.
        let Ok(sid) = ev.terminal_session_id.parse::<u64>() else {
            continue;
        };
        let pid = panes
            .iter()
            .find(|(ts, _)| ts.0 == sid)
            .and_then(|(_, pm)| pm.map(|p| p.0));
        let Some(pid) = pid else {
            eprintln!(
                "[notify] notification for session {} but no project pane — skipping",
                sid
            );
            continue;
        };
        let visible = window_focused && active_project == Some(pid);
        eprintln!(
            "[notify] notification sid={} pid={} window_focused={} active={:?} visible={}",
            sid, pid, window_focused, active_project, visible
        );
        if visible {
            continue;
        }
        projects.bump_unread(pid);
        eprintln!(
            "[notify] bumped pid={} → {} (total {})",
            pid,
            projects.unread_bells.get(&pid).copied().unwrap_or(0),
            projects.unread_total()
        );
    }
}

/// Latest known working directory for each terminal session, keyed by
/// `TerminalSession` id. Populated from the `terminal.cwd_changed` bus
/// events the terminal worker emits on every OSC 7 (i.e. on each shell
/// prompt). This is the *live* cwd — distinct from a project's persisted
/// `default_cwd` and from a terminal's spawn-time `initial_cwd`. Session-
/// scoped and not persisted: a GUI restart rebuilds it from the first
/// prompt of each live terminal.
#[derive(Resource, Default)]
pub struct TerminalCwds(pub std::collections::HashMap<u64, String>);

/// Mirror `terminal.cwd_changed` bus events into [`TerminalCwds`] so
/// in-app code (e.g. the `project.set_cwd_from_terminal` action) can ask
/// "what directory is this terminal in right now?" without re-parsing the
/// bus. Mirrors the event-shape handling of `apply_claude_notification_pulse`.
fn track_terminal_cwds(
    mut events: MessageReader<claude_bus_bevy::ClaudeBusEvent>,
    mut cwds: ResMut<TerminalCwds>,
) {
    for ev in events.read() {
        if ev.kind != jim_inference::event_kinds::TERMINAL_CWD_CHANGED {
            continue;
        }
        let Ok(sid) = ev.terminal_session_id.parse::<u64>() else {
            continue;
        };
        #[derive(serde::Deserialize)]
        struct CwdPayload {
            cwd: String,
        }
        let Ok(payload) = serde_json::from_str::<CwdPayload>(&ev.payload_json) else {
            continue;
        };
        cwds.0.insert(sid, payload.cwd);
    }
}

/// `project.set_cwd_from_terminal` — set the active/owning project's
/// remembered `default_cwd` to the current working directory of the
/// focused terminal. No-op (with a log line) when nothing is focused, the
/// focused pane isn't a terminal, the terminal has no project, or we
/// haven't yet seen a cwd for it (no OSC 7 emitted — e.g. a full-screen
/// program is running and the shell hasn't reprinted its prompt).
fn action_set_project_cwd_from_focused(ctx: &mut actions::ActionCtx) {
    let world = &mut ctx.world;
    let Some(entity) = world.resource::<jim_pane::FocusedPane>().0 else {
        eprintln!("[set-cwd] no focused pane");
        return;
    };
    let Some(session) = world.get::<jim_terminal::TerminalSession>(entity) else {
        eprintln!("[set-cwd] focused pane is not a terminal");
        return;
    };
    let sid = session.0;
    let Some(project) = world.get::<jim_pane::PaneProject>(entity) else {
        eprintln!("[set-cwd] focused terminal has no project");
        return;
    };
    let project_id = project.0;
    let Some(cwd) = world.resource::<TerminalCwds>().0.get(&sid).cloned() else {
        eprintln!(
            "[set-cwd] no known cwd for terminal session {} yet (waiting on OSC 7)",
            sid
        );
        return;
    };
    let changed = world
        .resource_mut::<projects::Projects>()
        .set_default_cwd(project_id, Some(cwd.clone()));
    eprintln!(
        "[set-cwd] project={} default_cwd={:?} changed={}",
        project_id, cwd, changed
    );
}

/// Clears the active project's unread count whenever the OS window is
/// focused — that's the moment "the user is looking at it" becomes
/// true. Runs every frame; the no-op fast path inside `clear_unread`
/// (returns false when count was already zero) keeps the cost free.
fn clear_active_unread(
    app_focused: Res<AppFocused>,
    mut projects: ResMut<Projects>,
) {
    if !app_focused.0 {
        return;
    }
    let Some(active) = projects.active else {
        return;
    };
    projects.clear_unread(active);
}

/// Push the sum of unread bell counts to the macOS Dock icon as a
/// badge label. Tracked via a `Local<u64>` so we only hit the FFI when
/// the value actually changes — `setBadgeLabel` is cheap but it's not
/// free, and most frames have no change.
#[cfg(target_os = "macos")]
fn sync_dock_badge(
    // NonSendMarker forces this system onto the main thread, which is
    // mandatory for NSDockTile / NSApplication AppKit calls. Without
    // it Bevy may schedule us on a worker thread and `MainThreadMarker`
    // refuses to construct → the badge never updates.
    _main: bevy::ecs::system::NonSendMarker,
    projects: Res<Projects>,
    mut last: Local<u64>,
) {
    let total = projects.unread_total();
    if total == *last {
        return;
    }
    eprintln!("[dock] total {} → {}", *last, total);
    *last = total;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{MainThreadMarker, NSString};
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("[dock] MainThreadMarker::new() returned None — not main thread?");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let tile = unsafe { app.dockTile() };
    let label = if total == 0 {
        None
    } else {
        Some(NSString::from_str(&total.to_string()))
    };
    unsafe { tile.setBadgeLabel(label.as_deref()) };
    eprintln!("[dock] setBadgeLabel({:?})", total);
}

#[cfg(not(target_os = "macos"))]
fn sync_dock_badge(_projects: Res<Projects>, _last: Local<u64>) {}

// ---------- style-bevy glue ----------

/// Mirror `Projects.active` into style-bevy's `ActiveProject`. Also
/// ensures each newly-observed project has its state.json loaded into
/// memory so dust timers + the per-project preset are available.
///
/// Note: this no longer touches `ActiveThemePath`. `presets.rs` is the
/// sole owner of that resource — it derives it from `ActiveStylePreset`
/// (which is itself loaded per-project from `ProjectStyleState`), so
/// theming follows the active project's saved preset automatically.
fn mirror_active_project_to_style(
    projects: Res<Projects>,
    mut active_proj: ResMut<jim_style::shader::ActiveProject>,
    mut state: ResMut<jim_style::ProjectStyleState>,
    data_dir: Option<Res<jim_style::StyleDataDir>>,
) {
    if !projects.is_changed() {
        return;
    }
    if active_proj.0 == projects.active {
        return;
    }
    active_proj.0 = projects.active;
    if let Some(pid) = projects.active {
        if let Some(d) = data_dir.as_ref() {
            jim_style::state::load_project_state(d, &mut state, pid);
        }
        // Intentionally NOT calling note_focus here — switching to a
        // project on startup or via the sidebar shouldn't blow away
        // accumulated dust. The mirror_focus_to_style hook records
        // actual focus gestures.
    }
}

/// Keep `ProjectThemes` (style-bevy's per-project theme cache) populated
/// for every project, so each pane can render in its OWN project's theme
/// — in the cube overview (all projects on screen) and in flat view.
///
/// Full reload when the project set or any project's preset changes;
/// targeted reload of just the active project when its theme file is
/// live-edited (only the active project's file is watched, so a
/// `ThemeChanged` means *its* tokens moved). A `(pid, preset)` signature
/// hash keeps this from re-reading 14 theme files every frame.
fn maintain_project_themes(
    projects: Res<Projects>,
    mut style_state: ResMut<jim_style::ProjectStyleState>,
    registry: Res<jim_style::StylePresetRegistry>,
    data_dir: Option<Res<jim_style::StyleDataDir>>,
    mut themes: ResMut<jim_style::ProjectThemes>,
    mut theme_changed: MessageReader<jim_style::ThemeChanged>,
    mut last_sig: Local<u64>,
) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let theme_edited = theme_changed.read().last().is_some();

    // Make sure EVERY project's saved style state (its preset) is in
    // memory, not just the ones visited this session. `load_project_state`
    // is idempotent (loads once, then a cheap map check), so this is fine
    // to run each frame. Without it, an unvisited project resolves with
    // `preset_of() == None` → default theme until you switch to it.
    if let Some(d) = data_dir.as_ref() {
        for p in &projects.list {
            jim_style::state::load_project_state(d, &mut style_state, p.id);
        }
    }

    let mut hasher = DefaultHasher::new();
    for p in &projects.list {
        p.id.hash(&mut hasher);
        style_state.preset_of(p.id).hash(&mut hasher);
    }
    let sig = hasher.finish();

    let dd = data_dir.as_deref();
    if sig != *last_sig {
        // Project set or a preset changed — rebuild the whole cache and
        // drop entries for projects that no longer exist.
        *last_sig = sig;
        let keep: std::collections::HashSet<u64> =
            projects.list.iter().map(|p| p.id).collect();
        themes.retain_projects(&keep);
        for p in &projects.list {
            themes.set(
                p.id,
                jim_style::resolve_project_theme(p.id, &style_state, &registry, dd),
            );
        }
    } else if theme_edited {
        // Live edit of the active project's theme.ft — reload just it.
        if let Some(pid) = projects.active {
            themes.set(
                pid,
                jim_style::resolve_project_theme(pid, &style_state, &registry, dd),
            );
        }
    }
}

/// Cmd+Shift+D opens the style dev panel (a funct widget). Lets you
/// scrub dust / edit / age / time_scale without waiting for real time
/// to pass. Spawning goes through the same `PendingActions.new_panes`
/// channel the radial menu uses, so all the usual pane-bevy chrome
/// applies.
/// `view.dev_panel` action (Cmd+Shift+D). Opens the style dev panel
/// (a funct widget). Dedups: each spawn leaves a fresh funct worker thread
/// ticking the script at 30 Hz (~50% CPU per duplicate), so if a dev
/// panel already exists anywhere on the canvas, silently do nothing.
fn action_open_dev_panel(ctx: &mut actions::ActionCtx) {
    let exists = {
        let mut q = ctx
            .world
            .query::<&jim_widget::script_widget::ScriptWidget>();
        q.iter(ctx.world).any(|w| w.script == "dev_panel.ft")
    };
    if exists {
        return;
    }
    let Some(active) = ctx.world.resource::<projects::Projects>().active else {
        return;
    };
    ctx.world
        .resource_mut::<projects::PendingActions>()
        .new_panes
        .push(projects::NewPaneRequest {
            kind: jim_widget::script_widget::PANE_KIND,
            project_id: active,
            origin: None,
            size: Some(Vec2::new(420.0, 280.0)),
            config: serde_json::json!({
                "script": "dev_panel.ft",
                "title": "Style dev panel",
            }),
        });
}

/// `view.toggle_cube` action. Mirrors the `IpcRequest::ToggleCube` path
/// (and `cube.rs`'s own Cmd+Shift+\ keybind) by flipping the prism's
/// pending-toggle flag.
fn action_toggle_cube(ctx: &mut actions::ActionCtx) {
    ctx.world.resource_mut::<cube::Prism>().pending_toggle = true;
}

/// `canvas.cycle_pan_preset` — advance to the next pan-gesture preset.
fn action_cycle_pan_preset(ctx: &mut actions::ActionCtx) {
    canvas::cycle_pan_preset(ctx.world);
}

/// `style.glaze_ui_showcase` — open the Glaze design-system showcase.
/// `glaze_ui` is a *subprocess widget* (NDJSON `WidgetMsg` on stdio), so
/// it spawns as a widget pane in the active project, not as its own
/// window. The binary is looked up next to the running executable
/// first, then in the dev target dir this build came from.
fn action_open_glaze_ui(ctx: &mut actions::ActionCtx) {
    // glaze_ui ships next to `jim` (target/release in dev, Contents/MacOS in
    // the bundle), so resolve it relative to the running exe. No baked-in
    // builder path — that only ever exists on the machine that compiled.
    let candidate = exe_dir().map(|d| d.join("glaze_ui"));
    let Some(bin) = candidate.filter(|p| p.exists()) else {
        error!(
            "glaze_ui binary not found next to {:?} — build it with \
             `cargo build --release` (it's a default workspace member)",
            exe_dir()
        );
        return;
    };
    let Some(active) = ctx.world.resource::<projects::Projects>().active else {
        return;
    };
    ctx.world
        .resource_mut::<projects::PendingActions>()
        .new_panes
        .push(projects::NewPaneRequest {
            kind: jim_widget::PANE_KIND,
            project_id: active,
            origin: None,
            size: Some(Vec2::new(820.0, 900.0)),
            config: serde_json::json!({
                "command": bin.to_string_lossy(),
                "title": "Glaze UI",
            }),
        });
}

/// `pane.close_focused` — route the focused pane through the normal close
/// path (runs the kind's `on_close`, then despawns). No-op when nothing is
/// focused.
fn action_close_focused(ctx: &mut actions::ActionCtx) {
    if let Some(e) = ctx.world.resource::<jim_pane::FocusedPane>().0 {
        ctx.world
            .resource_mut::<jim_pane::PendingPaneActions>()
            .close
            .push(e);
    }
}

/// `pane.focus_next` / `pane.focus_prev` — move keyboard focus to the next
/// / previous pane in the active project, ordered back-to-front by z and
/// wrapping around.
fn action_focus_next(ctx: &mut actions::ActionCtx) {
    cycle_focus(ctx.world, 1);
}

fn action_focus_prev(ctx: &mut actions::ActionCtx) {
    cycle_focus(ctx.world, -1);
}

fn cycle_focus(world: &mut World, dir: i32) {
    let Some(active) = world.resource::<projects::Projects>().active else {
        return;
    };
    // Active-project panes, ordered back-to-front by z so the cycle order
    // matches the visual stack.
    let mut panes: Vec<(Entity, f32)> = world
        .query_filtered::<(Entity, &jim_pane::PaneRect, &jim_pane::PaneProject), With<jim_pane::PaneTag>>()
        .iter(world)
        .filter(|(_, _, proj)| proj.0 == active)
        .map(|(e, rect, _)| (e, rect.z))
        .collect();
    if panes.is_empty() {
        return;
    }
    panes.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let order: Vec<Entity> = panes.into_iter().map(|(e, _)| e).collect();

    let cur = world.resource::<jim_pane::FocusedPane>().0;
    let next = match cur.and_then(|c| order.iter().position(|&e| e == c)) {
        Some(i) => {
            let n = order.len() as i32;
            order[(((i as i32 + dir) % n + n) % n) as usize]
        }
        // Nothing (or an off-project pane) focused: enter the stack from
        // the front when going forward, the back when going backward.
        None if dir >= 0 => *order.last().unwrap(),
        None => *order.first().unwrap(),
    };
    world.resource_mut::<jim_pane::FocusedPane>().0 = Some(next);
}

/// The single authority for `jim_pane::KeyboardOwner` — runs in
/// `PreUpdate`, before any keyboard consumer in `Update`, so every
/// handler sees a consistent owner for the frame. Precedence: a text-
/// entry modal (command palette or project rename) owns everything; else
/// the focused pane owns typing; else nobody. See the type docs in
/// `pane-bevy` for why this replaces the old per-handler focus gating.
fn compute_keyboard_owner(
    palette: Res<command_palette::CommandPalette>,
    renaming: Res<projects::Renaming>,
    pending_seq: Res<actions::PendingSequence>,
    focused: Res<jim_pane::FocusedPane>,
    mut owner: ResMut<jim_pane::KeyboardOwner>,
) {
    // A pending chord sequence also claims the keyboard: the continuation
    // key (e.g. the `C` in `⌘K C`) must reach the action matcher, not the
    // focused pane. The matcher itself special-cases `Modal` while a
    // sequence is in progress, so it keeps reading.
    let next = if renaming.id.is_some() || palette.open || !pending_seq.chords.is_empty() {
        jim_pane::KeyboardOwner::Modal
    } else if let Some(e) = focused.0 {
        jim_pane::KeyboardOwner::Pane(e)
    } else {
        jim_pane::KeyboardOwner::None
    };
    if *owner != next {
        *owner = next;
    }
}

/// Track the active theme's `canvas_bg` token in `ClearColor` so a
/// preset switch retones the void around the dust shader (visible at
/// pane rounded-corners + during the windex sweep).
fn sync_canvas_clear_color(
    theme: Res<jim_style::Theme>,
    mut clear: ResMut<ClearColor>,
) {
    if !theme.is_changed() {
        return;
    }
    let c = Color::LinearRgba(theme.color(jim_style::tokens::CANVAS_BG));
    if clear.0 != c {
        clear.0 = c;
    }
}

/// Switch the winit update mode between Continuous (every frame) and
/// Reactive (only on input + a 5s heartbeat) depending on whether the
/// active visual preset needs to animate. Continuous burns ~1.5 cores
/// at 60fps because the dust shader and chrome materials all redraw
/// every frame; Reactive is battery-friendly. The transition itself
/// is event-driven (preset switch), so reactive mode reliably wakes
/// up to handle it.
///
/// Rule: a preset that ships a chrome.wgsl that references
/// `params.time` is assumed to animate. Static custom shaders
/// (sketch, mesh, blueprint) are *not* animated and stay on
/// Reactive — they paint once per Reactive frame and that's fine.
fn maintain_winit_mode_for_animation(
    preset: Res<jim_style::ActiveStylePreset>,
    registry: Res<jim_style::StylePresetRegistry>,
    drawer: Res<drawer::Drawer>,
    prism: Res<cube::Prism>,
    expose: Res<expose::Expose>,
    palette: Res<command_palette::CommandPalette>,
    script_widgets: Query<&jim_widget::script_widget::ScriptWidget>,
    widget_anim: Res<jim_widget::anim::WidgetAnim>,
    // While the trace recorder is capturing it reads the span ring each tick;
    // it needs a steady ~10Hz reactive wake (not full Continuous) to advance.
    trace_recorder: Option<Res<jim_flame::TraceRecorder>>,
    mut settings: ResMut<bevy::winit::WinitSettings>,
    time: Res<Time>,
    mut pin_watch: ResMut<diagnostics::ContinuousWatch>,
    animated_panes: Query<(), With<AnimatedChromePane>>,
    mut chrome_animates: ResMut<ChromeAnimates>,
) {
    let preset_animates = preset.0.as_deref().map_or(false, |name| {
        registry
            .presets
            .iter()
            .find(|p| p.name == name)
            .map_or(false, |p| p.chrome_animates)
    });
    // Tell `push_chrome_time` whether any on-screen chrome shader reads
    // `params.time`: the global active preset, or any per-pane override
    // (cube overview). When false it skips dirtying every chrome material
    // each frame — the dominant idle GPU/CPU cost. Compare-before-write so
    // we don't mark the resource `Changed` every frame.
    let any_chrome_animates = preset_animates || !animated_panes.is_empty();
    if chrome_animates.0 != any_chrome_animates {
        chrome_animates.0 = any_chrome_animates;
    }
    // A funct widget that opted into animation via `set_animating(true)`
    // (e.g. the datalog IDE results pane draining a `datalog` subprocess in
    // `on_frame`, or chess polling Stockfish) also needs every frame. Without
    // this term, the reactive loop only wakes ~every 5s while the window is
    // idle, so the widget's `on_frame` tick — and thus its proc-drain —
    // arrives ~5s late even though the underlying work finished in ms.
    let widget_animating = script_widgets.iter().any(|w| w.is_animating());
    // A *slow-ticking* widget (auto-refresh poll, e.g. http.ft on a 300s
    // interval) wants periodic `on_frame` ticks but explicitly does NOT
    // need 60fps. Serving it from the reactive loop instead of Continuous
    // is the difference between ~150% CPU (one poller pins the whole app
    // at 60fps) and ~0%. Collect the smallest requested cadence so the
    // reactive wake is frequent enough for the most eager slow-ticker.
    let widget_tick_min: Option<f32> = script_widgets
        .iter()
        .filter_map(|w| w.tick_interval_secs())
        .fold(None, |acc, s| Some(acc.map_or(s, |a: f32| a.min(s))));
    // The drawer's slide and the 3D project prism (live textures + camera
    // animation) are the other sources of "needs every frame". The cooldown
    // keeps redrawing briefly after the prism closes so the flat panes
    // repaint instead of staying black.
    let want_continuous = preset_animates
        || widget_animating
        // A Glaze `transition` mid-flight (toggle knob slide, track
        // crossfade) needs per-frame ticks for its ~100-700ms duration.
        // If the loop stalls anyway (unfocused), the tween self-heals:
        // the next wake's large dt snaps it to its end state.
        || widget_anim.any_in_flight()
        || drawer.animating()
        || prism.active
        || prism.continuous_cooldown > 0
        // Exposé: keep painting while the grid is open (a sustained
        // source, like the prism — deliberately excluded from the
        // transient-pin warning below) and while it settles/closes.
        || expose.active
        || expose.continuous_cooldown > 0;
    // NOTE: an open command palette is deliberately NOT a Continuous source.
    // It only needs its DeepSeek worker result polled promptly; pinning full
    // 60fps for that is wasteful. Below it instead tightens the *reactive*
    // cadence to ~30Hz. Keystrokes still wake the loop instantly in reactive
    // mode (input events always wake), so typing stays snappy.

    // Regression canary: time how long we've been Continuous for a
    // *transient* reason (one that should resolve in seconds). Excludes
    // the intentionally-sustained sources (animated theme, active 3D prism,
    // open Exposé) — those legitimately stay continuous as long as the user
    // wants and must NOT trip the warning bar. The command palette is no
    // longer a Continuous source at all (it only tightens the reactive
    // cadence below), so it likewise never appears in transient_reasons.
    // If a transient reason holds past `CONTINUOUS_WARN_SECS`,
    // `update_continuous_pin_overlay` shows a yellow bar naming the
    // culprit. See diagnostics.rs.
    let mut transient_reasons: Vec<String> = Vec::new();
    if widget_animating {
        let mut stems: Vec<&str> = script_widgets
            .iter()
            .filter(|w| w.is_animating())
            .filter_map(|w| w.script_path.file_stem().and_then(|s| s.to_str()))
            .collect();
        stems.sort_unstable();
        stems.dedup();
        transient_reasons.push(format!("widget:{}", stems.join(",")));
    }
    if widget_anim.any_in_flight() {
        transient_reasons.push("glaze-transition".into());
    }
    if drawer.animating() {
        transient_reasons.push("drawer".into());
    }
    if prism.continuous_cooldown > 0 {
        transient_reasons.push("prism-cooldown".into());
    }
    if transient_reasons.is_empty() {
        pin_watch.held_secs = 0.0;
        pin_watch.reason.clear();
    } else {
        pin_watch.held_secs += time.delta_secs();
        pin_watch.reason = transient_reasons.join(" + ");
    }
    // Tick down the post-dismissal snooze. While it's positive the overlay
    // stays hidden even if the app is still pinned; when it hits zero a
    // still-stuck pin re-raises the bar.
    if pin_watch.mute_remaining_secs > 0.0 {
        pin_watch.mute_remaining_secs =
            (pin_watch.mute_remaining_secs - time.delta_secs()).max(0.0);
    }

    // Idle baseline is reactive(5s). A slow-ticker tightens that to its
    // cadence (clamped to [0.1s, 5s] — never slower than the baseline, and
    // floored so a sub-100ms request can't masquerade as cheap polling
    // when it really wants `set_animating`). For a 300s poll this stays at
    // 5s: the host wakes every 5s, the tick scheduler delivers the actual
    // `on_frame` only once the widget's 300s elapses. Near-zero CPU.
    // An active trace recorder tightens the reactive cadence to ~10Hz (like a
    // fast slow-ticker) so its capture advances smoothly without Continuous.
    let recording = trace_recorder.map_or(false, |r| r.recording);
    let effective_tick = match (widget_tick_min, recording) {
        (Some(iv), true) => Some(iv.min(0.1)),
        (Some(iv), false) => Some(iv),
        (None, true) => Some(0.1),
        (None, false) => None,
    }
    // Clamp before the palette term: the [0.1s, 5s] floor exists to stop
    // *widget-requested* sub-100ms intervals, not host-side cadences.
    .map(|iv: f32| iv.clamp(0.1, 5.0));
    // An open command palette tightens the cadence to ~30Hz — enough that
    // its DeepSeek worker result lands promptly and streamed transcript
    // updates read as live, at half the frames of the old Continuous pin.
    // Keystroke latency doesn't depend on this: input events wake the
    // reactive loop immediately.
    let effective_tick = if palette.open {
        const PALETTE_TICK: f32 = 1.0 / 30.0;
        Some(effective_tick.map_or(PALETTE_TICK, |iv| iv.min(PALETTE_TICK)))
    } else {
        effective_tick
    };
    let target = if want_continuous {
        bevy::winit::UpdateMode::Continuous
    } else if let Some(iv) = effective_tick {
        bevy::winit::UpdateMode::reactive(std::time::Duration::from_secs_f32(iv))
    } else {
        bevy::winit::UpdateMode::reactive(std::time::Duration::from_secs(5))
    };
    if settings.focused_mode != target {
        settings.focused_mode = target;
    }
    // A proc-polling widget (datalog query drain, chess vs Stockfish)
    // must keep ticking even when the window loses focus, or its work
    // hangs the moment the user clicks away. The other continuous
    // sources are decorative and don't need unfocused frames, so only an
    // animating widget escalates the unfocused mode.
    let unfocused_target = if widget_animating {
        bevy::winit::UpdateMode::Continuous
    } else if let Some(iv) = widget_tick_min {
        // Keep slow pollers ticking while unfocused too, but no faster
        // than they asked and never tighter than the 60s low-power floor.
        bevy::winit::UpdateMode::reactive_low_power(std::time::Duration::from_secs_f32(
            iv.clamp(0.1, 60.0),
        ))
    } else {
        bevy::winit::UpdateMode::reactive_low_power(std::time::Duration::from_secs(60))
    };
    if settings.unfocused_mode != unfocused_target {
        settings.unfocused_mode = unfocused_target;
    }
}

/// Cmd+Shift+T opens the live theme editor (a funct widget). OkLCh
/// steppers per color token; click to focus a token, then ± each
/// of L / C / h. Writes propagate to the active preset's `theme.ft`
/// via the bridge; notify watcher reloads it and the rest of the
/// app retones the same frame.
/// `view.theme_editor` action (Cmd+Shift+T). Opens the live theme editor
/// (a funct widget): OkLCh steppers per color token that write back to the
/// active preset's `theme.ft`. Dedups like the dev panel.
fn action_open_theme_editor(ctx: &mut actions::ActionCtx) {
    let exists = {
        let mut q = ctx
            .world
            .query::<&jim_widget::script_widget::ScriptWidget>();
        q.iter(ctx.world).any(|w| w.script == "theme_editor.ft")
    };
    if exists {
        return;
    }
    let Some(active) = ctx.world.resource::<projects::Projects>().active else {
        return;
    };
    ctx.world
        .resource_mut::<projects::PendingActions>()
        .new_panes
        .push(projects::NewPaneRequest {
            kind: jim_widget::script_widget::PANE_KIND,
            project_id: active,
            origin: None,
            size: Some(Vec2::new(420.0, 600.0)),
            config: serde_json::json!({
                "script": "theme_editor.ft",
                "title": "Theme editor",
            }),
        });
}

/// Cmd+Shift+S opens the style preset picker (a funct widget). Lists
/// every preset under `~/.jim/styles/` plus a `(per-project
/// theme)` entry; clicking switches the active style and persists the
/// choice. Same dedup logic as the dev panel.
/// `view.style_picker` action (Cmd+Shift+S). Opens the style preset
/// picker (a funct widget). No dedup: each instance is a parked, event-
/// driven worker (~zero idle CPU), so stacking a few is cheap.
fn action_open_style_picker(ctx: &mut actions::ActionCtx) {
    let Some(active) = ctx.world.resource::<projects::Projects>().active else {
        return;
    };
    ctx.world
        .resource_mut::<projects::PendingActions>()
        .new_panes
        .push(projects::NewPaneRequest {
            kind: jim_widget::script_widget::PANE_KIND,
            project_id: active,
            origin: None,
            size: Some(Vec2::new(280.0, 240.0)),
            config: serde_json::json!({
                "script": "style_picker.ft",
                "title": "Styles",
            }),
        });
}

/// `view.chess` action. Opens the chess widget — a funct widget that
/// plays against Stockfish over UCI. Dedups on the script name: each
/// instance spawns its own engine subprocess, so one board is plenty.
fn action_open_chess(ctx: &mut actions::ActionCtx) {
    let exists = {
        let mut q = ctx
            .world
            .query::<&jim_widget::script_widget::ScriptWidget>();
        q.iter(ctx.world).any(|w| w.script == "chess.ft")
    };
    if exists {
        return;
    }
    let Some(active) = ctx.world.resource::<projects::Projects>().active else {
        return;
    };
    ctx.world
        .resource_mut::<projects::PendingActions>()
        .new_panes
        .push(projects::NewPaneRequest {
            kind: jim_widget::script_widget::PANE_KIND,
            project_id: active,
            origin: None,
            size: Some(Vec2::new(520.0, 640.0)),
            config: serde_json::json!({
                "script": "chess.ft",
                "title": "Chess",
            }),
        });
}

/// When the user focuses any pane, mark that pane's project as
/// recently-active so its dust timer resets. Skips the very first
/// observation after startup — that one fires when the persistence
/// layer restores focus, and counting "we restored your focus state"
/// as engagement would zero out dust across restarts.
fn mirror_focus_to_style(
    focused: Res<jim_pane::FocusedPane>,
    pane_projects: Query<&jim_pane::PaneProject>,
    mut state: ResMut<jim_style::ProjectStyleState>,
    mut last: Local<Option<Entity>>,
    mut warmed_up: Local<bool>,
) {
    if !focused.is_changed() {
        return;
    }
    let Some(entity) = focused.0 else {
        *last = None;
        return;
    };
    if *last == Some(entity) {
        return;
    }
    *last = Some(entity);
    if !*warmed_up {
        *warmed_up = true;
        return;
    }
    if let Ok(pp) = pane_projects.get(entity) {
        state.note_focus(pp.0);
    }
}
