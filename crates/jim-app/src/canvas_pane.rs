//! Nested canvases — "gather" panes into a tile, descend into it, repeat.
//!
//! A nested canvas is a lightweight grouping layered on top of projects:
//! every pane belongs to a `(project, canvas)` pair (see
//! [`jim_pane::PaneCanvas`]; `canvas == 0` is the project root). A pane of
//! THIS kind (`"canvas"`) is a *tile* that owns a child canvas id; panes
//! whose `PaneCanvas` equals that id live "inside" the tile.
//!
//! The trick that keeps this cheap: gathered panes are never rendered
//! *inside* the tile. They're full top-level panes that
//! [`crate::projects::sync_visibility`] simply hides unless the user has
//! descended into their canvas. So there is no nested camera / nested
//! coordinate transform — descending just swaps which level is visible
//! and which level's pan/zoom is active.
//!
//! Interactions:
//! - **Gather**: drag a pane by its title bar and drop it onto a tile
//!   ([`jim_pane::PaneWindowDragReleased`]) → its `PaneCanvas` is rewritten
//!   to the tile's canvas id, so it vanishes from this level.
//! - **Descend**: double-click a tile ([`jim_pane::PaneDoubleClicked`]) →
//!   push the tile's canvas id onto [`CanvasNav`].
//! - **Ascend**: Cmd+Up pops a level. A breadcrumb shows the path.
//!
//! The tile renders a **mini-map of per-pane snapshots**: each pane is
//! screenshotted before it's hidden (at gather time, and refreshed on
//! leave), then drawn at its real relative position at a fixed scale, so
//! resizing the tile reveals more of the canvas rather than stretching.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy::sprite::Anchor;
use bevy::text::LineHeight;
use serde_json::{json, Value};

use jim_pane::{
    content_area, FocusedPane, InputConsumed, KeyboardOwner, PaneCanvas, PaneChrome,
    PaneDoubleClicked, PaneFont, PaneFontMetrics, PaneInputBlockZones, PaneKindSpec, PaneProject,
    PaneRect, PaneRegistry, PaneSnapId, PaneTag, PaneTitle, PaneViewport, PaneWindowDragReleased,
};

use crate::projects::{Projects, Sidebar};
use crate::MENU_OVERLAY_LAYER;

pub const PANE_KIND: &str = "canvas";

// ---------- Render-loop kick ----------

/// The app renders **reactively** (idle → only every 5s). While a
/// nested-canvas animation or thumbnail capture is actually in flight we
/// force the next frame, frame-by-frame, so the work runs at full rate
/// without pinning the app continuous on a timer. The drag itself is kept
/// awake by `handle_pane_mouse` (jim-pane) requesting redraws.
fn keep_canvas_awake(
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
    mut flow: ResMut<ThumbFlow>,
    gathering: Query<(), With<GatherAnim>>,
    restoring: Query<(), With<RestoreRect>>,
    fading: Query<(), With<CanvasFade>>,
) {
    // Age out captures that never landed (screenshot failure) so we don't
    // render forever. ~5s at 60fps is a generous ceiling for a readback.
    flow.capturing.retain_mut(|(_, age)| {
        *age += 1;
        *age < 300
    });
    let active = flow.pending.is_some()
        || !flow.capturing.is_empty()
        || !gathering.is_empty()
        || !restoring.is_empty()
        || !fading.is_empty();
    if active {
        redraw.write(bevy::window::RequestRedraw);
    }
}

// ---------- Navigation state ----------

/// The descent stack, per project: the nested-canvas ids the user has
/// descended into. Empty = at the project root. The last id is the
/// currently-visible level (what [`sync_visibility`] and the canvas
/// pan/zoom read).
#[derive(Resource, Default)]
pub struct CanvasNav {
    pub by_project: HashMap<u64, Vec<u64>>,
}

impl CanvasNav {
    /// The currently-visible nested-canvas id for a project (`0` = root).
    pub fn level(&self, project: u64) -> u64 {
        self.by_project
            .get(&project)
            .and_then(|p| p.last().copied())
            .unwrap_or(0)
    }
    /// The full descent path (canvas ids, root implied at the front).
    pub fn path(&self, project: u64) -> &[u64] {
        self.by_project
            .get(&project)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
    pub fn descend(&mut self, project: u64, canvas: u64) {
        self.by_project.entry(project).or_default().push(canvas);
    }
    pub fn ascend(&mut self, project: u64) -> Option<u64> {
        self.by_project.get_mut(&project).and_then(|p| p.pop())
    }
    /// Keep only the first `depth` canvases of a project's path. Used by
    /// breadcrumb clicks to jump up multiple levels at once.
    pub fn truncate_to(&mut self, project: u64, depth: usize) {
        if let Some(p) = self.by_project.get_mut(&project) {
            p.truncate(depth);
        }
    }
}

// ---------- Components ----------

/// A nested-canvas tile. Owns the `canvas_id` its gathered children point
/// at via [`PaneCanvas`]. `last_sig` caches the mini-map's input signature
/// so it only rebuilds when its contents (or their snapshots) change.
#[derive(Component, Clone, Debug, Default)]
pub struct CanvasPane {
    pub canvas_id: u64,
    last_sig: u64,
}

/// Marker on the mini-map child entities so a rebuild can despawn just
/// them (mirrors `IssueRowEntity` in the issues pane).
#[derive(Component)]
struct CanvasTileChild;

/// Loaded per-pane snapshot images, keyed by [`PaneSnapId`]. The tile
/// mini-map draws each member pane using its snapshot here. `generation`
/// bumps whenever an image (re)loads so tiles know to rebuild.
#[derive(Resource, Default)]
struct PaneThumbCache {
    by_id: HashMap<u64, (Handle<Image>, u64)>,
    generation: u64,
}

/// Fixed canvas-units → tile-pixels ratio for the mini-map. The scale is
/// constant so **resizing a tile reveals more of the canvas** rather than
/// rescaling its contents.
const MINI_SCALE: f32 = 0.15;

// ---------- Plugin ----------

pub struct CanvasPanePlugin;

impl Plugin for CanvasPanePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CanvasNav>()
            .init_resource::<BreadcrumbState>()
            .init_resource::<ThumbFlow>()
            .init_resource::<CanvasEjectQueue>()
            .init_resource::<PaneThumbCache>()
            .add_systems(Startup, register_kind)
            .add_systems(
                Update,
                (
                    handle_breadcrumb_click,
                    handle_descend,
                    handle_ascend,
                    capture_and_ascend,
                    handle_reparent,
                    animate_gather,
                    restore_gathered_rect,
                    animate_canvas_fade,
                    apply_canvas_eject,
                    load_pane_thumbs,
                    rebuild_canvas_tiles,
                    keep_canvas_awake,
                )
                    .chain(),
            )
            .add_systems(Update, render_breadcrumb);
    }
}

fn register_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Canvas",
        radial_icon: Some("⊞"),
        default_size: Vec2::new(260.0, 180.0),
        spawn: canvas_spawn,
        snapshot: canvas_snapshot,
        on_close: None,
    });
}

// ---------- spawn / snapshot ----------

fn canvas_spawn(world: &mut World, entity: Entity, _content_root: Entity, config: &Value) {
    // Restore re-uses the saved id; a fresh tile allocates a new one.
    let canvas_id = config
        .get("canvas_id")
        .and_then(|v| v.as_u64())
        .filter(|id| *id != 0)
        .unwrap_or_else(|| world.resource_mut::<Projects>().allocate_canvas_id());
    let name = config
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Canvas")
        .to_string();
    if let Some(mut t) = world.get_mut::<PaneTitle>(entity) {
        t.0 = name;
    }
    world.entity_mut(entity).insert((
        CanvasPane {
            canvas_id,
            ..default()
        },
        // Double-click to descend must keep working even when the tile is
        // pinned to the background.
        jim_pane::PaneDoubleClickable,
    ));
}

fn canvas_snapshot(world: &World, entity: Entity) -> Value {
    let canvas_id = world.get::<CanvasPane>(entity).map(|c| c.canvas_id).unwrap_or(0);
    let name = world
        .get::<PaneTitle>(entity)
        .map(|t| t.0.clone())
        .unwrap_or_default();
    json!({ "canvas_id": canvas_id, "name": name })
}

// ---------- Descend / ascend ----------

/// Double-clicking a tile descends into its canvas.
fn handle_descend(
    mut commands: Commands,
    mut events: MessageReader<PaneDoubleClicked>,
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    tiles: Query<(&CanvasPane, &PaneProject)>,
    mut nav: ResMut<CanvasNav>,
    mut focused: ResMut<FocusedPane>,
) {
    for ev in events.read() {
        if let Ok((tile, proj)) = tiles.get(ev.pane) {
            nav.descend(proj.0, tile.canvas_id);
            // The just-descended tile is now hidden; don't leave keyboard
            // focus pointing at it.
            focused.0 = None;
            if let Ok(w) = windows.single() {
                spawn_canvas_fade(&mut commands, w.width(), w.height(), sidebar.width);
            }
        }
    }
}

/// Cmd+Up ascends one level in the active project.
fn handle_ascend(
    keys: Res<ButtonInput<KeyCode>>,
    owner: Res<KeyboardOwner>,
    projects: Res<Projects>,
    nav: Res<CanvasNav>,
    mut flow: ResMut<ThumbFlow>,
) {
    if owner.is_modal() {
        return;
    }
    let cmd = keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight);
    if cmd && keys.just_pressed(KeyCode::ArrowUp) {
        if let Some(p) = projects.active {
            let depth = nav.path(p).len();
            if depth > 0 {
                flow.request(p, depth - 1);
            }
        }
    }
}

// ---------- Ascend flow: snapshot the level being left, then ascend ----------

/// A queued ascend. We capture a thumbnail of the currently-visible
/// canvas *before* hiding it, so the descent is deferred one frame past
/// the screenshot.
struct PendingAscend {
    project: u64,
    target_depth: usize,
    /// Frames to wait after the capture before applying the ascend. The
    /// screenshot is spawned on the frame the request is created (while
    /// the child level is still rendered); the nav change lands the next
    /// frame so it doesn't blank the capture.
    wait: u8,
}

#[derive(Resource, Default)]
struct ThumbFlow {
    pending: Option<PendingAscend>,
    /// Canvas captures whose screenshot has been requested but whose PNG
    /// hasn't been loaded onto its tile yet, with a frame-age. The
    /// screenshot readback is async and can finish several frames later —
    /// the app must keep rendering until it lands, or (in reactive idle)
    /// the readback never even completes. Cleared by `load_tile_thumbs`
    /// when the thumbnail loads; aged out as a failure backstop.
    capturing: Vec<(u64, u32)>,
}

impl ThumbFlow {
    /// Request an ascend to `target_depth` (last-wins if one is already
    /// queued).
    fn request(&mut self, project: u64, target_depth: usize) {
        self.pending = Some(PendingAscend {
            project,
            target_depth,
            wait: 1,
        });
    }
}

/// Drive the deferred ascend: snapshot each member pane on the request
/// frame (still on screen), then apply the nav change a frame later.
#[allow(clippy::too_many_arguments)]
fn capture_and_ascend(
    mut commands: Commands,
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    viewport: Res<PaneViewport>,
    mut projects: ResMut<Projects>,
    mut flow: ResMut<ThumbFlow>,
    mut nav: ResMut<CanvasNav>,
    mut focused: ResMut<FocusedPane>,
    members: Query<(Entity, &PaneRect, &PaneProject, &PaneCanvas, Option<&PaneSnapId>)>,
) {
    let Some((project, wait, depth)) = flow
        .pending
        .as_ref()
        .map(|r| (r.project, r.wait, r.target_depth))
    else {
        return;
    };
    if wait > 0 {
        // Snapshot every pane on the level we're leaving (still rendered).
        let leaving = nav.level(project);
        if leaving != 0 {
            if let Ok(window) = windows.single() {
                let mut crops: Vec<(PathBuf, u32, u32, u32, u32)> = Vec::new();
                for (e, rect, proj, canvas, snap) in members.iter() {
                    if proj.0 != project || canvas.0 != leaving {
                        continue;
                    }
                    let snap_id = ensure_snap_id(&mut commands, &mut projects, e, snap);
                    if let Some((x, y, w, h)) =
                        pane_window_crop(rect, &viewport, sidebar.width, window)
                    {
                        crops.push((pane_thumb_path(snap_id), x, y, w, h));
                        flow.capturing.retain(|(id, _)| *id != snap_id);
                        flow.capturing.push((snap_id, 0));
                    }
                }
                if !crops.is_empty() {
                    commands
                        .spawn(Screenshot::primary_window())
                        .observe(save_pane_thumbs(crops));
                }
            }
        }
        if let Some(req) = flow.pending.as_mut() {
            req.wait -= 1;
        }
        return;
    }
    // Snapshot frame is done — apply the ascend.
    nav.truncate_to(project, depth);
    focused.0 = None;
    flow.pending = None;
    if let Ok(w) = windows.single() {
        spawn_canvas_fade(&mut commands, w.width(), w.height(), sidebar.width);
    }
}

/// Return `pane`'s [`PaneSnapId`], allocating + attaching one if it has
/// none yet.
fn ensure_snap_id(
    commands: &mut Commands,
    projects: &mut Projects,
    pane: Entity,
    existing: Option<&PaneSnapId>,
) -> u64 {
    if let Some(s) = existing {
        if s.0 != 0 {
            return s.0;
        }
    }
    let id = projects.allocate_snap_id();
    commands.entity(pane).insert(PaneSnapId(id));
    id
}

/// Convert a pane's canvas-space rect to a physical-pixel crop rect inside
/// the on-screen canvas region. `None` if it's fully off-screen.
fn pane_window_crop(
    rect: &PaneRect,
    viewport: &PaneViewport,
    sidebar_w: f32,
    win: &Window,
) -> Option<(u32, u32, u32, u32)> {
    let scale = win.scale_factor();
    let tl = viewport.canvas_to_window(rect.pos);
    let size = rect.size * viewport.zoom;
    let x0 = tl.x.max(sidebar_w);
    let y0 = tl.y.max(0.0);
    let x1 = (tl.x + size.x).min(win.width());
    let y1 = (tl.y + size.y).min(win.height());
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        (x0 * scale) as u32,
        (y0 * scale) as u32,
        ((x1 - x0) * scale).max(1.0) as u32,
        ((y1 - y0) * scale).max(1.0) as u32,
    ))
}

/// Screenshot observer: crop the captured window to each pane's rect and
/// write its `pane-<snap_id>.png`.
fn save_pane_thumbs(crops: Vec<(PathBuf, u32, u32, u32, u32)>) -> impl FnMut(On<ScreenshotCaptured>) {
    const THUMB_MAX: u32 = 512;
    move |captured: On<ScreenshotCaptured>| {
        let dyn_img = match captured.image.clone().try_into_dynamic() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[canvas] pane thumb: could not read screenshot: {e:?}");
                return;
            }
        };
        let (iw, ih) = (dyn_img.width(), dyn_img.height());
        for (path, x, y, w, h) in &crops {
            let cx = (*x).min(iw.saturating_sub(1));
            let cy = (*y).min(ih.saturating_sub(1));
            let cw = (*w).min(iw - cx).max(1);
            let ch = (*h).min(ih - cy).max(1);
            let cropped = dyn_img.crop_imm(cx, cy, cw, ch);
            let thumb = cropped.resize(THUMB_MAX, THUMB_MAX, image::imageops::FilterType::Triangle);
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = thumb.to_rgb8().save_with_format(path, image::ImageFormat::Png) {
                eprintln!("[canvas] pane thumb save {} failed: {e}", path.display());
            }
        }
    }
}

fn thumb_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".jim").join("canvas-thumbs")
}

fn pane_thumb_path(snap_id: u64) -> PathBuf {
    thumb_dir().join(format!("pane-{snap_id}.png"))
}

/// Load freshly-captured per-pane snapshots into [`PaneThumbCache`]
/// (mtime-gated); bump the cache generation so tiles rebuild.
fn load_pane_thumbs(
    members: Query<&PaneSnapId>,
    mut cache: ResMut<PaneThumbCache>,
    mut images: ResMut<Assets<Image>>,
    mut flow: ResMut<ThumbFlow>,
) {
    let ids: HashSet<u64> = members.iter().map(|s| s.0).filter(|i| *i != 0).collect();
    for id in ids {
        let path = pane_thumb_path(id);
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if mtime == 0 {
            continue;
        }
        if let Some((_, m)) = cache.by_id.get(&id) {
            if *m == mtime {
                continue;
            }
        }
        let Some(img) = load_png(&path) else {
            continue;
        };
        let handle = images.add(img);
        cache.by_id.insert(id, (handle, mtime));
        cache.generation += 1;
        flow.capturing.retain(|(cid, _)| *cid != id);
    }
}

fn load_png(path: &std::path::Path) -> Option<Image> {
    use bevy::asset::RenderAssetUsages;
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
    let bytes = std::fs::read(path).ok()?;
    let rgba = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(Image::new(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba.into_raw(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    ))
}

// ---------- Eject (move a pane back out of a canvas) ----------

/// Panes the user asked to move out of their current nested canvas (one
/// level up). Filled by the right-click context menu; drained by
/// [`apply_canvas_eject`].
#[derive(Resource, Default)]
pub struct CanvasEjectQueue(pub Vec<Entity>);

fn apply_canvas_eject(
    mut commands: Commands,
    mut queue: ResMut<CanvasEjectQueue>,
    mut projects: ResMut<Projects>,
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
    panes: Query<&PaneCanvas>,
    tiles: Query<(&CanvasPane, Option<&PaneCanvas>)>,
) {
    if queue.0.is_empty() {
        return;
    }
    // The ejected pane's visibility change lands next frame — force it so
    // it doesn't wait for the reactive idle wake.
    redraw.write(bevy::window::RequestRedraw);
    // canvas id -> the level the owning tile itself sits on (its parent).
    let owner_level: HashMap<u64, u64> = tiles
        .iter()
        .map(|(t, lvl)| (t.canvas_id, lvl.map_or(0, |c| c.0)))
        .collect();
    for e in queue.0.drain(..) {
        let current = panes.get(e).map_or(0, |c| c.0);
        if current == 0 {
            continue; // already at the project root
        }
        let parent = owner_level.get(&current).copied().unwrap_or(0);
        if parent == 0 {
            commands.entity(e).remove::<PaneCanvas>();
        } else {
            commands.entity(e).insert(PaneCanvas(parent));
        }
        projects.terminals_dirty = true;
    }
}

// ---------- Gather (drag-onto-tile) ----------

/// When a title-bar drag ends over a nested-canvas tile, reparent the
/// dragged pane into that tile's canvas (down-only for now).
#[allow(clippy::too_many_arguments)]
fn handle_reparent(
    mut commands: Commands,
    mut events: MessageReader<PaneWindowDragReleased>,
    mut projects: ResMut<Projects>,
    mut flow: ResMut<ThumbFlow>,
    windows: Query<&Window>,
    sidebar: Res<Sidebar>,
    viewport: Res<PaneViewport>,
    dragged_q: Query<
        (&PaneRect, &PaneProject, Option<&PaneCanvas>, Option<&CanvasPane>, Option<&PaneSnapId>),
        With<PaneTag>,
    >,
    tiles: Query<(Entity, &PaneRect, &PaneProject, Option<&PaneCanvas>, &CanvasPane)>,
) {
    for ev in events.read() {
        let Ok((drect, dproj, dcanvas, dtile, dsnap)) = dragged_q.get(ev.pane) else {
            continue;
        };
        let drect = *drect;
        let dragged_level = dcanvas.map_or(0, |c| c.0);
        // If the dragged pane is itself a tile, its own canvas id — used
        // to reject dropping it into one of its own descendants (a cycle).
        let dragged_owns = dtile.map(|t| t.canvas_id).unwrap_or(0);

        // Map every tile's canvas id -> the level that tile itself sits
        // on, so we can walk a target canvas back toward the root and
        // detect a cycle.
        let owner_level: HashMap<u64, u64> = tiles
            .iter()
            .map(|(_, _, _, lvl, t)| (t.canvas_id, lvl.map_or(0, |c| c.0)))
            .collect();

        // Topmost tile under the drop point, in the same project + level
        // as the dragged pane (both are visible, so they share a level).
        let mut best: Option<(u64, f32, Vec2)> = None;
        for (e, rect, proj, lvl, tile) in tiles.iter() {
            if e == ev.pane || proj.0 != dproj.0 {
                continue;
            }
            if lvl.map_or(0, |c| c.0) != dragged_level {
                continue;
            }
            let p = ev.canvas_pt;
            let inside = p.x >= rect.pos.x
                && p.x <= rect.pos.x + rect.size.x
                && p.y >= rect.pos.y
                && p.y <= rect.pos.y + rect.size.y;
            if inside && best.map_or(true, |(_, z, _)| rect.z > z) {
                best = Some((tile.canvas_id, rect.z, rect.pos + rect.size * 0.5));
            }
        }

        let Some((target_canvas, _, tile_center)) = best else {
            continue;
        };
        // No-op if it's already there.
        if target_canvas == dragged_level {
            continue;
        }
        if creates_cycle(target_canvas, dragged_owns, &owner_level) {
            eprintln!(
                "[canvas] refusing to gather pane into canvas {} — would nest a tile inside its own descendant",
                target_canvas
            );
            continue;
        }
        // Snapshot the pane NOW, while it's still on screen at full size,
        // so the tile has its image the moment it's gathered (the pane is
        // hidden once the fly-in completes and can't be captured after).
        if let Ok(window) = windows.single() {
            let snap_id = ensure_snap_id(&mut commands, &mut projects, ev.pane, dsnap);
            if let Some((x, y, w, h)) = pane_window_crop(&drect, &viewport, sidebar.width, window) {
                commands
                    .spawn(Screenshot::primary_window())
                    .observe(save_pane_thumbs(vec![(pane_thumb_path(snap_id), x, y, w, h)]));
                flow.capturing.retain(|(id, _)| *id != snap_id);
                flow.capturing.push((snap_id, 0));
            }
        }
        // Animate the pane flying into the tile; the actual reparent
        // happens when the animation finishes (`animate_gather`).
        commands.entity(ev.pane).insert(GatherAnim {
            target_canvas,
            start: drect,
            dest_center: tile_center,
            elapsed: 0.0,
        });
        projects.terminals_dirty = true;
    }
}

// ---------- Animations ----------

const GATHER_DUR: f32 = 0.18;

/// A pane mid-flight into a canvas tile. While present its `PaneRect` is
/// lerped toward the tile; on completion the pane is reparented into the
/// tile's canvas and its original geometry restored (so it's the right
/// size when the user descends).
#[derive(Component)]
struct GatherAnim {
    target_canvas: u64,
    start: PaneRect,
    dest_center: Vec2,
    elapsed: f32,
}

/// A just-gathered pane's original geometry, restored once the pane is
/// actually hidden. We can't restore it on the completion frame — the
/// hide (`PaneCanvas` insert) is deferred, so the pane would render at
/// full size for one frame (a visible "pop"). Restoring while hidden
/// avoids that.
#[derive(Component)]
struct RestoreRect(PaneRect);

fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

fn animate_gather(
    mut commands: Commands,
    time: Res<Time>,
    mut q: Query<(Entity, &mut PaneRect, &mut GatherAnim)>,
) {
    for (e, mut rect, mut anim) in &mut q {
        anim.elapsed += time.delta_secs();
        let t = (anim.elapsed / GATHER_DUR).clamp(0.0, 1.0);
        let k = ease_out_cubic(t);
        let dest_pos = anim.dest_center - Vec2::splat(10.0);
        rect.pos = anim.start.pos.lerp(dest_pos, k);
        rect.size = anim.start.size.lerp(Vec2::splat(20.0), k);
        if t >= 1.0 {
            // Gather (hides it on the current level). Leave the rect at the
            // shrunk dest for now; `restore_gathered_rect` puts the real
            // geometry back once the pane is hidden, so it never flashes
            // back to full size on screen.
            commands
                .entity(e)
                .remove::<GatherAnim>()
                .insert((PaneCanvas(anim.target_canvas), RestoreRect(anim.start)));
        }
    }
}

/// Once a just-gathered pane is actually hidden, restore its original
/// geometry (off-screen, so no visible pop) so it's the right size/place
/// when the user descends into the canvas.
fn restore_gathered_rect(
    mut commands: Commands,
    mut q: Query<(Entity, &mut PaneRect, &RestoreRect, &Visibility)>,
) {
    for (e, mut rect, restore, vis) in &mut q {
        if *vis == Visibility::Hidden {
            *rect = restore.0;
            commands.entity(e).remove::<RestoreRect>();
        }
    }
}

const FADE_DUR: f32 = 0.2;
const FADE_START_ALPHA: f32 = 0.6;

/// A quick wash over the canvas region that fades out — the "open / close
/// a canvas" transition. Reveals the newly-shown level as it clears.
#[derive(Component)]
struct CanvasFade {
    elapsed: f32,
}

/// Spawn the transition wash over the canvas region (right of the sidebar).
fn spawn_canvas_fade(commands: &mut Commands, win_w: f32, win_h: f32, sidebar_w: f32) {
    let region_w = (win_w - sidebar_w).max(1.0);
    let cx = -win_w * 0.5 + sidebar_w + region_w * 0.5;
    commands.spawn((
        CanvasFade { elapsed: 0.0 },
        Sprite {
            color: Color::srgba(0.03, 0.03, 0.05, FADE_START_ALPHA),
            custom_size: Some(Vec2::new(region_w, win_h)),
            ..default()
        },
        Transform::from_xyz(cx, 0.0, 0.0),
        RenderLayers::layer(MENU_OVERLAY_LAYER),
    ));
}

fn animate_canvas_fade(
    mut commands: Commands,
    time: Res<Time>,
    mut q: Query<(Entity, &mut Sprite, &mut CanvasFade)>,
) {
    for (e, mut sprite, mut f) in &mut q {
        f.elapsed += time.delta_secs();
        let t = (f.elapsed / FADE_DUR).clamp(0.0, 1.0);
        sprite.color.set_alpha(FADE_START_ALPHA * (1.0 - t));
        if t >= 1.0 {
            commands.entity(e).despawn();
        }
    }
}

/// True if gathering a tile that owns `dragged_owns` into `target_canvas`
/// would create a cycle (the target canvas is a descendant of the dragged
/// tile). `0` for `dragged_owns` means the dragged pane isn't a tile, so
/// no cycle is possible.
fn creates_cycle(target_canvas: u64, dragged_owns: u64, owner_level: &HashMap<u64, u64>) -> bool {
    if dragged_owns == 0 {
        return false;
    }
    let mut lvl = target_canvas;
    let mut guard = 0;
    while lvl != 0 {
        if lvl == dragged_owns {
            return true;
        }
        lvl = owner_level.get(&lvl).copied().unwrap_or(0);
        guard += 1;
        if guard > 4096 {
            // Already-broken chain; treat as a cycle rather than spin.
            return true;
        }
    }
    false
}

// ---------- Tile mini-map ----------

/// One member pane's place in the mini-map.
struct Member {
    rect: Rect,
    snap_id: u64,
}

/// Rebuild a tile's mini-map whenever its members (or their snapshots)
/// change. Each member is drawn as its own captured snapshot at a FIXED
/// scale and its real relative position — so resizing the tile reveals
/// more of the canvas instead of stretching the image.
#[allow(clippy::too_many_arguments)]
fn rebuild_canvas_tiles(
    mut commands: Commands,
    mut tiles: Query<(&mut CanvasPane, &PaneRect, &PaneChrome)>,
    existing: Query<(Entity, &ChildOf), With<CanvasTileChild>>,
    members_q: Query<(&PaneRect, &PaneCanvas, Option<&PaneSnapId>)>,
    cache: Res<PaneThumbCache>,
    font: Res<PaneFont>,
    theme: Res<jim_style::Theme>,
) {
    use jim_style::tokens as t;
    let c = |id| Color::LinearRgba(theme.color(id));
    let fg = c(t::FG);
    let fg_muted = c(t::FG_MUTED);
    let accent = c(t::ACCENT);

    for (mut tile, rect, chrome) in &mut tiles {
        let mut members: Vec<Member> = members_q
            .iter()
            .filter(|(_, canvas, _)| canvas.0 == tile.canvas_id)
            .map(|(r, _, snap)| Member {
                rect: Rect::from_corners(r.pos, r.pos + r.size),
                snap_id: snap.map_or(0, |s| s.0),
            })
            .collect();

        let (_origin, content_size) = content_area(rect);
        let sig = tile_signature(tile.canvas_id, content_size, &members, &cache);
        if sig == tile.last_sig && !theme.is_changed() {
            continue;
        }
        tile.last_sig = sig;

        for (child, child_of) in &existing {
            if child_of.0 == chrome.content_root {
                commands.entity(child).despawn();
            }
        }
        if content_size.x <= 0.0 || content_size.y <= 0.0 {
            continue;
        }

        // Header label.
        commands.spawn((
            CanvasTileChild,
            ChildOf(chrome.content_root),
            Text2d::new(format!(
                "⊞  {} pane{}",
                members.len(),
                if members.len() == 1 { "" } else { "s" }
            )),
            TextFont {
                font: (font.0.clone()).into(),
                font_size: FontSize::Px(13.0),
                ..default()
            },
            LineHeight::Px(18.0),
            TextColor(fg_muted),
            Anchor::TOP_LEFT,
            Transform::from_xyz(2.0, -2.0, 0.3),
        ));

        let map_top = 24.0_f32;

        if members.is_empty() {
            commands.spawn((
                CanvasTileChild,
                ChildOf(chrome.content_root),
                Text2d::new("(empty — drag panes here)"),
                TextFont {
                    font: (font.0.clone()).into(),
                    font_size: FontSize::Px(11.0),
                    ..default()
                },
                LineHeight::Px(14.0),
                TextColor(fg_muted.with_alpha(0.7)),
                Anchor::TOP_LEFT,
                Transform::from_xyz(2.0, -(map_top + 4.0), 0.3),
            ));
            continue;
        }

        // Anchor the mini-map at the bounding-box top-left, then place each
        // member at its real relative position scaled by the fixed ratio.
        // The per-pane camera clips content to the tile, so a bigger tile
        // simply shows more.
        let origin = members
            .iter()
            .map(|m| m.rect.min)
            .fold(Vec2::splat(f32::INFINITY), |a, b| a.min(b));
        let margin = 6.0_f32;

        // Draw back-to-front by y/x so overlaps look sane.
        members.sort_by(|a, b| {
            a.rect
                .min
                .y
                .partial_cmp(&b.rect.min.y)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    a.rect
                        .min
                        .x
                        .partial_cmp(&b.rect.min.x)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        for (i, m) in members.iter().enumerate() {
            let local = (m.rect.min - origin) * MINI_SCALE + Vec2::new(margin, map_top + margin);
            let size = ((m.rect.max - m.rect.min) * MINI_SCALE).max(Vec2::splat(3.0));
            let z = 0.2 + i as f32 * 0.001;
            if let Some((handle, _)) = cache.by_id.get(&m.snap_id) {
                commands.spawn((
                    CanvasTileChild,
                    ChildOf(chrome.content_root),
                    Sprite {
                        image: handle.clone(),
                        custom_size: Some(size),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(local.x, -local.y, z),
                ));
            } else {
                // No snapshot yet — placeholder rect.
                let col = if i % 2 == 0 { accent } else { fg };
                commands.spawn((
                    CanvasTileChild,
                    ChildOf(chrome.content_root),
                    Sprite {
                        color: col.with_alpha(0.45),
                        custom_size: Some(size),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(local.x, -local.y, z),
                ));
            }
        }
    }
}

fn tile_signature(
    canvas_id: u64,
    content_size: Vec2,
    members: &[Member],
    cache: &PaneThumbCache,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canvas_id.hash(&mut h);
    ((content_size.x as i32), (content_size.y as i32)).hash(&mut h);
    cache.generation.hash(&mut h);
    members.len().hash(&mut h);
    for m in members {
        (
            m.rect.min.x as i32,
            m.rect.min.y as i32,
            m.rect.max.x as i32,
            m.rect.max.y as i32,
            m.snap_id,
        )
            .hash(&mut h);
    }
    h.finish()
}

// ---------- Breadcrumb ----------

const BREADCRUMB_FONT: f32 = 13.0;
const BREADCRUMB_TOP: f32 = 6.0;
const BREADCRUMB_H: f32 = 20.0;

#[derive(Resource, Default)]
struct BreadcrumbState {
    root: Option<Entity>,
    last_sig: u64,
    /// Per-crumb clickable rects in window-space + the descent depth to
    /// truncate to when clicked (crumb 0 = project root, crumb k keeps the
    /// first k canvases). Rebuilt with the visuals.
    hits: Vec<(Rect, usize)>,
}

#[derive(Component)]
struct CanvasBreadcrumb;

/// Render `↑  Project ▸ Canvas ▸ …` at the top of the canvas region while
/// the user is descended into a nested canvas. Each crumb is clickable
/// (jump up to that level); the leading `↑` pops one level. Hidden at the
/// project root.
fn render_breadcrumb(world: &mut World) {
    let Some(active) = world.resource::<Projects>().active else {
        clear_breadcrumb(world);
        return;
    };
    let path: Vec<u64> = world.resource::<CanvasNav>().path(active).to_vec();
    if path.is_empty() {
        clear_breadcrumb(world);
        return;
    }

    let project_name = world
        .resource::<Projects>()
        .list
        .iter()
        .find(|p| p.id == active)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "Project".to_string());

    let mut tile_names: HashMap<u64, String> = HashMap::new();
    {
        let mut q = world.query::<(&CanvasPane, &PaneTitle)>();
        for (cp, title) in q.iter(world) {
            tile_names.insert(cp.canvas_id, title.0.clone());
        }
    }
    // crumbs[0] = project (depth 0), crumbs[k] = path[k-1] (depth k).
    let mut crumbs: Vec<String> = vec![project_name];
    for id in &path {
        crumbs.push(tile_names.get(id).cloned().unwrap_or_else(|| "Canvas".to_string()));
    }

    let sig = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        crumbs.hash(&mut h);
        h.finish()
    };
    if world.resource::<BreadcrumbState>().root.is_some()
        && world.resource::<BreadcrumbState>().last_sig == sig
    {
        return;
    }
    clear_breadcrumb(world);

    let (win_w, win_h) = {
        let mut q = world.query::<&Window>();
        match q.iter(world).next() {
            Some(w) => (w.width(), w.height()),
            None => return,
        }
    };
    let sidebar_w = world.resource::<Sidebar>().width;
    let char_w = world.resource::<PaneFontMetrics>().char_width(BREADCRUMB_FONT);
    let font = world.resource::<PaneFont>().0.clone();
    let theme = world.resource::<jim_style::Theme>().clone();
    let fg = Color::LinearRgba(theme.color(jim_style::tokens::FG));
    let fg_muted = Color::LinearRgba(theme.color(jim_style::tokens::FG_MUTED));
    let accent = Color::LinearRgba(theme.color(jim_style::tokens::ACCENT));

    // Window-space layout (x right, y down from the top); convert to
    // world (center origin, y up) at spawn time.
    let left = sidebar_w + 12.0;
    let mut wx = left;
    let to_world = |wx: f32, wy: f32| Vec2::new(wx - win_w * 0.5, win_h * 0.5 - wy);

    let root = world
        .spawn((
            CanvasBreadcrumb,
            Transform::from_xyz(0.0, 0.0, 0.0),
            Visibility::Visible,
            RenderLayers::layer(MENU_OVERLAY_LAYER),
        ))
        .id();

    let mut hits: Vec<(Rect, usize)> = Vec::new();
    let sep = "  ▸  ";

    // Leading up affordance — ascend one level (depth = path.len()-1).
    let up_depth = path.len().saturating_sub(1);
    spawn_crumb(world, root, &font, "↑", accent, to_world(wx, BREADCRUMB_TOP));
    let up_w = char_w; // single glyph
    hits.push((
        Rect::from_corners(
            Vec2::new(wx - 3.0, BREADCRUMB_TOP),
            Vec2::new(wx + up_w + 3.0, BREADCRUMB_TOP + BREADCRUMB_H),
        ),
        up_depth,
    ));
    wx += up_w + char_w * 2.0;

    for (i, crumb) in crumbs.iter().enumerate() {
        let is_current = i + 1 == crumbs.len();
        let color = if is_current { accent } else { fg };
        spawn_crumb(world, root, &font, crumb, color, to_world(wx, BREADCRUMB_TOP));
        let w = crumb.chars().count() as f32 * char_w;
        hits.push((
            Rect::from_corners(
                Vec2::new(wx, BREADCRUMB_TOP),
                Vec2::new(wx + w, BREADCRUMB_TOP + BREADCRUMB_H),
            ),
            i, // clicking crumb i truncates the path to depth i
        ));
        wx += w;
        if !is_current {
            spawn_crumb(world, root, &font, sep, fg_muted, to_world(wx, BREADCRUMB_TOP));
            wx += sep.chars().count() as f32 * char_w;
        }
    }

    let mut bc = world.resource_mut::<BreadcrumbState>();
    bc.root = Some(root);
    bc.last_sig = sig;
    bc.hits = hits;
}

fn spawn_crumb(
    world: &mut World,
    root: Entity,
    font: &Handle<Font>,
    text: &str,
    color: Color,
    world_pos: Vec2,
) {
    world.spawn((
        ChildOf(root),
        Text2d::new(text.to_string()),
        TextFont {
            font: font.clone().into(),
            font_size: FontSize::Px(BREADCRUMB_FONT),
            ..default()
        },
        LineHeight::Px(BREADCRUMB_H),
        TextColor(color),
        Anchor::TOP_LEFT,
        Transform::from_xyz(world_pos.x, world_pos.y, 0.1),
        RenderLayers::layer(MENU_OVERLAY_LAYER),
    ));
}

/// Click a breadcrumb crumb (or the leading `↑`) to jump up to that level.
fn handle_breadcrumb_click(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut consumed: ResMut<InputConsumed>,
    block_zones: Res<PaneInputBlockZones>,
    state: Res<BreadcrumbState>,
    projects: Res<Projects>,
    mut flow: ResMut<ThumbFlow>,
) {
    if state.root.is_none() || consumed.0 || !buttons.just_pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else { return };
    let Some(pt) = window.cursor_position() else { return };
    // Don't steal clicks the sidebar (or other host chrome) owns.
    if block_zones
        .0
        .iter()
        .any(|r| pt.x >= r.min.x && pt.x <= r.max.x && pt.y >= r.min.y && pt.y <= r.max.y)
    {
        return;
    }
    let Some(active) = projects.active else { return };
    for (rect, depth) in &state.hits {
        if pt.x >= rect.min.x && pt.x <= rect.max.x && pt.y >= rect.min.y && pt.y <= rect.max.y {
            // Route through the capture flow so the level being left gets
            // a fresh snapshot before it's hidden.
            flow.request(active, *depth);
            consumed.0 = true;
            return;
        }
    }
}

fn clear_breadcrumb(world: &mut World) {
    let prev = {
        let mut bc = world.resource_mut::<BreadcrumbState>();
        bc.hits.clear();
        bc.root.take()
    };
    if let Some(root) = prev {
        let _ = world.despawn(root);
    }
}
