//! Host-side runtime for Glaze shader layers (Stage 3b).
//!
//! A widget sends a `Style.shader` carrying the WGSL fragment body the `glaze`
//! compiler produced. We wrap it in the canonical `GlazeUniforms` block, add it
//! to `Assets<Shader>` (cached by content hash), and run it on a quad at the
//! element's rect via a `Material2d` whose per-instance shader handle is pinned
//! in `specialize()` (same trick as `jim_style::DynamicMaterial`).
//!
//! `time`/`dt` are bumped every frame by `update_glaze_materials` — but ONLY on
//! the materials whose shader body actually reads the clock (see
//! [`GlazeAnimates`]), so a static shader layer is never re-uploaded to the GPU
//! on frames where nothing about it changed. Animation stays independent of the
//! (event-driven) widget content rebuild.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};

/// Content-local bounds and owning element for one live Glaze shader layer.
/// This stays on the material entity across frames, allowing interaction
/// uniforms to update without rebuilding the widget tree.
#[derive(Component, Clone, Debug)]
pub struct GlazeInteractionTarget {
    pub pane: Entity,
    pub element_id: Option<String>,
    pub rect: Rect,
}

/// Whether this layer's shader actually reads the per-frame clock
/// (`u.time`/`u.dt`). Stamped on the layer entity at creation from
/// [`body_reads_clock`], and consulted by `update_glaze_materials` so it only
/// pushes `time`/`dt` into the materials that use them.
///
/// This is the same anti-pattern the pane crate gated with `ChromeAnimates`
/// (`jim_pane::chrome_material`): an unconditional `Assets::iter_mut()` (or an
/// unconditional per-handle `get_mut`) marks every material `Changed`, forcing
/// the render world to recreate and re-upload every material's bind-group
/// buffer to the GPU on every rendered frame — even for shaders that never read
/// `time`/`dt`, and even for invisible panes. For a static Glaze layer (a
/// gradient, an inset shadow, or a `shader {}` that only reads
/// `hover`/`focus`/`size`) that upload is pure waste.
///
/// **Deriving the bit.** The Glaze compiler already computes exactly which
/// builtins a body reads (`glaze::CompiledShader::used`), which would be the
/// authoritative source — but that metadata is dropped at the protocol
/// boundary: a widget ships only `ShaderSpec { body: String, overlay }`, not the
/// `used` list. So we recover it host-side by scanning the fragment body for the
/// canonical uniform accessors the compiler emits (`Builtin::field()` maps
/// `time -> "u.time"`, `dt -> "u.dt"`). This scan also uniformly covers the
/// bodies we generate host-side (gradients, inset shadows) that never went
/// through the Glaze compiler at all. The trade-off: a body that mentions
/// `u.time` only inside a comment would be treated as animating — harmless
/// (it just keeps the old always-push behavior for that one layer), and the
/// compiler never emits such dead references anyway.
#[derive(Component, Clone, Copy, Debug)]
pub struct GlazeAnimates(pub bool);

/// Does this compiler-produced fragment body read the per-frame clock?
///
/// Matches the exact uniform accessors the Glaze backend emits for the two
/// clock builtins (`u.time`, `u.dt`). We require the char following the field
/// name to not continue an identifier, so a hypothetical future field like
/// `u.dt_scale` wouldn't be mistaken for `u.dt`.
pub fn body_reads_clock(body: &str) -> bool {
    fn reads_field(body: &str, field: &str) -> bool {
        let bytes = body.as_bytes();
        let mut from = 0;
        while let Some(rel) = body[from..].find(field) {
            let start = from + rel;
            let after = start + field.len();
            let next_is_ident = bytes
                .get(after)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
            if !next_is_ident {
                return true;
            }
            from = start + 1;
        }
        false
    }
    reads_field(body, "u.time") || reads_field(body, "u.dt")
}

/// Canonical per-frame inputs a Glaze shader may read. Field order matches the
/// WGSL struct in [`assemble_wgsl`]; `encase` (via `ShaderType`) inserts the
/// same std140 padding the WGSL side does.
#[derive(Clone, Copy, ShaderType)]
pub struct GlazeUniforms {
    pub time: f32,
    pub dt: f32,
    pub hover: f32,
    pub focus: f32,
    pub press: f32,
    /// eased 0..1 amount of the element's `checked` discrete state, fed from
    /// the `anim::WidgetAnim` store (Glaze `transition checked …`).
    pub checked: f32,
    /// element corner radius (px) — the assembled shader masks its output to a
    /// rounded-rect of this radius so shaders don't overpaint rounded corners.
    pub radius: f32,
    pub mouse: Vec2,
    pub size: Vec2,
    pub resolution: Vec2,
}

impl Default for GlazeUniforms {
    fn default() -> Self {
        GlazeUniforms {
            time: 0.0,
            dt: 0.0,
            hover: 0.0,
            focus: 0.0,
            press: 0.0,
            checked: 0.0,
            radius: 0.0,
            mouse: Vec2::ZERO,
            size: Vec2::splat(1.0),
            resolution: Vec2::splat(1.0),
        }
    }
}

#[derive(Asset, TypePath, AsBindGroup, Clone)]
#[bind_group_data(GlazeMaterialKey)]
pub struct GlazeMaterial {
    #[uniform(0)]
    pub u: GlazeUniforms,
    /// The per-instance fragment shader. `specialize` pins it; the pipeline
    /// cache keys on the handle via [`GlazeMaterialKey`].
    pub fragment: Handle<Shader>,
}

#[derive(Hash, PartialEq, Eq, Clone)]
pub struct GlazeMaterialKey {
    fragment: Handle<Shader>,
}

impl From<&GlazeMaterial> for GlazeMaterialKey {
    fn from(m: &GlazeMaterial) -> Self {
        Self {
            fragment: m.fragment.clone(),
        }
    }
}

impl Material2d for GlazeMaterial {
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Default
    }
    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }
    fn specialize(
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        key: Material2dKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        if let Some(fragment) = descriptor.fragment.as_mut() {
            fragment.shader = key.bind_group_data.fragment.clone();
        }
        Ok(())
    }
}

/// Caches generated `Shader` assets by the hash of their WGSL body, so a widget
/// re-render (which rebuilds the Element tree) reuses the compiled shader rather
/// than adding a fresh asset every time.
#[derive(Resource, Default)]
pub struct GlazeShaderCache {
    by_hash: HashMap<u64, Handle<Shader>>,
}

impl GlazeShaderCache {
    /// Get-or-create the `Shader` handle for a fragment body.
    pub fn handle_for(&mut self, body: &str, shaders: &mut Assets<Shader>) -> Handle<Shader> {
        let mut h = DefaultHasher::new();
        body.hash(&mut h);
        let key = h.finish();
        self.by_hash
            .entry(key)
            .or_insert_with(|| {
                shaders.add(Shader::from_wgsl(
                    assemble_wgsl(body),
                    "glaze://generated.wgsl",
                ))
            })
            .clone()
    }
}

/// Wrap a compiler-produced fragment body in a complete mesh2d fragment shader
/// with the canonical uniform block.
pub fn assemble_wgsl(body: &str) -> String {
    format!(
        "#import bevy_sprite::mesh2d_vertex_output::VertexOutput\n\
         \n\
         struct GlazeUniforms {{\n\
         \x20   time: f32,\n\x20   dt: f32,\n\x20   hover: f32,\n\x20   focus: f32,\n\
         \x20   press: f32,\n\x20   checked: f32,\n\x20   radius: f32,\n\x20   mouse: vec2<f32>,\n\x20   size: vec2<f32>,\n\x20   resolution: vec2<f32>,\n\
         }};\n\
         @group(#{{MATERIAL_BIND_GROUP}}) @binding(0) var<uniform> u: GlazeUniforms;\n\
         \n\
         // the compiler-produced body, as a callable so the fragment can mask it\n\
         fn glaze_body(in: VertexOutput) -> vec4<f32> {{\n{body}}}\n\
         \n\
         @fragment\n\
         fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {{\n\
         \x20   var col = glaze_body(in);\n\
         \x20   // clip to the element's rounded-rect (so shaders don't square off corners)\n\
         \x20   let p = (in.uv - vec2<f32>(0.5, 0.5)) * u.size;\n\
         \x20   let h = u.size * 0.5;\n\
         \x20   let r = min(u.radius, min(h.x, h.y));\n\
         \x20   let q = abs(p) - h + vec2<f32>(r, r);\n\
         \x20   let d = length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;\n\
         \x20   col.a = col.a * smoothstep(0.75, -0.75, d);\n\
         \x20   return col;\n\
         }}\n"
    )
}

pub struct GlazeMaterialPlugin;

impl Plugin for GlazeMaterialPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(Material2dPlugin::<GlazeMaterial>::default())
            .init_resource::<GlazeShaderCache>()
            .add_systems(Update, update_glaze_materials);
    }
}

fn ease(current: f32, target: f32, dt: f32) -> f32 {
    let amount = 1.0 - (-14.0 * dt.max(0.0)).exp();
    current + (target - current) * amount
}

/// Like [`ease`], but snaps to the target once within a hair of it. An
/// exponential ease approaches its target asymptotically and never lands
/// exactly, so without this it would produce a vanishingly small — but nonzero,
/// hence `Changed`-marking — write on every frame *forever* after any hover /
/// focus / press. Snapping lets a settled uniform reach a fixpoint where the
/// next frame's computed value equals the stored one, so `update_glaze_materials`
/// can skip the write (and the GPU re-upload) entirely.
fn ease_settle(current: f32, target: f32, dt: f32) -> f32 {
    let next = ease(current, target, dt);
    if (target - next).abs() < 1.0e-4 {
        target
    } else {
        next
    }
}

fn normalized_mouse(rect: Rect, point: Vec2) -> Vec2 {
    let size = rect.size().max(Vec2::splat(f32::EPSILON));
    (point - rect.min) / size
}

/// Drive clocks and interaction uniforms independently of the event-driven
/// widget rebuild.
///
/// The load-bearing invariant here is **do zero asset writes for a material
/// whose uniforms would be unchanged this frame**. `Assets::get_mut` (like
/// `iter_mut`) unconditionally flags the asset `Changed`, and a `Changed`
/// `Material2d` asset forces the render world to rebuild and re-upload its
/// bind-group buffer to the GPU that frame. Previously this system did an
/// unconditional `iter_mut()` (clocks) *and* an unconditional per-layer
/// `get_mut()` (interaction easing), so every live Glaze layer — animating or
/// not, visible or not — was re-uploaded every rendered frame. Now we read each
/// material immutably, compute its next uniforms, and only `get_mut` + write the
/// ones that actually differ:
///   * clocks (`time`/`dt`) advance only on layers whose body reads them
///     ([`GlazeAnimates`]) and whose owning pane is visible;
///   * interaction uniforms advance only while an ease is in flight or the
///     mouse/`checked` value moved — a settled layer reaches a fixpoint and
///     stops writing.
fn update_glaze_materials(
    time: Res<Time>,
    windows: Query<&Window>,
    viewport: Option<Res<jim_pane::PaneViewport>>,
    anim: Option<Res<crate::anim::WidgetAnim>>,
    buttons: Res<ButtonInput<bevy::input::mouse::MouseButton>>,
    panes: Query<
        (
            Entity,
            &jim_pane::PaneRect,
            Option<&Visibility>,
            Option<&crate::WidgetScroll>,
            Option<&crate::WidgetInputFocus>,
        ),
        With<jim_pane::PaneTag>,
    >,
    layers: Query<(
        Entity,
        Option<&GlazeInteractionTarget>,
        &MeshMaterial2d<GlazeMaterial>,
        Option<&GlazeAnimates>,
    )>,
    mut mats: ResMut<Assets<GlazeMaterial>>,
    mut pressed_layers: Local<HashSet<Entity>>,
) {
    let _t_prof = jim_pane::prof::sys_span("glaze_materials");
    let t = time.elapsed_secs();
    let dt = time.delta_secs();
    let cursor_canvas = windows
        .single()
        .ok()
        .and_then(Window::cursor_position)
        .zip(viewport.as_deref())
        .map(|(pt, viewport)| viewport.window_to_canvas(pt));
    let candidates: Vec<(Entity, jim_pane::PaneRect)> = panes
        .iter()
        .filter(|(_, _, vis, _, _)| !matches!(vis, Some(Visibility::Hidden)))
        .map(|(pane, rect, _, _, _)| (pane, *rect))
        .collect();
    let topmost = cursor_canvas
        .and_then(|pt| jim_pane::topmost_pane_at(pt, &candidates).map(|pane| (pane, pt)));
    let left_down = buttons.pressed(bevy::input::mouse::MouseButton::Left);
    if buttons.just_released(bevy::input::mouse::MouseButton::Left) || !left_down {
        pressed_layers.clear();
    }

    for (entity, target, handle, animates) in &layers {
        // Read the current uniforms without marking the asset `Changed`; we only
        // take a mutable handle (and pay the GPU re-upload) if something moved.
        let Some(cur) = mats.get(&handle.0).map(|m| m.u) else {
            continue;
        };
        let mut next = cur;
        let mut dirty = false;

        // Owning-pane state (+ visibility). Standalone users such as
        // `glaze_gallery` carry no interaction target / pane → treat as visible.
        let pane_state = target.and_then(|target| panes.get(target.pane).ok());
        let visible = pane_state
            .map(|(_, _, vis, _, _)| !matches!(vis, Some(Visibility::Hidden)))
            .unwrap_or(true);

        // Per-frame clock — only for shaders that read it, and only while the
        // owning pane is visible. A layer without a [`GlazeAnimates`] bit (e.g.
        // one spawned by a code path that predates the gate) keeps the old
        // always-push behavior, which is the safe/correct fallback. Because the
        // push is re-evaluated from live state every frame, a pane that becomes
        // visible again gets a fresh `time` on its very next tick — no stale
        // animation frame — so skipping while hidden costs no correctness.
        let animates = animates.map(|a| a.0).unwrap_or(true);
        if animates && visible {
            // `time` is monotonic, so this is a real change every frame the
            // clock advances (and a no-op if the app's clock is paused).
            if next.time != t {
                next.time = t;
                dirty = true;
            }
            if next.dt != dt {
                next.dt = dt;
                dirty = true;
            }
        }

        // Interaction uniforms (widget layers only — the standalone gallery has
        // no interaction metadata).
        if let Some(target) = target {
            let pointer = match (topmost, pane_state) {
                (Some((pane, pt)), Some((_, rect, _, scroll, _))) if pane == target.pane => {
                    let mut local = jim_pane::pt_to_content_local(pt, rect);
                    local.y += scroll.map(|s| s.y).unwrap_or(0.0);
                    Some(local)
                }
                _ => None,
            };
            let hovered = pointer.is_some_and(|pt| target.rect.contains(pt));
            if buttons.just_pressed(bevy::input::mouse::MouseButton::Left) && hovered {
                pressed_layers.insert(entity);
            }
            let focused = pane_state
                .and_then(|(_, _, _, _, focus)| focus)
                .is_some_and(|focus| target.element_id.as_deref() == Some(focus.id.as_str()));

            let hover = ease_settle(cur.hover, hovered as u8 as f32, dt);
            if hover != cur.hover {
                next.hover = hover;
                dirty = true;
            }
            let focus = ease_settle(cur.focus, focused as u8 as f32, dt);
            if focus != cur.focus {
                next.focus = focus;
                dirty = true;
            }
            let press = ease_settle(
                cur.press,
                (left_down && pressed_layers.contains(&entity)) as u8 as f32,
                dt,
            );
            if press != cur.press {
                next.press = press;
                dirty = true;
            }
            if let Some(pt) = pointer {
                let mouse = normalized_mouse(target.rect, pt);
                if mouse != cur.mouse {
                    next.mouse = mouse;
                    dirty = true;
                }
            }
            // `checked` is driven by the keyed animation store (eased there, not
            // here) so it stays continuous across full widget re-renders.
            if let (Some(anim), Some(id)) = (anim.as_deref(), target.element_id.as_deref()) {
                if let Some(v) = anim.eased(target.pane, id, "checked") {
                    if v != cur.checked {
                        next.checked = v;
                        dirty = true;
                    }
                }
            }
        }

        if dirty {
            if let Some(mut m) = mats.get_mut(&handle.0) {
                m.u = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_uniforms_default_inactive() {
        let uniforms = GlazeUniforms::default();
        assert_eq!(uniforms.hover, 0.0);
        assert_eq!(uniforms.focus, 0.0);
        assert_eq!(uniforms.press, 0.0);
        assert_eq!(uniforms.mouse, Vec2::ZERO);
    }

    #[test]
    fn interaction_easing_moves_toward_target() {
        let entered = ease(0.0, 1.0, 1.0 / 60.0);
        let left = ease(entered, 0.0, 1.0 / 60.0);
        assert!(entered > 0.0 && entered < 1.0);
        assert!(left >= 0.0 && left < entered);
    }

    #[test]
    fn body_reads_clock_detects_time_and_dt() {
        assert!(body_reads_clock("return vec4<f32>(u.time, 0.0, 0.0, 1.0);"));
        assert!(body_reads_clock("let a = u.dt * 2.0;\nreturn vec4<f32>(a);"));
        // static shaders that read other builtins must NOT be treated as animated
        assert!(!body_reads_clock(
            "return vec4<f32>(u.hover, u.focus, u.size.x, 1.0);"
        ));
        assert!(!body_reads_clock("return vec4<f32>(in.uv, 0.0, 1.0);"));
    }

    #[test]
    fn body_reads_clock_ignores_identifier_extensions() {
        // A hypothetical future field that merely starts with `dt`/`time` must
        // not be mistaken for the clock fields.
        assert!(!body_reads_clock("return vec4<f32>(u.dtscale, 0.0, 0.0, 1.0);"));
        assert!(!body_reads_clock("return vec4<f32>(u.timeline, 0.0, 0.0, 1.0);"));
    }

    #[test]
    fn ease_settle_reaches_fixpoint() {
        // Once settled at the target, the next tick must reproduce it exactly so
        // the caller can skip the write (no perpetual GPU re-upload).
        let mut v = 0.0;
        for _ in 0..10_000 {
            v = ease_settle(v, 1.0, 1.0 / 60.0);
        }
        assert_eq!(v, 1.0);
        assert_eq!(ease_settle(v, 1.0, 1.0 / 60.0), 1.0);
    }

    #[test]
    fn mouse_is_normalized_within_element_bounds() {
        let rect = Rect::new(10.0, 20.0, 110.0, 70.0);
        assert_eq!(
            normalized_mouse(rect, Vec2::new(60.0, 45.0)),
            Vec2::splat(0.5)
        );
    }
}
