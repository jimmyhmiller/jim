//! Widget button material — rounded-rect SDF with optional border and
//! soft drop shadow. One material per Button element rendered.
//!
//! All look properties come from theme tokens (via `WidgetPalette`),
//! so a single preset switch retones every button across every widget.

use bevy::asset::{AssetPath, embedded_path};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, ShaderType};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dPlugin};

pub struct WidgetButtonMaterialPlugin;

impl Plugin for WidgetButtonMaterialPlugin {
    fn build(&self, app: &mut App) {
        bevy::asset::embedded_asset!(app, "button_material.wgsl");
        app.add_plugins(Material2dPlugin::<WidgetButtonMaterial>::default())
            .add_systems(Startup, init_button_mesh);
    }
}

/// Shared unit quad mesh — every button reuses it and scales via its
/// `Transform`. One mesh, many materials.
#[derive(Resource, Clone)]
pub struct WidgetButtonMesh(pub Handle<Mesh>);

fn init_button_mesh(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    let handle = meshes.add(Rectangle::new(1.0, 1.0));
    commands.insert_resource(WidgetButtonMesh(handle));
}

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
pub struct WidgetButtonMaterial {
    #[uniform(0)]
    pub params: ButtonParams,
    /// Render in the blend phase instead of the default opaque phase.
    /// Used ONLY for the shadow companion quad (`params.shadow_only`),
    /// where the blend phase's per-pane-camera flakiness is harmless —
    /// a soft shadow that skips a frame is imperceptible, unlike a
    /// button face.
    pub blend: bool,
}

impl Material2d for WidgetButtonMaterial {
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("button_material.wgsl"))
                .with_source("embedded"),
        )
    }
    fn alpha_mode(&self) -> AlphaMode2d {
        // OPAQUE by default. Blend-mode Mesh2d is unreliable through per-pane
        // cameras (widget quads intermittently don't draw until the next
        // re-render — the Bevy 0.19 issue the vector paths dodge by flattening
        // alpha), and alpha-MASK is no better: switching to it to discard the
        // pixels outside the rounded rect made every panel face disappear and
        // the pane read as transparent. So translucency is composited in the
        // shader against `ButtonParams::ground` and the quad renders in the
        // dependable opaque phase. The cost is that the pixels OUTSIDE the
        // rounded shape are painted in the assumed ground, which shows as a
        // flat box whenever that guess is wrong (a control on a gradient or a
        // shader wash). Fixing that needs real geometry — a rounded-rect mesh
        // instead of an SDF in a quad — not another alpha mode.
        // Shadow companion quads opt into Blend (see `blend`); a soft halo
        // skipping a frame is imperceptible.
        if self.blend {
            AlphaMode2d::Blend
        } else {
            AlphaMode2d::Opaque
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, ShaderType)]
pub struct ButtonParams {
    /// Mesh extent in pixels (button + 2 × shadow_blur on each axis).
    pub mesh_size: Vec2,
    /// The clickable button rect inside the mesh.
    pub button_size: Vec2,
    pub corner_radius: f32,
    pub border_width: f32,
    pub bg: Vec4,
    pub border: Vec4,
    /// `(r, g, b, base_alpha)` — shadow at the rect edge.
    pub shadow_color: Vec4,
    pub shadow_blur: f32,
    pub shadow_offset_y: f32,
    /// > 0.5: this quad draws ONLY the soft shadow (outside the rect;
    /// the opaque body quad owns the inside pixels) with real alpha —
    /// pair with `WidgetButtonMaterial::blend`.
    pub shadow_only: f32,
    pub _pad1: f32,
    /// What sits visually behind this panel — the shader composites all
    /// translucency (corner AA, shadow falloff, semi-transparent fills)
    /// against this for the pixels it covers. See `alpha_mode` above.
    pub ground: Vec4,
}
