//! Project Exposé — a macOS Mission-Control-style overview for panes.
//!
//! One keystroke (⌘⇧E) spreads every pane of the **active `(project,
//! canvas)` level** out into a tidy grid so you can see them all at once;
//! another (or Esc, or clicking a pane) glides them back to exactly where
//! they were. It moves the *real* panes, so what you see is live.
//!
//! Behaviour:
//!
//! - **Scale, don't resize.** A pane is shrunk with [`PaneVisualScale`] (a
//!   visual transform on the whole pane), NOT by changing
//!   [`PaneRect::size`] — so a terminal/editor/widget keeps its full-size
//!   layout and is drawn like a thumbnail, never re-wrapped.
//! - **Spatial placement.** Each pane goes to the grid cell nearest its
//!   real on-canvas position (top-left → top-left cell), collisions
//!   resolved to the nearest free cell. Deterministic: same canvas → same
//!   grid, independent of query iteration order.
//! - **Hover** lifts + highlights the pane under the cursor.
//! - **Click** focuses that pane (keyboard focus + brought to front) and
//!   dismisses. It does NOT move the canvas unless the pane would be
//!   entirely off-screen where it returns to.
//! - **Fluid animation.** Position + visual scale tween with an ease-out
//!   cubic (a light stagger gives a cascade), modeled on `canvas_pane`'s
//!   `GatherAnim`.
//!
//! While open the transient layout is never persisted (guard in
//! `projects::save_if_dirty`), canvas pan/zoom is locked (guard in
//! `canvas::handle_pan_zoom_input`), and pane mouse input is suppressed so
//! our own hover/click handler owns interaction.

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy::winit::{UpdateMode, WinitSettings};

use jim_pane::{
    FocusedPane, PaneCanvas, PaneCanvasRegion, PaneClosing, PaneInputSuppressed, PanePinned,
    PaneProject, PaneRect, PaneScreenAnchored, PaneSnapId, PaneTag, PaneViewport, PaneVisualScale,
};

use crate::actions::{self, Action, ActionRun, AppActionsExt, KeyChord};
use crate::canvas::CanvasView;
use crate::canvas_pane::CanvasNav;
use crate::projects::Projects;

/// Tween length for the spread / return, seconds.
const EXPOSE_DUR: f32 = 0.30;
/// Per-pane start offset (grid index × this) for a gentle cascade.
const STAGGER: f32 = 0.010;
/// Inner padding of the grid region, window pixels.
const REGION_PAD: f32 = 44.0;
/// Fraction of each cell left as gutter around its pane thumbnail.
const CELL_GUTTER: f32 = 0.14;
/// Frames to keep the render loop awake after the grid settles / closes.
const COOLDOWN_FRAMES: u32 = 40;
/// Extra scale applied to the hovered pane (a subtle lift).
const HOVER_LIFT: f32 = 1.07;
/// Hover ease time-constant (seconds); smaller = snappier.
const HOVER_TAU: f32 = 0.07;

/// Exposé mode state. Toggled via the `view.toggle_expose` action.
#[derive(Resource, Default)]
pub struct Expose {
    /// True while the grid is shown (and while it animates open).
    pub active: bool,
    /// Set by the action / IPC / Esc / click; consumed by [`drive_expose_toggle`].
    pub pending_toggle: bool,
    /// Keeps the loop Continuous for a few frames after the last change.
    pub continuous_cooldown: u32,
    /// A pane clicked in the grid, to focus once we've closed.
    pending_focus: Option<Entity>,
    /// The pane currently under the cursor (for the hover lift).
    hovered: Option<Entity>,
}

/// A spread pane's slot: where it returns to, and its settled grid scale
/// (the base the hover lift multiplies).
#[derive(Component)]
struct ExposeSlot {
    home: Vec2,
    base_scale: f32,
}

/// A pane mid-tween (spreading out or returning home). Position is in
/// canvas space; `scale` is the target [`PaneVisualScale`] value.
#[derive(Component)]
struct ExposeAnim {
    start_pos: Vec2,
    target_pos: Vec2,
    start_scale: f32,
    target_scale: f32,
    elapsed: f32,
    delay: f32,
}

fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// One pane going into the layout: entity, current rect, stable snap id.
type Item = (Entity, PaneRect, u64);

/// The pane under a window-pixel cursor, given each candidate's on-screen
/// thumbnail footprint (`pos + size * zoom * visual_scale`). Topmost wins.
fn pane_at_cursor<'a>(
    cursor: Vec2,
    viewport: &PaneViewport,
    it: impl Iterator<Item = (Entity, &'a PaneRect, f32)>,
) -> Option<Entity> {
    let mut best: Option<(Entity, f32)> = None;
    for (e, r, vscale) in it {
        let tl = viewport.canvas_to_window(r.pos);
        let size = r.size * viewport.zoom * vscale;
        let inside = cursor.x >= tl.x
            && cursor.x <= tl.x + size.x
            && cursor.y >= tl.y
            && cursor.y <= tl.y + size.y;
        if inside && best.map_or(true, |(_, z)| r.z > z) {
            best = Some((e, r.z));
        }
    }
    best.map(|(e, _)| e)
}

/// Lay the eligible panes out into a tidy grid, placing each at the cell
/// nearest its real on-canvas position (collisions → nearest free cell).
///
/// Returns `(entity, target_pos_canvas, visual_scale)` per pane. Pure and
/// deterministic: a fixed set of panes at fixed positions always yields
/// the same assignment, independent of query iteration order.
fn compute_expose_layout(
    items: &[Item],
    viewport: &PaneViewport,
    region: Option<&PaneCanvasRegion>,
    window: Option<&Window>,
) -> Vec<(Entity, Vec2, f32)> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }

    // Window-pixel bounds of the drawable canvas region (right of the
    // sidebar), inset by padding.
    let (mut wmin, mut wmax) = match region {
        Some(r) if r.active => (r.min, r.max),
        _ => {
            let (w, h) = window
                .map(|win| (win.width(), win.height()))
                .unwrap_or((1440.0, 900.0));
            (Vec2::ZERO, Vec2::new(w, h))
        }
    };
    wmin += Vec2::splat(REGION_PAD);
    wmax -= Vec2::splat(REGION_PAD);
    let region_px = (wmax - wmin).max(Vec2::splat(1.0));
    let aspect = (region_px.x / region_px.y).max(0.01);

    // Grid dimensions: ~sqrt(n), biased to the region's aspect.
    let cols = ((n as f32 * aspect).sqrt().round() as usize).clamp(1, n);
    let rows = n.div_ceil(cols);
    let cell = Vec2::new(region_px.x / cols as f32, region_px.y / rows as f32);

    // Each pane's current on-screen center, and the bounding box over them.
    let centers: Vec<Vec2> = items
        .iter()
        .map(|(_, r, _)| viewport.canvas_to_window(r.pos + r.size * 0.5))
        .collect();
    let mut mn = centers[0];
    let mut mx = centers[0];
    for c in &centers {
        mn = mn.min(*c);
        mx = mx.max(*c);
    }
    let span = (mx - mn).max(Vec2::splat(1.0));

    // The cell each pane *wants* — its normalized position mapped onto the
    // grid. A pane top-left on the canvas wants the top-left cell.
    let desired: Vec<(usize, usize)> = centers
        .iter()
        .map(|c| {
            let nx = ((c.x - mn.x) / span.x).clamp(0.0, 1.0);
            let ny = ((c.y - mn.y) / span.y).clamp(0.0, 1.0);
            let col = (nx * cols.saturating_sub(1) as f32).round() as usize;
            let row = (ny * rows.saturating_sub(1) as f32).round() as usize;
            (col.min(cols - 1), row.min(rows - 1))
        })
        .collect();

    // Assign in a deterministic order (by desired cell, then a total-order
    // tie-break) so collision resolution is reproducible.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        desired[a]
            .1
            .cmp(&desired[b].1)
            .then(desired[a].0.cmp(&desired[b].0))
            .then(items[a].2.cmp(&items[b].2))
            .then(items[a].0.index().cmp(&items[b].0.index()))
    });

    let mut occupied = vec![false; cols * rows];
    let at = |c: usize, r: usize| r * cols + c;
    let mut assigned: Vec<(usize, usize)> = vec![(0, 0); n];
    for &i in &order {
        let (dc, dr) = desired[i];
        let cell_i = if !occupied[at(dc, dr)] {
            (dc, dr)
        } else {
            // Nearest free cell by squared distance, then row, then col.
            let mut best: Option<(f32, usize, usize)> = None;
            for r in 0..rows {
                for c in 0..cols {
                    if occupied[at(c, r)] {
                        continue;
                    }
                    let d2 = (c as f32 - dc as f32).powi(2) + (r as f32 - dr as f32).powi(2);
                    let better = match best {
                        None => true,
                        Some((bd, br, bc)) => d2 < bd || (d2 == bd && (r, c) < (br, bc)),
                    };
                    if better {
                        best = Some((d2, r, c));
                    }
                }
            }
            let (_, r, c) = best.expect("cols*rows >= n, so a free cell always exists");
            (c, r)
        };
        occupied[at(cell_i.0, cell_i.1)] = true;
        assigned[i] = cell_i;
    }

    // Turn each assignment into a target position + visual scale.
    let inner = cell * (1.0 - CELL_GUTTER);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (c, r) = assigned[i];
        let cell_center = wmin + Vec2::new((c as f32 + 0.5) * cell.x, (r as f32 + 0.5) * cell.y);
        let (e, rect, _) = &items[i];
        let footprint_full = (rect.size * viewport.zoom).max(Vec2::splat(1.0));
        let s = (inner.x / footprint_full.x)
            .min(inner.y / footprint_full.y)
            .min(1.0);
        let displayed = footprint_full * s;
        let visual_tl_px = cell_center - displayed * 0.5;
        let target_pos = viewport.window_to_canvas(visual_tl_px);
        out.push((*e, target_pos, s));
    }
    out
}

/// Consume a pending toggle: spread the panes out, or bring them home.
#[allow(clippy::too_many_arguments)]
fn drive_expose_toggle(
    mut commands: Commands,
    mut expose: ResMut<Expose>,
    projects: Res<Projects>,
    nav: Res<CanvasNav>,
    viewport: Res<PaneViewport>,
    region: Option<Res<PaneCanvasRegion>>,
    windows: Query<&Window>,
    mut settings: ResMut<WinitSettings>,
    mut focused: ResMut<FocusedPane>,
    mut suppressed: ResMut<PaneInputSuppressed>,
    mut view: ResMut<CanvasView>,
    // Open reads p0 (candidate panes); close mutates p1 (spread panes).
    // A spread pane matches both, so they must share access via a ParamSet.
    mut panes_set: ParamSet<(
        Query<
            (Entity, &PaneRect, &PaneProject, Option<&PaneCanvas>, Option<&PaneSnapId>),
            (With<PaneTag>, Without<PanePinned>, Without<PaneScreenAnchored>, Without<PaneClosing>),
        >,
        Query<(Entity, &mut PaneRect, &PaneVisualScale, &ExposeSlot)>,
    )>,
) {
    if !expose.pending_toggle {
        return;
    }
    expose.pending_toggle = false;

    let Some(active_project) = projects.active else {
        return;
    };
    let active_level = nav.level(active_project);

    if !expose.active {
        // ---- OPEN ----
        let items: Vec<Item> = panes_set
            .p0()
            .iter()
            .filter(|(_, _, proj, canvas, _)| {
                proj.0 == active_project && canvas.map_or(0, |c| c.0) == active_level
            })
            .map(|(e, r, _, _, snap)| (e, *r, snap.map_or(0, |s| s.0)))
            .collect();
        if items.is_empty() {
            return;
        }
        let targets =
            compute_expose_layout(&items, &viewport, region.as_deref(), windows.single().ok());
        for (idx, (e, target_pos, target_scale)) in targets.into_iter().enumerate() {
            let Some((_, r, _)) = items.iter().find(|(pe, ..)| *pe == e) else { continue };
            let start_pos = r.pos;
            commands.entity(e).insert((
                ExposeSlot { home: start_pos, base_scale: target_scale },
                PaneVisualScale(1.0),
                ExposeAnim {
                    start_pos,
                    target_pos,
                    start_scale: 1.0,
                    target_scale,
                    elapsed: 0.0,
                    delay: idx as f32 * STAGGER,
                },
            ));
        }
        expose.active = true;
        expose.hovered = None;
        // Paint immediately so the first tween frame isn't laggy, and take
        // pane input so hover/click drive the grid, not drag/focus.
        settings.focused_mode = UpdateMode::Continuous;
        suppressed.0 = true;
    } else {
        // ---- CLOSE ----
        let focus_target = expose.pending_focus.take();
        // Bring the selected pane to the front of the z-stack (what a
        // normal focusing click does) so it returns home on top.
        let mut returning = panes_set.p1();
        let max_z = returning.iter().map(|(_, r, _, _)| r.z).fold(0.0_f32, f32::max);
        for (e, mut rect, vscale, slot) in returning.iter_mut() {
            if focus_target == Some(e) && rect.z < max_z {
                rect.z = max_z + 1.0;
            }
            commands.entity(e).insert(ExposeAnim {
                start_pos: rect.pos,
                target_pos: slot.home,
                start_scale: vscale.0,
                target_scale: 1.0,
                elapsed: 0.0,
                delay: 0.0,
            });
        }
        expose.active = false;
        expose.hovered = None;
        expose.continuous_cooldown = COOLDOWN_FRAMES;
        suppressed.0 = false;

        // Focus the clicked pane. Only move the canvas if the pane would be
        // ENTIRELY off-screen where it returns to — any visible sliver
        // leaves the view exactly as it was. Never rescale/reset zoom.
        if let Some(target) = focus_target {
            focused.0 = Some(target);
            if let Ok((_, rect, _, slot)) = returning.get(target) {
                let (rmin, rmax) = match region.as_deref() {
                    Some(r) if r.active => (r.min, r.max),
                    _ => (
                        Vec2::ZERO,
                        windows
                            .single()
                            .map(|w| Vec2::new(w.width(), w.height()))
                            .unwrap_or(Vec2::new(1440.0, 900.0)),
                    ),
                };
                let home_tl = viewport.canvas_to_window(slot.home);
                let home_br = viewport.canvas_to_window(slot.home + rect.size);
                let any_overlap = home_tl.x < rmax.x
                    && home_br.x > rmin.x
                    && home_tl.y < rmax.y
                    && home_br.y > rmin.y;
                if !any_overlap {
                    let st = view.state_mut((active_project, active_level));
                    st.pan = slot.home - Vec2::splat(24.0);
                    st.clamp_pan();
                }
            }
        }
    }
}

/// Advance every in-flight pane tween (position + visual scale).
fn animate_expose(
    mut commands: Commands,
    time: Res<Time>,
    expose: Res<Expose>,
    mut q: Query<(Entity, &mut PaneRect, &mut PaneVisualScale, &mut ExposeAnim)>,
) {
    // Clamp dt: the toggle can fire right after the reactive loop idled, so
    // the first frame's delta could be seconds — which would snap the tween
    // to its end and skip the animation. (Same guard the cube uses.)
    let dt = time.delta_secs().min(1.0 / 30.0);
    for (e, mut rect, mut vscale, mut anim) in &mut q {
        anim.elapsed += dt;
        let local = (anim.elapsed - anim.delay).max(0.0);
        let t = (local / EXPOSE_DUR).clamp(0.0, 1.0);
        let k = ease_out_cubic(t);
        // lerp(a, b, 1.0) == b exactly, so a completed close lands the pane
        // on its precise original pos and scale 1.0 — nothing persisted drifts.
        let pos = anim.start_pos.lerp(anim.target_pos, k);
        if rect.pos != pos {
            rect.pos = pos;
        }
        let sc = anim.start_scale + (anim.target_scale - anim.start_scale) * k;
        if vscale.0 != sc {
            vscale.0 = sc;
        }
        if t >= 1.0 {
            commands.entity(e).remove::<ExposeAnim>();
            // Closing (grid no longer active): pane is home — drop the slot
            // and visual scale so it's a normal pane again.
            if !expose.active {
                commands
                    .entity(e)
                    .remove::<ExposeSlot>()
                    .remove::<PaneVisualScale>();
            }
        }
    }
}

/// While the grid is open, pick the pane under the cursor and ease every
/// settled pane's scale toward its base (or base × lift when hovered).
fn expose_hover(
    time: Res<Time>,
    mut expose: ResMut<Expose>,
    mut focused: ResMut<FocusedPane>,
    windows: Query<&Window>,
    viewport: Res<PaneViewport>,
    // Hit-test against each slot's stable base scale (not the live
    // `PaneVisualScale`), so this read-only query doesn't collide with the
    // mutable `settled` query below over `PaneVisualScale`.
    hit: Query<(Entity, &PaneRect, &ExposeSlot)>,
    // Only lift panes that have settled (intro tween done); animating panes
    // are driven by `animate_expose`.
    mut settled: Query<(Entity, &mut PaneVisualScale, &ExposeSlot), Without<ExposeAnim>>,
) {
    if !expose.active {
        return;
    }
    // Which pane is under the cursor?
    let hovered = windows
        .single()
        .ok()
        .and_then(|w| w.cursor_position())
        .and_then(|cursor| {
            pane_at_cursor(
                cursor,
                &viewport,
                hit.iter().map(|(e, r, slot)| (e, r, slot.base_scale)),
            )
        });
    if expose.hovered != hovered {
        expose.hovered = hovered;
    }
    // Give the hovered pane the focus outline as an extra hover cue.
    if let Some(h) = hovered {
        if focused.0 != Some(h) {
            focused.0 = Some(h);
        }
    }

    let dt = time.delta_secs().min(1.0 / 30.0);
    let a = (dt / HOVER_TAU).min(1.0);
    for (e, mut vscale, slot) in &mut settled {
        let want = if expose.hovered == Some(e) {
            slot.base_scale * HOVER_LIFT
        } else {
            slot.base_scale
        };
        let s = vscale.0 + (want - vscale.0) * a;
        // Snap when close enough so we stop marking Changed every frame.
        let s = if (s - want).abs() < 0.001 { want } else { s };
        if vscale.0 != s {
            vscale.0 = s;
        }
    }
}

/// While the grid is open: Esc closes it; a left-click selects the pane
/// under the cursor (closes + focuses it).
fn expose_input(
    mut expose: ResMut<Expose>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut keys: MessageReader<KeyboardInput>,
    windows: Query<&Window>,
    viewport: Res<PaneViewport>,
    panes: Query<(Entity, &PaneRect, &PaneVisualScale), With<ExposeSlot>>,
) {
    if !expose.active {
        keys.clear();
        return;
    }

    for ev in keys.read() {
        if ev.state.is_pressed() && ev.key_code == KeyCode::Escape {
            expose.pending_toggle = true;
        }
    }

    if buttons.just_pressed(MouseButton::Left) {
        let Ok(win) = windows.single() else { return };
        let Some(cursor) = win.cursor_position() else { return };
        if let Some(e) =
            pane_at_cursor(cursor, &viewport, panes.iter().map(|(e, r, v)| (e, r, v.0)))
        {
            expose.pending_focus = Some(e);
            expose.pending_toggle = true;
        }
    }
}

/// Tick down the post-settle cooldown (runs every frame it's positive,
/// since the loop stays Continuous while it is).
fn tick_expose_cooldown(mut expose: ResMut<Expose>) {
    if expose.continuous_cooldown > 0 {
        expose.continuous_cooldown -= 1;
    }
}

/// `view.toggle_expose` action handler — just flips the pending toggle.
fn action_toggle_expose(ctx: &mut actions::ActionCtx) {
    ctx.world.resource_mut::<Expose>().pending_toggle = true;
}

pub struct ExposePlugin;

impl Plugin for ExposePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Expose>()
            .add_action(Action {
                id: "view.toggle_expose",
                title: "Toggle Exposé",
                category: "View",
                keywords: &[
                    "expose", "exposé", "overview", "windows", "spread", "grid",
                    "mission control", "tile", "arrange",
                ],
                radial_icon: Some("▦"),
                default_keys: const { &[KeyChord::cmd_shift(KeyCode::KeyE)] },
                run: ActionRun::Custom(action_toggle_expose),
            })
            .add_systems(
                Update,
                (
                    expose_input,
                    drive_expose_toggle,
                    animate_expose,
                    expose_hover,
                    tick_expose_cooldown,
                )
                    .chain(),
            );
    }
}
