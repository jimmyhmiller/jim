//! Shared **tool-button icons**: crisp little vector glyphs tessellated with
//! lyon, used by both the in-pane island toolbar (Mode 1) and the floating
//! canvas toolbar (Mode 2). Drawing real strokes (instead of Unicode glyphs)
//! keeps the icons sharp and font-independent — the app font is JetBrains Mono,
//! which has no reliable geometric-shape coverage.
//!
//! Geometry is authored in a unit box `[0,1]²` with **y-down** (top-left origin,
//! matching the toolbar's content-local button rects). [`icon_meshes`] maps each
//! polyline into a button rect and returns y-up content-local meshes ready to
//! spawn as `Mesh2d` children of a pane's `content_root`.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::prelude::*;

use jim_pane::{ChromeStyle, ChromeTextStyle};
use whiteboard_core::render::Color as WbColor;

use lyon::math::point as lpoint;
use lyon::path::Path as LyonPath;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, LineCap, LineJoin, StrokeOptions,
    StrokeTessellator, StrokeVertex, VertexBuffers,
};

// ---------- Shared button theme + palette ----------

/// Fixed accent ink colors shared by both toolbars. The first swatch ("ink") is
/// *not* here — it's the theme foreground, injected so the default pen is always
/// visible against the current surface.
pub const ACCENT_COLORS: &[WbColor] = &[
    WbColor::rgb(0xe0, 0x31, 0x31), // red
    WbColor::rgb(0x2f, 0x9e, 0x44), // green
    WbColor::rgb(0x18, 0x71, 0xe8), // blue
    WbColor::rgb(0xf0, 0x8c, 0x00), // orange
    WbColor::rgb(0x9c, 0x36, 0xb5), // purple
];

/// The swatch palette for the current theme: foreground ink first, then accents.
pub fn palette(fg: WbColor) -> Vec<WbColor> {
    let mut v = Vec::with_capacity(ACCENT_COLORS.len() + 1);
    v.push(fg);
    v.extend_from_slice(ACCENT_COLORS);
    v
}

/// Resolved colors for toolbar chrome + palette, built from the shared
/// [`ChromeStyle`]/[`ChromeTextStyle`] resources so both toolbars match the app.
pub struct ButtonTheme {
    /// Inactive button cell fill.
    pub cell: Color,
    /// Active (selected) button cell fill — the theme accent.
    pub cell_active: Color,
    /// Hover cell fill (between inactive and active).
    pub hover: Color,
    /// Button label / icon ink.
    pub label: Color,
    /// Swatch palette (foreground ink + accents).
    pub palette: Vec<WbColor>,
}

fn lin_to_color(v: Vec4) -> Color {
    Color::LinearRgba(bevy::color::LinearRgba::new(v.x, v.y, v.z, v.w))
}

pub fn color_to_wb(c: Color) -> WbColor {
    let s = c.to_srgba();
    let q = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;
    WbColor::rgb(q(s.red), q(s.green), q(s.blue))
}

/// Convert a whiteboard color into a Bevy color.
pub fn wb_to_color(c: WbColor) -> Color {
    Color::srgb(
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
    )
}

/// Linear-space lerp between two colors.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let a = a.to_linear();
    let b = b.to_linear();
    Color::LinearRgba(bevy::color::LinearRgba::new(
        a.red + (b.red - a.red) * t,
        a.green + (b.green - a.green) * t,
        a.blue + (b.blue - a.blue) * t,
        a.alpha + (b.alpha - a.alpha) * t,
    ))
}

impl ButtonTheme {
    pub fn from_theme(chrome: Option<&ChromeStyle>, text: Option<&ChromeTextStyle>) -> Self {
        let label = text
            .map(|t| t.title_focused)
            .unwrap_or(Color::srgb(0.92, 0.92, 0.95));
        let (cell, cell_active) = match chrome {
            Some(c) => (
                lin_to_color(c.title_bg_focused),
                lin_to_color(c.border_focused),
            ),
            None => (
                Color::srgb(0.22, 0.22, 0.26),
                Color::srgb(0.30, 0.42, 0.72),
            ),
        };
        // Hover = halfway between inactive and active cell fill.
        let hover = lerp_color(cell, cell_active, 0.5);
        ButtonTheme {
            cell,
            cell_active,
            hover,
            label,
            palette: palette(color_to_wb(label)),
        }
    }
}

/// Which tool/action a button represents — selects its icon glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Select,
    Freedraw,
    Rectangle,
    Ellipse,
    Diamond,
    Line,
    Arrow,
    Text,
    Eraser,
    /// A trash glyph for the "clear the board" action.
    Trash,
}

/// The polylines that make up an icon, in the unit box (y-down). Each inner
/// `Vec` is one open stroked path.
fn icon_polylines(icon: Icon) -> Vec<Vec<(f32, f32)>> {
    match icon {
        // Mouse-cursor arrow.
        Icon::Select => vec![vec![
            (0.30, 0.18),
            (0.30, 0.74),
            (0.43, 0.61),
            (0.52, 0.80),
            (0.60, 0.76),
            (0.50, 0.58),
            (0.67, 0.56),
            (0.30, 0.18),
        ]],
        // Freehand squiggle.
        Icon::Freedraw => vec![vec![
            (0.15, 0.60),
            (0.30, 0.42),
            (0.45, 0.64),
            (0.60, 0.40),
            (0.72, 0.55),
            (0.85, 0.40),
        ]],
        Icon::Rectangle => vec![vec![
            (0.16, 0.22),
            (0.84, 0.22),
            (0.84, 0.78),
            (0.16, 0.78),
            (0.16, 0.22),
        ]],
        Icon::Ellipse => vec![circle(0.5, 0.5, 0.34, 0.36)],
        Icon::Diamond => vec![vec![
            (0.50, 0.14),
            (0.86, 0.50),
            (0.50, 0.86),
            (0.14, 0.50),
            (0.50, 0.14),
        ]],
        Icon::Line => vec![vec![(0.18, 0.82), (0.82, 0.18)]],
        Icon::Arrow => vec![
            vec![(0.18, 0.82), (0.82, 0.18)],
            // Arrowhead barbs at the (0.82,0.18) tip.
            vec![(0.82, 0.18), (0.60, 0.24)],
            vec![(0.82, 0.18), (0.76, 0.42)],
        ],
        // Capital "T".
        Icon::Text => vec![
            vec![(0.24, 0.26), (0.76, 0.26)],
            vec![(0.50, 0.26), (0.50, 0.80)],
        ],
        // Slanted eraser block.
        Icon::Eraser => vec![
            vec![
                (0.20, 0.60),
                (0.52, 0.28),
                (0.80, 0.44),
                (0.48, 0.76),
                (0.20, 0.60),
            ],
            vec![(0.39, 0.69), (0.66, 0.36)],
        ],
        // Trash can: lid + body + two ribs.
        Icon::Trash => vec![
            vec![(0.22, 0.30), (0.78, 0.30)],
            vec![(0.40, 0.30), (0.43, 0.22), (0.57, 0.22), (0.60, 0.30)],
            vec![
                (0.28, 0.30),
                (0.32, 0.82),
                (0.68, 0.82),
                (0.72, 0.30),
            ],
            vec![(0.42, 0.40), (0.44, 0.72)],
            vec![(0.58, 0.40), (0.56, 0.72)],
        ],
    }
}

/// A circle as a closed polyline (24 segments), scaled by separate x/y radii so
/// it stays round inside a non-square cell.
fn circle(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<(f32, f32)> {
    let n = 24;
    let mut pts = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        pts.push((cx + rx * a.cos(), cy + ry * a.sin()));
    }
    pts
}

/// Tessellate `icon` inside the content-local button rect `(x, y, w, h)`
/// (top-left origin, y-down) into y-up meshes ready to parent under a
/// `content_root`. `stroke_w` is the icon line width in content px.
pub fn icon_meshes(icon: Icon, x: f32, y: f32, w: f32, h: f32, stroke_w: f32) -> Vec<Mesh> {
    icon_polylines(icon)
        .into_iter()
        .filter_map(|poly| {
            let mapped: Vec<(f32, f32)> = poly
                .iter()
                .map(|(ux, uy)| (x + ux * w, -(y + uy * h)))
                .collect();
            stroke_mesh(&mapped, stroke_w)
        })
        .collect()
}

/// A single horizontal stroke of the given width, centered in the rect — used by
/// the stroke-width picker buttons so each shows its actual line thickness.
pub fn width_sample_mesh(x: f32, y: f32, w: f32, h: f32, width: f32) -> Option<Mesh> {
    let cy = -(y + h * 0.5);
    let pad = w * 0.22;
    stroke_mesh(&[(x + pad, cy), (x + w - pad, cy)], width.clamp(1.0, h * 0.6))
}

/// A filled rounded rectangle mesh for the given content-local (y-down) rect.
/// Used for button cell fills and the island backdrop panel.
pub fn rounded_rect_mesh(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<Mesh> {
    let pts = rounded_rect_points(x, y, w, h, radius);
    fill_mesh(&pts)
}

/// Sample a rounded-rect outline (content-local y-down rect → y-up points) with
/// a few segments per corner.
fn rounded_rect_points(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Vec<(f32, f32)> {
    let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    // y-up mesh-space edges.
    let l = x;
    let rt = x + w;
    let top = -y;
    let bot = -(y + h);
    let seg = 4;
    use std::f32::consts::{FRAC_PI_2, PI};
    // Each corner's arc center plus the angle it STARTS at; every arc sweeps 90°
    // clockwise (`a = start - t·90°`). The start angles must put each arc in its
    // own quadrant so consecutive arcs are joined by the rect's straight edges —
    // e.g. the top-right arc runs from the top edge (90°) to the right edge (0°),
    // so its next neighbour (bottom-right, starting at 0°) continues straight
    // down the right edge. Getting these 90° off (the old bug) folds the outline
    // into a 4-pointed star, which only became visible once the meshes rendered.
    let corners = [
        (rt - r, top - r, FRAC_PI_2),       // top-right:    90° → 0°
        (rt - r, bot + r, 0.0_f32),         // bottom-right:  0° → -90°
        (l + r, bot + r, FRAC_PI_2 * 3.0),  // bottom-left: 270° → 180°
        (l + r, top - r, PI),               // top-left:    180° → 90°
    ];
    let mut pts = Vec::new();
    for (cx, cy, start) in corners {
        for i in 0..=seg {
            let a = start - i as f32 / seg as f32 * FRAC_PI_2;
            pts.push((cx + r * a.cos(), cy + r * a.sin()));
        }
    }
    pts
}

/// Fill a closed y-up polygon into a triangle mesh.
fn fill_mesh(points: &[(f32, f32)]) -> Option<Mesh> {
    if points.len() < 3 {
        return None;
    }
    let mut b = LyonPath::builder();
    b.begin(lpoint(points[0].0, points[0].1));
    for p in &points[1..] {
        b.line_to(lpoint(p.0, p.1));
    }
    b.end(true);
    let path = b.build();

    let mut buf: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = FillTessellator::new();
    let opts = FillOptions::default().with_tolerance(0.1);
    tess.tessellate_path(
        &path,
        &opts,
        &mut BuffersBuilder::new(&mut buf, |v: FillVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    )
    .ok()?;
    buffers_to_mesh(buf)
}

/// Stroke a y-up polyline into a triangle mesh with round caps/joins.
fn stroke_mesh(points: &[(f32, f32)], width: f32) -> Option<Mesh> {
    if points.len() < 2 {
        return None;
    }
    let mut b = LyonPath::builder();
    b.begin(lpoint(points[0].0, points[0].1));
    for p in &points[1..] {
        b.line_to(lpoint(p.0, p.1));
    }
    b.end(false);
    let path = b.build();

    let mut buf: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = StrokeTessellator::new();
    let opts = StrokeOptions::default()
        .with_line_width(width)
        .with_line_cap(LineCap::Round)
        .with_line_join(LineJoin::Round)
        .with_tolerance(0.1);
    tess.tessellate_path(
        &path,
        &opts,
        &mut BuffersBuilder::new(&mut buf, |v: StrokeVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    )
    .ok()?;
    buffers_to_mesh(buf)
}

/// Pack tessellation output into a Bevy triangle mesh.
fn buffers_to_mesh(buf: VertexBuffers<[f32; 2], u32>) -> Option<Mesh> {
    if buf.indices.is_empty() {
        return None;
    }
    let positions: Vec<[f32; 3]> = buf.vertices.iter().map(|v| [v[0], v[1], 0.0]).collect();
    let normals: Vec<[f32; 3]> = vec![[0.0, 0.0, 1.0]; positions.len()];
    let uvs: Vec<[f32; 2]> = vec![[0.0, 0.0]; positions.len()];
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(buf.indices));
    Some(mesh)
}

/// Map a [`whiteboard_core::interaction::Tool`] to its [`Icon`].
pub fn tool_icon(tool: whiteboard_core::interaction::Tool) -> Option<Icon> {
    use whiteboard_core::interaction::Tool;
    Some(match tool {
        Tool::Select => Icon::Select,
        Tool::Freedraw => Icon::Freedraw,
        Tool::Rectangle => Icon::Rectangle,
        Tool::Ellipse => Icon::Ellipse,
        Tool::Diamond => Icon::Diamond,
        Tool::Line => Icon::Line,
        Tool::Arrow => Icon::Arrow,
        Tool::Text => Icon::Text,
        Tool::Eraser => Icon::Eraser,
        _ => return None,
    })
}
