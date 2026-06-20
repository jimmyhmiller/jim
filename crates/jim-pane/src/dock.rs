//! Docking: snap floating panes together into a shared frame.
//!
//! A **dock** is a lightweight container pane that *slaves the
//! [`PaneRect`] of its member panes* to a layout. It never touches a
//! member's content: each member stays a full, independent pane (own
//! camera, render layer, scroll, focus, keyboard). The dock only decides
//! *where each member sits* and moves / resizes / raises them as a unit.
//! It is N real panes that travel together inside a frame, not one pane
//! multiplexing several widgets.
//!
//! # Layout = a split tree
//!
//! A dock's layout is a [`DockNode`] tree: interior `Split` nodes divide
//! their rect among children either horizontally (columns) or vertically
//! (rows), each child weighted by a fraction; leaves are member panes.
//! This gives arbitrary nested layouts (sidebars, grids, IDE shells, …),
//! a resizable splitter at every interior boundary, and makes
//! [`DockTemplate`]s just preset trees.
//!
//! # How a dock comes to be
//!
//! - **Snap** (hold **Option/Alt** while dragging a pane's title bar):
//!   drop onto a free pane → a dock materializes around both, split on
//!   the edge you dropped near; drop onto an existing dock cell → the
//!   pane splits that cell. Without Option, drags are normal.
//! - **Scriptable**: [`create_dock`] / `jimctl dock` frame panes via a
//!   template.
//!
//! Dragging a member's title bar OUT undocks it (no modifier).

use std::collections::HashMap;

use bevy::input::mouse::MouseButton;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use serde_json::Value;

use crate::{
    content_area, next_pane_z, spawn_pane, FocusedPane, PaneChrome, PaneKindMarker, PaneKindSpec,
    PaneMouseMode, PanePinned, PaneProject, PaneRect, PaneRegistry, PaneScreenAnchored, PaneTag,
    PaneViewport, SpawnedPane, MARGIN, MIN_PANE_SIZE, TITLE_H,
};

/// Registry key for the dock container pane kind.
pub const DOCK_KIND: &str = "dock";

/// RenderLayer for the drop-highlight overlay. A dedicated camera at a
/// very high `order` renders ONLY this layer, so the highlight draws
/// above every per-pane camera (whose order is `(z*100)+1`). The host
/// MUST reserve this layer in `PanePlugin.reserved_layers`.
pub const DOCK_OVERLAY_LAYER: usize = 29;
const DOCK_OVERLAY_ORDER: isize = 900_000;

/// Gap between cells, in canvas units; the dock background shows through
/// as a seam / splitter.
const GUTTER: f32 = 6.0;
/// Half-thickness of the splitter hit zone around a gutter centerline.
const SPLITTER_HIT: f32 = 7.0;
/// Minimum fraction a split child may shrink to (keeps cells grabbable).
const MIN_FRAC: f32 = 0.06;

// ---------- Split tree ----------

/// Which way the dropped pane sits relative to the target cell.
#[derive(Copy, Clone, Debug)]
pub enum DropEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl DropEdge {
    /// Left/Right → horizontal split; Top/Bottom → vertical.
    fn horizontal(self) -> bool {
        matches!(self, DropEdge::Left | DropEdge::Right)
    }
    /// True if the NEW pane goes *before* the target in child order.
    fn before(self) -> bool {
        matches!(self, DropEdge::Left | DropEdge::Top)
    }
}

/// A node in a dock's layout tree.
#[derive(Clone, Debug)]
pub enum DockNode {
    /// A member pane fills this cell.
    Leaf(Entity),
    /// An unfilled slot — a drop here fills the WHOLE cell (templates are
    /// skeletons of these you populate by dragging panes in).
    Empty,
    /// Divide the rect among `children`, weighted by `fracs` (parallel,
    /// summing to ~1). `horizontal` → columns (left→right); else rows.
    Split {
        horizontal: bool,
        fracs: Vec<f32>,
        children: Vec<DockNode>,
    },
}

/// Does this subtree contain any cell (a member leaf OR an empty slot)?
/// Used to decide when a dock's tree has truly emptied out (vs. a
/// skeleton that still has empty slots to fill).
fn has_content(node: &DockNode) -> bool {
    match node {
        DockNode::Leaf(_) | DockNode::Empty => true,
        DockNode::Split { children, .. } => children.iter().any(has_content),
    }
}

impl DockNode {
    fn leaves_into(&self, out: &mut Vec<Entity>) {
        match self {
            DockNode::Leaf(e) => out.push(*e),
            DockNode::Empty => {}
            DockNode::Split { children, .. } => {
                for c in children {
                    c.leaves_into(out);
                }
            }
        }
    }

    fn leaves(&self) -> Vec<Entity> {
        let mut v = Vec::new();
        self.leaves_into(&mut v);
        v
    }

    /// Remove the leaf for `target`. Returns true if found. Fracs of the
    /// containing split are renormalized; single-child collapse is done
    /// separately by [`collapse`].
    fn remove_leaf(&mut self, target: Entity) -> bool {
        if let DockNode::Split { children, fracs, .. } = self {
            if let Some(idx) = children
                .iter()
                .position(|c| matches!(c, DockNode::Leaf(e) if *e == target))
            {
                children.remove(idx);
                if idx < fracs.len() {
                    fracs.remove(idx);
                }
                renorm(fracs);
                return true;
            }
            for c in children.iter_mut() {
                if c.remove_leaf(target) {
                    return true;
                }
            }
        }
        false
    }
}

/// Replace single-child splits with their child, bottom-up.
fn collapse(node: DockNode) -> DockNode {
    match node {
        DockNode::Leaf(e) => DockNode::Leaf(e),
        DockNode::Empty => DockNode::Empty,
        DockNode::Split {
            horizontal,
            mut fracs,
            children,
        } => {
            let mut new_children: Vec<DockNode> = children.into_iter().map(collapse).collect();
            if new_children.len() == 1 {
                return new_children.pop().unwrap();
            }
            if fracs.len() != new_children.len() {
                fracs = even(new_children.len());
            }
            DockNode::Split {
                horizontal,
                fracs,
                children: new_children,
            }
        }
    }
}

/// Insert `new` beside the leaf `target` on `edge`. `root` is mutated in
/// place; handles the root-is-the-target-leaf case too.
fn apply_insert(root: &mut DockNode, target: Entity, new: Entity, edge: DropEdge) {
    // Root is the target leaf → wrap it in a fresh split.
    if let DockNode::Leaf(e) = root {
        if *e == target {
            *root = wrap_pair(*e, new, edge);
        }
        return;
    }
    insert_beside(root, target, new, edge);
}

fn wrap_pair(existing: Entity, new: Entity, edge: DropEdge) -> DockNode {
    let children = if edge.before() {
        vec![DockNode::Leaf(new), DockNode::Leaf(existing)]
    } else {
        vec![DockNode::Leaf(existing), DockNode::Leaf(new)]
    };
    // The new "sidebar-ish" cell (the one on the edge) gets the smaller
    // share for horizontal splits; vertical splits are even.
    let fracs = if edge.horizontal() {
        if edge.before() {
            vec![0.3, 0.7]
        } else {
            vec![0.7, 0.3]
        }
    } else {
        vec![0.5, 0.5]
    };
    DockNode::Split {
        horizontal: edge.horizontal(),
        fracs,
        children,
    }
}

fn insert_beside(node: &mut DockNode, target: Entity, new: Entity, edge: DropEdge) -> bool {
    let want_h = edge.horizontal();
    if let DockNode::Split {
        horizontal,
        fracs,
        children,
    } = node
    {
        if let Some(idx) = children
            .iter()
            .position(|c| matches!(c, DockNode::Leaf(e) if *e == target))
        {
            if *horizontal == want_h {
                // Same axis: split the target cell's share with the newcomer.
                let f = fracs.get(idx).copied().unwrap_or(1.0 / children.len() as f32);
                fracs[idx] = f * 0.5;
                let at = if edge.before() { idx } else { idx + 1 };
                fracs.insert(at, f * 0.5);
                children.insert(at, DockNode::Leaf(new));
            } else {
                // Cross axis: replace the leaf with a nested split.
                children[idx] = wrap_pair(target, new, edge);
            }
            return true;
        }
        for c in children.iter_mut() {
            if insert_beside(c, target, new, edge) {
                return true;
            }
        }
    }
    false
}

fn node_at_path_mut<'a>(root: &'a mut DockNode, path: &[usize]) -> Option<&'a mut DockNode> {
    let mut cur = root;
    for &i in path {
        match cur {
            DockNode::Split { children, .. } => cur = children.get_mut(i)?,
            DockNode::Leaf(_) | DockNode::Empty => return None,
        }
    }
    Some(cur)
}

fn even(n: usize) -> Vec<f32> {
    if n == 0 {
        Vec::new()
    } else {
        vec![1.0 / n as f32; n]
    }
}

fn renorm(fracs: &mut Vec<f32>) {
    let sum: f32 = fracs.iter().copied().sum();
    if sum <= f32::EPSILON {
        let n = fracs.len();
        *fracs = even(n);
    } else {
        for f in fracs.iter_mut() {
            *f /= sum;
        }
    }
}

// ---------- Layout walk ----------

/// One cell's computed rect (canvas space). `slot` is the member pane,
/// or `None` for an empty slot. `path` locates the node in the tree.
struct CellRect {
    slot: Option<Entity>,
    pos: Vec2,
    size: Vec2,
    path: Vec<usize>,
}

/// A draggable boundary between two children of one split node.
struct SplitHandle {
    path: Vec<usize>,
    boundary: usize,
    horizontal: bool,
    /// Gutter centerline (x for horizontal splits, y for vertical).
    center: f32,
    node_pos: Vec2,
    node_size: Vec2,
}

fn walk_layout(
    node: &DockNode,
    pos: Vec2,
    size: Vec2,
    path: &[usize],
    cells: &mut Vec<CellRect>,
    handles: &mut Vec<SplitHandle>,
) {
    match node {
        DockNode::Leaf(e) => cells.push(CellRect {
            slot: Some(*e),
            pos,
            size,
            path: path.to_vec(),
        }),
        DockNode::Empty => cells.push(CellRect {
            slot: None,
            pos,
            size,
            path: path.to_vec(),
        }),
        DockNode::Split {
            horizontal,
            fracs,
            children,
        } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            let gut = GUTTER * (n.saturating_sub(1)) as f32;
            if *horizontal {
                let avail = (size.x - gut).max(0.0);
                let mut x = pos.x;
                for (i, c) in children.iter().enumerate() {
                    let w = avail * fracs.get(i).copied().unwrap_or(1.0 / n as f32);
                    let mut p = path.to_vec();
                    p.push(i);
                    walk_layout(c, Vec2::new(x, pos.y), Vec2::new(w, size.y), &p, cells, handles);
                    if i + 1 < n {
                        handles.push(SplitHandle {
                            path: path.to_vec(),
                            boundary: i,
                            horizontal: true,
                            center: x + w + GUTTER * 0.5,
                            node_pos: pos,
                            node_size: size,
                        });
                    }
                    x += w + GUTTER;
                }
            } else {
                let avail = (size.y - gut).max(0.0);
                let mut y = pos.y;
                for (i, c) in children.iter().enumerate() {
                    let h = avail * fracs.get(i).copied().unwrap_or(1.0 / n as f32);
                    let mut p = path.to_vec();
                    p.push(i);
                    walk_layout(c, Vec2::new(pos.x, y), Vec2::new(size.x, h), &p, cells, handles);
                    if i + 1 < n {
                        handles.push(SplitHandle {
                            path: path.to_vec(),
                            boundary: i,
                            horizontal: false,
                            center: y + h + GUTTER * 0.5,
                            node_pos: pos,
                            node_size: size,
                        });
                    }
                    y += h + GUTTER;
                }
            }
        }
    }
}

fn dock_inner(rect: &PaneRect) -> (Vec2, Vec2) {
    let (origin_local, size) = content_area(rect);
    let inner_pos = rect.pos + Vec2::new(origin_local.x, TITLE_H + MARGIN);
    (inner_pos, size)
}

// ---------- Components ----------

/// A container pane whose [`Dock::root`] tree lays out its member panes.
#[derive(Component, Clone, Debug, Default)]
pub struct Dock {
    /// Layout tree; `None` while empty (no members yet).
    pub root: Option<DockNode>,
    /// Collapse member chrome (shadow + close button) so the dock reads
    /// as one surface rather than stacked floating windows.
    pub collapse_chrome: bool,
    /// True for a fixed-shape **template skeleton**: undocking a member
    /// turns its cell back into an empty slot (the shape is preserved)
    /// instead of removing the cell. Ad-hoc docks (snapped together) have
    /// this false and shrink when a member leaves.
    pub template: bool,
}

/// Replace the `Leaf(target)` somewhere in the tree with an empty slot,
/// preserving the surrounding shape. Returns true if found.
fn replace_leaf_with_empty(node: &mut DockNode, target: Entity) -> bool {
    match node {
        DockNode::Leaf(e) if *e == target => {
            *node = DockNode::Empty;
            true
        }
        DockNode::Split { children, .. } => children.iter_mut().any(|c| replace_leaf_with_empty(c, target)),
        _ => false,
    }
}

/// Replace the empty slot at `path` with `Leaf(pane)`.
fn fill_slot(root: &mut DockNode, path: &[usize], pane: Entity) -> bool {
    if let Some(node) = node_at_path_mut(root, path) {
        if matches!(node, DockNode::Empty) {
            *node = DockNode::Leaf(pane);
            return true;
        }
    }
    false
}

impl Dock {
    /// Member pane entities in left/top-to-right/bottom leaf order. The
    /// index is the persisted slot.
    pub fn member_entities(&self) -> Vec<Entity> {
        self.root.as_ref().map(|r| r.leaves()).unwrap_or_default()
    }
}

/// Marks a pane as a member of `dock`. [`dock_layout`] owns its `PaneRect`.
#[derive(Component, Copy, Clone, Debug)]
pub struct DockMember {
    pub dock: Entity,
}

/// Temporary marker on a freshly restored pane, carrying its dock group
/// id + slot until [`link_restored_docks`] rebuilds membership.
#[derive(Component, Copy, Clone, Debug)]
pub struct PendingDockLink {
    pub group: u64,
    pub slot: usize,
}

/// Temporary: the dock's serialized layout tree (leaves = slot indices),
/// parked on the dock entity at restore until its members exist.
#[derive(Component, Clone)]
struct PendingDockTree(Value);

// ---------- Resources ----------

#[derive(Resource, Default)]
struct DockDrag {
    pane: Option<Entity>,
    target: Option<DropTarget>,
}

#[derive(Clone)]
struct DropTarget {
    /// The pane/dock under the cursor.
    anchor: Entity,
    anchor_is_dock: bool,
    kind: DropKind,
}

#[derive(Clone)]
enum DropKind {
    /// Anchor is a free pane → make a new dock, split on this edge.
    NewDock { edge: DropEdge },
    /// Split an occupied cell on this edge.
    SplitLeaf { leaf: Entity, edge: DropEdge },
    /// Fill an empty slot whole (its node path; empty `path` = a dock with
    /// no slots yet).
    FillSlot { path: Vec<usize> },
}

#[derive(Resource, Default)]
struct SplitterDrag {
    dock: Option<Entity>,
    path: Vec<usize>,
    boundary: usize,
    horizontal: bool,
    node_pos: Vec2,
    node_size: Vec2,
}

/// Marker on the single reusable drop-highlight sprite.
#[derive(Component)]
struct DockHighlightMarker;

/// A faint placeholder rect drawn over an empty slot so it reads as a
/// droppable region. Child of the dock's `content_root`; rebuilt when the
/// dock's tree or rect changes.
#[derive(Component)]
struct DockSlotPlaceholder {
    dock: Entity,
}

// ---------- Plugin ----------

pub struct DockPlugin;

impl Plugin for DockPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DockDrag>()
            .init_resource::<SplitterDrag>()
            .add_systems(Startup, (register_dock_kind, spawn_dock_overlay))
            // All dock interaction runs in Update BEFORE the pane chrome
            // chain (`PaneViewportReaders` / `handle_pane_mouse`): button
            // edge events are only valid after PreUpdate input, and snap
            // needs `PaneMouseMode` still in `WindowDrag` (cleared later).
            .add_systems(
                Update,
                (
                    dock_drag_track,
                    dock_snap_apply,
                    splitter_drag,
                    link_restored_docks,
                    collapse_member_chrome,
                    dock_focus_raise,
                    dock_layout,
                    render_dock_slots,
                )
                    .chain()
                    .before(crate::PaneViewportReaders),
            );
    }
}

fn register_dock_kind(mut registry: ResMut<PaneRegistry>) {
    registry.register(PaneKindSpec {
        kind: DOCK_KIND,
        display_name: "Dock",
        radial_icon: Some("▤"),
        default_size: Vec2::new(720.0, 460.0),
        spawn: dock_spawn_from_config,
        snapshot: dock_snapshot,
        on_close: Some(dock_on_close),
    });
}

fn spawn_dock_overlay(mut commands: Commands) {
    use bevy::camera::visibility::RenderLayers;
    commands.spawn((
        Camera2d,
        Camera {
            order: DOCK_OVERLAY_ORDER,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderLayers::layer(DOCK_OVERLAY_LAYER),
    ));
    commands.spawn((
        DockHighlightMarker,
        Sprite {
            color: Color::srgba(0.40, 0.62, 1.0, 0.22),
            custom_size: Some(Vec2::ZERO),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, 0.0),
        Visibility::Hidden,
        RenderLayers::layer(DOCK_OVERLAY_LAYER),
    ));
}

fn show_highlight(
    q: &mut Query<(&mut Transform, &mut Sprite, &mut Visibility), With<DockHighlightMarker>>,
    canvas_pos: Vec2,
    canvas_size: Vec2,
    viewport: &PaneViewport,
    window: &Window,
) {
    let Ok((mut t, mut s, mut v)) = q.single_mut() else {
        return;
    };
    let win = Vec2::new(window.width(), window.height());
    let screen_tl = viewport.canvas_to_window(canvas_pos);
    let screen_size = canvas_size * viewport.zoom;
    let center = screen_tl + screen_size * 0.5;
    t.translation = Vec3::new(center.x - win.x * 0.5, win.y * 0.5 - center.y, 0.0);
    s.custom_size = Some(screen_size);
    if *v != Visibility::Visible {
        *v = Visibility::Visible;
    }
}

fn hide_highlight(
    q: &mut Query<(&mut Transform, &mut Sprite, &mut Visibility), With<DockHighlightMarker>>,
) {
    if let Ok((_, _, mut v)) = q.single_mut() {
        if *v != Visibility::Hidden {
            *v = Visibility::Hidden;
        }
    }
}

// ---------- Kind spawn / snapshot ----------

fn dock_spawn_from_config(world: &mut World, entity: Entity, _content_root: Entity, config: &Value) {
    let collapse_chrome = config
        .get("collapse_chrome")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let template = config.get("template").and_then(|v| v.as_bool()).unwrap_or(false);
    world.entity_mut(entity).insert(Dock {
        root: None,
        collapse_chrome,
        template,
    });
    if let Some(tree) = config.get("tree") {
        world.entity_mut(entity).insert(PendingDockTree(tree.clone()));
    }
}

fn dock_snapshot(world: &World, entity: Entity) -> Value {
    let Some(d) = world.get::<Dock>(entity) else {
        return serde_json::json!({});
    };
    let mut out = serde_json::json!({
        "collapse_chrome": d.collapse_chrome,
        "template": d.template,
    });
    if let Some(root) = &d.root {
        let leaves = root.leaves();
        let index: HashMap<Entity, usize> =
            leaves.iter().enumerate().map(|(i, e)| (*e, i)).collect();
        out["tree"] = tree_to_json(root, &index);
    }
    out
}

fn tree_to_json(node: &DockNode, index: &HashMap<Entity, usize>) -> Value {
    match node {
        DockNode::Leaf(e) => serde_json::json!({ "leaf": index.get(e).copied().unwrap_or(0) }),
        DockNode::Empty => serde_json::json!({ "empty": true }),
        DockNode::Split {
            horizontal,
            fracs,
            children,
        } => serde_json::json!({
            "h": horizontal,
            "f": fracs,
            "c": children.iter().map(|c| tree_to_json(c, index)).collect::<Vec<_>>(),
        }),
    }
}

fn tree_from_json(v: &Value, map: &HashMap<usize, Entity>) -> Option<DockNode> {
    if v.get("empty").and_then(|x| x.as_bool()).unwrap_or(false) {
        return Some(DockNode::Empty);
    }
    if let Some(leaf) = v.get("leaf").and_then(|x| x.as_u64()) {
        // A member that didn't restore (closed pane) leaves an empty slot
        // rather than vanishing the cell.
        return Some(
            map.get(&(leaf as usize))
                .map(|e| DockNode::Leaf(*e))
                .unwrap_or(DockNode::Empty),
        );
    }
    let h = v.get("h")?.as_bool()?;
    let mut fracs: Vec<f32> = v
        .get("f")?
        .as_array()?
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect();
    let children: Vec<DockNode> = v
        .get("c")?
        .as_array()?
        .iter()
        .filter_map(|c| tree_from_json(c, map))
        .collect();
    if children.is_empty() {
        return None;
    }
    if fracs.len() != children.len() {
        fracs = even(children.len());
    }
    Some(DockNode::Split {
        horizontal: h,
        fracs,
        children,
    })
}

/// Closing a dock frees its members back to floating panes.
fn dock_on_close(world: &mut World, entity: Entity) {
    let members = world
        .get::<Dock>(entity)
        .map(|d| d.member_entities())
        .unwrap_or_default();
    for m in members {
        if world.get_entity(m).is_ok() {
            world.entity_mut(m).remove::<DockMember>();
        }
    }
}

// ---------- Layout system ----------

fn is_free_pane(
    e: Entity,
    panes: &Query<(Has<Dock>, Has<DockMember>, Has<PanePinned>, Has<PaneScreenAnchored>), With<PaneTag>>,
) -> bool {
    match panes.get(e) {
        Ok((is_dock, is_member, pinned, anchored)) => !is_dock && !is_member && !pinned && !anchored,
        Err(_) => false,
    }
}

/// Position every dock's member cells, pruning dead members.
fn dock_layout(
    mut docks: Query<(&PaneRect, &mut Dock), Without<DockMember>>,
    mut rects: Query<&mut PaneRect, (With<DockMember>, Without<Dock>)>,
    alive: Query<(), With<PaneTag>>,
) {
    for (dock_rect, mut dock) in &mut docks {
        // Prune dead members. On a template skeleton a closed member's
        // cell reverts to an empty slot; on an ad-hoc dock it's removed.
        let is_template = dock.template;
        if let Some(mut r) = dock.root.take() {
            let dead: Vec<Entity> = r.leaves().into_iter().filter(|e| alive.get(*e).is_err()).collect();
            if is_template {
                for d in dead {
                    replace_leaf_with_empty(&mut r, d);
                }
                dock.root = Some(r);
            } else {
                for d in dead {
                    r.remove_leaf(d);
                }
                let r = collapse(r);
                // Keep while any cell remains; otherwise drop the tree.
                dock.root = if has_content(&r) { Some(r) } else { None };
            }
        }
        let Some(root) = &dock.root else { continue };

        let (inner_pos, inner_size) = dock_inner(dock_rect);
        let member_z = dock_rect.z + 1.0;
        let mut cells = Vec::new();
        let mut handles = Vec::new();
        walk_layout(root, inner_pos, inner_size, &[], &mut cells, &mut handles);

        for cell in cells {
            let Some(member) = cell.slot else { continue }; // empty slot
            let want = PaneRect {
                pos: cell.pos,
                size: Vec2::new(cell.size.x.max(1.0), cell.size.y.max(1.0)),
                z: member_z,
            };
            if let Ok(mut r) = rects.get_mut(member) {
                if r.pos != want.pos || r.size != want.size || r.z != want.z {
                    *r = want;
                }
            }
        }
    }
}

/// Raise the whole dock group when a member/dock is focused.
fn dock_focus_raise(
    focused: Res<FocusedPane>,
    members: Query<&DockMember>,
    is_dock: Query<(), With<Dock>>,
    member_of: Query<&DockMember>,
    mut rects: Query<(Entity, &mut PaneRect, Has<PanePinned>), With<PaneTag>>,
) {
    if !focused.is_changed() {
        return;
    }
    let Some(f) = focused.0 else { return };
    let dock_entity = if is_dock.get(f).is_ok() {
        Some(f)
    } else {
        members.get(f).ok().map(|m| m.dock)
    };
    let Some(dock_entity) = dock_entity else { return };

    let mut max_other = 0.0_f32;
    for (e, r, pinned) in &rects {
        if pinned || e == dock_entity {
            continue;
        }
        if let Ok(m) = member_of.get(e) {
            if m.dock == dock_entity {
                continue;
            }
        }
        if r.z > max_other {
            max_other = r.z;
        }
    }
    if let Ok((_, mut r, _)) = rects.get_mut(dock_entity) {
        let want = max_other + 1.0;
        if r.z < want {
            r.z = want;
        }
    }
}

/// Collapse member chrome (drop shadow + close button) on docking so a
/// dock reads as one cohesive surface; restore on undock. Member title
/// bars stay as slim per-cell drag handles.
fn collapse_member_chrome(
    added: Query<(&DockMember, &PaneChrome), Added<DockMember>>,
    docks: Query<&Dock>,
    mut removed: RemovedComponents<DockMember>,
    chromes: Query<&PaneChrome>,
    mut vis: Query<&mut Visibility>,
) {
    for (dm, chrome) in &added {
        let collapse = docks.get(dm.dock).map(|d| d.collapse_chrome).unwrap_or(true);
        if !collapse {
            continue;
        }
        for e in [chrome.shadow, chrome.close_button] {
            if let Ok(mut v) = vis.get_mut(e) {
                if *v != Visibility::Hidden {
                    *v = Visibility::Hidden;
                }
            }
        }
    }
    for entity in removed.read() {
        if let Ok(chrome) = chromes.get(entity) {
            for e in [chrome.shadow, chrome.close_button] {
                if let Ok(mut v) = vis.get_mut(e) {
                    if *v != Visibility::Inherited {
                        *v = Visibility::Inherited;
                    }
                }
            }
        }
    }
}

/// Draw a faint placeholder over each empty slot so the user can see
/// where to drop. Rebuilt only when a dock's tree or rect changes.
#[allow(clippy::type_complexity)]
fn render_dock_slots(
    mut commands: Commands,
    changed: Query<
        (Entity, &PaneRect, &Dock, &PaneChrome),
        (Or<(Changed<Dock>, Changed<PaneRect>)>, Without<DockMember>),
    >,
    existing: Query<(Entity, &DockSlotPlaceholder)>,
) {
    for (dock_e, rect, dock, chrome) in &changed {
        // Clear this dock's old placeholders.
        for (pe, p) in &existing {
            if p.dock == dock_e {
                commands.entity(pe).despawn();
            }
        }
        let Some(root) = &dock.root else { continue };
        let (ip, is) = dock_inner(rect);
        let mut cells = Vec::new();
        let mut handles = Vec::new();
        walk_layout(root, ip, is, &[], &mut cells, &mut handles);
        for c in cells {
            if c.slot.is_some() {
                continue; // only empty slots get a placeholder
            }
            // content_root-local coords (y-down → negative y).
            let lx = c.pos.x - ip.x;
            let ly = -(c.pos.y - ip.y);
            commands.spawn((
                DockSlotPlaceholder { dock: dock_e },
                ChildOf(chrome.content_root),
                Sprite {
                    color: Color::srgba(0.40, 0.62, 1.0, 0.06),
                    custom_size: Some(Vec2::new(c.size.x.max(1.0), c.size.y.max(1.0))),
                    ..default()
                },
                Anchor::TOP_LEFT,
                Transform::from_xyz(lx, ly, 0.05),
            ));
        }
    }
}

// ---------- Drag tracking + snap ----------

#[allow(clippy::type_complexity)]
fn dock_drag_track(
    mut commands: Commands,
    windows: Query<&Window>,
    keys: Res<ButtonInput<KeyCode>>,
    viewport: Res<PaneViewport>,
    mode: Res<PaneMouseMode>,
    mut drag: ResMut<DockDrag>,
    mut docks: Query<&mut Dock>,
    member_q: Query<&DockMember>,
    free_q: Query<
        (Has<Dock>, Has<DockMember>, Has<PanePinned>, Has<PaneScreenAnchored>),
        With<PaneTag>,
    >,
    pane_rects: Query<
        (
            Entity,
            &PaneRect,
            Has<Dock>,
            Has<DockMember>,
            Has<PanePinned>,
            Option<&Visibility>,
            Option<&PaneProject>,
        ),
        (With<PaneTag>, Without<DockHighlightMarker>),
    >,
    mut highlight: Query<(&mut Transform, &mut Sprite, &mut Visibility), With<DockHighlightMarker>>,
) {
    let dragged = match *mode {
        PaneMouseMode::WindowDrag { pane, .. } => Some(pane),
        _ => None,
    };
    let Some(dragged) = dragged else {
        drag.pane = None;
        drag.target = None;
        hide_highlight(&mut highlight);
        return;
    };

    // Dragging a member's title bar OUT undocks it (no modifier). On a
    // template skeleton the cell becomes an empty slot again (shape kept);
    // on an ad-hoc dock the cell is removed and the tree shrinks.
    if let Ok(dm) = member_q.get(dragged) {
        if let Ok(mut dock) = docks.get_mut(dm.dock) {
            let is_template = dock.template;
            if let Some(mut r) = dock.root.take() {
                if is_template {
                    replace_leaf_with_empty(&mut r, dragged);
                    dock.root = Some(r);
                } else {
                    r.remove_leaf(dragged);
                    let r = collapse(r);
                    dock.root = if has_content(&r) { Some(r) } else { None };
                }
            }
        }
        commands.entity(dragged).remove::<DockMember>();
    }

    // Docking engages ONLY while Option (Alt) is held.
    let docking = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if !docking || !is_free_pane(dragged, &free_q) {
        drag.pane = None;
        drag.target = None;
        hide_highlight(&mut highlight);
        return;
    }
    drag.pane = Some(dragged);

    let Ok(window) = windows.single() else { return };
    let Some(cursor) = window.cursor_position() else {
        hide_highlight(&mut highlight);
        return;
    };
    let cur = viewport.window_to_canvas(cursor);
    let dragged_project = pane_rects.get(dragged).ok().and_then(|t| t.6.map(|p| p.0));

    // Topmost OTHER visible same-project pane under the cursor.
    let mut best: Option<(Entity, bool, f32)> = None;
    for (e, r, has_dock, has_member, pinned, vis, proj) in &pane_rects {
        if e == dragged || pinned || has_member {
            continue;
        }
        if matches!(vis, Some(Visibility::Hidden)) {
            continue;
        }
        if proj.map(|p| p.0) != dragged_project {
            continue;
        }
        let inside = cur.x >= r.pos.x
            && cur.x <= r.pos.x + r.size.x
            && cur.y >= r.pos.y
            && cur.y <= r.pos.y + r.size.y;
        if inside && best.map_or(true, |(_, _, z)| r.z > z) {
            best = Some((e, has_dock, r.z));
        }
    }

    let Some((anchor, anchor_is_dock, _)) = best else {
        drag.target = None;
        hide_highlight(&mut highlight);
        return;
    };
    let Ok((_, anchor_rect, _, _, _, _, _)) = pane_rects.get(anchor) else {
        drag.target = None;
        hide_highlight(&mut highlight);
        return;
    };

    // Resolve the drop: which cell under the cursor, and how it lands.
    let (kind, hi_pos, hi_size) = if anchor_is_dock {
        let root = docks.get(anchor).ok().and_then(|d| d.root.clone());
        let (ip, is) = dock_inner(anchor_rect);
        match root {
            // Empty dock with no slots yet → first drop fills it whole.
            None => (DropKind::FillSlot { path: Vec::new() }, ip, is),
            Some(root) => {
                let mut cells = Vec::new();
                let mut handles = Vec::new();
                walk_layout(&root, ip, is, &[], &mut cells, &mut handles);
                let hit = cells.into_iter().find(|c| {
                    cur.x >= c.pos.x
                        && cur.x <= c.pos.x + c.size.x
                        && cur.y >= c.pos.y
                        && cur.y <= c.pos.y + c.size.y
                });
                match hit {
                    None => {
                        drag.target = None;
                        hide_highlight(&mut highlight);
                        return;
                    }
                    Some(c) => match c.slot {
                        // Empty slot → fill the WHOLE cell.
                        None => (DropKind::FillSlot { path: c.path }, c.pos, c.size),
                        // Occupied → split that cell on the nearest edge.
                        Some(leaf) => {
                            let edge = edge_in_rect(cur, c.pos, c.size);
                            let (hp, hs) = edge_half(c.pos, c.size, edge);
                            (DropKind::SplitLeaf { leaf, edge }, hp, hs)
                        }
                    },
                }
            }
        }
    } else {
        // Free pane: drop near an edge makes a dock split that way.
        let edge = edge_in_rect(cur, anchor_rect.pos, anchor_rect.size);
        let (hp, hs) = edge_half(anchor_rect.pos, anchor_rect.size, edge);
        (DropKind::NewDock { edge }, hp, hs)
    };

    drag.target = Some(DropTarget {
        anchor,
        anchor_is_dock,
        kind,
    });
    show_highlight(&mut highlight, hi_pos, hi_size, &viewport, window);
}

fn edge_in_rect(cur: Vec2, pos: Vec2, size: Vec2) -> DropEdge {
    let fx = ((cur.x - pos.x) / size.x.max(1.0)).clamp(0.0, 1.0);
    let fy = ((cur.y - pos.y) / size.y.max(1.0)).clamp(0.0, 1.0);
    let dl = fx;
    let dr = 1.0 - fx;
    let dt = fy;
    let db = 1.0 - fy;
    let m = dl.min(dr).min(dt).min(db);
    if m == dl {
        DropEdge::Left
    } else if m == dr {
        DropEdge::Right
    } else if m == dt {
        DropEdge::Top
    } else {
        DropEdge::Bottom
    }
}

fn edge_half(pos: Vec2, size: Vec2, edge: DropEdge) -> (Vec2, Vec2) {
    match edge {
        DropEdge::Left => (pos, Vec2::new(size.x * 0.5, size.y)),
        DropEdge::Right => (pos + Vec2::new(size.x * 0.5, 0.0), Vec2::new(size.x * 0.5, size.y)),
        DropEdge::Top => (pos, Vec2::new(size.x, size.y * 0.5)),
        DropEdge::Bottom => (pos + Vec2::new(0.0, size.y * 0.5), Vec2::new(size.x, size.y * 0.5)),
    }
}

/// Finalize a snap on left-button release.
fn dock_snap_apply(world: &mut World) {
    let released = world
        .resource::<ButtonInput<MouseButton>>()
        .just_released(MouseButton::Left);
    if !released {
        return;
    }
    let (Some(pane), Some(target)) = ({
        let drag = world.resource::<DockDrag>();
        (drag.pane, drag.target.clone())
    }) else {
        return;
    };
    {
        let mut drag = world.resource_mut::<DockDrag>();
        drag.pane = None;
        drag.target = None;
    }
    {
        let mut q = world.query_filtered::<&mut Visibility, With<DockHighlightMarker>>();
        for mut v in q.iter_mut(world) {
            *v = Visibility::Hidden;
        }
    }

    if world.get_entity(pane).is_err() || world.get_entity(target.anchor).is_err() {
        return;
    }
    match target.kind {
        DropKind::NewDock { edge } => create_dock_around(world, target.anchor, pane, edge),
        DropKind::SplitLeaf { leaf, edge } => insert_into_dock(world, target.anchor, leaf, pane, edge),
        DropKind::FillSlot { path } => fill_dock_slot(world, target.anchor, &path, pane),
    }
}

/// Fill an empty slot (or an empty dock) with `pane`.
fn fill_dock_slot(world: &mut World, dock: Entity, path: &[usize], pane: Entity) {
    if let Some(mut d) = world.get_mut::<Dock>(dock) {
        match &mut d.root {
            Some(root) => {
                fill_slot(root, path, pane);
            }
            None => d.root = Some(DockNode::Leaf(pane)),
        }
    } else {
        return;
    }
    world.entity_mut(pane).insert(DockMember { dock });
}

/// Insert `pane` into an existing `dock`, splitting `target_leaf` on `edge`.
fn insert_into_dock(world: &mut World, dock: Entity, target_leaf: Entity, pane: Entity, edge: DropEdge) {
    if let Some(mut d) = world.get_mut::<Dock>(dock) {
        match &mut d.root {
            Some(root) => apply_insert(root, target_leaf, pane, edge),
            None => d.root = Some(DockNode::Leaf(pane)), // empty dock
        }
    } else {
        return;
    }
    world.entity_mut(pane).insert(DockMember { dock });
}

/// Auto-create a dock framing `anchor` + `pane`, split on `edge`.
fn create_dock_around(world: &mut World, anchor: Entity, pane: Entity, edge: DropEdge) {
    let Some(anchor_rect) = world.get::<PaneRect>(anchor).copied() else {
        return;
    };
    let project = world.get::<PaneProject>(anchor).map(|p| p.0);

    // Grow the frame in the split direction to make room for the new cell.
    let extra = if edge.horizontal() {
        Vec2::new(anchor_rect.size.x.min(360.0) + GUTTER, 0.0)
    } else {
        Vec2::new(0.0, anchor_rect.size.y.min(280.0) + GUTTER)
    };
    let frame_pos = anchor_rect.pos - Vec2::new(MARGIN, TITLE_H + MARGIN);
    let frame_size = anchor_rect.size + Vec2::new(2.0 * MARGIN, TITLE_H + 2.0 * MARGIN) + extra;
    let z = next_pane_z(world);

    let SpawnedPane { entity: dock, .. } = spawn_pane(
        world,
        DOCK_KIND,
        "Dock",
        PaneRect { pos: frame_pos, size: frame_size, z },
        project,
    );
    world.entity_mut(dock).insert(Dock {
        root: Some(wrap_pair(anchor, pane, edge)),
        collapse_chrome: true,
        template: false,
    });
    // Members keep their existing PaneProject — docking never moves a pane
    // between projects.
    world.entity_mut(anchor).insert(DockMember { dock });
    world.entity_mut(pane).insert(DockMember { dock });
}

// ---------- Templates ----------

/// Preset dock layouts built from the split tree.
#[derive(Copy, Clone, Debug)]
pub enum DockTemplate {
    /// N equal columns.
    Columns,
    /// N equal rows.
    Rows,
    /// Narrow first column + the rest as columns.
    Sidebar,
    /// Roughly square grid (rows of columns).
    Grid,
    /// First member large on top, the rest as a row beneath.
    MainBottom,
    /// All-but-last as a row of columns on TOP, the last member as a
    /// full-width cell across the BOTTOM (e.g. left | right / bottom).
    ColumnsBottom,
}

impl DockTemplate {
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "columns" | "cols" | "column" => DockTemplate::Columns,
            "rows" | "row" => DockTemplate::Rows,
            "sidebar" | "side" => DockTemplate::Sidebar,
            "grid" => DockTemplate::Grid,
            "main-bottom" | "mainbottom" | "main" => DockTemplate::MainBottom,
            "columns-bottom" | "cols-bottom" | "lr-bottom" | "lrb" => DockTemplate::ColumnsBottom,
            _ => return None,
        })
    }
}

fn build_template(members: &[Entity], t: DockTemplate) -> Option<DockNode> {
    build_from_nodes(members.iter().map(|e| DockNode::Leaf(*e)).collect(), t)
}

fn build_skeleton(n: usize, t: DockTemplate) -> Option<DockNode> {
    build_from_nodes(vec![DockNode::Empty; n], t)
}

/// Arrange pre-made cell nodes (leaves or empty slots) into a template
/// shape. Shared by [`build_template`] (filled) and [`build_skeleton`].
fn build_from_nodes(nodes: Vec<DockNode>, t: DockTemplate) -> Option<DockNode> {
    let n = nodes.len();
    if n == 0 {
        return None;
    }
    if n == 1 {
        return Some(nodes.into_iter().next().unwrap());
    }
    let group = |ms: &[DockNode], horizontal: bool| DockNode::Split {
        horizontal,
        fracs: even(ms.len()),
        children: ms.to_vec(),
    };
    Some(match t {
        DockTemplate::Columns => group(&nodes, true),
        DockTemplate::Rows => group(&nodes, false),
        DockTemplate::Sidebar => {
            let rest = &nodes[1..];
            let rest_node = if rest.len() == 1 {
                rest[0].clone()
            } else {
                group(rest, true)
            };
            DockNode::Split {
                horizontal: true,
                fracs: vec![0.25, 0.75],
                children: vec![nodes[0].clone(), rest_node],
            }
        }
        DockTemplate::MainBottom => {
            let rest = &nodes[1..];
            let rest_node = if rest.len() == 1 {
                rest[0].clone()
            } else {
                group(rest, true)
            };
            DockNode::Split {
                horizontal: false,
                fracs: vec![0.7, 0.3],
                children: vec![nodes[0].clone(), rest_node],
            }
        }
        DockTemplate::ColumnsBottom => {
            // Top row = all but the last cell as columns; bottom = the
            // last cell, full width.
            let top = &nodes[..n - 1];
            let top_node = if top.len() == 1 {
                top[0].clone()
            } else {
                group(top, true)
            };
            DockNode::Split {
                horizontal: false,
                fracs: vec![0.65, 0.35],
                children: vec![top_node, nodes[n - 1].clone()],
            }
        }
        DockTemplate::Grid => {
            let per_row = (n as f32).sqrt().ceil() as usize;
            let row_nodes: Vec<DockNode> = nodes
                .chunks(per_row)
                .map(|chunk| {
                    if chunk.len() == 1 {
                        chunk[0].clone()
                    } else {
                        group(chunk, true)
                    }
                })
                .collect();
            if row_nodes.len() == 1 {
                row_nodes.into_iter().next().unwrap()
            } else {
                DockNode::Split {
                    horizontal: false,
                    fracs: even(row_nodes.len()),
                    children: row_nodes,
                }
            }
        }
    })
}

// ---------- Public scriptable API ----------

/// Frame a set of panes into a new dock via `template` (default columns).
/// Skips dead / already-docked / pinned / anchored / dock entities, in the
/// given order. Returns the dock entity, or `None` for < 2 valid members.
pub fn create_dock(world: &mut World, members: &[Entity]) -> Option<Entity> {
    create_dock_template(world, members, DockTemplate::Columns)
}

pub fn create_dock_template(
    world: &mut World,
    members: &[Entity],
    template: DockTemplate,
) -> Option<Entity> {
    let mut valid: Vec<Entity> = Vec::new();
    for &e in members {
        if valid.contains(&e) || world.get_entity(e).is_err() {
            continue;
        }
        let free = world.get::<PaneTag>(e).is_some()
            && world.get::<Dock>(e).is_none()
            && world.get::<DockMember>(e).is_none()
            && world.get::<PanePinned>(e).is_none()
            && world.get::<PaneScreenAnchored>(e).is_none();
        if free {
            valid.push(e);
        }
    }
    if valid.len() < 2 {
        return None;
    }

    let mut min = Vec2::splat(f32::INFINITY);
    let mut max = Vec2::splat(f32::NEG_INFINITY);
    for &e in &valid {
        if let Some(r) = world.get::<PaneRect>(e) {
            min = min.min(r.pos);
            max = max.max(r.pos + r.size);
        }
    }
    let union_size = (max - min).max(Vec2::new(MIN_PANE_SIZE.x * valid.len() as f32, MIN_PANE_SIZE.y));
    let frame_pos = min - Vec2::new(MARGIN, TITLE_H + MARGIN);
    let frame_size = union_size + Vec2::new(2.0 * MARGIN, TITLE_H + 2.0 * MARGIN);
    let project = world.get::<PaneProject>(valid[0]).map(|p| p.0);
    let z = next_pane_z(world);

    let SpawnedPane { entity: dock, .. } = spawn_pane(
        world,
        DOCK_KIND,
        "Dock",
        PaneRect { pos: frame_pos, size: frame_size, z },
        project,
    );
    world.entity_mut(dock).insert(Dock {
        root: build_template(&valid, template),
        collapse_chrome: true,
        template: false,
    });
    for &m in &valid {
        world.entity_mut(m).insert(DockMember { dock });
    }
    Some(dock)
}

/// Default number of slots a `template` skeleton ships with.
fn default_slots(t: DockTemplate) -> usize {
    match t {
        DockTemplate::Columns | DockTemplate::Rows | DockTemplate::Sidebar => 2,
        DockTemplate::MainBottom | DockTemplate::ColumnsBottom => 3,
        DockTemplate::Grid => 4,
    }
}

/// Spawn an EMPTY template skeleton (a dock of empty slots you populate
/// by dragging panes in). `slots` defaults to the template's natural
/// count. Returns the dock entity.
pub fn create_template_skeleton(
    world: &mut World,
    template: DockTemplate,
    slots: Option<usize>,
    project_id: Option<u64>,
    pos: Vec2,
    size: Vec2,
) -> Entity {
    let n = slots.unwrap_or_else(|| default_slots(template)).max(1);
    let z = next_pane_z(world);
    let SpawnedPane { entity: dock, .. } = spawn_pane(
        world,
        DOCK_KIND,
        "Dock",
        PaneRect { pos, size, z },
        project_id,
    );
    world.entity_mut(dock).insert(Dock {
        root: build_skeleton(n, template),
        collapse_chrome: true,
        template: true,
    });
    dock
}

// ---------- Splitter ----------

#[allow(clippy::type_complexity)]
fn splitter_drag(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    viewport: Res<PaneViewport>,
    mode: Res<PaneMouseMode>,
    mut consumed: ResMut<crate::InputConsumed>,
    mut split: ResMut<SplitterDrag>,
    mut docks: Query<(Entity, &PaneRect, &mut Dock, Option<&Visibility>)>,
) {
    let Ok(window) = windows.single() else { return };
    let Some(cursor) = window.cursor_position() else { return };
    let cur = viewport.window_to_canvas(cursor);

    if buttons.just_released(MouseButton::Left) {
        split.dock = None;
    }

    if buttons.just_pressed(MouseButton::Left)
        && split.dock.is_none()
        && matches!(*mode, PaneMouseMode::Idle)
        && !consumed.0
    {
        let mut best: Option<(Entity, f32, SplitHandle)> = None; // (dock, z, handle)
        for (e, rect, dock, vis) in &docks {
            if matches!(vis, Some(Visibility::Hidden)) {
                continue;
            }
            let Some(root) = &dock.root else { continue };
            let (ip, is) = dock_inner(rect);
            let mut cells = Vec::new();
            let mut handles = Vec::new();
            walk_layout(root, ip, is, &[], &mut cells, &mut handles);
            for h in handles {
                let along = if h.horizontal { cur.x } else { cur.y };
                let within = cur.x >= h.node_pos.x
                    && cur.x <= h.node_pos.x + h.node_size.x
                    && cur.y >= h.node_pos.y
                    && cur.y <= h.node_pos.y + h.node_size.y;
                if within && (along - h.center).abs() <= SPLITTER_HIT {
                    if best.as_ref().map_or(true, |(_, z, _)| rect.z > *z) {
                        best = Some((e, rect.z, h));
                    }
                }
            }
        }
        if let Some((dock, _, h)) = best {
            split.dock = Some(dock);
            split.path = h.path;
            split.boundary = h.boundary;
            split.horizontal = h.horizontal;
            split.node_pos = h.node_pos;
            split.node_size = h.node_size;
            consumed.0 = true;
        }
    }

    let Some(dock_entity) = split.dock else { return };
    if !buttons.pressed(MouseButton::Left) {
        return;
    }
    let Ok((_, _, mut dock, _)) = docks.get_mut(dock_entity) else {
        split.dock = None;
        return;
    };
    let (Some(root), path, b) = (dock.root.as_mut(), split.path.clone(), split.boundary) else {
        return;
    };
    let Some(DockNode::Split { fracs, .. }) = node_at_path_mut(root, &path) else {
        return;
    };
    if b + 1 >= fracs.len() {
        return;
    }
    // Boundary follows the cursor as a fraction of the split node's extent.
    let (origin, extent, along) = if split.horizontal {
        (split.node_pos.x, split.node_size.x, cur.x)
    } else {
        (split.node_pos.y, split.node_size.y, cur.y)
    };
    let cursor_frac = ((along - origin) / extent.max(1.0)).clamp(0.0, 1.0);
    let left_cum: f32 = fracs.iter().take(b).copied().sum();
    let pair = fracs[b] + fracs[b + 1];
    let new_b = (cursor_frac - left_cum).clamp(MIN_FRAC, pair - MIN_FRAC);
    fracs[b] = new_b;
    fracs[b + 1] = pair - new_b;
    consumed.0 = true;
}

// ---------- Restore linking ----------

fn link_restored_docks(
    mut commands: Commands,
    pending: Query<(Entity, &PendingDockLink, &PaneKindMarker)>,
    trees: Query<&PendingDockTree>,
    mut docks: Query<&mut Dock>,
) {
    if pending.is_empty() {
        return;
    }
    let mut groups: HashMap<u64, (Option<Entity>, Vec<(usize, Entity)>)> = HashMap::new();
    for (e, link, kind) in &pending {
        let entry = groups.entry(link.group).or_default();
        if kind.0 == DOCK_KIND {
            entry.0 = Some(e);
        } else {
            entry.1.push((link.slot, e));
        }
    }
    for (_group, (dock, members)) in groups {
        let Some(dock) = dock else { continue };
        let slot_to_entity: HashMap<usize, Entity> = members.iter().map(|(s, e)| (*s, *e)).collect();
        // Rebuild the tree from the dock's parked JSON (slot indices →
        // entities). Fall back to even columns if the tree is missing.
        let root = trees
            .get(dock)
            .ok()
            .and_then(|t| tree_from_json(&t.0, &slot_to_entity))
            .map(collapse)
            .or_else(|| {
                let mut ms: Vec<(usize, Entity)> = members.clone();
                ms.sort_by_key(|(s, _)| *s);
                let ents: Vec<Entity> = ms.into_iter().map(|(_, e)| e).collect();
                build_template(&ents, DockTemplate::Columns)
            });
        if let Ok(mut d) = docks.get_mut(dock) {
            d.root = root;
        }
        for (_, m) in &members {
            commands.entity(*m).insert(DockMember { dock });
            commands.entity(*m).remove::<PendingDockLink>();
        }
        commands.entity(dock).remove::<PendingDockLink>();
        commands.entity(dock).remove::<PendingDockTree>();
    }
}

// ---------- Host-facing helpers ----------

/// Co-members of `pane`'s dock (excluding `pane`), if docked.
pub fn dock_co_members(world: &World, pane: Entity) -> Vec<Entity> {
    let Some(dm) = world.get::<DockMember>(pane) else {
        return Vec::new();
    };
    world
        .get::<Dock>(dm.dock)
        .map(|d| d.member_entities().into_iter().filter(|m| *m != pane).collect())
        .unwrap_or_default()
}
