//! Vector paths for `CanvasItem::Path` — an SVG-path-subset parser plus
//! `lyon` tessellation into Bevy 2d meshes.
//!
//! Canvas items are otherwise axis-aligned rects, rotated rects, sprites and
//! text, which is why every chart widget in `widgets/` fakes a line as a row of
//! rotated rects (see `df_view_line.ft`). Real charts need filled areas, arcs
//! and curves, so this module gives the canvas one primitive that draws
//! anything: a path.
//!
//! Coordinates are canvas space (pixels from the canvas box's top-left, y
//! DOWN). Bevy's content-root space is y-up, so every vertex is negated on the
//! way into lyon — the mesh is then spawned at the canvas origin without any
//! further flipping, exactly like `CanvasItem::Rect`.
//!
//! Supported commands: `M m L l H h V v C c S s Q q T t A a Z z` — i.e. the
//! whole SVG path grammar except the seldom-used `B`/bearing extensions.
//! Anything else is a hard parse error naming the offending byte, never a
//! silently-dropped segment.

use bevy::asset::RenderAssetUsages;
use bevy::math::Vec2;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};

use lyon::math::point as lpoint;
use lyon::path::Path as LyonPath;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, LineCap as LCap, LineJoin as LJoin,
    StrokeOptions, StrokeTessellator, StrokeVertex, VertexBuffers,
};

use crate::protocol::{PathCap, PathJoin};

/// A parsed path plus the bounds of the points that built it. The bounds are
/// a control-point hull (an over-approximation for curves), which is all the
/// canvas scroll-extent pass needs.
#[derive(Debug)]
pub struct ParsedPath {
    pub path: LyonPath,
    pub min: Vec2,
    pub max: Vec2,
}

impl ParsedPath {
    /// Lowest extent in CANVAS coordinates (y-down), for scroll bounds.
    pub fn bottom(&self) -> f32 {
        // Vertices were negated into y-up space on the way in; the canvas-space
        // bottom is therefore the most-negative y.
        -self.min.y
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Scanner<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Scanner<'a> {
    fn new(s: &'a str) -> Self {
        Scanner {
            s: s.as_bytes(),
            i: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\r' | b'\n' | b',' => self.i += 1,
                _ => break,
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.i).copied()
    }

    /// True when the next token is a number, i.e. the current command repeats
    /// with another parameter set (`M 0 0 10 10` = moveto then lineto).
    fn at_number(&mut self) -> bool {
        matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.')
    }

    fn number(&mut self) -> Result<f32, String> {
        self.skip_ws();
        let start = self.i;
        if matches!(self.s.get(self.i), Some(b'-') | Some(b'+')) {
            self.i += 1;
        }
        while matches!(self.s.get(self.i), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            while matches!(self.s.get(self.i), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.s.get(self.i), Some(b'e') | Some(b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'-') | Some(b'+')) {
                self.i += 1;
            }
            if matches!(self.s.get(self.i), Some(c) if c.is_ascii_digit()) {
                while matches!(self.s.get(self.i), Some(c) if c.is_ascii_digit()) {
                    self.i += 1;
                }
            } else {
                self.i = save;
            }
        }
        if self.i == start {
            return Err(format!(
                "expected a number at byte {} of the path data, found {:?}",
                start,
                self.s.get(start).map(|c| *c as char)
            ));
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse::<f32>().ok())
            .ok_or_else(|| {
                format!(
                    "{:?} is not a valid number (byte {start} of the path data)",
                    String::from_utf8_lossy(&self.s[start..self.i])
                )
            })
    }

    /// SVG arc flags may be written unseparated (`a1 1 0 011 1`), so a flag is
    /// exactly one `0` or `1` character.
    fn flag(&mut self) -> Result<bool, String> {
        self.skip_ws();
        match self.s.get(self.i) {
            Some(b'0') => {
                self.i += 1;
                Ok(false)
            }
            Some(b'1') => {
                self.i += 1;
                Ok(true)
            }
            other => Err(format!(
                "expected an arc flag (0 or 1) at byte {} of the path data, found {:?}",
                self.i,
                other.map(|c| *c as char)
            )),
        }
    }
}

/// Builder state: lyon wants explicit begin/end per subpath, and the smooth
/// curve commands (`S`/`T`) need the previous control point.
struct Builder {
    b: lyon::path::path::Builder,
    open: bool,
    /// Current point, canvas space (y-down).
    cur: Vec2,
    /// Start of the current subpath, for `Z`.
    start: Vec2,
    /// Reflection source for `S` (last cubic control) and `T` (last quadratic
    /// control). `None` when the previous command was not the matching kind.
    last_cubic_ctrl: Option<Vec2>,
    last_quad_ctrl: Option<Vec2>,
    min: Vec2,
    max: Vec2,
    any: bool,
}

impl Builder {
    fn new() -> Self {
        Builder {
            b: LyonPath::builder(),
            open: false,
            cur: Vec2::ZERO,
            start: Vec2::ZERO,
            last_cubic_ctrl: None,
            last_quad_ctrl: None,
            min: Vec2::splat(f32::INFINITY),
            max: Vec2::splat(f32::NEG_INFINITY),
            any: false,
        }
    }

    /// Canvas point → lyon point (y-up), recording the bounds.
    fn pt(&mut self, p: Vec2) -> lyon::math::Point {
        let flipped = Vec2::new(p.x, -p.y);
        self.min = self.min.min(flipped);
        self.max = self.max.max(flipped);
        self.any = true;
        lpoint(flipped.x, flipped.y)
    }

    fn move_to(&mut self, p: Vec2) {
        if self.open {
            self.b.end(false);
        }
        let lp = self.pt(p);
        self.b.begin(lp);
        self.open = true;
        self.cur = p;
        self.start = p;
    }

    /// Every drawing command needs an open subpath; SVG allows `L` before any
    /// `M` (the current point starts at the origin), so open one implicitly.
    fn ensure_open(&mut self) {
        if !self.open {
            let cur = self.cur;
            let lp = self.pt(cur);
            self.b.begin(lp);
            self.open = true;
            self.start = cur;
        }
    }

    fn line_to(&mut self, p: Vec2) {
        self.ensure_open();
        let lp = self.pt(p);
        self.b.line_to(lp);
        self.cur = p;
    }

    fn cubic_to(&mut self, c1: Vec2, c2: Vec2, to: Vec2) {
        self.ensure_open();
        let (l1, l2, lt) = (self.pt(c1), self.pt(c2), self.pt(to));
        self.b.cubic_bezier_to(l1, l2, lt);
        self.cur = to;
        self.last_cubic_ctrl = Some(c2);
    }

    fn quad_to(&mut self, c: Vec2, to: Vec2) {
        self.ensure_open();
        let (lc, lt) = (self.pt(c), self.pt(to));
        self.b.quadratic_bezier_to(lc, lt);
        self.cur = to;
        self.last_quad_ctrl = Some(c);
    }

    fn close(&mut self) {
        if self.open {
            self.b.end(true);
            self.open = false;
        }
        self.cur = self.start;
    }

    fn finish(mut self) -> Option<ParsedPath> {
        if self.open {
            self.b.end(false);
        }
        if !self.any {
            return None;
        }
        let path = self.b.build();
        if path.iter().next().is_none() {
            return None;
        }
        Some(ParsedPath {
            path,
            min: self.min,
            max: self.max,
        })
    }
}

/// Parse SVG path data into a tessellation-ready path in y-up space.
///
/// Returns `Ok(None)` for data that describes no geometry at all (an empty
/// string, or a lone `M`), which callers treat as "nothing to draw" — an error
/// is reserved for data that is actually malformed.
pub fn parse(d: &str) -> Result<Option<ParsedPath>, String> {
    let mut sc = Scanner::new(d);
    let mut b = Builder::new();
    let mut cmd: Option<u8> = None;

    loop {
        let c = match sc.peek() {
            None => break,
            Some(c) => c,
        };
        // A number where a command is expected repeats the previous command,
        // except that a repeated `M`/`m` means lineto (per the SVG spec).
        if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' {
            match cmd {
                Some(b'M') => cmd = Some(b'L'),
                Some(b'm') => cmd = Some(b'l'),
                Some(_) => {}
                None => {
                    return Err(format!(
                        "path data starts with a number at byte {}; it must start with a command \
                         (M/m)",
                        sc.i
                    ));
                }
            }
        } else {
            sc.i += 1;
            cmd = Some(c);
        }

        let rel = cmd.map(|c| c.is_ascii_lowercase()).unwrap_or(false);
        let cur = b.cur;
        let base = if rel { cur } else { Vec2::ZERO };

        match cmd {
            Some(b'M') | Some(b'm') => {
                let p = Vec2::new(sc.number()?, sc.number()?) + base;
                b.move_to(p);
                b.last_cubic_ctrl = None;
                b.last_quad_ctrl = None;
            }
            Some(b'L') | Some(b'l') => {
                let p = Vec2::new(sc.number()?, sc.number()?) + base;
                b.line_to(p);
                b.last_cubic_ctrl = None;
                b.last_quad_ctrl = None;
            }
            Some(b'H') | Some(b'h') => {
                let x = sc.number()? + base.x;
                b.line_to(Vec2::new(x, cur.y));
                b.last_cubic_ctrl = None;
                b.last_quad_ctrl = None;
            }
            Some(b'V') | Some(b'v') => {
                let y = sc.number()? + base.y;
                b.line_to(Vec2::new(cur.x, y));
                b.last_cubic_ctrl = None;
                b.last_quad_ctrl = None;
            }
            Some(b'C') | Some(b'c') => {
                let c1 = Vec2::new(sc.number()?, sc.number()?) + base;
                let c2 = Vec2::new(sc.number()?, sc.number()?) + base;
                let to = Vec2::new(sc.number()?, sc.number()?) + base;
                b.cubic_to(c1, c2, to);
                b.last_quad_ctrl = None;
            }
            Some(b'S') | Some(b's') => {
                // Reflect the previous cubic control point about the current
                // point; with no previous cubic the control point IS the
                // current point (SVG rule).
                let c1 = match b.last_cubic_ctrl {
                    Some(prev) => cur * 2.0 - prev,
                    None => cur,
                };
                let c2 = Vec2::new(sc.number()?, sc.number()?) + base;
                let to = Vec2::new(sc.number()?, sc.number()?) + base;
                b.cubic_to(c1, c2, to);
                b.last_quad_ctrl = None;
            }
            Some(b'Q') | Some(b'q') => {
                let c = Vec2::new(sc.number()?, sc.number()?) + base;
                let to = Vec2::new(sc.number()?, sc.number()?) + base;
                b.quad_to(c, to);
                b.last_cubic_ctrl = None;
            }
            Some(b'T') | Some(b't') => {
                let c = match b.last_quad_ctrl {
                    Some(prev) => cur * 2.0 - prev,
                    None => cur,
                };
                let to = Vec2::new(sc.number()?, sc.number()?) + base;
                b.quad_to(c, to);
                b.last_cubic_ctrl = None;
            }
            Some(b'A') | Some(b'a') => {
                let rx = sc.number()?;
                let ry = sc.number()?;
                let rot = sc.number()?;
                let large = sc.flag()?;
                let sweep = sc.flag()?;
                let to = Vec2::new(sc.number()?, sc.number()?) + base;
                arc_to(&mut b, rx, ry, rot, large, sweep, to);
                b.last_cubic_ctrl = None;
                b.last_quad_ctrl = None;
            }
            Some(b'Z') | Some(b'z') => {
                b.close();
                b.last_cubic_ctrl = None;
                b.last_quad_ctrl = None;
            }
            Some(other) => {
                return Err(format!(
                    "unsupported path command {:?} at byte {} — supported commands are \
                     M L H V C S Q T A Z (and their relative lowercase forms)",
                    other as char,
                    sc.i - 1
                ));
            }
            None => unreachable!("cmd is set before this match"),
        }
    }

    Ok(b.finish())
}

/// SVG elliptical arc → cubic beziers, via the endpoint-to-center conversion
/// in the SVG spec (implementation notes, F.6.5). Emitted in ≤90° pieces so
/// the cubic approximation stays under a tenth of a pixel at chart radii.
fn arc_to(b: &mut Builder, rx: f32, ry: f32, x_rot_deg: f32, large: bool, sweep: bool, to: Vec2) {
    let from = b.cur;
    // Degenerate radii mean a straight line (SVG spec F.6.2).
    if rx == 0.0 || ry == 0.0 || from == to {
        b.line_to(to);
        return;
    }
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    let phi = x_rot_deg.to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();

    let dx2 = (from.x - to.x) / 2.0;
    let dy2 = (from.y - to.y) / 2.0;
    let x1p = cos_phi * dx2 + sin_phi * dy2;
    let y1p = -sin_phi * dx2 + cos_phi * dy2;

    // Scale the radii up if they are too small to span the endpoints (F.6.6).
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }

    let num = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let mut coef = if den == 0.0 { 0.0 } else { (num / den).sqrt() };
    if large == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;

    let cx = cos_phi * cxp - sin_phi * cyp + (from.x + to.x) / 2.0;
    let cy = sin_phi * cxp + cos_phi * cyp + (from.y + to.y) / 2.0;

    let ang = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let dot = ux * vx + uy * vy;
        let len = ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt();
        let mut a = if len == 0.0 {
            0.0
        } else {
            (dot / len).clamp(-1.0, 1.0).acos()
        };
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };

    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let theta1 = ang(1.0, 0.0, ux, uy);
    let mut delta = ang(ux, uy, vx, vy);
    if !sweep && delta > 0.0 {
        delta -= std::f32::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f32::consts::TAU;
    }

    let segments = (delta.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = delta / segments as f32;
    // Control-point distance for a cubic approximating a circular arc of
    // `step` radians (the standard 4/3·tan(θ/4) constant).
    let k = 4.0 / 3.0 * (step / 4.0).tan();

    let mut t = theta1;
    for _ in 0..segments {
        let t2 = t + step;
        let (sin1, cos1) = t.sin_cos();
        let (sin2, cos2) = t2.sin_cos();

        // Point + tangent on the unit circle, mapped through the ellipse.
        let map = |c: f32, s: f32| -> Vec2 {
            Vec2::new(
                cx + rx * c * cos_phi - ry * s * sin_phi,
                cy + rx * c * sin_phi + ry * s * cos_phi,
            )
        };
        let d_map = |c: f32, s: f32| -> Vec2 {
            Vec2::new(
                -rx * s * cos_phi - ry * c * sin_phi,
                -rx * s * sin_phi + ry * c * cos_phi,
            )
        };

        let p1 = map(cos1, sin1);
        let p2 = map(cos2, sin2);
        let c1 = p1 + d_map(cos1, sin1) * k;
        let c2 = p2 - d_map(cos2, sin2) * k;
        b.cubic_to(c1, c2, p2);
        t = t2;
    }
    // Land exactly on the requested endpoint; the trig above can drift a
    // fraction of a pixel and an unclosed donut segment shows it.
    b.cur = to;
}

// ---------------------------------------------------------------------------
// Tessellation
// ---------------------------------------------------------------------------

fn buffers_to_mesh(buf: VertexBuffers<[f32; 2], u32>) -> Option<Mesh> {
    if buf.indices.is_empty() {
        return None;
    }
    let positions: Vec<[f32; 3]> = buf.vertices.iter().map(|v| [v[0], v[1], 0.0]).collect();
    let normals: Vec<[f32; 3]> = vec![[0.0, 0.0, 1.0]; positions.len()];
    let uvs: Vec<[f32; 2]> = vec![[0.0, 0.0]; positions.len()];
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(buf.indices));
    Some(mesh)
}

/// Triangulate the path's interior (non-zero fill rule, matching SVG's
/// default) into a mesh.
pub fn fill_mesh(path: &LyonPath) -> Option<Mesh> {
    let mut buf: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = FillTessellator::new();
    let opts = FillOptions::default().with_tolerance(0.1);
    tess.tessellate_path(
        path,
        &opts,
        &mut BuffersBuilder::new(&mut buf, |v: FillVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    )
    .ok()?;
    buffers_to_mesh(buf)
}

/// Expand the path's outline to `width` px and triangulate that.
pub fn stroke_mesh(path: &LyonPath, width: f32, cap: PathCap, join: PathJoin) -> Option<Mesh> {
    let mut buf: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = StrokeTessellator::new();
    let opts = StrokeOptions::default()
        .with_line_width(width.max(0.35))
        .with_line_cap(match cap {
            PathCap::Butt => LCap::Butt,
            PathCap::Round => LCap::Round,
            PathCap::Square => LCap::Square,
        })
        .with_line_join(match join {
            PathJoin::Miter => LJoin::MiterClip,
            PathJoin::Round => LJoin::Round,
            PathJoin::Bevel => LJoin::Bevel,
        })
        .with_tolerance(0.1);
    tess.tessellate_path(
        path,
        &opts,
        &mut BuffersBuilder::new(&mut buf, |v: StrokeVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    )
    .ok()?;
    buffers_to_mesh(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(d: &str) -> ParsedPath {
        parse(d)
            .unwrap_or_else(|e| panic!("parse {d:?} failed: {e}"))
            .unwrap_or_else(|| panic!("parse {d:?} produced no geometry"))
    }

    #[test]
    fn empty_data_is_no_geometry_not_an_error() {
        assert!(parse("").unwrap().is_none());
        assert!(parse("   ").unwrap().is_none());
    }

    #[test]
    fn y_is_flipped_into_bevy_space() {
        // Canvas y-down 0..100 becomes y-up 0..-100.
        let p = parsed("M 0 0 L 10 100");
        assert_eq!(p.min.y, -100.0);
        assert_eq!(p.max.y, 0.0);
        assert_eq!(p.max.x, 10.0);
        assert_eq!(p.bottom(), 100.0);
    }

    #[test]
    fn relative_commands_accumulate() {
        let abs = parsed("M 10 10 L 20 10 L 20 20");
        let rel = parsed("m 10 10 l 10 0 l 0 10");
        assert_eq!(abs.min, rel.min);
        assert_eq!(abs.max, rel.max);
    }

    #[test]
    fn repeated_parameters_repeat_the_command() {
        // A bare `M` followed by extra pairs continues as lineto.
        let a = parsed("M 0 0 10 10 20 20");
        let b = parsed("M 0 0 L 10 10 L 20 20");
        assert_eq!(a.max, b.max);
    }

    #[test]
    fn h_and_v_hold_the_other_axis() {
        let p = parsed("M 5 7 H 25 V 27");
        assert_eq!(p.max.x, 25.0);
        assert_eq!(p.min.y, -27.0);
        assert_eq!(p.max.y, -7.0);
    }

    #[test]
    fn smooth_cubic_reflects_the_previous_control() {
        // S with no preceding curve puts the control point on the current
        // point, so the hull is just the endpoints.
        let p = parsed("M 0 0 S 10 0 10 10");
        assert_eq!(p.max.x, 10.0);
        // With a preceding C, the reflected control extends the hull past the
        // literal coordinates in the data.
        let q = parsed("M 0 0 C 0 -10 10 -10 10 0 S 20 10 20 0");
        assert!(q.min.y < 0.0, "reflected control should lift the hull");
    }

    #[test]
    fn arc_flags_may_be_unseparated() {
        // "a1 1 0 011 1" packs large=0, sweep=1, x=1, y=1.
        let p = parse("M 0 0 a1 1 0 011 1").expect("compact arc flags should parse");
        assert!(p.is_some());
    }

    #[test]
    fn half_circle_arc_spans_its_diameter() {
        // Sweep from (0,50) to (100,50) with r=50: a half circle whose top
        // reaches y=0 (canvas) → y=0 flipped, bottom stays at the endpoints.
        let p = parsed("M 0 50 A 50 50 0 0 1 100 50");
        assert!((p.max.x - 100.0).abs() < 0.5, "max x = {}", p.max.x);
        assert!((p.max.y - 0.0).abs() < 0.5, "arc apex y = {}", p.max.y);
    }

    #[test]
    fn unsupported_command_is_a_loud_error() {
        let err = parse("M 0 0 X 3").unwrap_err();
        assert!(err.contains("unsupported path command"), "{err}");
        assert!(err.contains('X'), "{err}");
    }

    #[test]
    fn leading_number_is_an_error() {
        let err = parse("10 10 L 20 20").unwrap_err();
        assert!(err.contains("must start with a command"), "{err}");
    }

    #[test]
    fn truncated_parameters_are_an_error() {
        let err = parse("M 0 0 L 10").unwrap_err();
        assert!(err.contains("expected a number"), "{err}");
    }

    #[test]
    fn closed_triangle_fills() {
        let p = parsed("M 0 0 L 100 0 L 50 80 Z");
        let mesh = fill_mesh(&p.path).expect("a closed triangle should tessellate");
        assert!(mesh.count_vertices() >= 3);
    }

    #[test]
    fn open_polyline_strokes() {
        let p = parsed("M 0 0 L 100 0 L 100 100");
        let mesh =
            stroke_mesh(&p.path, 2.0, PathCap::Round, PathJoin::Round).expect("stroke tessellates");
        assert!(mesh.count_vertices() >= 4);
    }

    #[test]
    fn scientific_notation_parses() {
        let p = parsed("M 0 0 L 1e2 5e-1");
        assert_eq!(p.max.x, 100.0);
    }
}
