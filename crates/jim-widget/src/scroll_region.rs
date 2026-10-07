//! Widget scroll regions: a box inside a pane that scrolls on its own.
//!
//! A pane clips its content with its camera's viewport, and nothing clips
//! INSIDE a pane — so a region cannot simply move its children up: they
//! would draw over whatever sits above the box. Each overflowing
//! `Element::Scroll` therefore gets:
//!
//! - its own render layer (from the pane [`PaneLayerAllocator`]), carried by
//!   a [`LayerRoot`] on the region's content root, so layer stamping puts
//!   the region's content there and not on the pane camera;
//! - a camera that draws that layer into a texture exactly the size of the
//!   box, positioned over the content at the current scroll offset;
//! - a sprite in the pane showing the texture where the box is.
//!
//! The texture is what clips, and it is ordinary pane content, so stacking
//! against other panes is whatever the pane's own is. Drawing the region
//! camera straight to the window instead would need a camera order between
//! its pane's camera and the next pane up, and pane orders are packed with
//! no room between them.
//!
//! Region cameras render in [`REGION_CAMERA_ORDER`]'s band, before every
//! window camera and before the slide-view picture band (which photographs
//! panes, region textures included), so a texture is always complete when
//! it is sampled.
//!
//! Scrolling a region moves its camera and re-merges its hit targets; it
//! does not re-render the widget.

use std::collections::{HashMap, HashSet};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ClearColorConfig, ImageRenderTarget, RenderTarget};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use bevy::sprite::Anchor;
use jim_pane::camera::{LayerRoot, PaneCameraOf};
use jim_pane::PaneLayerAllocator;

use crate::WidgetTargets;

/// Base of the region cameras' order band. Below the slide-view picture
/// band (-100_000..-1) and every window camera (0+).
pub const REGION_CAMERA_ORDER: isize = -200_000;

/// Depth of the texture sprite above the region element.
const SPRITE_DZ: f32 = 0.005;

/// Every live region, per pane.
#[derive(Resource, Default)]
pub struct ScrollRegions {
    by_pane: HashMap<Entity, PaneRegions>,
}

#[derive(Default)]
struct PaneRegions {
    regions: HashMap<String, Region>,
    /// Lengths of the pane's own (non-region) target lists after its last
    /// render — region targets are appended past these and replaced on
    /// every scroll.
    base: BaseLens,
}

struct Region {
    layer: usize,
    camera: Entity,
    image: Handle<Image>,
    image_px: UVec2,
    scale: f32,
    rect: Rect,
    content_h: f32,
    z: f32,
    root: Entity,
    local: WidgetTargets,
    scroll_y: f32,
}

impl Region {
    fn max_scroll(&self) -> f32 {
        (self.content_h - self.rect.height()).max(0.0)
    }
}

/// Marks a region's camera.
#[derive(Component)]
pub struct ScrollRegionCamera;

/// What reconciling a pane's regions needs besides `Commands` and images.
#[derive(SystemParam)]
pub struct ScrollRegionHost<'w> {
    regions: ResMut<'w, ScrollRegions>,
    layers: ResMut<'w, PaneLayerAllocator>,
}

impl ScrollRegionHost<'_> {
    /// Give every overflowing region from `pane`'s latest render its layer,
    /// camera, texture and sprite (creating or updating them), drop regions
    /// that are gone, and merge region targets into `targets`.
    ///
    /// Call right after `render::render` for the pane. `scale` is the
    /// window's scale factor: the texture is rendered at device resolution,
    /// and its text is shaped at the same factor it was measured at.
    #[allow(clippy::too_many_arguments)]
    pub fn reconcile(
        &mut self,
        commands: &mut Commands,
        images: &mut Assets<Image>,
        pane: Entity,
        content_root: Entity,
        targets: &mut WidgetTargets,
        scale: f32,
    ) {
        let found = std::mem::take(&mut targets.scroll_regions);
        if found.is_empty() && !self.regions.by_pane.contains_key(&pane) {
            return;
        }
        let entry = self.regions.by_pane.entry(pane).or_default();
        let mut seen: HashSet<String> = HashSet::new();
        for r in found {
            if !seen.insert(r.key.clone()) {
                eprintln!(
                    "[widget] two scroll regions share the key {:?}; give each a distinct `id`",
                    r.key
                );
                continue;
            }
            let px = (r.rect.size() * scale).ceil().as_uvec2().max(UVec2::ONE);
            let region = entry.regions.entry(r.key.clone()).or_insert_with(|| {
                let layer = self.layers.allocate();
                let image = images.add(target_image(px));
                let camera = commands
                    .spawn((
                        Camera2d,
                        Camera {
                            order: REGION_CAMERA_ORDER + layer as isize,
                            clear_color: ClearColorConfig::Custom(r.ground),
                            ..default()
                        },
                        image_target(&image, scale),
                        // An image target is single-sampled; the default
                        // `Sample4` against it is a fatal validation error.
                        Msaa::Off,
                        RenderLayers::from_layers(&[layer]),
                        ScrollRegionCamera,
                    ))
                    .id();
                Region {
                    layer,
                    camera,
                    image,
                    image_px: px,
                    scale,
                    rect: r.rect,
                    content_h: r.content_h,
                    z: r.z,
                    root: r.root,
                    local: WidgetTargets::default(),
                    scroll_y: 0.0,
                }
            });
            if region.image_px != px || region.scale != scale {
                // Replace the asset behind the same handle: the sprite and
                // camera keep pointing at it.
                images.insert(region.image.id(), target_image(px)).ok();
                commands
                    .entity(region.camera)
                    .insert(image_target(&region.image, scale));
                region.image_px = px;
                region.scale = scale;
            }
            commands.entity(region.camera).insert(Camera {
                order: REGION_CAMERA_ORDER + region.layer as isize,
                clear_color: ClearColorConfig::Custom(r.ground),
                ..default()
            });
            region.rect = r.rect;
            region.content_h = r.content_h;
            region.z = r.z;
            region.root = r.root;
            region.local = r.targets;
            region.scroll_y = region.scroll_y.clamp(0.0, region.max_scroll());

            commands.entity(r.root).insert((
                LayerRoot(region.layer),
                RenderLayers::from_layers(&[region.layer]),
            ));
            commands.spawn((
                ChildOf(content_root),
                Sprite {
                    image: region.image.clone(),
                    custom_size: Some(r.rect.size()),
                    ..default()
                },
                Anchor::TOP_LEFT,
                Transform::from_xyz(r.rect.min.x, -r.rect.min.y, r.z + SPRITE_DZ),
            ));
        }
        let layers = &mut self.layers;
        entry.regions.retain(|key, region| {
            let keep = seen.contains(key);
            if !keep {
                release(commands, images, layers, region);
            }
            keep
        });
        entry.base = BaseLens::of(targets);
        merge(entry, targets);
        if entry.regions.is_empty() {
            self.regions.by_pane.remove(&pane);
        }
    }

    /// Scroll the region under `pt` (content_root-local, pre-scroll) by
    /// `dy` pixels (positive = content moves up). Returns false when no
    /// scrollable region is under the point, so the caller scrolls the pane
    /// instead. Re-merges the pane's targets at the new offset.
    pub fn scroll_at(&mut self, pane: Entity, pt: Vec2, dy: f32, targets: &mut WidgetTargets) -> bool {
        let Some(entry) = self.regions.by_pane.get_mut(&pane) else {
            return false;
        };
        let Some(region) = entry
            .regions
            .values_mut()
            .find(|r| r.rect.contains(pt) && r.max_scroll() > 0.0)
        else {
            return false;
        };
        let want = (region.scroll_y + dy).clamp(0.0, region.max_scroll());
        if want != region.scroll_y {
            region.scroll_y = want;
            merge(entry, targets);
        }
        true
    }

    /// Free the regions of panes that no longer exist.
    fn forget_dead(&mut self, commands: &mut Commands, images: &mut Assets<Image>, alive: impl Fn(Entity) -> bool) {
        let dead: Vec<Entity> = self.regions.by_pane.keys().copied().filter(|p| !alive(*p)).collect();
        for pane in dead {
            if let Some(entry) = self.regions.by_pane.remove(&pane) {
                for region in entry.regions.values() {
                    release(commands, images, &mut self.layers, region);
                }
            }
        }
    }
}

fn release(commands: &mut Commands, images: &mut Assets<Image>, layers: &mut PaneLayerAllocator, region: &Region) {
    commands.entity(region.camera).try_despawn();
    images.remove(region.image.id());
    layers.free(region.layer);
}

fn target_image(size: UVec2) -> Image {
    let mut image = Image::new_target_texture(size.x.max(1), size.y.max(1), TextureFormat::Bgra8UnormSrgb, None);
    // Never read on the CPU.
    image.asset_usage = RenderAssetUsages::RENDER_WORLD;
    image
}

fn image_target(image: &Handle<Image>, scale_factor: f32) -> RenderTarget {
    RenderTarget::Image(ImageRenderTarget {
        handle: image.clone(),
        scale_factor,
    })
}

/// Lengths of every positional target list — the pane's own targets end
/// here, region targets follow.
#[derive(Default, Clone, Copy)]
struct BaseLens {
    clicks: usize,
    links: usize,
    spans: usize,
    sliders: usize,
    selects: usize,
    tooltips: usize,
    dialogs: usize,
    popovers: usize,
    toasts: usize,
    anims: usize,
    hover_washes: usize,
    context_menus: usize,
}

impl BaseLens {
    fn of(t: &WidgetTargets) -> Self {
        Self {
            clicks: t.clicks.len(),
            links: t.links.len(),
            spans: t.spans.len(),
            sliders: t.sliders.len(),
            selects: t.selects.len(),
            tooltips: t.tooltips.len(),
            dialogs: t.dialogs.len(),
            popovers: t.popovers.len(),
            toasts: t.toasts.len(),
            anims: t.anims.len(),
            hover_washes: t.hover_washes.len(),
            context_menus: t.context_menus.len(),
        }
    }

    fn truncate(&self, t: &mut WidgetTargets) {
        t.clicks.truncate(self.clicks);
        t.links.truncate(self.links);
        t.spans.truncate(self.spans);
        t.sliders.truncate(self.sliders);
        t.selects.truncate(self.selects);
        t.tooltips.truncate(self.tooltips);
        t.dialogs.truncate(self.dialogs);
        t.popovers.truncate(self.popovers);
        t.toasts.truncate(self.toasts);
        t.anims.truncate(self.anims);
        t.hover_washes.truncate(self.hover_washes);
        t.context_menus.truncate(self.context_menus);
    }
}

/// Replace the region part of `targets` with every region's targets at its
/// current scroll: moved into pane content space and limited to the box.
/// Hit areas are clipped to the box, so a row scrolled half out of view is
/// clickable only where it shows; anchors and text spans keep their full
/// rect (menus position from it, selection maps characters across it) and
/// are dropped only once wholly outside.
fn merge(entry: &PaneRegions, targets: &mut WidgetTargets) {
    entry.base.truncate(targets);
    for region in entry.regions.values() {
        let offset = region.rect.min - Vec2::new(0.0, region.scroll_y);
        let clip = region.rect;
        let moved = |r: Rect| Rect::from_corners(r.min + offset, r.max + offset);
        let shown = |r: Rect| {
            let m = moved(r).intersect(clip);
            (!m.is_empty()).then_some(m)
        };
        let visible = |r: Rect| !moved(r).intersect(clip).is_empty();
        let l = &region.local;
        for c in &l.clicks {
            if let Some(rect) = shown(c.rect) {
                targets.clicks.push(crate::ClickTarget { rect, ..c.clone() });
            }
        }
        for k in &l.links {
            if let Some(rect) = shown(k.rect) {
                targets.links.push(crate::LinkTarget { rect, ..k.clone() });
            }
        }
        for s in &l.spans {
            if visible(s.rect) {
                targets.spans.push(crate::TextSpan { rect: moved(s.rect), ..s.clone() });
            }
        }
        for s in &l.sliders {
            if let Some(rect) = shown(s.rect) {
                targets.sliders.push(crate::SliderTarget {
                    rect,
                    value_x0: s.value_x0 + offset.x,
                    ..s.clone()
                });
            }
        }
        for s in &l.selects {
            if visible(s.anchor) {
                targets.selects.push(crate::SelectTarget { anchor: moved(s.anchor), ..s.clone() });
            }
        }
        for t in &l.tooltips {
            if let Some(anchor) = shown(t.anchor) {
                targets.tooltips.push(crate::TooltipTarget { anchor, ..t.clone() });
            }
        }
        for p in &l.popovers {
            if visible(p.anchor) {
                targets.popovers.push(crate::PopoverTarget { anchor: moved(p.anchor), ..p.clone() });
            }
        }
        for w in &l.hover_washes {
            if let Some(rect) = shown(w.rect) {
                targets.hover_washes.push(crate::HoverWash {
                    rect,
                    region: Some(crate::RegionWash {
                        root: region.root,
                        layer: region.layer,
                        rect: w.rect,
                        z: w.z,
                    }),
                    ..w.clone()
                });
            }
        }
        for c in &l.context_menus {
            if let Some(rect) = shown(c.rect) {
                targets.context_menus.push(crate::ContextTarget { rect, ..c.clone() });
            }
        }
        // Not positional: they apply wherever the content is.
        targets.dialogs.extend(l.dialogs.iter().cloned());
        targets.toasts.extend(l.toasts.iter().cloned());
        targets.anims.extend(l.anims.iter().cloned());
    }
}

/// Place each region camera over its content at the current scroll, and
/// run it only while its pane's camera runs.
///
/// PostUpdate, after transform propagation (the region root's world
/// position is only final then) and before frusta/visibility (which read
/// the camera's transform). The camera has no parent, so its
/// `GlobalTransform` is written directly — waiting a frame for propagation
/// would make the region lag a frame behind a pane being dragged.
#[allow(clippy::type_complexity)]
pub fn sync_region_cameras(
    regions: Res<ScrollRegions>,
    roots: Query<(&GlobalTransform, &InheritedVisibility), Without<ScrollRegionCamera>>,
    mut cams: Query<(&mut Transform, &mut GlobalTransform, &mut Camera, &mut Projection), With<ScrollRegionCamera>>,
    pane_cams: Query<(&PaneCameraOf, &Camera), Without<ScrollRegionCamera>>,
) {
    for (pane, entry) in &regions.by_pane {
        let pane_active = pane_cams
            .iter()
            .find(|(of, _)| of.0 == *pane)
            .is_some_and(|(_, cam)| cam.is_active);
        for region in entry.regions.values() {
            let Ok((mut t, mut gt, mut cam, mut proj)) = cams.get_mut(region.camera) else {
                continue;
            };
            let Ok((root_gt, root_vis)) = roots.get(region.root) else {
                if cam.is_active {
                    cam.is_active = false;
                }
                continue;
            };
            let (scale, _, origin) = root_gt.to_scale_rotation_translation();
            let zoom = scale.x;
            let size = region.rect.size();
            let center = origin
                + Vec3::new(
                    size.x * 0.5 * zoom,
                    -(size.y * 0.5 + region.scroll_y) * zoom,
                    0.0,
                );
            let want = Transform::from_translation(center);
            if *t != want {
                *t = want;
                *gt = GlobalTransform::from(want);
            }
            if let Projection::Orthographic(ortho) = &mut *proj {
                if ortho.scale != zoom {
                    ortho.scale = zoom;
                }
            }
            let active = pane_active && root_vis.get();
            if cam.is_active != active {
                cam.is_active = active;
            }
        }
    }
}

/// Free the regions of panes that are gone.
pub fn forget_dead_region_panes(
    mut commands: Commands,
    mut host: ScrollRegionHost,
    mut images: ResMut<Assets<Image>>,
    panes: Query<(), With<jim_pane::PaneTag>>,
) {
    host.forget_dead(&mut commands, &mut images, |p| panes.get(p).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(rect: Rect, content_h: f32, scroll_y: f32, local: WidgetTargets) -> Region {
        Region {
            layer: 1,
            camera: Entity::PLACEHOLDER,
            image: Handle::default(),
            image_px: UVec2::ONE,
            scale: 1.0,
            rect,
            content_h,
            z: 1.0,
            root: Entity::PLACEHOLDER,
            local,
            scroll_y,
        }
    }

    fn click(id: &str, y0: f32, y1: f32) -> crate::ClickTarget {
        crate::ClickTarget {
            id: id.into(),
            kind: crate::ClickKind::Button,
            rect: Rect::new(0.0, y0, 200.0, y1),
        }
    }

    fn pane_with(region_: Region, own: Vec<crate::ClickTarget>) -> (PaneRegions, WidgetTargets) {
        let targets = WidgetTargets {
            clicks: own,
            ..Default::default()
        };
        let mut entry = PaneRegions::default();
        entry.base = BaseLens::of(&targets);
        entry.regions.insert("sidebar".into(), region_);
        (entry, targets)
    }

    /// A region row is hit where it SHOWS: moved into pane space by the
    /// region's position and scroll, and cut to the box.
    #[test]
    fn region_targets_follow_the_scroll_and_are_clipped_to_the_box() {
        let local = WidgetTargets {
            clicks: vec![click("row12", 250.0, 270.0), click("row0", 0.0, 20.0)],
            ..Default::default()
        };
        // Box at (10,100)-(210,300): 200px tall over 800px of content.
        let rect = Rect::new(10.0, 100.0, 210.0, 300.0);
        let (mut entry, mut targets) = pane_with(region(rect, 800.0, 0.0, local), vec![click("header", 0.0, 30.0)]);

        merge(&entry, &mut targets);
        let ids: Vec<&str> = targets.clicks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["header", "row0"], "row12 is below the box at scroll 0");

        entry.regions.get_mut("sidebar").unwrap().scroll_y = 160.0;
        merge(&entry, &mut targets);
        let ids: Vec<&str> = targets.clicks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["header", "row12"], "re-merging replaces, never accumulates");
        let row = &targets.clicks[1].rect;
        // 100 (box top) + 250 (row in content) - 160 (scroll) = 190.
        assert_eq!((row.min.x, row.min.y, row.max.y), (10.0, 190.0, 210.0));
    }

    #[test]
    fn a_row_half_out_of_the_box_is_clickable_only_where_it_shows() {
        let local = WidgetTargets {
            clicks: vec![click("edge", 190.0, 230.0)],
            ..Default::default()
        };
        let rect = Rect::new(0.0, 0.0, 200.0, 200.0);
        let (entry, mut targets) = pane_with(region(rect, 800.0, 0.0, local), vec![]);
        merge(&entry, &mut targets);
        assert_eq!(targets.clicks[0].rect.max.y, 200.0);
    }
}
