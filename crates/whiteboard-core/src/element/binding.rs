//! Arrow-to-shape binding: the model/geometry, not the UI wiring.
//!
//! Reimplemented from Excalidraw's `packages/element/src/binding.ts`
//! (`bindingBorderTest` / `maxBindingGap`, `determineFocusDistance` /
//! `determineFocusPoint`, `calculateFocusAndGap`, `updateBoundPoint`). We do not
//! vendor any JavaScript; this is a Rust reimplementation that keeps the same
//! field meaning for [`PointBinding`] (`focus` in `-1..=1`, `gap >= 0`).
//!
//! # Model (Excalidraw perpendicular-offset focus)
//!
//! This is a faithful port of Excalidraw's binding (validated against a
//! differential oracle running the real `@excalidraw/element` — see
//! `oracle/` and `tests/oracle_parity.rs`):
//!
//! - `focus ∈ -1..=1` is a **perpendicular offset** of the binding aim relative
//!   to the target center, *not* an angle. `focus == 0` means the arrow aims
//!   straight **through the center**; `±1` reaches a corner/edge. Crucially this
//!   is resolved against the arrow's **adjacent vertex** (the neighbouring point),
//!   which is what lets a `focus == 0` arrow keep pointing at the center as the
//!   target moves — see [`determine_focus_distance`] / [`determine_focus_point`].
//! - `gap` is the clearance from the endpoint to the target outline, floored at 1
//!   and capped by [`max_binding_gap`].
//! - The attached endpoint ([`binding_endpoint`]) is where the ray from the
//!   adjacent vertex through the focus point crosses the target outline **expanded
//!   outward by `gap`** (the Minkowski sum with a disk of radius `gap`: edges
//!   pushed out plus a `gap`-radius arc at each corner), taking the crossing
//!   nearest the adjacent vertex. Port of `updateBoundPoint`.
//!
//! Edge models: rectangles/diamonds use their polygon outline; ellipses use the
//! inscribed ellipse. Rotation is honoured (the segment is resolved in the
//! target's frame). The corner expansion uses exact circular arcs where upstream
//! uses offset Bézier corners, so sharp-corner endpoints can differ sub-pixel.

use crate::geometry::{
    distance_to_outline, element_bounds, point_rotate_rads, segment_intersection, Point, Rect, Vec2,
};
use crate::scene::Scene;

use super::{Element, ElementId, ElementKind, LinearData, PointBinding};

/// Excalidraw `FIXED_BINDING_DISTANCE`.
pub const FIXED_BINDING_DISTANCE: f64 = 5.0;
/// Excalidraw `BINDING_HIGHLIGHT_THICKNESS`.
pub const BINDING_HIGHLIGHT_THICKNESS: f64 = 10.0;

/// Excalidraw `maxBindingGap` (zoom fixed at 1): the largest gap a binding will
/// store, scaled to the target's size. Diamonds use a `1/√2` ratio.
pub fn max_binding_gap(element: &Element) -> f64 {
    let (w, h) = (element.width, element.height);
    let shape_ratio = if matches!(element.kind, ElementKind::Diamond) {
        1.0 / 2.0_f64.sqrt()
    } else {
        1.0
    };
    let smaller = shape_ratio * w.min(h);
    16.0_f64
        .max((0.25 * smaller).min(32.0))
        .max(BINDING_HIGHLIGHT_THICKNESS + FIXED_BINDING_DISTANCE)
}

/// Excalidraw binding `gap` for an endpoint at `edge`: the clearance from the
/// bound endpoint to the target outline, floored at 1 and capped at
/// [`max_binding_gap`]. (`distanceToElement` reduces to distance-to-outline for
/// the non-rounded rectangles/diamonds we model.)
pub fn binding_gap(element: &Element, edge: Point) -> f64 {
    let raw = 1.0_f64.max(distance_to_outline(element, edge));
    raw.min(max_binding_gap(element))
}

/// The modelled outline shape of a bindable target.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EdgeModel {
    /// Axis-aligned rectangle (rectangles, diamonds, text, images, frames).
    Rect,
    /// Inscribed ellipse of the bounding rect.
    Ellipse,
}

impl EdgeModel {
    fn of(element: &Element) -> EdgeModel {
        match element.kind {
            ElementKind::Ellipse => EdgeModel::Ellipse,
            _ => EdgeModel::Rect,
        }
    }
}

/// Whether an element can be an arrow-binding target.
///
/// Mirrors upstream `isBindableElement`: any non-linear, non-deleted shape with a
/// fillable/closed silhouette. Linear elements (lines/arrows/freedraw), text and
/// selection marquees are excluded.
pub fn is_bindable_element(element: &Element) -> bool {
    if element.is_deleted {
        return false;
    }
    matches!(
        element.kind,
        ElementKind::Rectangle
            | ElementKind::Ellipse
            | ElementKind::Diamond
            | ElementKind::Image(_)
            | ElementKind::Frame(_)
    )
}

/// Excalidraw `determineFocusDistance`: the signed, normalized perpendicular
/// offset of an arrow's binding ray relative to the target's center.
///
/// `focus == 0` means the ray (from `adjacent` through the bound `edge` point)
/// aims straight through the center; `±1` reaches the modelled corner/edge. This
/// is the offset Excalidraw stores in [`PointBinding::focus`]; it is what lets an
/// arrow keep aiming at the *center* of a shape rather than snapping to an edge.
///
/// Faithful port: the interceptor ray is intersected with the target's two
/// extended diagonals (axis-aligned cross for diamonds) and the nearer crossing's
/// center-distance is normalized by the half-diagonal. Both `adjacent` and `edge`
/// are global scene points.
pub fn determine_focus_distance(element: &Element, adjacent: Point, edge: Point) -> f64 {
    let center = element.center();
    if adjacent == edge {
        return 0.0;
    }
    let angle = element.angle;
    let rotated_a = point_rotate_rads(adjacent, center, -angle);
    let rotated_b = point_rotate_rads(edge, center, -angle);

    // sign = -sign( (rotated_b - adjacent) × (rotated_b - center) )
    let v_ba = Vec2::new(rotated_b.x - adjacent.x, rotated_b.y - adjacent.y);
    let v_bc = Vec2::new(rotated_b.x - center.x, rotated_b.y - center.y);
    let sign = -(v_ba.cross(v_bc)).signum();

    let (x, y, w, h) = (element.x, element.y, element.width, element.height);
    let reach = (w * 2.0).max(h * 2.0);
    let dir = Vec2::new(rotated_b.x - rotated_a.x, rotated_b.y - rotated_a.y).normalized();
    let interceptor_end = Point::new(rotated_b.x + dir.x * reach, rotated_b.y + dir.y * reach);

    let is_diamond = matches!(element.kind, ElementKind::Diamond);
    let interceptees: [(Point, Point); 2] = if is_diamond {
        [
            (Point::new(x + w / 2.0, y - h), Point::new(x + w / 2.0, y + h * 2.0)),
            (Point::new(x - w, y + h / 2.0), Point::new(x + w * 2.0, y + h / 2.0)),
        ]
    } else {
        [
            (Point::new(x - w, y - h), Point::new(x + w * 2.0, y + h * 2.0)),
            (Point::new(x + w * 2.0, y - h), Point::new(x - w, y + h * 2.0)),
        ]
    };
    let axes: [(Point, Point); 2] = if is_diamond {
        [
            (Point::new(x + w / 2.0, y), Point::new(x + w / 2.0, y + h)),
            (Point::new(x, y + h / 2.0), Point::new(x + w, y + h / 2.0)),
        ]
    } else {
        [
            (Point::new(x, y), Point::new(x + w, y + h)),
            (Point::new(x + w, y), Point::new(x, y + h)),
        ]
    };

    let mut hits: Vec<Point> = Vec::new();
    for ic in &interceptees {
        if let Some(p) = segment_intersection(rotated_b, interceptor_end, ic.0, ic.1) {
            hits.push(p);
        }
    }
    // Nearest crossing to the edge point first (Excalidraw orders by distance to
    // `b`, then pairs index-wise with the axes for the diamond denominator).
    hits.sort_by(|g, h2| {
        edge.distance_sq(*g)
            .partial_cmp(&edge.distance_sq(*h2))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut ratios: Vec<f64> = hits
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            let denom = if is_diamond {
                axes[idx].0.distance(axes[idx].1) / 2.0
            } else {
                (w * w + h * h).sqrt() / 2.0
            };
            sign * center.distance(*p) / denom
        })
        .collect();
    ratios.sort_by(|g, h2| {
        g.abs()
            .partial_cmp(&h2.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ratios.first().copied().unwrap_or(0.0)
}

/// The topmost bindable element whose modelled edge/area is within `tolerance`
/// scene units of `point`.
///
/// Port of upstream `getHoveredElementForBinding` + `bindingBorderTest`: a point
/// inside the shape, or within the binding gap of its border, binds. We iterate
/// in reverse paint order so the visually-topmost candidate wins.
pub fn bindable_element_at(scene: &Scene, point: Point, tolerance: f64) -> Option<ElementId> {
    // `iter_live` yields in paint order (bottom first) but is not double-ended,
    // so we keep the last matching candidate to land on the topmost one.
    scene
        .iter_live()
        .filter(|e| is_bindable_element(e))
        .filter(|e| within_binding_distance(e, point, tolerance))
        .map(|e| e.id.clone())
        .last()
}

/// Whether `point` is inside the modelled target or within `tolerance` of its
/// edge. `tolerance` is the binding gap the interaction layer supplies.
fn within_binding_distance(element: &Element, point: Point, tolerance: f64) -> bool {
    let rect = element_bounds(element);
    match EdgeModel::of(element) {
        EdgeModel::Rect => signed_rect_distance(rect, point) <= tolerance,
        EdgeModel::Ellipse => signed_ellipse_distance(rect, point) <= tolerance,
    }
}

/// Compute the binding for an arrow endpoint aimed at `target`.
///
/// `adjacent` is the arrow's neighbouring vertex (the *other* end of a straight
/// arrow) — Excalidraw needs it to encode `focus` as a perpendicular offset, and
/// it is what lets the endpoint keep aiming through the target as the target
/// moves. `focus == 0` means "aim at the center"; `gap` is the clearance from the
/// endpoint to the target outline. Port of `calculateFocusAndGap`.
pub fn compute_binding(arrow_endpoint: Point, adjacent: Point, target: &Element) -> PointBinding {
    // Released *inside* the shape → anchor to that fixed interior point (so the
    // arrow can live at the centre / a specific spot and move with the shape).
    // Released near the edge / outside → normal edge binding (focus + gap). This
    // is the "snap to an edge OR snap to a point inside" the user asked for.
    if let Some(fixed) = interior_fixed_point(target, arrow_endpoint) {
        return PointBinding {
            element_id: target.id.clone(),
            focus: 0.0,
            gap: 0.0,
            fixed_point: Some(fixed),
        };
    }
    PointBinding {
        element_id: target.id.clone(),
        focus: determine_focus_distance(target, adjacent, arrow_endpoint),
        gap: binding_gap(target, arrow_endpoint),
        fixed_point: None,
    }
}

/// If `point` is inside `target` (with a small margin so a release right on the
/// edge still snaps to the edge), return its position normalized to the target's
/// bounding box in the *unrotated* frame — `(0.5, 0.5)` is the centre. Otherwise
/// `None`.
fn interior_fixed_point(target: &Element, point: Point) -> Option<[f64; 2]> {
    if target.width <= 0.0 || target.height <= 0.0 {
        return None;
    }
    let center = target.center();
    let local = point_rotate_rads(point, center, -target.angle);
    // Margin in from the edge before a release counts as "interior".
    let margin = (target.width.min(target.height) * 0.12).clamp(2.0, 14.0);
    let inside_x = local.x >= target.x + margin && local.x <= target.x + target.width - margin;
    let inside_y = local.y >= target.y + margin && local.y <= target.y + target.height - margin;
    if inside_x && inside_y {
        Some([
            (local.x - target.x) / target.width,
            (local.y - target.y) / target.height,
        ])
    } else {
        None
    }
}

/// The scene-space position of a fixed interior binding point on `target`.
pub fn fixed_bound_point(target: &Element, fixed: [f64; 2]) -> Point {
    let center = target.center();
    let local = Point::new(
        target.x + fixed[0] * target.width,
        target.y + fixed[1] * target.height,
    );
    point_rotate_rads(local, center, target.angle)
}

/// Excalidraw `determineFocusPoint`: the scene-space point an arrow bound with
/// `focus` aims *through*. `focus == 0` → the target center; otherwise one of the
/// four corners (rect) / edge-midpoints (diamond) scaled toward the center by
/// `|focus|`, picked by which angular sector `adjacent` falls in. `adjacent` is
/// the arrow's neighbouring vertex.
pub fn determine_focus_point(element: &Element, focus: f64, adjacent: Point) -> Point {
    let center = element.center();
    if focus == 0.0 {
        return center;
    }
    let (x, y, w, h) = (element.x, element.y, element.width, element.height);
    let base = if matches!(element.kind, ElementKind::Diamond) {
        [
            Point::new(x, y + h / 2.0),
            Point::new(x + w / 2.0, y),
            Point::new(x + w, y + h / 2.0),
            Point::new(x + w / 2.0, y + h),
        ]
    } else {
        [
            Point::new(x, y),
            Point::new(x + w, y),
            Point::new(x + w, y + h),
            Point::new(x, y + h),
        ]
    };
    // center + (corner - center) * |focus|, then rotate into the element's frame.
    let f = focus.abs();
    let r: Vec<Point> = base
        .iter()
        .map(|c| {
            let p = Point::new(center.x + (c.x - center.x) * f, center.y + (c.y - center.y) * f);
            point_rotate_rads(p, center, element.angle)
        })
        .collect();

    let h = |p: Point, q: Point| Vec2::new(p.x - q.x, p.y - q.y);
    let n = |a: Vec2, b: Vec2| a.cross(b);
    let pos = focus > 0.0;
    let sector = |i: usize, j: usize, k: usize| {
        n(h(adjacent, r[i]), h(r[j], r[i])) > 0.0 && n(h(adjacent, r[j]), h(r[k], r[j])) < 0.0
    };
    let sector_neg = |i: usize, j: usize, k: usize| {
        n(h(adjacent, r[i]), h(r[j], r[i])) > 0.0 && n(h(adjacent, r[k]), h(r[i], r[k])) < 0.0
    };
    let s = if pos {
        [
            sector(0, 1, 2),
            sector(1, 2, 3),
            sector(2, 3, 0),
            sector(3, 0, 1),
        ]
    } else {
        [
            sector_neg(0, 1, 3),
            sector_neg(1, 2, 0),
            sector_neg(2, 3, 1),
            sector_neg(3, 0, 2),
        ]
    };
    if s[0] {
        if pos { r[1] } else { r[0] }
    } else if s[1] {
        if pos { r[2] } else { r[1] }
    } else if s[2] {
        if pos { r[3] } else { r[2] }
    } else if pos {
        r[0]
    } else {
        r[3]
    }
}

/// The scene-space point an arrow endpoint should sit at for the given binding:
/// where the ray from `adjacent` through the focus point crosses the target's
/// outline expanded outward by `gap`, taking the crossing nearest `adjacent`.
/// Port of `updateBoundPoint` (non-elbow path).
pub fn binding_endpoint(target: &Element, focus: f64, gap: f64, adjacent: Point) -> Point {
    let focus_pt = determine_focus_point(target, focus, adjacent);
    let gap = gap.max(0.0);
    if gap == 0.0 {
        return focus_pt;
    }
    let dir = Vec2::new(focus_pt.x - adjacent.x, focus_pt.y - adjacent.y);
    if dir.length_sq() == 0.0 {
        return focus_pt;
    }
    let dir = dir.normalized();

    // When the adjacent vertex is close to (or inside) the target — e.g. two
    // bound shapes sitting near each other — a full `gap` would push the expanded
    // outline *past* the adjacent vertex, so the endpoint either overshoots to the
    // far side or the two endpoints cross over. Clamp the gap to the adjacent's
    // clearance from the outline so the endpoint always stays on the adjacent's
    // side. (For shapes far apart this clamp is a no-op, so it leaves the common
    // case — and the oracle fixtures — untouched.)
    let clearance = distance_to_outline(target, adjacent);
    let gap = gap.min(clearance);

    // Scan the *whole* line through the adjacent vertex (both directions), not
    // just the forward ray, then take the crossing nearest the adjacent vertex —
    // i.e. the expanded edge facing the rest of the arrow. Scanning only forward
    // would miss that near edge when the adjacent vertex lies inside the expanded
    // outline, landing the endpoint on the far side instead.
    let reach = adjacent.distance(focus_pt) + target.width.max(target.height) * 2.0 + gap * 2.0;
    let ahead = Point::new(adjacent.x + dir.x * reach, adjacent.y + dir.y * reach);
    let behind = Point::new(adjacent.x - dir.x * reach, adjacent.y - dir.y * reach);
    let mut hits = intersect_element_with_line(target, behind, ahead, gap);
    hits.sort_by(|a, b| {
        adjacent
            .distance_sq(*a)
            .partial_cmp(&adjacent.distance_sq(*b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.first().copied().unwrap_or(focus_pt)
}

/// Intersections of segment `a..b` with `target`'s outline expanded outward by
/// `gap` (the Minkowski sum with a disk of radius `gap`: edges pushed out plus a
/// circular arc of radius `gap` at each corner). Port of
/// `intersectElementWithLineSegment` for the bindable shapes.
pub fn intersect_element_with_line(target: &Element, a: Point, b: Point, gap: f64) -> Vec<Point> {
    match target.kind {
        ElementKind::Ellipse => ellipse_line_intersections(target, a, b, gap),
        ElementKind::Diamond => {
            let (x, y, w, h) = (target.x, target.y, target.width, target.height);
            let verts = [
                Point::new(x + w / 2.0, y),
                Point::new(x + w, y + h / 2.0),
                Point::new(x + w / 2.0, y + h),
                Point::new(x, y + h / 2.0),
            ];
            expanded_polygon_intersections(target, &verts, a, b, gap)
        }
        _ => {
            let (x, y, w, h) = (target.x, target.y, target.width, target.height);
            let verts = [
                Point::new(x, y),
                Point::new(x + w, y),
                Point::new(x + w, y + h),
                Point::new(x, y + h),
            ];
            expanded_polygon_intersections(target, &verts, a, b, gap)
        }
    }
}

/// Intersections of segment `a..b` with the convex polygon `verts` (axis-aligned,
/// in the element's *upright* frame) expanded outward by `gap`: each edge pushed
/// out by `gap` along its outward normal, plus a `gap`-radius arc at each vertex.
/// `verts` are rotated by the element's angle around its center first, so the
/// result is in scene space.
fn expanded_polygon_intersections(
    element: &Element,
    verts: &[Point],
    a: Point,
    b: Point,
    gap: f64,
) -> Vec<Point> {
    let center = element.center();
    let angle = element.angle;
    let v: Vec<Point> = verts
        .iter()
        .map(|p| point_rotate_rads(*p, center, angle))
        .collect();
    let nverts = v.len();
    let mut out: Vec<Point> = Vec::new();

    for i in 0..nverts {
        let vi = v[i];
        let vj = v[(i + 1) % nverts];
        let edge = Vec2::new(vj.x - vi.x, vj.y - vi.y);
        // Outward normal: perpendicular pointing away from the center.
        let mut nrm = Vec2::new(edge.y, -edge.x).normalized();
        let mid = Point::new((vi.x + vj.x) * 0.5, (vi.y + vj.y) * 0.5);
        if nrm.x * (mid.x - center.x) + nrm.y * (mid.y - center.y) < 0.0 {
            nrm = Vec2::new(-nrm.x, -nrm.y);
        }
        let o1 = Point::new(vi.x + nrm.x * gap, vi.y + nrm.y * gap);
        let o2 = Point::new(vj.x + nrm.x * gap, vj.y + nrm.y * gap);
        if let Some(p) = segment_intersection(a, b, o1, o2) {
            out.push(p);
        }
        // Corner arc at vi: circle of radius gap; keep crossings that actually
        // land on the expanded outline (nearest outline feature is the vertex).
        for p in circle_segment_intersections(vi, gap, a, b) {
            if (distance_to_outline(element, p) - gap).abs() < 1e-3 {
                out.push(p);
            }
        }
    }

    dedup_points(out)
}

/// Intersections of segment `a..b` with `element`'s inscribed ellipse grown by
/// `gap` on each radius. Works in the element's upright frame (un-rotate the
/// segment, solve against the axis-aligned ellipse, rotate results back).
fn ellipse_line_intersections(element: &Element, a: Point, b: Point, gap: f64) -> Vec<Point> {
    let center = element.center();
    let rx = element.width / 2.0 + gap;
    let ry = element.height / 2.0 + gap;
    if rx <= 0.0 || ry <= 0.0 {
        return Vec::new();
    }
    let la = point_rotate_rads(a, center, -element.angle);
    let lb = point_rotate_rads(b, center, -element.angle);
    // Normalize to the unit circle: u = ((p - center) / (rx, ry)).
    let to_unit = |p: Point| Point::new((p.x - center.x) / rx, (p.y - center.y) / ry);
    let ua = to_unit(la);
    let ub = to_unit(lb);
    let d = Vec2::new(ub.x - ua.x, ub.y - ua.y);
    let aa = d.x * d.x + d.y * d.y;
    if aa == 0.0 {
        return Vec::new();
    }
    let bb = 2.0 * (ua.x * d.x + ua.y * d.y);
    let cc = ua.x * ua.x + ua.y * ua.y - 1.0;
    let disc = bb * bb - 4.0 * aa * cc;
    if disc < 0.0 {
        return Vec::new();
    }
    let sq = disc.sqrt();
    let mut out = Vec::new();
    for t in [(-bb - sq) / (2.0 * aa), (-bb + sq) / (2.0 * aa)] {
        if (-1e-9..=1.0 + 1e-9).contains(&t) {
            let ux = ua.x + d.x * t;
            let uy = ua.y + d.y * t;
            // Back to scene space.
            let local = Point::new(center.x + ux * rx, center.y + uy * ry);
            out.push(point_rotate_rads(local, center, element.angle));
        }
    }
    out
}

/// Intersections of segment `a..b` with a circle (`center`, `radius`).
fn circle_segment_intersections(center: Point, radius: f64, a: Point, b: Point) -> Vec<Point> {
    let d = Vec2::new(b.x - a.x, b.y - a.y);
    let aa = d.x * d.x + d.y * d.y;
    if aa == 0.0 {
        return Vec::new();
    }
    let fx = a.x - center.x;
    let fy = a.y - center.y;
    let bb = 2.0 * (fx * d.x + fy * d.y);
    let cc = fx * fx + fy * fy - radius * radius;
    let disc = bb * bb - 4.0 * aa * cc;
    if disc < 0.0 {
        return Vec::new();
    }
    let sq = disc.sqrt();
    let mut out = Vec::new();
    for t in [(-bb - sq) / (2.0 * aa), (-bb + sq) / (2.0 * aa)] {
        if (-1e-9..=1.0 + 1e-9).contains(&t) {
            out.push(Point::new(a.x + d.x * t, a.y + d.y * t));
        }
    }
    out
}

/// Drop points that coincide with an earlier one (corner arc / offset edge can
/// produce the same tangent point twice).
fn dedup_points(pts: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::new();
    for p in pts {
        if !out.iter().any(|q| q.distance_sq(p) < 1e-6) {
            out.push(p);
        }
    }
    out
}

/// New endpoints for a bound arrow after its target(s) moved.
///
/// Returns `(start, end)` in scene space. `None` for an endpoint means that end
/// is unbound and the caller should leave it untouched. This function does **not**
/// mutate the scene (the arrow and its targets are borrowed immutably); the caller
/// applies the returned points, preserving the arrow's intermediate points and
/// only moving the bound ends.
///
/// Port of the relevant half of upstream `updateBoundPoint` /
/// `bindOrUnbindLinearElement`: for each bound end we read the *current* target
/// bounds and recompute the endpoint from the stored `focus`/`gap`.
pub fn update_bound_arrow(scene: &Scene, arrow_id: &ElementId) -> Option<BoundEndpoints> {
    let arrow = scene.get(arrow_id)?;
    let data = match &arrow.kind {
        ElementKind::Arrow(d) | ElementKind::Line(d) => d,
        _ => return None,
    };
    let n = data.points.len();
    if n < 2 {
        return Some(BoundEndpoints {
            start: None,
            end: None,
        });
    }
    // Each bound endpoint is recomputed from the *current* position of its
    // neighbouring vertex (the adjacent point) through the stored focus/gap, so
    // the arrow keeps aiming at the same spot as the target moves.
    let global = |i: usize| Point::new(arrow.x + data.points[i].x, arrow.y + data.points[i].y);
    let resolve = |binding: &PointBinding, adjacent: Point| -> Option<Point> {
        let target = scene.get(&binding.element_id)?;
        if !is_bindable_element(target) {
            return None;
        }
        // A fixed interior point follows the shape directly; otherwise resolve the
        // edge binding from the adjacent vertex + focus/gap.
        if let Some(fixed) = binding.fixed_point {
            return Some(fixed_bound_point(target, fixed));
        }
        Some(binding_endpoint(target, binding.focus, binding.gap, adjacent))
    };
    let start = data
        .start_binding
        .as_ref()
        .and_then(|b| resolve(b, global(1)));
    let end = data
        .end_binding
        .as_ref()
        .and_then(|b| resolve(b, global(n - 2)));

    Some(BoundEndpoints { start, end })
}

/// The recomputed scene-space endpoints for a bound arrow. Each is `None` when
/// that end is unbound (or its target is missing) and must be left as-is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundEndpoints {
    pub start: Option<Point>,
    pub end: Option<Point>,
}

/// Apply recomputed endpoints to a linear element's relative `points`, keeping
/// intermediate points intact and only moving the bound first/last vertices.
///
/// Endpoints are in **scene** space; the element's `points` are relative to its
/// `(x, y)` origin, so each endpoint is converted back to element-relative space.
/// Returns whether anything changed. (Convenience for the caller's apply step; it
/// takes `&mut LinearData` plus the element origin so it stays free of `Scene`
/// borrow juggling.)
pub fn apply_bound_endpoints(
    data: &mut LinearData,
    origin: Point,
    endpoints: BoundEndpoints,
) -> bool {
    let mut changed = false;
    if let (Some(p), Some(first)) = (endpoints.start, data.points.first().copied()) {
        let rel = Point::new(p.x - origin.x, p.y - origin.y);
        if rel != first {
            data.points[0] = rel;
            changed = true;
        }
    }
    if let (Some(p), Some(last)) = (endpoints.end, data.points.last().copied()) {
        let rel = Point::new(p.x - origin.x, p.y - origin.y);
        if rel != last {
            let idx = data.points.len() - 1;
            data.points[idx] = rel;
            changed = true;
        }
    }
    changed
}

// ----- Rect edge model -------------------------------------------------------

/// Signed distance from `point` to the rect border: negative inside, `0` on the
/// border, positive outside (true Euclidean distance when outside).
fn signed_rect_distance(rect: Rect, point: Point) -> f64 {
    let dx = (rect.min_x() - point.x).max(point.x - rect.max_x());
    let dy = (rect.min_y() - point.y).max(point.y - rect.max_y());
    if dx <= 0.0 && dy <= 0.0 {
        // Inside: distance to the nearest edge, reported as negative.
        dx.max(dy)
    } else {
        // Outside: standard AABB exterior distance.
        let ox = dx.max(0.0);
        let oy = dy.max(0.0);
        (ox * ox + oy * oy).sqrt()
    }
}

// ----- Ellipse edge model ----------------------------------------------------

/// Signed distance from `point` to the inscribed ellipse of `rect`: negative
/// inside, positive outside. Uses the gradient-normalized implicit value, which
/// is the standard cheap first-order distance estimate and is exact along the
/// axes (sufficient for the gap term given the documented approximation).
fn signed_ellipse_distance(rect: Rect, point: Point) -> f64 {
    let center = rect.center();
    let rx = rect.width / 2.0;
    let ry = rect.height / 2.0;
    if rx == 0.0 || ry == 0.0 {
        return point.distance(center);
    }
    let dx = point.x - center.x;
    let dy = point.y - center.y;
    let f = (dx * dx) / (rx * rx) + (dy * dy) / (ry * ry) - 1.0;
    // Gradient magnitude of the implicit function, for a first-order distance.
    let gx = 2.0 * dx / (rx * rx);
    let gy = 2.0 * dy / (ry * ry);
    let grad = (gx * gx + gy * gy).sqrt();
    if grad == 0.0 {
        // At the center.
        -rx.min(ry)
    } else {
        f / grad
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{Arrowhead, LinearData};
    use crate::geometry::Point;

    fn rect_el(id: &str, x: f64, y: f64, w: f64, h: f64) -> Element {
        Element::new(ElementId::from(id), 1, x, y, w, h, ElementKind::Rectangle)
    }

    fn ellipse_el(id: &str, x: f64, y: f64, w: f64, h: f64) -> Element {
        Element::new(ElementId::from(id), 1, x, y, w, h, ElementKind::Ellipse)
    }

    fn arrow_el(id: &str, points: Vec<Point>) -> Element {
        // The element origin is the first point; width/height from extents are
        // not load-bearing for these tests.
        let data = LinearData::arrow(points.clone());
        Element::new(
            ElementId::from(id),
            1,
            0.0,
            0.0,
            0.0,
            0.0,
            ElementKind::Arrow(data),
        )
    }

    fn approx(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "expected {b}, got {a}");
    }

    fn approx_pt(p: Point, q: Point, eps: f64) {
        approx(p.x, q.x, eps);
        approx(p.y, q.y, eps);
    }

    // ---- is_bindable / bindable_element_at ----

    #[test]
    fn bindable_classification() {
        assert!(is_bindable_element(&rect_el("r", 0.0, 0.0, 10.0, 10.0)));
        assert!(is_bindable_element(&ellipse_el("e", 0.0, 0.0, 10.0, 10.0)));
        assert!(is_bindable_element(&Element::new(
            ElementId::from("d"),
            1,
            0.0,
            0.0,
            10.0,
            10.0,
            ElementKind::Diamond,
        )));
        // Arrows / lines are not bindable targets.
        assert!(!is_bindable_element(&arrow_el(
            "a",
            vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)]
        )));
        // Deleted shapes are excluded.
        let mut del = rect_el("x", 0.0, 0.0, 10.0, 10.0);
        del.is_deleted = true;
        assert!(!is_bindable_element(&del));
    }

    #[test]
    fn bindable_element_at_inside_and_near_edge() {
        let mut scene = Scene::new();
        scene.insert(rect_el("r", 0.0, 0.0, 100.0, 50.0));

        // Inside.
        assert_eq!(
            bindable_element_at(&scene, Point::new(50.0, 25.0), 5.0),
            Some(ElementId::from("r"))
        );
        // 3 units outside the right edge, tolerance 5 → binds; tolerance 2 → none.
        assert_eq!(
            bindable_element_at(&scene, Point::new(103.0, 25.0), 5.0),
            Some(ElementId::from("r"))
        );
        assert_eq!(
            bindable_element_at(&scene, Point::new(103.0, 25.0), 2.0),
            None
        );
        // Far away.
        assert_eq!(
            bindable_element_at(&scene, Point::new(500.0, 500.0), 5.0),
            None
        );
    }

    #[test]
    fn bindable_element_at_picks_topmost() {
        let mut scene = Scene::new();
        scene.insert(rect_el("bottom", 0.0, 0.0, 100.0, 100.0));
        scene.insert(rect_el("top", 0.0, 0.0, 100.0, 100.0));
        // Both overlap the point; the later-inserted (topmost) wins.
        assert_eq!(
            bindable_element_at(&scene, Point::new(50.0, 50.0), 5.0),
            Some(ElementId::from("top"))
        );
    }

    #[test]
    fn bindable_element_at_skips_arrows() {
        let mut scene = Scene::new();
        scene.insert(arrow_el(
            "a",
            vec![Point::new(0.0, 0.0), Point::new(100.0, 0.0)],
        ));
        assert_eq!(
            bindable_element_at(&scene, Point::new(50.0, 0.0), 5.0),
            None
        );
    }

    // ---- compute_binding focus/gap (Excalidraw perpendicular-offset model) ----

    /// Collinearity of `p` with the segment `a..b` (zero when on the line).
    fn cross_collinear(a: Point, b: Point, p: Point) -> f64 {
        (p.x - a.x) * (b.y - a.y) - (p.y - a.y) * (b.x - a.x)
    }

    #[test]
    fn aim_at_center_gives_zero_focus() {
        // Approaching the centre along the horizontal axis → focus 0 (aim at the
        // centre), with the gap measured to the near edge.
        let r = rect_el("r", 100.0, 0.0, 100.0, 50.0); // center (150,25), left edge 100
        let adjacent = Point::new(0.0, 25.0);
        let endpoint = Point::new(90.0, 25.0); // 10px outside the left edge, centre height
        let b = compute_binding(endpoint, adjacent, &r);
        approx(b.focus, 0.0, 1e-9);
        approx(b.gap, 10.0, 1e-9);
    }

    #[test]
    fn off_center_aim_gives_nonzero_focus() {
        // Aiming below the centre line → nonzero focus.
        let r = rect_el("r", 100.0, 0.0, 100.0, 50.0); // center (150,25)
        let adjacent = Point::new(0.0, 25.0);
        let endpoint = Point::new(90.0, 45.0); // below centre height
        let b = compute_binding(endpoint, adjacent, &r);
        assert!(b.focus.abs() > 0.0);
        assert!(b.focus.abs() <= 1.0 + 1e-9);
    }

    #[test]
    fn release_inside_binds_to_fixed_interior_point() {
        // Releasing the endpoint inside the shape anchors it to that interior point
        // (normalized to the box) instead of snapping to the edge — so an arrow can
        // live at the centre and follow the shape.
        let r = rect_el("r", 0.0, 0.0, 100.0, 100.0);
        let b = compute_binding(Point::new(50.0, 50.0), Point::new(-50.0, 50.0), &r);
        assert_eq!(b.fixed_point, Some([0.5, 0.5]), "centre release → fixed centre point");
        approx_pt(fixed_bound_point(&r, b.fixed_point.unwrap()), Point::new(50.0, 50.0), 1e-9);

        // A release near the edge still uses edge binding (no fixed point).
        let edge = compute_binding(Point::new(98.0, 50.0), Point::new(-50.0, 50.0), &r);
        assert!(edge.fixed_point.is_none(), "near-edge release stays edge binding");
    }

    // ---- update_bound_arrow: the headline behavior ----

    fn bind_arrow_to_target(
        scene: &mut Scene,
        arrow_id: &str,
        end_target: &Element,
        endpoint: Point,
    ) {
        let arrow = scene.get(&ElementId::from(arrow_id)).unwrap();
        // Adjacent = the END's neighbouring vertex, in global coords.
        let adjacent = match &arrow.kind {
            ElementKind::Arrow(d) | ElementKind::Line(d) => {
                let n = d.points.len();
                Point::new(arrow.x + d.points[n - 2].x, arrow.y + d.points[n - 2].y)
            }
            _ => endpoint,
        };
        let binding = compute_binding(endpoint, adjacent, end_target);
        let arrow = scene.get_mut(&ElementId::from(arrow_id)).unwrap();
        if let ElementKind::Arrow(d) = &mut arrow.kind {
            d.end_binding = Some(binding);
        }
    }

    #[test]
    fn bound_endpoint_keeps_aiming_through_center_as_target_moves() {
        let mut scene = Scene::new();

        // Target rect centred at (150,25): (100,0)-(200,50), left edge x=100.
        scene.insert(rect_el("r", 100.0, 0.0, 100.0, 50.0));

        // Arrow from (0,25) to (90,25): its end is 10px left of the rect's left
        // edge, aiming straight at the centre horizontally → focus 0, gap 10.
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(0.0, 25.0), Point::new(90.0, 25.0)],
        ));
        let target = scene.get(&ElementId::from("r")).unwrap().clone();
        bind_arrow_to_target(&mut scene, "arrow", &target, Point::new(90.0, 25.0));

        let binding = match &scene.get(&ElementId::from("arrow")).unwrap().kind {
            ElementKind::Arrow(d) => d.end_binding.clone().unwrap(),
            _ => unreachable!(),
        };
        approx(binding.focus, 0.0, 1e-9);
        approx(binding.gap, 10.0, 1e-9);

        // Recompute against the current target: endpoint sits at left edge - gap.
        let ep = update_bound_arrow(&scene, &ElementId::from("arrow")).unwrap();
        approx_pt(ep.end.unwrap(), Point::new(90.0, 25.0), 1e-6);

        // Move the rectangle right 40 and down 10 → centre (190,35), left edge 140.
        {
            let r = scene.get_mut(&ElementId::from("r")).unwrap();
            r.x += 40.0;
            r.y += 10.0;
        }
        let end = update_bound_arrow(&scene, &ElementId::from("arrow"))
            .unwrap()
            .end
            .unwrap();
        // The endpoint still aims THROUGH the new centre: it is collinear with the
        // fixed adjacent vertex (0,25) and the new centre (190,35) — this is the
        // "stays pointing at the middle" behaviour. (The old angular model failed
        // this: it pointed at a fixed compass angle instead.)
        let adjacent = Point::new(0.0, 25.0);
        let center = Point::new(190.0, 35.0);
        approx(cross_collinear(adjacent, center, end), 0.0, 1e-4);
        // And it lands gap (10) outside the new left edge (x = 140 - 10 = 130).
        approx(end.x, 130.0, 1e-6);

        // The gap to the (new) outline is preserved at 10.
        let new_r = scene.get(&ElementId::from("r")).unwrap();
        approx(distance_to_outline(new_r, end), 10.0, 1e-3);
    }

    // ---- "too close" / overshoot regression ----

    #[test]
    fn endpoint_does_not_overshoot_when_adjacent_is_inside_expanded_outline() {
        // Box (100,0)-(200,100), centre (150,50). With gap 16 the expanded outline
        // reaches x=84 on the left. Put the adjacent vertex at (110,50) — *inside*
        // the box. Aiming at the centre (focus 0), the endpoint must land on the
        // NEAR (left) facing edge, never overshoot to the far (right) edge (~216).
        let r = rect_el("r", 100.0, 0.0, 100.0, 100.0);
        let adjacent = Point::new(110.0, 50.0);
        let e = binding_endpoint(&r, 0.0, 16.0, adjacent);
        assert!(
            e.x < 150.0,
            "endpoint must be on the near (left) side of centre, got {e:?}"
        );
        // Definitely not on the far side (the old forward-only bug).
        assert!(e.x < 160.0, "endpoint overshot to the far side: {e:?}");
    }

    #[test]
    fn endpoint_stays_on_facing_side_from_either_direction() {
        let r = rect_el("r", 100.0, 0.0, 100.0, 100.0); // centre (150,50)
                                                        // Approaching from the right: endpoint on the right facing edge.
        let from_right = binding_endpoint(&r, 0.0, 12.0, Point::new(400.0, 50.0));
        assert!(from_right.x > 150.0, "right approach → right edge, {from_right:?}");
        // Approaching from the left: endpoint on the left facing edge.
        let from_left = binding_endpoint(&r, 0.0, 12.0, Point::new(-200.0, 50.0));
        assert!(from_left.x < 150.0, "left approach → left edge, {from_left:?}");
    }

    #[test]
    fn two_close_boxes_both_bound_do_not_cross_over() {
        // A (0,0)-(100,100) and B (130,0)-(230,100): 30px apart.
        let mut scene = Scene::new();
        scene.insert(rect_el("a", 0.0, 0.0, 100.0, 100.0));
        scene.insert(rect_el("b", 130.0, 0.0, 100.0, 100.0));
        // Arrow drawn between the facing edges, both ends bound.
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(95.0, 50.0), Point::new(135.0, 50.0)],
        ));
        let a = scene.get(&ElementId::from("a")).unwrap().clone();
        let b = scene.get(&ElementId::from("b")).unwrap().clone();
        // Bind start→A (adjacent = end) and end→B (adjacent = start).
        {
            let arrow = scene.get_mut(&ElementId::from("arrow")).unwrap();
            if let ElementKind::Arrow(d) = &mut arrow.kind {
                d.start_binding = Some(compute_binding(
                    Point::new(95.0, 50.0),
                    Point::new(135.0, 50.0),
                    &a,
                ));
                d.end_binding = Some(compute_binding(
                    Point::new(135.0, 50.0),
                    Point::new(95.0, 50.0),
                    &b,
                ));
            }
        }
        let ep = update_bound_arrow(&scene, &ElementId::from("arrow")).unwrap();
        let start = ep.start.unwrap();
        let end = ep.end.unwrap();
        // Both endpoints sit in the gap between the boxes, facing edges, and the
        // arrow still points forward (no reversal, no overshoot to a far edge).
        assert!(
            start.x > 50.0 && start.x <= 130.0,
            "start on A's right / in the gap, got {start:?}"
        );
        assert!(
            end.x >= 100.0 && end.x < 180.0,
            "end on B's left / in the gap, got {end:?}"
        );
        assert!(end.x >= start.x, "arrow must not reverse: {start:?} -> {end:?}");
    }

    #[test]
    fn overlapping_boxes_endpoint_stays_finite_and_near() {
        // Boxes overlapping heavily; a bound endpoint must stay near the target,
        // not fly off to a far edge.
        let r = rect_el("r", 100.0, 0.0, 60.0, 60.0); // centre (130,30)
        let adjacent = Point::new(125.0, 30.0); // deep inside
        let e = binding_endpoint(&r, 0.0, 16.0, adjacent);
        assert!(e.x.is_finite() && e.y.is_finite());
        // Stays within the (slightly expanded) box bounds — no far-side jump.
        assert!((80.0..=180.0).contains(&e.x), "endpoint flew off: {e:?}");
    }

    // ---- broader follow coverage (focus≠0, ellipse, diamond, vertical, rotat.) ----

    #[test]
    fn nonzero_focus_aims_off_center_and_is_collinear_through_focus_point() {
        let r = rect_el("r", 100.0, 0.0, 100.0, 60.0); // centre (150,30)
        let adjacent = Point::new(0.0, 30.0);
        // Drop the endpoint below the centre line so focus is nonzero.
        let drawn = Point::new(95.0, 45.0);
        let b = compute_binding(drawn, adjacent, &r);
        assert!(b.focus.abs() > 0.0);
        // The recomputed endpoint is collinear with adjacent and the focus point.
        let fp = determine_focus_point(&r, b.focus, adjacent);
        let e = binding_endpoint(&r, b.focus, b.gap, adjacent);
        approx(cross_collinear(adjacent, fp, e), 0.0, 1e-4);
    }

    #[test]
    fn ellipse_follow_keeps_clearance() {
        let mut scene = Scene::new();
        scene.insert(ellipse_el("e", 100.0, 0.0, 100.0, 100.0)); // centre (150,50)
        // End just OUTSIDE the left vertex (x=100) so this is an edge binding.
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(0.0, 50.0), Point::new(90.0, 50.0)],
        ));
        let target = scene.get(&ElementId::from("e")).unwrap().clone();
        bind_arrow_to_target(&mut scene, "arrow", &target, Point::new(90.0, 50.0));
        // Move the ellipse and check the endpoint still sits ~gap outside it.
        scene.get_mut(&ElementId::from("e")).unwrap().x += 60.0;
        let end = update_bound_arrow(&scene, &ElementId::from("arrow"))
            .unwrap()
            .end
            .unwrap();
        let moved = scene.get(&ElementId::from("e")).unwrap();
        let binding = match &scene.get(&ElementId::from("arrow")).unwrap().kind {
            ElementKind::Arrow(d) => d.end_binding.clone().unwrap(),
            _ => unreachable!(),
        };
        approx(distance_to_outline(moved, end), binding.gap, 0.5);
    }

    #[test]
    fn diamond_follow_aims_through_center() {
        let mut scene = Scene::new();
        scene.insert(Element::new(
            ElementId::from("d"),
            1,
            100.0,
            0.0,
            120.0,
            80.0,
            ElementKind::Diamond,
        ));
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(0.0, 40.0), Point::new(150.0, 40.0)],
        ));
        let target = scene.get(&ElementId::from("d")).unwrap().clone();
        bind_arrow_to_target(&mut scene, "arrow", &target, Point::new(150.0, 40.0));
        scene.get_mut(&ElementId::from("d")).unwrap().x += 50.0;
        let end = update_bound_arrow(&scene, &ElementId::from("arrow"))
            .unwrap()
            .end
            .unwrap();
        // Collinear with the fixed adjacent (0,40) and the diamond's new centre.
        let moved = scene.get(&ElementId::from("d")).unwrap();
        approx(cross_collinear(Point::new(0.0, 40.0), moved.center(), end), 0.0, 0.5);
    }

    #[test]
    fn vertical_arrangement_follows() {
        let mut scene = Scene::new();
        scene.insert(rect_el("r", 0.0, 200.0, 100.0, 60.0)); // centre (50,230)
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(50.0, 0.0), Point::new(50.0, 190.0)],
        ));
        let target = scene.get(&ElementId::from("r")).unwrap().clone();
        bind_arrow_to_target(&mut scene, "arrow", &target, Point::new(50.0, 190.0));
        // Move the box down; the vertical arrow should track its top edge.
        scene.get_mut(&ElementId::from("r")).unwrap().y += 40.0;
        let end = update_bound_arrow(&scene, &ElementId::from("arrow"))
            .unwrap()
            .end
            .unwrap();
        approx(end.x, 50.0, 1e-4);
        // Top edge now at y=240; gap 10 above → y≈230.
        let moved = scene.get(&ElementId::from("r")).unwrap();
        let binding = match &scene.get(&ElementId::from("arrow")).unwrap().kind {
            ElementKind::Arrow(d) => d.end_binding.clone().unwrap(),
            _ => unreachable!(),
        };
        approx(distance_to_outline(moved, end), binding.gap, 1e-3);
        assert!(end.y < 240.0, "endpoint above the top edge, got {end:?}");
    }

    #[test]
    fn start_binding_follows_too() {
        let mut scene = Scene::new();
        scene.insert(rect_el("r", 100.0, 0.0, 100.0, 50.0)); // centre (150,25)
                                                            // Arrow whose START (points[0]) is near the box, END far right.
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(90.0, 25.0), Point::new(400.0, 25.0)],
        ));
        // Bind the START: adjacent = points[1] (the far end).
        {
            let arrow = scene.get(&ElementId::from("arrow")).unwrap();
            let adjacent = Point::new(400.0, 25.0);
            let r = scene.get(&ElementId::from("r")).unwrap();
            let b = compute_binding(Point::new(90.0, 25.0), adjacent, r);
            let arrow = scene.get_mut(&ElementId::from("arrow")).unwrap();
            if let ElementKind::Arrow(d) = &mut arrow.kind {
                d.start_binding = Some(b);
            }
            let _ = arrow;
        }
        scene.get_mut(&ElementId::from("r")).unwrap().x += 50.0; // box → right edge 250
        let ep = update_bound_arrow(&scene, &ElementId::from("arrow")).unwrap();
        let start = ep.start.unwrap();
        assert!(ep.end.is_none());
        // Box moved right; the START tracks the box's right edge (facing the END).
        let moved = scene.get(&ElementId::from("r")).unwrap();
        approx(distance_to_outline(moved, start), 10.0, 1e-3);
        assert!(start.x > 150.0, "start tracked to the right edge, got {start:?}");
    }

    #[test]
    fn rotated_target_follows_against_upright_outline() {
        let mut scene = Scene::new();
        let mut r = rect_el("r", 100.0, 0.0, 100.0, 100.0);
        r.angle = std::f64::consts::FRAC_PI_4; // 45°
        scene.insert(r);
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(0.0, 50.0), Point::new(120.0, 50.0)],
        ));
        let target = scene.get(&ElementId::from("r")).unwrap().clone();
        bind_arrow_to_target(&mut scene, "arrow", &target, Point::new(120.0, 50.0));
        // Just assert it produces a finite endpoint near the (rotated) target.
        let end = update_bound_arrow(&scene, &ElementId::from("arrow"))
            .unwrap()
            .end
            .unwrap();
        assert!(end.x.is_finite() && end.y.is_finite());
        let moved = scene.get(&ElementId::from("r")).unwrap();
        assert!(end.distance(moved.center()) < 200.0);
    }

    #[test]
    fn update_keeps_intermediate_points_and_only_moves_bound_end() {
        let mut scene = Scene::new();
        scene.insert(rect_el("r", 100.0, 0.0, 100.0, 50.0));
        // 3-point arrow; only the end is bound.
        scene.insert(arrow_el(
            "arrow",
            vec![
                Point::new(0.0, 25.0),
                Point::new(45.0, 60.0),
                Point::new(90.0, 25.0),
            ],
        ));
        let target = scene.get(&ElementId::from("r")).unwrap().clone();
        bind_arrow_to_target(&mut scene, "arrow", &target, Point::new(90.0, 25.0));

        // Move target.
        scene.get_mut(&ElementId::from("r")).unwrap().x += 40.0;

        let ep = update_bound_arrow(&scene, &ElementId::from("arrow")).unwrap();
        assert!(ep.start.is_none()); // start unbound
        let new_end = ep.end.unwrap();

        // Apply and check the middle point is untouched.
        let arrow = scene.get_mut(&ElementId::from("arrow")).unwrap();
        let origin = Point::new(arrow.x, arrow.y);
        if let ElementKind::Arrow(d) = &mut arrow.kind {
            let changed = apply_bound_endpoints(d, origin, ep);
            assert!(changed);
            assert_eq!(d.points[0], Point::new(0.0, 25.0)); // start unchanged
            assert_eq!(d.points[1], Point::new(45.0, 60.0)); // middle unchanged
            approx_pt(d.points[2], Point::new(new_end.x, new_end.y), 1e-9);
        }
    }

    #[test]
    fn update_unbound_arrow_returns_none_ends() {
        let mut scene = Scene::new();
        scene.insert(arrow_el(
            "arrow",
            vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)],
        ));
        let ep = update_bound_arrow(&scene, &ElementId::from("arrow")).unwrap();
        assert!(ep.start.is_none());
        assert!(ep.end.is_none());
    }

    #[test]
    fn update_missing_target_yields_none_for_that_end() {
        let mut scene = Scene::new();
        let mut data = LinearData::arrow(vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)]);
        data.end_arrowhead = Some(Arrowhead::Triangle);
        data.end_binding = Some(PointBinding {
            element_id: ElementId::from("ghost"),
            focus: 0.0,
            gap: 5.0,
            fixed_point: None,
        });
        let el = Element::new(
            ElementId::from("arrow"),
            1,
            0.0,
            0.0,
            10.0,
            0.0,
            ElementKind::Arrow(data),
        );
        scene.insert(el);
        let ep = update_bound_arrow(&scene, &ElementId::from("arrow")).unwrap();
        assert!(ep.end.is_none());
    }

    #[test]
    fn update_non_linear_element_returns_none() {
        let mut scene = Scene::new();
        scene.insert(rect_el("r", 0.0, 0.0, 10.0, 10.0));
        assert!(update_bound_arrow(&scene, &ElementId::from("r")).is_none());
    }

    #[test]
    fn signed_rect_distance_inside_outside() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        // Outside right by 10.
        approx(signed_rect_distance(r, Point::new(110.0, 25.0)), 10.0, 1e-9);
        // Outside corner: (103,54) → (3,4) → 5.
        approx(signed_rect_distance(r, Point::new(103.0, 54.0)), 5.0, 1e-9);
        // Inside near right edge by 5 → negative.
        approx(signed_rect_distance(r, Point::new(95.0, 25.0)), -5.0, 1e-9);
    }
}
