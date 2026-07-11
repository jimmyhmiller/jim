//! Minimal Bevy-native terminal emulator built on libghostty-vt.
//!
//! Extracted from `jim-app` as a self-contained widget crate. The app
//! shell (`jim_app`) depends on this crate and adds [`TerminalPlugin`]
//! to register the terminal pane kind, its font/atlas, and the terminal
//! systems. Shell-coupled glue (scroll into the active project, bell /
//! Claude-notification badge pulses) stays in `jim_app`.
//!
//! ## Threading
//!
//! `libghostty_vt::Terminal` is `!Send + !Sync`, so we can't store it as a
//! Bevy `Component`. Instead a single `NonSend<TerminalStore>` resource
//! owns a `HashMap<Entity, TerminalData>`. Entities still carry Send
//! components (rect, chrome, rev counter, row-entity pool); the
//! non-Send runtime lives in the store keyed by the same entity.
//!
//! ## Rendering
//!
//! Each terminal grid is a single textured quad sampling glyphs from a
//! shared `GlyphAtlas`; `sync_grid` rewrites the cells texture when the
//! worker publishes a new snapshot.

use std::collections::HashMap;
use std::path::PathBuf;

use bevy::image::{Image, TextureAtlasLayout};
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::sprite::Anchor;

use libghostty_vt::style::RgbColor;
use jim_pane::{
    spawn_pane, FocusedPane, PaneContentPressed, PaneFont, PaneKindMarker, PaneRect, PaneRegistry,
    PaneTag, SpawnedPane, MARGIN, TITLE_H,
};
use serde_json::Value;

pub mod atlas;
pub mod command_watch;
pub mod daemon_client;
pub mod osc7;
pub mod pty;
pub mod selection;
pub mod term_material;
pub mod vt;
pub mod worker;

/// Re-export of the daemon protocol from the headless crate so existing
/// callers can continue to write `jim_terminal::daemon_proto::*`.
pub use jim_daemon::proto as daemon_proto;

use atlas::GlyphAtlas;
use term_material::{
    make_cells_image, pack_rgb, GpuCell, TermMaterial, TermMaterialPlugin, TermParams,
};
use pty::PtySize;
use worker::{MouseAction, MouseBtn, SnapCell, WorkerHandle, WorkerMsg};

pub const FONT_SIZE: f32 = 14.0;
pub const LINE_HEIGHT: f32 = 18.0;
pub const SCROLLBACK_LINES: usize = 100_000;

/// Stable identifier for terminal panes. Stored on every terminal pane
/// in `PaneKindMarker` and referenced by the registry.
pub const PANE_KIND: &str = "terminal";

/// Candidate monospace fonts tried in order, first readable one wins.
/// SF Mono is preferred — Apple ships it with Terminal.app, so any Mac
/// that has launched Terminal.app once has it — but we fall back to other
/// common monospace faces so the terminal still starts on a machine that
/// lacks it. Override the whole search with the `TERMINAL_BEVY_FONT` env
/// var (absolute path to a `.otf`/`.ttf`).
const PRIMARY_FONT_CANDIDATES: &[&str] = &[
    "/Library/Fonts/SF-Mono-Regular.otf",
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/Library/Fonts/Andale Mono.ttf",
];

/// Loads the primary monospace font and leaks it into a `'static` slice
/// so the atlas (which holds a borrow of the font bytes for `swash`) sees
/// a stable address for the program's lifetime. Tries `TERMINAL_BEVY_FONT`
/// first, then [`PRIMARY_FONT_CANDIDATES`]; panics with the full list if
/// none can be read (the terminal cannot render without a font).
pub fn load_primary_font() -> &'static [u8] {
    let mut tried: Vec<String> = Vec::new();
    let override_path = std::env::var_os("TERMINAL_BEVY_FONT");
    let candidates = override_path
        .as_ref()
        .map(|p| std::path::Path::new(p))
        .into_iter()
        .map(std::borrow::Cow::Borrowed)
        .chain(
            PRIMARY_FONT_CANDIDATES
                .iter()
                .map(|p| std::borrow::Cow::Owned(std::path::PathBuf::from(p))),
        );
    for path in candidates {
        match std::fs::read(path.as_ref()) {
            Ok(bytes) => return Box::leak(bytes.into_boxed_slice()),
            Err(e) => tried.push(format!("  {}: {}", path.as_ref().display(), e)),
        }
    }
    panic!(
        "no usable monospace font found — set TERMINAL_BEVY_FONT to an \
         absolute .otf/.ttf path. Tried:\n{}",
        tried.join("\n")
    );
}

/// Root for all on-disk persistence (projects + per-terminal scrollback).
/// `~/.jim/` on every supported platform.
///
/// Delegates to `jim_daemon::data_dir` so the daemon process and
/// the editor process agree on the location of socket / pid files.
pub fn data_dir() -> Option<PathBuf> {
    jim_daemon::data_dir()
}

/// Per-terminal scrollback log. Raw pty bytes are appended here as they
/// flow from the child; on restore the bytes are replayed into the new
/// libghostty Terminal so the visible scrollback persists across runs.
pub fn scrollback_path(session_id: u64) -> Option<PathBuf> {
    let mut p = data_dir()?;
    p.push("scrollback");
    Some(p.join(format!("{}.bytes", session_id)))
}

/// Unix socket the per-session daemon listens on. Forwards to the
/// daemon crate so client and daemon share a single source of truth.
pub fn socket_path(session_id: u64) -> Option<PathBuf> {
    jim_daemon::socket_path(session_id)
}

/// PID file the daemon writes on startup. Same delegation as
/// [`socket_path`].
pub fn pid_path(session_id: u64) -> Option<PathBuf> {
    jim_daemon::pid_path(session_id)
}

/// Pick the shell to launch in a new daemon. Resolution: `$SHELL` →
/// passwd entry → `/bin/sh`. Returns a `Vec<String>` so it slots into
/// `Command::new(args[0]).args(&args[1..])` cleanly. Matches what
/// `Pty::spawn` used to do before we moved PTY ownership into the daemon.
///
/// `-l` makes it a login shell so macOS's `/etc/zprofile` runs
/// `/usr/libexec/path_helper` — without it `PATH` is missing
/// `/opt/homebrew/bin` etc., breaking tools like autojump.
pub fn default_shell_command() -> Vec<String> {
    use std::path::PathBuf as PB;
    let shell = match std::env::var_os("SHELL") {
        Some(s) if !s.is_empty() => PB::from(s),
        _ => match nix::unistd::User::from_uid(nix::unistd::getuid()) {
            Ok(Some(user)) => user.shell,
            _ => PB::from("/bin/sh"),
        },
    };
    vec![shell.to_string_lossy().into_owned(), "-l".to_string()]
}

// ---------- Per-entity runtime ----------

/// Per-entity handle to the worker thread. Plain `Send` data — the
/// `!Send` libghostty `Terminal` lives entirely on the worker, so the
/// main Bevy thread sees only the snapshot mutex + a message channel.
pub struct TerminalData {
    pub worker: WorkerHandle,
}

#[derive(Default, Resource)]
pub struct TerminalStore {
    pub map: HashMap<Entity, TerminalData>,
}

// ---------- Components (Send) ----------

/// Stable id used to key per-terminal on-disk state (scrollback log,
/// layout snapshot in `projects.json`). Allocated by `Projects` and
/// preserved across restarts so a restored terminal finds its old
/// scrollback file.
#[derive(Component, Copy, Clone, Debug)]
pub struct TerminalSession(pub u64);

/// Cursor sprite child of a terminal pane's content_root. Held on the
/// pane entity so `sync_grid` can position/show-hide the cursor without
/// looking it up by traversal.
#[derive(Component, Copy, Clone)]
pub struct TerminalCursor(pub Entity);

/// Per-terminal GPU grid state. One `TermMaterial` + cells texture per
/// pane; the worker → sync_grid pipeline rewrites texels of the cells
/// texture and Bevy re-uploads it.
///
/// `last_rendered_generation` is compared against the worker's snapshot
/// generation to skip whole frames when the grid hasn't changed.
#[derive(Component)]
pub struct TermGrid {
    pub material: Handle<term_material::TermMaterial>,
    pub cells_image: Handle<Image>,
    pub mesh: Handle<Mesh>,
    /// Entity (child of `content_root`) carrying the `Mesh2d` +
    /// `MeshMaterial2d<TermMaterial>`.
    pub render_entity: Entity,
    pub cols: u16,
    pub rows: u16,
    pub last_rendered_generation: u64,
    /// Was this pane visible the last time sync_grid touched it? Used
    /// to detect the hidden→visible transition so we can force a full
    /// repaint of the cells texture (the worker has been processing pty
    /// bytes the whole time but not pushing snapshots into the GPU).
    pub was_visible: bool,
}

/// Bumped whenever the Terminal for this entity is mutated (vt bytes
/// processed, resize). `sync_grid` rebuilds row spans when it differs
/// from the value we last rendered.
#[derive(Component, Default)]
pub struct TerminalRev(pub u64);

/// Per-terminal bell-tracking state. `last_seen` mirrors the worker's
/// `bell_count` so we only react to *new* bells — incrementing the
/// project counter once each, never every frame the counter is non-zero.
#[derive(Component, Default)]
pub struct BellPulse {
    pub last_seen: u64,
}

#[derive(Resource)]
pub struct MonoFont(pub Handle<Font>);

#[derive(Resource, Copy, Clone)]
pub struct MonoMetrics {
    pub cell_width: f32,
}

/// Per-terminal text selection.
///
/// `anchor` and `head` are `(col, absolute_row)`:
/// - `col` is a grid column, `i32` so out-of-bounds drag positions
///   don't lose direction.
/// - `absolute_row` is `i64`, indexing into libghostty's *total*
///   scrollable area (scrollback + active) — i.e.,
///   `snapshot.viewport_offset + viewport_row` at the moment of the
///   click. Anchoring against the absolute row makes a selection
///   follow its content while the user scrolls the viewport.
///
/// Limitation: when libghostty's bounded scrollback wraps (oldest line
/// pushed out), all absolute rows shift down by one. We don't
/// compensate; selections older than the wrap point will drift. In
/// practice selections are short-lived enough that this is fine.
#[derive(Component, Default, Debug)]
pub struct TerminalSelection {
    pub anchor: Option<(i32, i64)>,
    pub head: Option<(i32, i64)>,
    /// True while the user is mid-drag selecting. Cleared on mouse-up.
    /// Per-frame drag-update checks this instead of consulting a global
    /// mouse-mode enum.
    pub dragging: bool,
    /// Pool of overlay sprite entities visualising the selection
    /// (children of the terminal's `content_root`). Rebuilt by the
    /// selection-render system as the selection changes.
    pub overlays: Vec<Entity>,
    /// Inputs the current `overlays` were built from. `render_selection_overlays`
    /// compares the live inputs against this each frame and skips the
    /// (expensive) despawn/respawn of one sprite-per-row when nothing that
    /// affects overlay geometry changed. `None` whenever there are no
    /// overlays (inactive selection, zero-size grid, or never built).
    pub overlay_cache: Option<SelectionOverlayCache>,
}

/// Snapshot of every input that determines the selection-overlay geometry.
/// While a selection is live the render system would otherwise despawn and
/// respawn a `Sprite` per selected visible row EVERY frame (and lock the
/// snapshot mutex for `viewport_offset`) even when the resulting strips are
/// byte-identical. Caching these lets it rebuild only on a real change —
/// moving the selection, scrolling, resizing, or a theme recolor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionOverlayCache {
    /// Normalised selection endpoints, `(col, absolute_row)`.
    pub start: (i32, i64),
    pub end: (i32, i64),
    /// libghostty viewport offset at build time — scrolling during a
    /// selection slides the visible strips, so a change here must rebuild.
    pub offset: i64,
    /// Grid dimensions and cell width; a resize or font-metric change alters
    /// strip extents and positions. (`LINE_HEIGHT` is a compile-time const,
    /// so it is intentionally not tracked.)
    pub cols: i32,
    pub rows: i32,
    pub cell_w: f32,
    /// Resolved selection color; a theme change recolors every strip.
    pub color: Color,
    /// The `content_root` the strips are parented under; if a pane's content
    /// root entity ever changes the strips must be reparented (rebuilt).
    pub content_root: Entity,
}

impl TerminalSelection {
    pub fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
        self.dragging = false;
    }
    pub fn is_active(&self) -> bool {
        match (self.anchor, self.head) {
            (Some(a), Some(h)) => a != h,
            _ => false,
        }
    }
    /// Return (start, end) normalised so start ≤ end in line-flow order.
    pub fn normalised(&self) -> Option<((i32, i64), (i32, i64))> {
        let (a, h) = (self.anchor?, self.head?);
        let order = (a.1, a.0) <= (h.1, h.0);
        if order {
            Some((a, h))
        } else {
            Some((h, a))
        }
    }
}

// ---------- Plugin ----------

/// Registers ONLY the terminal-widget concerns: the GPU material, the
/// selection plugin, the terminal font/atlas startup, the terminal pane
/// kind, and the per-frame terminal systems (grid sync, keyboard, resize,
/// file drop, content press, selection drag). The app shell adds this via
/// `app.add_plugins(jim_terminal::TerminalPlugin)` and layers its own
/// camera / project / canvas / etc. plugins on top.
pub struct TerminalPlugin;

impl Plugin for TerminalPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(TermMaterialPlugin)
            .add_plugins(selection::SelectionPlugin)
            .add_systems(Startup, (setup_terminal_font, register_terminal_kind))
            .add_systems(
                Update,
                (
                    handle_terminal_mouse_report,
                    handle_terminal_content_press,
                    handle_terminal_selection_drag,
                    handle_resize,
                    handle_keyboard,
                    handle_file_drop,
                    sync_grid,
                ),
            );
    }
}

/// Set up the terminal's monospace font, glyph atlas, cell metrics, and
/// the `TerminalStore`. Split out of the app shell's old
/// `setup_camera_and_font`: this half owns everything terminal-specific
/// (and the `PaneFont` / `PaneFontMetrics` chrome glyphs, which are the
/// same SF Mono face), while the camera/overlay-camera setup stays in
/// `jim_app`.
///
/// Exposed so the shell can order its own startup systems relative to it
/// (e.g. `jim_editor::setup_editor_font.after(setup_terminal_font)`).
///
/// Note we deliberately do *not* call `CosmicFontSystem::load_system_fonts`
/// — Text2d/cosmic-text isn't on the rendering path anymore (we draw
/// glyphs from our own atlas), and loading every font on the system
/// adds ~100ms of cold-start cost for nothing.
pub fn setup_terminal_font(world: &mut World) {
    let font_bytes: &'static [u8] = load_primary_font();

    let font_handle = world
        .resource_mut::<Assets<Font>>()
        .add(Font::from_bytes(font_bytes.to_vec()));
    world.insert_resource(MonoFont(font_handle.clone()));
    // pane-bevy uses this for chrome glyphs (close button, title text).
    world.insert_resource(PaneFont(font_handle));

    let cell_width = measure_cell_width(font_bytes, FONT_SIZE);
    world.insert_resource(MonoMetrics { cell_width });
    world.insert_resource(jim_pane::PaneFontMetrics {
        cell_width,
        font_size: FONT_SIZE,
    });

    // Build the glyph atlas now — pre-rasterizes printable ASCII so
    // first-frame rendering doesn't pay for it. Needs mutable access
    // to both `Assets<Image>` and `Assets<TextureAtlasLayout>` at the
    // same time; `resource_scope` lifts one out so we can grab the other.
    let atlas = world.resource_scope::<Assets<Image>, _>(|world, mut images| {
        let mut layouts = world.resource_mut::<Assets<TextureAtlasLayout>>();
        GlyphAtlas::new(
            font_bytes,
            FONT_SIZE,
            cell_width,
            LINE_HEIGHT,
            &mut images,
            &mut layouts,
        )
    });
    world.insert_resource(atlas);

    // Init the NonSend store once — spawners populate it per entity.
    world.insert_resource(TerminalStore::default());
}

fn register_terminal_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(jim_pane::PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Terminal",
        radial_icon: Some(">_"),
        default_size: Vec2::new(640.0, 400.0),
        spawn: terminal_spawn_from_config,
        snapshot: terminal_snapshot,
        on_close: Some(terminal_on_close),
    });
}

fn terminal_spawn_from_config(
    world: &mut World,
    entity: Entity,
    content_root: Entity,
    config: &Value,
) {
    let session_id = config
        .get("session_id")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| terminal_id_allocator(world));
    let replay_bytes = scrollback_path(session_id).and_then(|p| std::fs::read(&p).ok());
    let initial_cwd = terminal_initial_cwd(world, entity);
    populate_terminal_pane(
        world,
        entity,
        content_root,
        session_id,
        initial_cwd,
        replay_bytes,
    );
}

fn terminal_snapshot(world: &World, entity: Entity) -> Value {
    let session_id = world
        .get::<TerminalSession>(entity)
        .map(|s| s.0)
        .unwrap_or(0);
    serde_json::json!({ "session_id": session_id })
}

fn terminal_on_close(world: &mut World, entity: Entity) {
    if let Some(store) = world.get_resource::<TerminalStore>()
        && let Some(data) = store.map.get(&entity)
    {
        data.worker.send(WorkerMsg::Shutdown);
    }
    if let Some(mut store) = world.get_resource_mut::<TerminalStore>() {
        store.map.remove(&entity);
    }
    let session_id = world.get::<TerminalSession>(entity).map(|s| s.0);
    if let Some(id) = session_id
        && let Some(p) = scrollback_path(id)
    {
        let _ = std::fs::remove_file(&p);
    }
    terminal_mark_dirty(world);
}

fn measure_cell_width(font_bytes: &[u8], font_size: f32) -> f32 {
    use skrifa::instance::{LocationRef, Size};
    use skrifa::{FontRef, MetadataProvider};
    let font = FontRef::from_index(font_bytes, 0).expect("embedded font must parse");
    let metrics = font.glyph_metrics(Size::new(font_size), LocationRef::default());
    let gid = font.charmap().map('M').expect("font must contain 'M'");
    metrics
        .advance_width(gid)
        .expect("'M' must have an advance width")
}

// ---------- Spawn ----------

/// Create one terminal entity with its chrome + spawn a shell on its pty.
/// Returns the entity so the caller can set focus, tweak z, etc.
///
/// `project_id` tags the terminal with `ProjectMembership` so the sidebar
/// can group + show/hide it. It is REQUIRED: a terminal with no project
/// leaks across every project.
///
/// `session_id` is the persistence key — the worker logs raw pty bytes
/// to `scrollback_path(session_id)` and on restart loads the same file
/// back into a fresh Terminal via `replay_bytes`.
pub fn spawn_terminal(
    world: &mut World,
    rect: PaneRect,
    project_id: u64,
    session_id: u64,
    replay_bytes: Option<Vec<u8>>,
) -> Entity {
    let SpawnedPane {
        entity: terminal_entity,
        content_root,
    } = spawn_pane(world, PANE_KIND, "Terminal", rect, Some(project_id));
    let initial_cwd = terminal_initial_cwd(world, terminal_entity);
    populate_terminal_pane(
        world,
        terminal_entity,
        content_root,
        session_id,
        initial_cwd,
        replay_bytes,
    );
    terminal_entity
}

/// Insert terminal-specific components on an already-spawned pane, spawn
/// its worker, and add the cursor child under `content_root`. Shared
/// between `spawn_terminal` and the registry restore path.
///
/// `initial_cwd` overrides the daemon's default-to-$HOME behavior for
/// this pane's shell. Used to honor a project's remembered
/// `default_cwd` (populated by the inference layer). Only consulted
/// when the daemon has to fork a fresh shell — attaching to an
/// already-running daemon ignores it.
pub fn populate_terminal_pane(
    world: &mut World,
    terminal_entity: Entity,
    content_root: Entity,
    session_id: u64,
    initial_cwd: Option<String>,
    replay_bytes: Option<Vec<u8>>,
) {
    let cell_width = world.resource::<MonoMetrics>().cell_width;
    let rect = *world
        .get::<PaneRect>(terminal_entity)
        .expect("pane entity must already have PaneRect");
    let (cols, rows) = grid_size_for_rect(rect.size, cell_width);

    // Spawn the worker thread up front so the libghostty Terminal +
    // Pty + render iterators all live on the worker side.
    let wakeup = world
        .get_resource::<bevy::winit::EventLoopProxyWrapper>()
        .map(|w| bevy::winit::EventLoopProxy::clone(w));
    let worker = WorkerHandle::spawn(
        session_id,
        default_shell_command(),
        initial_cwd,
        PtySize {
            cols,
            rows,
            cell_width_px: cell_width as u16,
            cell_height_px: LINE_HEIGHT as u16,
        },
        SCROLLBACK_LINES,
        scrollback_path(session_id),
        replay_bytes,
        wakeup,
    )
    .expect("WorkerHandle::spawn");
    let data = TerminalData { worker };

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

    // Build the GPU grid for this terminal: one mesh + one cells texture +
    // one material instance. `sync_grid` updates the cells texture
    // texel-by-texel when the worker publishes a new snapshot.
    let term_grid = build_term_grid(world, content_root, cell_width, cols, rows);

    world.entity_mut(terminal_entity).insert((
        TerminalRev::default(),
        term_grid,
        TerminalSelection::default(),
        BellPulse::default(),
        TerminalSession(session_id),
        TerminalCursor(cursor),
    ));

    world
        .get_resource_mut::<TerminalStore>()
        .expect("TerminalStore resource (did setup_terminal_font run?)")
        .map
        .insert(terminal_entity, data);
}

pub fn grid_size_for_rect(size: Vec2, cell_width: f32) -> (u16, u16) {
    let content_w = (size.x - 2.0 * MARGIN).max(0.0);
    let content_h = (size.y - TITLE_H - 2.0 * MARGIN).max(0.0);
    let cols = ((content_w / cell_width).floor() as u16).max(1);
    let rows = ((content_h / LINE_HEIGHT).floor() as u16).max(1);
    (cols, rows)
}

/// Spawn the single quad+material that renders an entire terminal grid
/// on the GPU. Returns the `TermGrid` component that goes on the pane
/// entity; the render entity itself becomes a child of `content_root`.
///
/// Public so other VT-grid pane kinds (jim-emacs) can build the same
/// pipeline; `sync_grid` then serves them via their `TerminalStore` entry.
pub fn build_term_grid(
    world: &mut World,
    content_root: Entity,
    cell_width: f32,
    cols: u16,
    rows: u16,
) -> TermGrid {
    // Snapshot atlas geometry up-front so we don't hold the resource
    // borrow across the asset writes below.
    let (atlas_cols, atlas_slot_w, atlas_slot_h, atlas_stride_w, atlas_stride_h, atlas_dim, atlas_image) = {
        let atlas = world.resource::<GlyphAtlas>();
        (
            atlas.cols_per_row(),
            atlas.slot_w(),
            atlas.slot_h(),
            atlas.stride_w(),
            atlas.stride_h(),
            atlas.dim(),
            atlas.image.clone(),
        )
    };

    // Initial background is the worker's default bg (matches what a
    // freshly-spawned libghostty Terminal reports for unwritten cells).
    let default_bg = pack_rgb(13, 15, 20);
    let cells_image = make_cells_image(cols as u32, rows as u32, default_bg);
    let cells_handle = world.resource_mut::<Assets<Image>>().add(cells_image);

    let grid_w = cols as f32 * cell_width;
    let grid_h = rows as f32 * LINE_HEIGHT;
    let mesh_handle = world
        .resource_mut::<Assets<Mesh>>()
        .add(Mesh::from(Rectangle::new(grid_w, grid_h)));

    let params = TermParams {
        cols: cols as u32,
        rows: rows as u32,
        atlas_cols,
        atlas_slot_w,
        atlas_slot_h,
        atlas_dim,
        atlas_stride_w,
        atlas_stride_h,
    };
    let material_handle = world.resource_mut::<Assets<TermMaterial>>().add(TermMaterial {
        params,
        atlas: atlas_image,
        cells: cells_handle.clone(),
    });

    // `Rectangle` mesh is centered on its origin; shift it so top-left
    // lands at the content_root origin (matches where the cursor sprite
    // and the previous per-cell sprites lived).
    //
    // `Visibility::Inherited` is load-bearing: Bevy's 2D extract path
    // queries `&ViewVisibility`, which only exists if `Visibility` (and
    // its required `InheritedVisibility`/`ViewVisibility` companions)
    // is on the entity. Without it the mesh silently never reaches the
    // render world, the shader never runs, and the pane shows the chrome
    // background through what looks like a blank quad.
    let render_entity = world
        .spawn((
            ChildOf(content_root),
            bevy::mesh::Mesh2d(mesh_handle.clone()),
            bevy::sprite_render::MeshMaterial2d(material_handle.clone()),
            Transform::from_xyz(grid_w * 0.5, -(grid_h * 0.5), 0.0),
            Visibility::Inherited,
        ))
        .id();

    TermGrid {
        material: material_handle,
        cells_image: cells_handle,
        mesh: mesh_handle,
        render_entity,
        cols,
        rows,
        last_rendered_generation: 0,
        was_visible: true,
    }
}

// ---------- Resize ----------

/// When a terminal's rect resolves to a different grid dimension than
/// the worker's snapshot reports, send a `Resize` message. The worker
/// applies it, the next snapshot reflects the new dims, and `sync_grid`
/// resizes its sprite pools accordingly.
fn handle_resize(
    metrics: Res<MonoMetrics>,
    store: Res<TerminalStore>,
    rect_q: Query<(Entity, &PaneRect)>,
) {
    for (entity, rect) in &rect_q {
        // Store membership is the gate: every VT-grid pane kind
        // (terminal, emacs) registers its worker here.
        let Some(data) = store.map.get(&entity) else {
            continue;
        };
        let (cols, rows) = grid_size_for_rect(rect.size, metrics.cell_width);
        let (snap_cols, snap_rows) = {
            let g = data.worker.snapshot.lock().expect("snapshot lock");
            (g.cols, g.rows)
        };
        if cols == snap_cols && rows == snap_rows {
            continue;
        }
        data.worker.send(WorkerMsg::Resize {
            cols,
            rows,
            cell_w_px: metrics.cell_width as u32,
            cell_h_px: LINE_HEIGHT as u32,
        });
    }
}

// ---------- Keyboard ----------

/// Translate Bevy key events to VT bytes for the focused terminal.
///
/// Direct mapping (not libghostty's key encoder) for v0 simplicity and
/// to fix space/printable keys landing as `Key::Space` / `Key::Character`
/// rather than going through an encoder path that requires a separate
/// text stream.
fn handle_keyboard(
    mut events: MessageReader<KeyboardInput>,
    mods: Res<ButtonInput<KeyCode>>,
    focused: Res<FocusedPane>,
    owner: Res<jim_pane::KeyboardOwner>,
    store: Res<TerminalStore>,
    kinds: Query<&PaneKindMarker>,
    mut last_drop_reason: Local<Option<&'static str>>,
) {
    // Diagnostic: log on the *transition* into a drop reason whenever
    // a real press event is being dropped. Logs once per reason-change
    // (not per event) so a stuck-Cmd or dead-shell scenario surfaces
    // exactly one stderr line, not a flood.
    let buffered: Vec<KeyboardInput> = events.read().cloned().collect();
    let any_press = buffered.iter().any(|e| e.state.is_pressed());
    let mut report = |reason: &'static str| {
        if any_press && last_drop_reason.as_deref() != Some(reason) {
            eprintln!("[handle_keyboard] dropping key press: {reason}");
            *last_drop_reason = Some(reason);
        }
    };
    // Re-emit so the rest of the function can iterate buffered events
    // without re-reading the channel.
    let mut events_iter = buffered.iter();

    // Skip unless the focused pane is a terminal.
    let target_kind = focused.0.and_then(|e| kinds.get(e).ok());
    if !matches!(target_kind, Some(k) if k.0 == PANE_KIND) {
        report("focused pane is not a terminal");
        return;
    }
    // Central keyboard ownership: a text modal (command palette, project
    // rename) holds `KeyboardOwner::Modal`, so even though this terminal is
    // still the focused pane, it must not consume keystrokes. (Subsumes the
    // old explicit `Renaming` check.)
    if matches!(focused.0, Some(t) if !owner.allows_pane(t)) {
        report("keyboard owned by a text modal");
        return;
    }
    let Some(target) = focused.0 else {
        report("no focused pane");
        return;
    };
    let Some(data) = store.map.get(&target) else {
        report("focused pane has no terminal data");
        return;
    };
    let child_alive = {
        let g = data.worker.snapshot.lock().expect("snapshot lock");
        g.child_alive
    };
    if !child_alive {
        report("shell process has exited (child_alive=false)");
        return;
    }

    let shift = mods.pressed(KeyCode::ShiftLeft) || mods.pressed(KeyCode::ShiftRight);
    let ctrl = mods.pressed(KeyCode::ControlLeft) || mods.pressed(KeyCode::ControlRight);
    let alt = mods.pressed(KeyCode::AltLeft) || mods.pressed(KeyCode::AltRight);
    let cmd = mods.pressed(KeyCode::SuperLeft) || mods.pressed(KeyCode::SuperRight);

    // Cmd-modified keys are owned by app-level handlers (copy/paste,
    // future shortcuts). Skip routing them to the pty so Cmd+C doesn't
    // also send "c" to the shell.
    if cmd {
        report("Cmd modifier held — see stuck-modifier note");
        return;
    }
    // We made it past every gate. Clear any stale drop reason so the
    // next drop transition logs again.
    *last_drop_reason = None;

    // For v0 we always emit xterm-style cursor-key escapes (CSI A/B/C/D);
    // we don't have main-thread visibility into the worker's DECCKM mode.
    // Most shells/readline work fine with these — apps that need SS3
    // form (like vim in normal mode) will be addressed when we route
    // mode bits through the snapshot.
    let app_cursor = false;

    let mut out: Vec<u8> = Vec::with_capacity(16);

    for ev in events_iter.by_ref() {
        if !ev.state.is_pressed() {
            continue;
        }

        // Ctrl + printable letter → control byte (Ctrl+A = 0x01, etc.).
        if ctrl && !alt {
            if let KeyCode::KeyA
            | KeyCode::KeyB
            | KeyCode::KeyC
            | KeyCode::KeyD
            | KeyCode::KeyE
            | KeyCode::KeyF
            | KeyCode::KeyG
            | KeyCode::KeyH
            | KeyCode::KeyI
            | KeyCode::KeyJ
            | KeyCode::KeyK
            | KeyCode::KeyL
            | KeyCode::KeyM
            | KeyCode::KeyN
            | KeyCode::KeyO
            | KeyCode::KeyP
            | KeyCode::KeyQ
            | KeyCode::KeyR
            | KeyCode::KeyS
            | KeyCode::KeyT
            | KeyCode::KeyU
            | KeyCode::KeyV
            | KeyCode::KeyW
            | KeyCode::KeyX
            | KeyCode::KeyY
            | KeyCode::KeyZ = ev.key_code
            {
                let b = keycode_to_ctrl_byte(ev.key_code);
                out.push(b);
                continue;
            }
        }

        // Option+Left / Option+Right: send the readline word-jump
        // bytes (ESC+b, ESC+f) instead of the regular arrow CSI. zsh,
        // bash, fish, and friends all bind these to backward-word /
        // forward-word, matching what macOS Terminal.app and iTerm2 do
        // for Option+arrow. We do this *before* the named-key branch
        // so the regular arrow encoding doesn't fire.
        if alt
            && matches!(ev.key_code, KeyCode::ArrowLeft | KeyCode::ArrowRight)
        {
            out.push(0x1b);
            out.push(if matches!(ev.key_code, KeyCode::ArrowLeft) {
                b'b'
            } else {
                b'f'
            });
            continue;
        }

        // Named keys we know the VT encoding for. Arrows / Home / End
        // honor DECCKM.
        if let Some(bytes) = named_key_bytes(&ev.key_code, app_cursor) {
            // Option+Enter sends ESC+CR (the iTerm2-compatible "meta
            // newline" convention). Shells / readline bind \e\r to
            // self-insert-newline, so the user gets a literal LF in
            // their command line instead of submitting it. Same trick
            // Terminal.app's "Option-Enter inserts newline" uses.
            //
            // Option+Backspace sends ESC+0x7f, which readline/zle bind
            // to `backward-kill-word`. Same meta-prefix convention as
            // Option+Enter.
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

        // Printable text via Bevy's logical_key.
        match &ev.logical_key {
            Key::Character(s) => {
                // Alt acts as Meta (the "Use Option as Meta key" behavior
                // emacs/readline users expect): prefix ESC and, crucially,
                // emit the *base* character derived from the physical key,
                // not the composed logical key. On macOS, holding Option
                // composes the character — Option+x → `≈`, Option+a → `å` —
                // so trusting `logical_key` here would send emacs `M-≈`
                // instead of `M-x`, which is exactly the "meta key doesn't
                // work" symptom. Re-deriving from `key_code` fixes it.
                if alt && !ctrl {
                    out.push(0x1b);
                    if let Some(ch) = keycode_to_base_char(ev.key_code, shift) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    } else {
                        // No base-char mapping for this physical key; fall
                        // back to whatever the OS composed.
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
            _ => {
                let _ = shift; // informational — most shifting already baked into Character.
            }
        }
    }

    if !out.is_empty() {
        data.worker.send(WorkerMsg::Input(out));
        // Real terminals snap the viewport back to the active region
        // the moment you type — otherwise hitting Enter while scrolled
        // up leaves you staring at history while your shell scrolls
        // past below. Match that behavior.
        data.worker.send(WorkerMsg::ScrollToBottom);
    }
}

/// Route Finder/Files-app drag-drops onto a terminal pane: insert the
/// dropped file's absolute path (POSIX single-quoted) into the pty,
/// followed by a trailing space. Mirrors what Terminal.app and iTerm2
/// do when you drag a file onto their window — Claude Code's prompt
/// then sees the path as plain text and can read the image from disk.
///
/// Bevy fires one `DroppedFile` event per file, so multi-file drops
/// land as space-separated tokens for free.
fn handle_file_drop(
    mut drops: MessageReader<bevy::window::FileDragAndDrop>,
    windows: Query<&Window>,
    // The window's raw AppKit handle, used to query the live pointer from
    // AppKit at drop time. Lives on the window entity as an ECS component
    // (unlike `WinitWindows`, which is a main-thread-local, not a resource).
    #[cfg(target_os = "macos")] window_handles: Query<&bevy::window::RawHandleWrapper>,
    // Forces this system onto the main thread so the AppKit calls below are
    // sound (NSView/NSWindow are main-thread-only).
    #[cfg(target_os = "macos")] _main_thread: bevy::ecs::system::NonSendMarker,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<TerminalStore>,
    panes: Query<(Entity, &PaneRect, &PaneKindMarker, Option<&Visibility>), With<PaneTag>>,
    mut focused: ResMut<FocusedPane>,
) {
    for ev in drops.read() {
        let bevy::window::FileDragAndDrop::DroppedFile { window, path_buf } = ev else {
            continue;
        };
        // winit emits no `CursorMoved` during a native macOS file drag, so
        // `Window::cursor_position()` is frozen wherever the pointer last
        // was BEFORE the drag — with several terminals open it routes the
        // path to the wrong pane. Ask AppKit for the live pointer position
        // at drop time instead; fall back to Bevy's stale value only if the
        // native lookup fails (e.g. non-macOS).
        #[cfg(target_os = "macos")]
        let mut pt = window_handles
            .get(*window)
            .ok()
            .and_then(pointer_pos_from_handle);
        #[cfg(not(target_os = "macos"))]
        let mut pt: Option<Vec2> = None;
        let native_ok = pt.is_some();
        if pt.is_none() {
            pt = windows.get(*window).ok().and_then(|w| w.cursor_position());
        }
        eprintln!("[file-drop] dbg: native_ok={native_ok} pt={pt:?}");
        let Some(pt) = pt else {
            // No live pointer and no cursor sample (window not focused yet,
            // or pointer left between drop start + finish). Without a
            // position we can't pick a pane — skip rather than guess.
            eprintln!(
                "[file-drop] no pointer position — ignoring drop of {}",
                path_buf.display()
            );
            continue;
        };
        let visible: Vec<(Entity, PaneRect)> = panes
            .iter()
            .filter(|(_, _, kind, vis)| {
                kind.0 == PANE_KIND && !matches!(vis, Some(Visibility::Hidden))
            })
            .map(|(e, r, _, _)| (e, *r))
            .collect();
        let canvas_pt = viewport.window_to_canvas(pt);
        eprintln!(
            "[file-drop] dbg: pt=({:.1},{:.1}) canvas=({:.1},{:.1}) vp(origin=({:.1},{:.1}) pan=({:.1},{:.1}) zoom={:.3}) panes={:?}",
            pt.x, pt.y, canvas_pt.x, canvas_pt.y,
            viewport.origin.x, viewport.origin.y, viewport.pan.x, viewport.pan.y, viewport.zoom,
            visible.iter().map(|(e, r)| (*e, r.pos.x, r.pos.y, r.size.x, r.size.y, r.z)).collect::<Vec<_>>()
        );
        let Some(target) = jim_pane::topmost_pane_at(canvas_pt, &visible)
        else {
            eprintln!(
                "[file-drop] no terminal under cursor — ignoring drop of {}",
                path_buf.display()
            );
            continue;
        };
        let Some(data) = store.map.get(&target) else {
            continue;
        };

        // Canonicalize to an absolute path so the receiving shell /
        // Claude Code resolves the file regardless of its cwd. Fall
        // back to the raw path if canonicalize fails (e.g., the source
        // is a symlink the user wants preserved as-typed).
        let abs = std::fs::canonicalize(path_buf).unwrap_or_else(|_| path_buf.clone());
        let quoted = posix_single_quote(&abs.to_string_lossy());
        let mut bytes = quoted.into_bytes();
        bytes.push(b' ');
        data.worker.send(WorkerMsg::Input(bytes));
        data.worker.send(WorkerMsg::ScrollToBottom);
        focused.0 = Some(target);
    }
}

/// The live pointer position in Bevy window-logical coordinates (top-left
/// origin, logical points) — the same space as `Window::cursor_position()`.
///
/// We mirror winit's own cursor math: take the pointer in the NSWindow's
/// base coordinate system (`mouseLocationOutsideOfEventStream`, which is
/// current regardless of the event stream, so it's valid mid-drag) and run
/// it through the content view's `convertPoint:fromView:nil`. winit's view
/// overrides `isFlipped -> true`, so the result already has a top-left
/// origin in logical points — no Y-flip or scale-factor correction needed.
#[cfg(target_os = "macos")]
fn pointer_pos_from_handle(raw: &bevy::window::RawHandleWrapper) -> Option<Vec2> {
    use objc2_app_kit::NSView;
    use raw_window_handle::RawWindowHandle;

    let RawWindowHandle::AppKit(h) = raw.get_window_handle() else {
        return None;
    };
    // SAFETY: AppKit hands back a live NSView pointer for this window, and
    // the calling system is pinned to the main thread (`NonSendMarker`),
    // where these AppKit UI calls are required to happen.
    let view: &NSView = unsafe { &*(h.ns_view.as_ptr() as *const NSView) };
    let ns_window = view.window()?;
    let win_pt = unsafe { ns_window.mouseLocationOutsideOfEventStream() };
    let view_pt = view.convertPoint_fromView(win_pt, None);
    Some(Vec2::new(view_pt.x as f32, view_pt.y as f32))
}

/// POSIX-safe shell quoting: wrap in single quotes; embed any literal
/// `'` as `'\''` (close-quote, escaped quote, reopen-quote). Always
/// safe regardless of the path's contents — spaces, $, *, !, newlines
/// are all preserved literally by the shell.
fn posix_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn keycode_to_ctrl_byte(code: KeyCode) -> u8 {
    // Ctrl+A = 0x01 ... Ctrl+Z = 0x1a.
    let base = match code {
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
        _ => 0,
    };
    base
}

/// Map a *physical* key to its base US-QWERTY character, applying `shift`.
///
/// Used for the Alt-as-Meta path: macOS composes Option+key into accented
/// glyphs (Option+x → `≈`), so we can't trust the OS-composed logical key
/// for meta sequences. Re-deriving from the physical `KeyCode` gives emacs
/// the `M-x`, `M-<`, `M-%`, `M-1`… bytes it expects. Returns `None` for keys
/// with no printable base char (the caller then falls back to the composed
/// bytes). US layout only for v0 — matches the direct-mapping philosophy of
/// `handle_keyboard`.
fn keycode_to_base_char(code: KeyCode, shift: bool) -> Option<char> {
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

fn named_key_bytes(code: &KeyCode, app_cursor: bool) -> Option<&'static [u8]> {
    Some(match code {
        KeyCode::Enter | KeyCode::NumpadEnter => b"\r",
        KeyCode::Tab => b"\t",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Escape => b"\x1b",
        KeyCode::Delete => b"\x1b[3~",
        KeyCode::Insert => b"\x1b[2~",
        KeyCode::PageUp => b"\x1b[5~",
        KeyCode::PageDown => b"\x1b[6~",
        KeyCode::ArrowUp => {
            if app_cursor {
                b"\x1bOA"
            } else {
                b"\x1b[A"
            }
        }
        KeyCode::ArrowDown => {
            if app_cursor {
                b"\x1bOB"
            } else {
                b"\x1b[B"
            }
        }
        KeyCode::ArrowRight => {
            if app_cursor {
                b"\x1bOC"
            } else {
                b"\x1b[C"
            }
        }
        KeyCode::ArrowLeft => {
            if app_cursor {
                b"\x1bOD"
            } else {
                b"\x1b[D"
            }
        }
        KeyCode::Home => {
            if app_cursor {
                b"\x1bOH"
            } else {
                b"\x1b[H"
            }
        }
        KeyCode::End => {
            if app_cursor {
                b"\x1bOF"
            } else {
                b"\x1b[F"
            }
        }
        _ => return None,
    })
}

// ---------- Mouse / chrome ----------

/// Convert a window-space cursor position to a cell coord (col, row)
/// inside the terminal at `rect`. The result is intentionally not
/// clamped — the caller owns clipping to the actual grid bounds.
pub fn pt_to_cell(pt: Vec2, rect: &PaneRect, cell_width: f32) -> (i32, i32) {
    let local = jim_pane::pt_to_content_local(pt, rect);
    let col = (local.x / cell_width).floor() as i32;
    let row = (local.y / LINE_HEIGHT).floor() as i32;
    (col, row)
}

/// Snapshot's `viewport_offset` for `entity`'s terminal, or 0 if the
/// store doesn't know about it (e.g. mid-spawn).
pub fn viewport_offset_of(store: &TerminalStore, entity: Entity) -> u64 {
    let Some(data) = store.map.get(&entity) else {
        return 0;
    };
    let g = data.worker.snapshot.lock().expect("snapshot lock");
    g.viewport_offset
}

/// Promote a `(col, viewport_row)` cell to a `(col, absolute_row)`
/// selection cell using a terminal's current `viewport_offset`. Done at
/// click/drag time so the selection stays pinned to its content while
/// the user scrolls (see [`TerminalSelection`]).
fn promote_to_absolute(cell: (i32, i32), viewport_offset: u64) -> (i32, i64) {
    (cell.0, viewport_offset as i64 + cell.1 as i64)
}

/// Start a selection drag inside a terminal pane in response to a
/// pane-bevy content press event.
fn handle_terminal_content_press(
    mut presses: MessageReader<PaneContentPressed>,
    metrics: Res<MonoMetrics>,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<TerminalStore>,
    rects: Query<&PaneRect>,
    kinds: Query<&PaneKindMarker>,
    mut selections: Query<&mut TerminalSelection>,
) {
    for ev in presses.read() {
        let Ok(kind) = kinds.get(ev.pane) else {
            continue;
        };
        if kind.0 != PANE_KIND {
            continue;
        }
        // If the child grabbed the mouse (any tracking mode), a plain
        // click is reported to it by `handle_terminal_mouse_report`, not
        // turned into a local text selection. Shift is the escape hatch:
        // Shift+drag always selects locally, matching xterm.
        if mouse_tracking_of(&store, ev.pane) && !ev.shift {
            continue;
        }
        // Clear any other terminal's selection.
        for mut sel in &mut selections {
            sel.clear();
        }
        let Ok(rect) = rects.get(ev.pane) else { continue };
        let viewport_cell =
            pt_to_cell(viewport.window_to_canvas(ev.window_pt), rect, metrics.cell_width);
        let cell = promote_to_absolute(viewport_cell, viewport_offset_of(&store, ev.pane));
        if let Ok(mut sel) = selections.get_mut(ev.pane) {
            sel.anchor = Some(cell);
            sel.head = Some(cell);
            sel.dragging = true;
        }
    }
}

/// Update the selection head while LMB is held; clear `dragging` on
/// release. Mirrors editor-bevy's `handle_text_select_drag` shape.
fn handle_terminal_selection_drag(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    metrics: Res<MonoMetrics>,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<TerminalStore>,
    mut selections: Query<(Entity, &PaneRect, &PaneKindMarker, &mut TerminalSelection)>,
) {
    if buttons.just_released(MouseButton::Left) {
        for (_, _, kind, mut sel) in &mut selections {
            if kind.0 == PANE_KIND {
                sel.dragging = false;
            }
        }
        return;
    }
    if !buttons.pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else { return };
    let Some(pt) = window.cursor_position() else { return };
    let pt_canvas = viewport.window_to_canvas(pt);

    for (entity, rect, kind, mut sel) in &mut selections {
        if kind.0 != PANE_KIND || !sel.dragging {
            continue;
        }
        let viewport_cell = pt_to_cell(pt_canvas, rect, metrics.cell_width);
        let cell = promote_to_absolute(viewport_cell, viewport_offset_of(&store, entity));
        sel.head = Some(cell);
    }
}

/// Whether `entity`'s terminal currently has any mouse tracking mode
/// active (read straight from the worker's published snapshot).
fn mouse_tracking_of(store: &TerminalStore, entity: Entity) -> bool {
    mouse_modes_of(store, entity).0
}

/// Public form of [`mouse_tracking_of`] for the app shell. When this is
/// true for the pane under the cursor, host-level right-click / drag
/// gestures (the per-pane context menu, canvas pan) should yield so the
/// click is reported to the child instead. Returns false for any entity
/// that isn't a live terminal.
pub fn pane_mouse_tracking(store: &TerminalStore, entity: Entity) -> bool {
    mouse_tracking_of(store, entity)
}

/// `(mouse_tracking, mouse_motion)` for `entity`'s terminal. `mouse_motion`
/// is true only when the child asked for drag/hover reports (DECSET
/// 1002/1003), so the main thread can avoid shipping motion in the common
/// press/release-only case.
fn mouse_modes_of(store: &TerminalStore, entity: Entity) -> (bool, bool) {
    let Some(data) = store.map.get(&entity) else {
        return (false, false);
    };
    let g = data.worker.snapshot.lock().expect("snapshot lock");
    (g.mouse_tracking, g.mouse_motion)
}

/// In-progress mouse-report gesture: which terminal grabbed the mouse on
/// press, the button that started it, and the last window cursor position
/// (so we only ship motion when the cursor actually moved).
#[derive(Default)]
struct MouseReportState {
    target: Option<Entity>,
    button: Option<MouseBtn>,
    last_pt: Option<Vec2>,
}

/// The physical buttons we translate into mouse reports, paired with the
/// encoder-side identity. Wheel notches keep their own path
/// (`WorkerMsg::Wheel`). Middle is deliberately absent: the app shell
/// reserves middle-drag for canvas panning, and TUIs rarely need the
/// middle button — so it stays a pan gesture rather than a report.
const REPORT_BUTTONS: [(MouseButton, MouseBtn); 2] = [
    (MouseButton::Left, MouseBtn::Left),
    (MouseButton::Right, MouseBtn::Right),
];

/// Forward mouse presses / drags / releases to a terminal whose child
/// enabled mouse tracking (vim, tmux, htop, lazygit, …). The worker owns
/// the actual escape-sequence encoding; this system just decides *which*
/// pane a gesture targets and *when* to send an event.
///
/// Coordination with text selection lives in `handle_terminal_content_press`:
/// while tracking is on, a plain (non-Shift) click is reported here and the
/// selection path bails, so the two never both fire on one click.
fn handle_terminal_mouse_report(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    metrics: Res<MonoMetrics>,
    viewport: Res<jim_pane::PaneViewport>,
    store: Res<TerminalStore>,
    panes: Query<(Entity, &PaneRect, &PaneKindMarker, &Visibility)>,
    mut state: Local<MouseReportState>,
) {
    let Ok(window) = windows.single() else { return };
    let Some(win_pt) = window.cursor_position() else {
        // Cursor left the window — any in-flight gesture resolves on the
        // next release we actually observe. Nothing to report right now.
        return;
    };
    let moved = state.last_pt != Some(win_pt);
    state.last_pt = Some(win_pt);

    let just_changed = REPORT_BUTTONS
        .iter()
        .any(|(b, _)| buttons.just_pressed(*b) || buttons.just_released(*b));
    // Idle fast-path: no movement, no button transition, no live gesture.
    if !moved && !just_changed && state.target.is_none() {
        return;
    }

    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    let sup = keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight);

    let canvas_pt = viewport.window_to_canvas(win_pt);
    let cell_w = metrics.cell_width.round().max(1.0) as u16;
    let cell_h = LINE_HEIGHT.round().max(1.0) as u16;
    let any_button = REPORT_BUTTONS.iter().any(|(b, _)| buttons.pressed(*b));

    // Hit-test against ALL panes (respecting z-order) so a widget sitting
    // on top of a terminal correctly swallows the click; only proceed when
    // the topmost pane under the cursor is itself a terminal.
    let mut all_panes: Vec<(Entity, PaneRect)> = Vec::new();
    let mut term_entities: Vec<Entity> = Vec::new();
    for (e, r, _k, vis) in &panes {
        if matches!(vis, Visibility::Hidden) {
            continue;
        }
        all_panes.push((e, *r));
        // Any VT-backed pane (terminal, emacs) can be a mouse-report
        // target; the per-pane `mouse_tracking` snapshot flag still
        // gates whether a report is actually sent.
        if store.map.contains_key(&e) {
            term_entities.push(e);
        }
    }
    let hovered = jim_pane::topmost_pane_at(canvas_pt, &all_panes)
        .filter(|e| term_entities.contains(e));

    let report = |pane: Entity, action: MouseAction, button: Option<MouseBtn>| {
        let Some((_, rect)) = all_panes.iter().find(|(e, _)| *e == pane) else {
            return;
        };
        let local = jim_pane::pt_to_content_local(canvas_pt, rect);
        if let Some(data) = store.map.get(&pane) {
            data.worker.send(WorkerMsg::Mouse {
                action,
                button,
                x: local.x,
                y: local.y,
                cell_w,
                cell_h,
                ctrl,
                alt,
                sup,
                any_button,
            });
        }
    };

    // Presses. Shift forces local selection, so we never start a report
    // gesture while it's held.
    if !shift {
        for (mb, mbtn) in REPORT_BUTTONS {
            if buttons.just_pressed(mb) {
                if let Some(pane) = hovered {
                    if mouse_tracking_of(&store, pane) {
                        report(pane, MouseAction::Press, Some(mbtn));
                        state.target = Some(pane);
                        state.button = Some(mbtn);
                    }
                }
            }
        }
    }

    // Motion. Only when the cursor actually moved and the child wants it.
    if moved {
        if let Some(pane) = state.target {
            // Drag: report with the gesture's originating button.
            if any_button && mouse_modes_of(&store, pane).1 {
                report(pane, MouseAction::Motion, state.button);
            }
        } else if let Some(pane) = hovered {
            // Bare hover (any-event tracking, DECSET 1003) — no button.
            if !shift && mouse_modes_of(&store, pane).1 {
                report(pane, MouseAction::Motion, None);
            }
        }
    }

    // Releases go to the pane that grabbed the gesture, so a drag that
    // ends outside the pane still delivers its release to the right child.
    for (mb, mbtn) in REPORT_BUTTONS {
        if buttons.just_released(mb) {
            if let Some(pane) = state.target {
                report(pane, MouseAction::Release, Some(mbtn));
            }
        }
    }
    if state.target.is_some() && !any_button {
        state.target = None;
        state.button = None;
    }
}

// ---------- Rendering ----------

fn sync_grid(
    metrics: Res<MonoMetrics>,
    theme: Res<jim_style::Theme>,
    themes: Res<jim_style::ProjectThemes>,
    mut atlas: ResMut<GlyphAtlas>,
    mut images: ResMut<Assets<Image>>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
    mut materials: ResMut<Assets<TermMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    store: Res<TerminalStore>,
    mut terminals: Query<
        (
            Entity,
            &TerminalCursor,
            &mut TermGrid,
            &Visibility,
            Option<&jim_pane::PaneProject>,
        ),
        With<PaneTag>,
    >,
    mut transform_q: Query<&mut Transform>,
    mut vis_q: Query<&mut Visibility, Without<TermGrid>>,
    mut prof: Local<SyncGridProfile>,
) {
    use std::time::Instant;
    let frame_start = Instant::now();

    // Theme-driven defaults, substituted in below for any cell whose fg
    // or bg matches libghostty's reported `default_fg/default_bg` (plain
    // text the shell didn't color). Resolved PER PANE from its project's
    // theme so each terminal reads in its own project's palette (a paper
    // project gives ink-on-cream, a terminal project phosphor-on-black),
    // including all faces in the cube overview. These globals are the
    // fallback for panes with no project theme cached.
    let global_default_fg = lin_to_rgb_bytes(theme.color(jim_style::tokens::FG));
    let global_default_bg = lin_to_rgb_bytes(theme.color(jim_style::tokens::BG));
    let theme_changed = theme.is_changed() || themes.is_changed();

    let mut local_cells: Vec<SnapCell> = Vec::new();
    let mut local_dirty_rows: Vec<bool> = Vec::new();

    let mut work_done = false;
    let mut lock_ns: u128 = 0;
    let mut mutate_ns: u128 = 0;
    let mut cells_touched = 0u64;

    // Scratch reused across terminals: avoids per-frame allocation in
    // the dirty-row hot path.
    let mut pending_writes: Vec<(usize, GpuCell)> = Vec::new();

    for (entity, cursor_marker, mut grid, vis, proj) in &mut terminals {
        // Per-pane theme defaults: this terminal's project theme if known,
        // else the global (active) theme.
        let (theme_default_fg, theme_default_bg) = proj
            .and_then(|p| themes.get(p.0))
            .map(|t| {
                (
                    lin_to_rgb_bytes(t.color(jim_style::tokens::FG)),
                    lin_to_rgb_bytes(t.color(jim_style::tokens::BG)),
                )
            })
            .unwrap_or((global_default_fg, global_default_bg));
        let _prof = jim_pane::prof::pane_span(entity.to_bits(), "terminal");
        // No kind gate: TermGrid + a TerminalStore entry mean this pane
        // renders through the shared VT-grid pipeline (terminal, emacs).
        let Some(data) = store.map.get(&entity) else {
            continue;
        };

        // Propagate this pane's visibility to the worker so it skips the
        // 60Hz Bevy wake while the user isn't looking. The worker keeps
        // processing pty bytes either way — the libghostty terminal
        // state stays correct — but inactive-project panes contribute
        // zero to per-frame schedule cost.
        let is_hidden = matches!(vis, Visibility::Hidden);
        // Release-store so the worker's Acquire-load of the same edge
        // (worker.rs, top of `worker_loop`) can't miss the transition —
        // this is what guarantees the reveal publish is never skipped.
        data.worker
            .visible
            .store(!is_hidden, std::sync::atomic::Ordering::Release);
        if is_hidden {
            grid.was_visible = false;
            continue;
        }
        // First frame after un-hide: the worker SKIPS snapshot publishes
        // while hidden, so its last-published grid is stale. Force a full
        // repaint here to paint whatever it last published, and nudge the
        // worker so it observes the visibility edge, publishes a fresh
        // full snapshot, and wakes us — the next frame repaints from that.
        // Worst case is one frame of the pane's last-visible content; the
        // nudge keeps the worker from sitting in its multi-second poll(2)
        // before catching up (which would leave the pane stale for
        // seconds — the one thing we must never do).
        let just_shown = !grid.was_visible;
        grid.was_visible = true;
        if just_shown {
            data.worker.wake();
        }

        // Lock briefly, read the cheap scalars, and only clone the (large)
        // cells + dirty_rows vectors when a repaint is actually due. An
        // idle-but-visible terminal wakes here every frame the app renders
        // for ANY reason; it used to pay two full-grid memcpys per frame
        // unconditionally, then throw them away at the `nothing_changed`
        // check below. We now decide up front — under the SAME single lock
        // acquisition, using the exact negation of `nothing_changed` — and
        // skip both copies entirely on idle frames.
        //
        // This is safe against dropping dirty-row information: the worker
        // (worker.rs) bumps `generation` on every publish, and the cells +
        // dirty_rows vectors are only ever mutated during a publish, under
        // this very mutex. So an unchanged `generation` guarantees both the
        // cells AND the dirty_rows are byte-identical to what we'd have
        // copied — there is nothing new to consume. The worker fully
        // REPLACES dirty_rows each publish (it does not accumulate flags for
        // the renderer to clear), so we never owe it a read.
        let lock_t = Instant::now();
        let (cols, rows, default_fg, default_bg, cursor, generation, pool_changed, work_pending) = {
            let g = data.worker.snapshot.lock().expect("snapshot lock");
            let cols = g.cols;
            let rows = g.rows;
            let generation = g.generation;
            let pool_changed = grid.cols != cols || grid.rows != rows;
            // Identical predicate to the historical `nothing_changed` guard,
            // hoisted ahead of the copy so the copy can be elided.
            let work_pending = pool_changed
                || just_shown
                || theme_changed
                || grid.last_rendered_generation != generation;
            if work_pending {
                local_cells.clear();
                local_cells.extend_from_slice(&g.cells);
                local_dirty_rows.clear();
                local_dirty_rows.extend_from_slice(&g.dirty_rows);
            }
            (
                cols,
                rows,
                g.default_fg,
                g.default_bg,
                g.cursor,
                generation,
                pool_changed,
                work_pending,
            )
        };
        lock_ns += lock_t.elapsed().as_nanos();

        // Cursor — compare-before-write to avoid spurious Changed.
        let cursor_entity = cursor_marker.0;
        if let Ok(mut v) = vis_q.get_mut(cursor_entity) {
            let want = if cursor.is_some() {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
            if *v != want {
                *v = want;
            }
        }
        if let Some((cx, cy)) = cursor {
            if let Ok(mut t) = transform_q.get_mut(cursor_entity) {
                let wx = cx as f32 * metrics.cell_width;
                let wy = -(cy as f32) * LINE_HEIGHT;
                let wz = 1.0;
                if t.translation.x != wx
                    || t.translation.y != wy
                    || t.translation.z != wz
                {
                    t.translation.x = wx;
                    t.translation.y = wy;
                    t.translation.z = wz;
                }
            }
        }

        // `work_pending` was decided under the lock above (its exact
        // negation is the historical `nothing_changed` predicate); if no
        // work is due we already skipped the cell copy, so bail now. Cursor
        // visibility/position was still reconciled above every frame, as
        // before. `pool_changed` was computed under the same lock and is
        // reused below for the resize + force-all paths.
        if !work_pending {
            continue;
        }
        work_done = true;
        let mutate_t = Instant::now();

        // Resize the GPU grid: replace the cells image + mesh + uniform
        // params. Cheap because there's only one of each per terminal.
        if pool_changed {
            let bg_packed = pack_rgb(
                theme_default_bg.0,
                theme_default_bg.1,
                theme_default_bg.2,
            );
            // Replace cells image in place — keep the same Handle so the
            // material doesn't need rebinding.
            if let Some(mut img) = images.get_mut(&grid.cells_image) {
                *img = make_cells_image(cols as u32, rows as u32, bg_packed);
            }
            // Replace mesh contents (same handle stays bound).
            let grid_w = cols as f32 * metrics.cell_width;
            let grid_h = rows as f32 * LINE_HEIGHT;
            if let Some(mut mesh) = meshes.get_mut(&grid.mesh) {
                *mesh = Mesh::from(Rectangle::new(grid_w, grid_h));
            }
            if let Some(mut mat) = materials.get_mut(&grid.material) {
                mat.params.cols = cols as u32;
                mat.params.rows = rows as u32;
            }
            if let Ok(mut t) = transform_q.get_mut(grid.render_entity) {
                t.translation.x = grid_w * 0.5;
                t.translation.y = -(grid_h * 0.5);
            }
            grid.cols = cols;
            grid.rows = rows;
        }

        // Pass 1: resolve glyph indices and collect (idx, GpuCell) for
        // every dirty cell. Atlas lookups borrow `images` mutably (atlas
        // may insert a new glyph and re-upload), so we can't hold a
        // `Image::get_mut(cells_image)` borrow at the same time. Two
        // passes keeps the borrow checker happy and lets us compare
        // existing-vs-new in pass 2.
        pending_writes.clear();
        let force_all = pool_changed || just_shown || theme_changed;
        for r in 0..rows as usize {
            let row_dirty = force_all
                || local_dirty_rows.get(r).copied().unwrap_or(true);
            if !row_dirty {
                continue;
            }
            let row_base = r * cols as usize;
            for c in 0..cols as usize {
                let idx = row_base + c;
                let cell = match local_cells.get(idx) {
                    Some(c) => *c,
                    None => continue,
                };
                let (final_fg, final_bg) = if cell.inverse {
                    (cell.bg, cell.fg)
                } else {
                    (cell.fg, cell.bg)
                };
                // Substitute theme defaults for cells the shell didn't
                // color explicitly. libghostty has already filled in
                // its own palette default at the worker; we recognize
                // it by exact-equal byte match and swap in the theme
                // color instead. False positives (shell explicitly set
                // a color that happens to equal libghostty's default)
                // are visually identical to the user's intent, since
                // the theme picks "the same color the shell would've
                // shown" anyway.
                let theme_fg =
                    if final_fg.r == default_fg.r
                        && final_fg.g == default_fg.g
                        && final_fg.b == default_fg.b
                    {
                        theme_default_fg
                    } else {
                        (final_fg.r, final_fg.g, final_fg.b)
                    };
                let theme_bg =
                    if final_bg.r == default_bg.r
                        && final_bg.g == default_bg.g
                        && final_bg.b == default_bg.b
                    {
                        theme_default_bg
                    } else {
                        (final_bg.r, final_bg.g, final_bg.b)
                    };
                let glyph_index =
                    atlas.lookup_or_insert(cell.ch, &mut images, &mut layouts);
                let gpu = GpuCell {
                    glyph_index,
                    fg_packed: pack_rgb(theme_fg.0, theme_fg.1, theme_fg.2),
                    bg_packed: pack_rgb(theme_bg.0, theme_bg.1, theme_bg.2),
                    flags: 0,
                };
                pending_writes.push((idx, gpu));
                cells_touched += 1;
            }
        }

        // Pass 2: filter no-op writes (cells whose state didn't change)
        // by reading from the current cells image, then mutate it in one
        // go. Reading via `Assets::get` doesn't mark the asset Changed —
        // important so we don't re-upload the texture every frame when
        // libghostty flagged a row dirty but no visible state moved.
        if pending_writes.is_empty() {
            grid.last_rendered_generation = generation;
            mutate_ns += mutate_t.elapsed().as_nanos();
            continue;
        }
        let mut needs_upload = false;
        {
            let current = images
                .get(&grid.cells_image)
                .expect("cells image must be alive");
            let current_cells: &[GpuCell] = bytemuck::cast_slice(
                current
                    .data
                    .as_ref()
                    .expect("cells image must have CPU data")
                    .as_slice(),
            );
            for (idx, new_cell) in &pending_writes {
                if current_cells
                    .get(*idx)
                    .map_or(true, |existing| existing != new_cell)
                {
                    needs_upload = true;
                    break;
                }
            }
        }
        if needs_upload {
            if let Some(mut img) = images.get_mut(&grid.cells_image) {
                let dst: &mut [GpuCell] = bytemuck::cast_slice_mut(
                    img.data
                        .as_mut()
                        .expect("cells image must have CPU data"),
                );
                for (idx, new_cell) in &pending_writes {
                    if let Some(slot) = dst.get_mut(*idx) {
                        if slot != new_cell {
                            *slot = *new_cell;
                        }
                    }
                }
            }
            // Mutating the cells image alone re-extracts the GpuImage with
            // a fresh wgpu texture, but the material's bind group still
            // caches the OLD texture view — so without poking the material
            // too, the shader keeps sampling the pre-modification upload.
            // (Bevy's own tilemap_chunk material follows this same pattern;
            // see `bevy_sprite_render::tilemap_chunk::update_chunk` —
            // `materials.get_mut(material.id())` is the load-bearing line.)
            let _ = materials.get_mut(&grid.material);
        }

        grid.last_rendered_generation = generation;
        mutate_ns += mutate_t.elapsed().as_nanos();
    }
    if work_done && std::env::var("TERMINAL_PROFILE").is_ok() {
        prof.frames += 1;
        prof.frame_ns += frame_start.elapsed().as_nanos();
        prof.lock_ns += lock_ns;
        prof.mutate_ns += mutate_ns;
        prof.cells += cells_touched;
        if prof.frames >= 30 {
            eprintln!(
                "[render] {} frames | avg frame {:>5.2} ms | lock {:>4.2} ms | mutate {:>4.2} ms | {:>5.0} cells/frame",
                prof.frames,
                (prof.frame_ns as f64 / 1_000_000.0) / prof.frames as f64,
                (prof.lock_ns as f64 / 1_000_000.0) / prof.frames as f64,
                (prof.mutate_ns as f64 / 1_000_000.0) / prof.frames as f64,
                prof.cells as f64 / prof.frames as f64,
            );
            *prof = SyncGridProfile::default();
        }
    }
}

#[derive(Default)]
struct SyncGridProfile {
    frames: u64,
    frame_ns: u128,
    lock_ns: u128,
    mutate_ns: u128,
    cells: u64,
}

// ---------- helpers ----------

#[allow(dead_code)]
fn rgb_to_color(c: RgbColor) -> Color {
    Color::srgb(c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0)
}

/// Linear-RGB theme color → (r, g, b) byte triple in sRGB space, the
/// format libghostty + `pack_rgb` use. Used by `sync_grid` to convert
/// theme tokens before stuffing them into per-cell colors.
fn lin_to_rgb_bytes(c: bevy::color::LinearRgba) -> (u8, u8, u8) {
    let srgb = Color::LinearRgba(c).to_srgba();
    (
        (srgb.red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (srgb.green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (srgb.blue.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

// ---------- shell-coupling seams ----------
//
// The spawn / restore path needs two pieces of project state that live in
// `jim_app` (`Projects`): a fresh terminal session id and a pane's
// initial cwd. To keep this crate free of a `jim_app` dependency, the
// shell installs closures into these resources at startup; the terminal
// code calls through them. When absent (e.g. headless tests) they fall
// back to inert defaults.

/// Allocator for fresh terminal session ids, installed by the shell so
/// the registry restore path can mint ids without depending on `Projects`.
#[derive(Resource)]
pub struct TerminalIdAllocator(pub Box<dyn Fn(&mut World) -> u64 + Send + Sync>);

/// Resolver for a pane's initial cwd (its project's remembered
/// `default_cwd`), installed by the shell.
#[derive(Resource)]
pub struct TerminalInitialCwd(pub Box<dyn Fn(&World, Entity) -> Option<String> + Send + Sync>);

/// Hook the shell installs so terminal close can flag its project's
/// terminal layout dirty (for persistence).
#[derive(Resource)]
pub struct TerminalDirtyHook(pub Box<dyn Fn(&mut World) + Send + Sync>);

fn terminal_id_allocator(world: &mut World) -> u64 {
    if let Some(alloc) = world.remove_resource::<TerminalIdAllocator>() {
        let id = (alloc.0)(world);
        world.insert_resource(alloc);
        id
    } else {
        // No allocator installed (headless / test) — derive a unique-ish
        // id from the clock so two terminals don't collide on scrollback.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

fn terminal_initial_cwd(world: &World, entity: Entity) -> Option<String> {
    let resolver = world.get_resource::<TerminalInitialCwd>()?;
    (resolver.0)(world, entity)
}

fn terminal_mark_dirty(world: &mut World) {
    if let Some(hook) = world.remove_resource::<TerminalDirtyHook>() {
        (hook.0)(world);
        world.insert_resource(hook);
    }
}
