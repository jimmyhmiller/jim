//! Geometry primitives shared across the whole library.
//!
//! These are the foundation types. Bounds computation and hit-testing live in
//! their own submodules (added in Phase 1) but build on the primitives here.
//!
//! Angle convention matches Excalidraw: angles are in **radians**, measured
//! clockwise (because screen-space y grows downward), and element rotation is
//! around the element's center.

mod bounds;
mod hit;
mod path;
mod point;
mod rect;
mod transform;

pub use bounds::{element_bounds, element_line_segments};
pub use hit::{distance_to_outline, hit_test, point_distance_to_segment};
pub use path::{Path, PathBuilder, PathSegment};
pub use point::{Point, Vec2};
pub use rect::Rect;
pub use transform::Transform;

use std::f64::consts::PI;

/// Two angles compare equal within this tolerance.
pub const EPSILON: f64 = 1e-6;

/// Rotate `point` around `center` by `angle` radians (clockwise in screen space).
///
/// Port of Excalidraw `math`'s `pointRotateRads`.
#[inline]
pub fn point_rotate_rads(point: Point, center: Point, angle: f64) -> Point {
    let (sin, cos) = angle.sin_cos();
    let dx = point.x - center.x;
    let dy = point.y - center.y;
    Point {
        x: dx * cos - dy * sin + center.x,
        y: dx * sin + dy * cos + center.y,
    }
}

/// Intersection point of two line **segments**, or `None` if they don't cross
/// within both segments (parallel, or the crossing lies off either segment).
///
/// Port of Excalidraw `math`'s `lineSegmentIntersectionPoints`: the infinite
/// lines are intersected, then the point is required to lie on both segments
/// (within `EPSILON` of the parameter range).
pub fn segment_intersection(a0: Point, a1: Point, b0: Point, b1: Point) -> Option<Point> {
    let r = Vec2::new(a1.x - a0.x, a1.y - a0.y);
    let s = Vec2::new(b1.x - b0.x, b1.y - b0.y);
    let denom = r.cross(s);
    if denom.abs() < 1e-12 {
        return None; // parallel or degenerate
    }
    let qp = Vec2::new(b0.x - a0.x, b0.y - a0.y);
    let t = qp.cross(s) / denom;
    let u = qp.cross(r) / denom;
    let on = |v: f64| v >= -EPSILON && v <= 1.0 + EPSILON;
    if on(t) && on(u) {
        Some(Point::new(a0.x + t * r.x, a0.y + t * r.y))
    } else {
        None
    }
}

/// Normalize an angle into the `[0, 2π)` range.
#[inline]
pub fn normalize_angle(angle: f64) -> f64 {
    let two_pi = 2.0 * PI;
    let a = angle % two_pi;
    if a < 0.0 {
        a + two_pi
    } else {
        a
    }
}

/// Linear interpolation between `a` and `b`.
#[inline]
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_quarter_turn() {
        let p = Point::new(1.0, 0.0);
        let r = point_rotate_rads(p, Point::ORIGIN, PI / 2.0);
        assert!((r.x - 0.0).abs() < EPSILON, "x={}", r.x);
        assert!((r.y - 1.0).abs() < EPSILON, "y={}", r.y);
    }

    #[test]
    fn rotate_around_center_is_identity_at_center() {
        let c = Point::new(5.0, 5.0);
        let r = point_rotate_rads(c, c, 1.234);
        assert!((r.x - c.x).abs() < EPSILON);
        assert!((r.y - c.y).abs() < EPSILON);
    }

    #[test]
    fn normalize_wraps_negative() {
        let a = normalize_angle(-PI / 2.0);
        assert!((a - (3.0 * PI / 2.0)).abs() < EPSILON);
    }
}
