//! Turn a whiteboard `RenderScene` (a flat list of backend-neutral
//! `DrawCommand`s) into Bevy 2d entities under a parent `content_root`.
//!
//! Fills and strokes are tessellated into triangle meshes with `lyon` (proper
//! round caps/joins, cubic-bezier flattening); text becomes `Text2d`. Every
//! spawned entity carries [`WbRendered`] so the caller can clear the previous
//! frame's geometry before re-rendering.
//!
//! Coordinate spaces: the whiteboard scene is y-down; `content_root`-local space
//! (like every other pane kind here) is y-up, so we negate y per vertex. Paint
//! order (first command = bottom) is preserved by assigning each command a
//! strictly increasing local z within a small span that stays *below* the pane's
//! title cover.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh2d, PrimitiveTopology};
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::sprite_render::{AlphaMode2d, ColorMaterial, MeshMaterial2d};

use lyon::math::point as lpoint;
use lyon::path::Path as LyonPath;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, LineCap as LCap, LineJoin as LJoin,
    StrokeOptions, StrokeTessellator, StrokeVertex, VertexBuffers,
};

use whiteboard_core::geometry::{Path, PathSegment, Point, Transform as WbTransform};
use whiteboard_core::render::{
    Color as WbColor, DrawCommand, LineCap, LineJoin, Paint, RenderScene, Stroke,
};

/// Marks an entity spawned by [`render_scene_into`] so it can be cleared on the
/// next rebuild.
#[derive(Component)]
pub struct WbRendered;

/// Total local-z span used for paint ordering. Kept under the pane title-cover's
/// z (0.26 relative to the pane root, content_root sits at 0.2) so drawn content
/// never paints over the chrome even if a stroke wanders up into the title band.
const Z_SPAN: f32 = 0.05;

/// Render `scene` into fresh child entities of `parent`. The caller is
/// responsible for despawning any previous [`WbRendered`] children first.
pub fn render_scene_into(
    scene: &RenderScene,
    parent: Entity,
    font: &Handle<Font>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    commands: &mut Commands,
) {
    render_scene_into_layer(scene, parent, font, None, meshes, materials, commands)
}

/// Like [`render_scene_into`] but stamps every spawned entity with `layer`
/// when `Some`. Pane hosts pass `None` and rely on jim-pane's RenderLayers
/// propagation; the canvas background passes its overlay layer so the drawing
/// renders on a dedicated camera ABOVE all panes (children of a non-pane root
/// get no propagation, so the layer must be set explicitly here).
#[allow(clippy::too_many_arguments)]
pub fn render_scene_into_layer(
    scene: &RenderScene,
    parent: Entity,
    font: &Handle<Font>,
    layer: Option<&bevy::camera::visibility::RenderLayers>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    commands: &mut Commands,
) {
    let n = scene.commands.len().max(1) as f32;

    // Transform stack. Geometry commands are emitted in pre-transform space; the
    // active transform maps them toward screen space (matches every backend's
    // push/pop discipline). For an identity-viewport pane this stays identity.
    let mut cur = WbTransform::IDENTITY;
    let mut stack: Vec<WbTransform> = Vec::new();
    let mut warned_image = false;

    for (i, cmd) in scene.commands.iter().enumerate() {
        let z = (i as f32 + 1.0) / (n + 1.0) * Z_SPAN;
        match cmd {
            DrawCommand::PushTransform(t) => {
                stack.push(cur);
                cur = t.then(&cur);
            }
            DrawCommand::PopTransform => {
                cur = stack.pop().unwrap_or(WbTransform::IDENTITY);
            }
            // Frame clipping is not exposed (no Frame tool); ignore clip commands
            // rather than mis-clipping.
            DrawCommand::PushClip(_) | DrawCommand::PopClip => {}
            DrawCommand::FillPath { path, paint } => {
                if let Some(mesh) = tessellate_fill(path, &cur) {
                    spawn_mesh(
                        mesh, paint_color(paint), z, parent, layer, meshes, materials, commands,
                    );
                }
            }
            DrawCommand::StrokePath {
                path,
                stroke,
                paint,
            } => {
                if let Some(mesh) = tessellate_stroke(path, stroke, &cur) {
                    spawn_mesh(
                        mesh, paint_color(paint), z, parent, layer, meshes, materials, commands,
                    );
                }
            }
            DrawCommand::DrawText { run, paint } => {
                spawn_text(run, paint_color(paint), &cur, z, font, parent, layer, commands);
            }
            DrawCommand::DrawImage { .. } => {
                if !warned_image {
                    warned_image = true;
                    eprintln!(
                        "[jim-whiteboard] DrawImage encountered but image rendering is not \
                         implemented; skipping. (No image tool is exposed, so this should be \
                         unreachable.)"
                    );
                }
            }
        }
    }
}

fn paint_color(paint: &Paint) -> Color {
    match paint {
        Paint::Solid(c) => wb_to_bevy(*c),
    }
}

fn wb_to_bevy(c: WbColor) -> Color {
    Color::srgba(
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        c.a as f32 / 255.0,
    )
}

/// Average linear scale of a transform (so a stroke width drawn through a zoomed
/// viewport thickens with the zoom). 1.0 for the identity-viewport pane.
fn transform_scale(t: &WbTransform) -> f64 {
    t.determinant().abs().sqrt()
}

/// Map a scene point through the active transform into content-root-local space
/// (y-up). Returns a lyon point.
#[inline]
fn mapped(t: &WbTransform, p: Point) -> lyon::math::Point {
    let q = t.apply(p);
    lpoint(q.x as f32, -q.y as f32)
}

/// Build a lyon path from a whiteboard path, applying `t` to every point. Each
/// subpath (begun by a `MoveTo`) is `end()`-ed; `Close` ends it closed.
fn build_lyon(path: &Path, t: &WbTransform) -> Option<LyonPath> {
    let mut b = LyonPath::builder();
    let mut open = false;
    for seg in &path.segments {
        match seg {
            PathSegment::MoveTo(p) => {
                if open {
                    b.end(false);
                }
                b.begin(mapped(t, *p));
                open = true;
            }
            PathSegment::LineTo(p) => {
                if !open {
                    b.begin(mapped(t, *p));
                    open = true;
                } else {
                    b.line_to(mapped(t, *p));
                }
            }
            PathSegment::CubicTo { c1, c2, to } => {
                if !open {
                    b.begin(mapped(t, *c1));
                    open = true;
                }
                b.cubic_bezier_to(mapped(t, *c1), mapped(t, *c2), mapped(t, *to));
            }
            PathSegment::Close => {
                if open {
                    b.end(true);
                    open = false;
                }
            }
        }
    }
    if open {
        b.end(false);
    }
    let built = b.build();
    if built.iter().next().is_none() {
        None
    } else {
        Some(built)
    }
}

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

fn tessellate_fill(path: &Path, t: &WbTransform) -> Option<Mesh> {
    let lpath = build_lyon(path, t)?;
    let mut buf: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = FillTessellator::new();
    let opts = FillOptions::default().with_tolerance(0.15);
    tess.tessellate_path(
        &lpath,
        &opts,
        &mut BuffersBuilder::new(&mut buf, |v: FillVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    )
    .ok()?;
    buffers_to_mesh(buf)
}

fn tessellate_stroke(path: &Path, stroke: &Stroke, t: &WbTransform) -> Option<Mesh> {
    let lpath = build_lyon(path, t)?;
    let scale = transform_scale(t);
    let width = (stroke.width * scale).max(0.35) as f32;
    let mut buf: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = StrokeTessellator::new();
    let opts = StrokeOptions::default()
        .with_line_width(width)
        .with_line_cap(map_cap(stroke.cap))
        .with_line_join(map_join(stroke.join))
        .with_tolerance(0.15);
    tess.tessellate_path(
        &lpath,
        &opts,
        &mut BuffersBuilder::new(&mut buf, |v: StrokeVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    )
    .ok()?;
    buffers_to_mesh(buf)
}

fn map_cap(c: LineCap) -> LCap {
    match c {
        LineCap::Round => LCap::Round,
        LineCap::Butt => LCap::Butt,
        LineCap::Square => LCap::Square,
    }
}

fn map_join(j: LineJoin) -> LJoin {
    match j {
        LineJoin::Round => LJoin::Round,
        LineJoin::Miter => LJoin::MiterClip,
        LineJoin::Bevel => LJoin::Bevel,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_mesh(
    mesh: Mesh,
    color: Color,
    z: f32,
    parent: Entity,
    layer: Option<&bevy::camera::visibility::RenderLayers>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    commands: &mut Commands,
) {
    let mesh_h = meshes.add(mesh);
    let mat_h = materials.add(ColorMaterial {
        color,
        alpha_mode: AlphaMode2d::Blend,
        ..default()
    });
    let mut e = commands.spawn((
        Mesh2d(mesh_h),
        MeshMaterial2d(mat_h),
        Transform::from_xyz(0.0, 0.0, z),
        WbRendered,
        ChildOf(parent),
    ));
    if let Some(layer) = layer {
        e.insert(layer.clone());
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn spawn_text(
    run: &whiteboard_core::text::TextRun,
    color: Color,
    t: &WbTransform,
    z: f32,
    font: &Handle<Font>,
    parent: Entity,
    layer: Option<&bevy::camera::visibility::RenderLayers>,
    commands: &mut Commands,
) {
    if run.text.is_empty() {
        return;
    }
    let scale = transform_scale(t) as f32;
    let ascent = run.font.size * run.font.line_height.max(1.0) * 0.0 + run.font.size * 0.8;
    // origin is baseline-left; top of the glyph box ≈ baseline - ascent.
    let top = Point::new(run.origin.x, run.origin.y - ascent);
    let p = t.apply(top);
    let mut e = commands.spawn((
        Text2d::new(run.text.clone()),
        TextFont {
            font: (font.clone()).into(),
            font_size: FontSize::Px((run.font.size as f32 * scale).max(4.0)),
            ..default()
        },
        TextColor(color),
        Anchor::TOP_LEFT,
        Transform::from_xyz(p.x as f32, -p.y as f32, z),
        WbRendered,
        ChildOf(parent),
    ));
    if let Some(layer) = layer {
        e.insert(layer.clone());
    }
}
