//! The app, drawn a second time, inside a slide.
//!
//! An `application:` slide says "look at the real app". The deck's own pane
//! IS part of the real app, so the slide it renders shows the app that
//! contains it — and that picture contains the pane, which contains the
//! picture. Any pane rendering the deck does the same. That is the whole
//! feature: a slide that is recursive when you go and look at it.
//!
//! ## Additive, and that is the point
//!
//! The first attempt produced the picture by re-pointing EVERY camera in
//! Jim at an offscreen texture and blitting that to the screen. It put the
//! whole editor downstream of this one feature: two fatal MSAA validation
//! crashes, and then a window showing a copy of the app that was missing
//! most of the app. You were no longer looking at Jim, so Jim was unusable.
//!
//! Nothing here touches how Jim draws to the screen. The app renders to the
//! window exactly as it always has. This module ADDS a second, private set
//! of cameras that draw the same content into an image of its own — the
//! same shape as `cube.rs`, which has been doing this per pane for the
//! project prism all along. The worst a bug in here can do is make one
//! slide wrong.
//!
//! ## How the picture is composed
//!
//! The window is drawn by a layer-0 camera (canvas, sidebar, pane chrome)
//! with one viewport-clipped camera per pane on top. The picture is built
//! the same way, into an image instead of the window:
//!
//! ```text
//!   order 76_000   layer 0, whole image   → canvas + sidebar + chrome
//!   order 76_001+  one per visible pane   → that pane's content, clipped
//!   order 79_999   blit scene → shown     → what the slide samples
//! ```
//!
//! [`jim_pane::camera::pane_camera_setup_for`] already takes the target's
//! size and scale factor as parameters, so the viewports come out right for
//! a smaller image without reimplementing any of the clamping.
//!
//! ## Why two images
//!
//! The deck's pane is one of the panes being photographed, and the sprite
//! showing the picture lives on the deck's render layer — so the deck's own
//! camera draws the picture INTO the picture. That is the recursion, and it
//! is also a read-write hazard if the sprite samples the image being
//! written. So the face cameras always write `scene`, a blit copies it to
//! `shown` afterwards, and the sprite samples `shown`: one frame behind,
//! which is what makes the nesting infinite instead of one level deep.
//!
//! ## MSAA
//!
//! Every camera here is `Msaa::Off`, without exception. An image render
//! target is single-sampled, Jim sets `Msaa` nowhere so cameras default to
//! `Sample4`, and Bevy treats the mismatch as a FATAL validation error —
//! it quits the app. One straggler is enough. This is the constraint
//! `cube.rs` satisfies the same way.

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{Camera, ClearColorConfig, ImageRenderTarget, RenderTarget};
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;

use jim_pane::camera::{PaneCanvasRegion, pane_camera_setup_for};
use jim_pane::{
    MARGIN, PaneChromeOverride, PaneLayer, PaneRect, PaneScreenAnchored, PaneTag, PaneViewport,
    TITLE_H,
};

/// World z for the picture inside a host pane.
///
/// It has to clear the slide's own background, and a widget's content sits
/// at its PANE's world z — 88, 262, 500 for a presenting deck — so a fixed
/// low z puts the picture behind the slide on any pane above it, which is
/// exactly how this first failed to appear. It cannot simply be enormous
/// either: `Camera2d`'s default orthographic depth range is ±1000 and
/// anything beyond is clipped away entirely.
///
/// Only the host's own render layer is drawn by the host's camera, and the
/// picture is the only thing on it that is not widget content, so one
/// constant safely below the far plane is enough.
const PICTURE_Z: f32 = 900.0;

/// The picture is rendered at the window's FULL device resolution.
///
/// It was capped (~1100px, then logical size) while it only ever appeared
/// inside a pane a few hundred pixels across. The dive makes that
/// indefensible: the overlay briefly IS your display, so anything below
/// device resolution means double-clicking visibly drops the whole screen
/// to a softer copy of itself on the very first frame.
///
/// Pixel-exact costs a device-resolution second render while an
/// `application:` slide is up. That is the price of the effect being
/// invisible at the moment it starts.
fn picture_scale(window: &Window) -> f32 {
    window.scale_factor()
}

/// How long one level of the dive takes.
const DIVE_SECONDS: f32 = 0.75;

/// Frames to keep rendering after the picture is rebuilt.
///
/// The recursion fills in ONE LEVEL PER RENDERED FRAME: the cameras draw
/// the app into `scene`, and the in-slide sprite samples `shown`, which is
/// last frame's copy. So the first frame after a slide change is blank, the
/// second is one level deep, and so on.
///
/// Jim is reactive and only draws on events, so without this a slide change
/// gets roughly the single frame the deck's own re-render asks for — the
/// picture sits empty or flat until you happen to move the mouse. A burst
/// is enough because the depth does not decay: once frames stop, `shown`
/// keeps whatever nesting it reached. Holding the loop Continuous for as
/// long as the slide is up would cost ~1.5 cores for no further depth.
const REBUILD_FRAMES: u32 = 45;

/// Camera order band, above every window pane camera (which top out at
/// 75_150) and below the whiteboard overlay (80_000). These cameras never
/// draw to the window, but orders are global and must not collide — Bevy
/// logs "unpredictable render results" every frame when they do.
const BAND_START: isize = 76_000;
const BAND_BLIT: isize = 79_999;

/// The dive overlay draws to the WINDOW, over everything — sidebar,
/// whiteboard overlay, panes — because the whole app is what appears to
/// zoom. Below the menu overlay (100_000) so a menu can never end up
/// underneath it.
const DIVE_CAMERA_ORDER: isize = 95_000;

/// The two images behind a slide's picture.
#[derive(Resource)]
struct SlidePicture {
    /// What the private cameras draw into, this frame.
    scene: Handle<Image>,
    /// A copy of `scene` from last frame — what the slide samples.
    shown: Handle<Image>,
    /// Physical size of both images.
    size: UVec2,
    /// Resolution reduction folded into the render targets' scale factor,
    /// so viewports stay in the window's logical units.
    cap: f32,
    /// Which panes the current cameras were built for, in draw order.
    ///
    /// The camera set is rebuilt only when this changes. Respawning ~10
    /// cameras every frame thrashes Bevy's view and texture caches, and
    /// this repo has already learned once that per-frame entity churn is
    /// what turns a working feature into an unusable one.
    built_for: Vec<(Entity, usize)>,
    /// Hosts the current sprites were built for.
    hosts: Vec<Entity>,
    /// Last `SlideTargets::bump` acted on. Any change means a slide moved,
    /// which needs a burst of frames even when nothing about the camera set
    /// does.
    last_bump: u64,
}

/// Is a dive in flight? Read by the shell's update-mode decision.
///
/// Jim is reactive by default and only redraws on events, so without this
/// the animation would advance one frame per mouse twitch. It is a
/// transient pin, held for [`DIVE_SECONDS`] and released.
#[derive(Resource, Default)]
pub struct SlideDive {
    active: bool,
    /// Frames left of the post-rebuild burst. See [`REBUILD_FRAMES`].
    cooldown: u32,
}

impl SlideDive {
    /// Does the loop need to keep drawing? True during a dive, and for a
    /// short burst after the picture is rebuilt so the recursion can fill.
    pub fn animating(&self) -> bool {
        self.active || self.cooldown > 0
    }
}

/// A dive in progress, `t` running 0 → 1 over exactly one LEVEL.
///
/// The whole screen appears to zoom, so this is not a transform on the
/// in-slide picture — it is a full-window overlay OF that picture whose
/// sampled window shrinks from "the entire app" to "the nested copy". At
/// `t = 0` the overlay is pixel-identical to what is already on screen
/// (one frame stale), so it appears without a seam; at `t = 1` the nested
/// copy fills the display, and that copy IS the app. Dropping the overlay
/// there leaves you one level down with nothing to give it away, which is
/// what lets you keep diving forever.
#[derive(Component)]
struct Diving {
    t: f32,
    /// The host being dived into — its content area is the zoom target.
    host: Entity,
}

/// One of this module's private cameras.
#[derive(Component)]
struct PictureCamera;

/// The full-window camera that draws a dive overlay.
///
/// Deliberately NOT a `PictureCamera`: that marker means "part of the set
/// that composes the picture", and those are despawned wholesale whenever
/// the pane set changes. Sharing it would have let a rebuild mid-dive kill
/// the overlay, and — worse — let the end of a dive despawn the chrome and
/// blit cameras the picture itself depends on.
#[derive(Component)]
struct DiveCamera;

/// A per-pane picture camera, and the pane it photographs. Kept across
/// frames and updated in place; only a change in the pane SET rebuilds.
#[derive(Component)]
struct PictureCameraOf(Entity);

/// A picture sprite shown inside this host pane.
#[derive(Component)]
struct PictureSpriteOf(Entity);

/// The sprite that shows the picture inside a host pane.
#[derive(Component)]
struct PictureSprite;

/// Everything this module does in `Update`.
///
/// Exists so the shell's update-mode decision can run AFTER it. Both live
/// in `Update` and were unordered, so Bevy was free to decide the frame
/// cadence BEFORE the burst that asks for frames was set — the request
/// would then sit unread until the next frame, which in reactive mode only
/// arrives when the user causes an event. That is precisely the "the slide
/// changed but the picture didn't fill in until I moved the mouse" bug: the
/// burst was correct and simply never observed in time.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlideViewSet;

pub struct SlideViewPlugin;

impl Plugin for SlideViewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SlideDive>()
            .add_systems(Startup, create_images)
            .add_systems(
                Update,
                (drive_picture, start_dive, apply_dive)
                    .chain()
                    .in_set(SlideViewSet)
                    .after(crate::present::PresentSet)
                    .after(crate::projects::sync_visibility),
            );
    }
}

fn target_image(size: UVec2) -> Image {
    let mut image = Image::new_target_texture(
        size.x.max(1),
        size.y.max(1),
        TextureFormat::Bgra8UnormSrgb,
        None,
    );
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

/// Claim both handles up front; a resize replaces the asset behind them.
fn create_images(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    commands.insert_resource(SlidePicture {
        scene: images.add(target_image(UVec2::ONE)),
        shown: images.add(target_image(UVec2::ONE)),
        size: UVec2::ZERO,
        cap: 1.0,
        built_for: Vec::new(),
        hosts: Vec::new(),
        last_bump: 0,
    });
}

/// Fit `source` inside `region` without cropping, preserving aspect.
fn contain(region: Vec2, source: Vec2) -> Vec2 {
    if region.x <= 0.0 || region.y <= 0.0 || source.x <= 0.0 || source.y <= 0.0 {
        return Vec2::ZERO;
    }
    source * (region.x / source.x).min(region.y / source.y)
}

/// Window pixels to the world space a pane camera sees.
///
/// Pane cameras use an unscaled orthographic projection centred on their
/// own viewport, so one world unit is one window pixel. Deriving the sprite
/// from the pane's PROJECTED (on-screen) rect therefore picks up canvas pan
/// and zoom for free.
fn to_world(screen: Vec2, window: Vec2) -> Vec2 {
    Vec2::new(screen.x - window.x * 0.5, window.y * 0.5 - screen.y)
}

/// The pane's on-screen rect: already window pixels when anchored, else
/// projected through the canvas viewport. Mirrors `jim_pane::camera`.
fn screen_rect(rect: &PaneRect, anchored: bool, viewport: &PaneViewport) -> PaneRect {
    if anchored {
        *rect
    } else {
        viewport.projected_rect(rect)
    }
}

/// The area a pane's widget occupies: its rect less the chrome insets.
fn content_rect(rect: &PaneRect, title_h: f32) -> (Vec2, Vec2) {
    let pos = rect.pos + Vec2::new(MARGIN, title_h + MARGIN);
    let size = Vec2::new(
        (rect.size.x - 2.0 * MARGIN).max(0.0),
        (rect.size.y - title_h - 2.0 * MARGIN).max(0.0),
    );
    (pos, size)
}

type PaneQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static PaneRect,
        &'static PaneLayer,
        &'static InheritedVisibility,
        Option<&'static PaneScreenAnchored>,
        Option<&'static PaneChromeOverride>,
    ),
    With<PaneTag>,
>;

/// Keep the picture's cameras and sprites matching the world.
///
/// Two rates, deliberately. The camera SET is rebuilt only when the panes
/// being photographed change — a slide change, a pane opening or closing.
/// Their viewports and transforms are updated every frame, which is what
/// `jim_pane::camera::sync_pane_cameras` does for the real ones and costs a
/// few field writes. Respawning them per frame instead would churn Bevy's
/// view caches for no benefit.
#[allow(clippy::too_many_arguments)]
fn drive_picture(
    mut commands: Commands,
    mut picture: ResMut<SlidePicture>,
    mut dive: ResMut<SlideDive>,
    mut images: ResMut<Assets<Image>>,
    targets: Res<crate::slide_targets::SlideTargets>,
    windows: Query<&Window>,
    viewport: Option<Res<PaneViewport>>,
    region: Option<Res<PaneCanvasRegion>>,
    panes: PaneQuery,
    mut cameras: Query<(Entity, &PictureCameraOf, &mut Camera, &mut Transform)>,
    mut sprites: Query<
        (Entity, &PictureSpriteOf, &mut Sprite, &mut Transform),
        Without<PictureCameraOf>,
    >,
    all_owned: Query<Entity, Or<(With<PictureCamera>, With<PictureSprite>)>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let logical = Vec2::new(window.width(), window.height());
    if logical.x <= 0.0 || logical.y <= 0.0 {
        return;
    }

    // Every pane whose deck is on an `application:`/`project:` slide. NOT
    // gated on presenting: "any pane that has that presentation should show
    // it" — a deck sitting on the canvas recurses too, which is also the
    // only way to look at one while using the app normally.
    let mut hosts: Vec<Entity> = targets
        .by_host
        .keys()
        .copied()
        .filter(|host| panes.get(*host).is_ok_and(|p| p.3.get()))
        .collect();
    hosts.sort();

    // A slide moved. Burst BEFORE the no-hosts bail: leaving an
    // `application:` slide needs frames just as much as arriving on one —
    // the deck still has to draw whatever it moved to.
    if picture.last_bump != targets.bump {
        picture.last_bump = targets.bump;
        dive.cooldown = REBUILD_FRAMES;
    }

    if hosts.is_empty() {
        if !picture.built_for.is_empty() || !picture.hosts.is_empty() {
            for entity in &all_owned {
                commands.entity(entity).despawn();
            }
            picture.built_for.clear();
            picture.hosts.clear();
        }
        return;
    }

    let viewport = viewport.as_deref().copied().unwrap_or_default();
    let region = region.as_deref().copied();

    // Panes to photograph, in the order the window draws them.
    let mut visible: Vec<(Entity, PaneRect, usize, f32)> = panes
        .iter()
        .filter(|(_, _, _, vis, _, _)| vis.get())
        .map(|(entity, rect, layer, _, anchored, _)| {
            (
                entity,
                screen_rect(rect, anchored.is_some(), &viewport),
                layer.0,
                rect.z,
            )
        })
        .collect();
    visible.sort_by(|a, b| a.3.total_cmp(&b.3).then(a.0.cmp(&b.0)));
    let signature: Vec<(Entity, usize)> = visible.iter().map(|(e, _, l, _)| (*e, *l)).collect();

    let cap = picture_scale(window);
    let size = (logical * cap).ceil().as_uvec2().max(UVec2::ONE);
    let resized = picture.size != size;
    if resized {
        for handle in [picture.scene.clone(), picture.shown.clone()] {
            if let Err(error) = images.insert(handle.id(), target_image(size)) {
                error!("[slide-view] could not size the picture: {error}");
                return;
            }
        }
        picture.size = size;
    }
    picture.cap = cap;

    if picture.built_for != signature || picture.hosts != hosts || resized {
        for entity in &all_owned {
            commands.entity(entity).despawn();
        }
        spawn_picture(
            &mut commands,
            &picture,
            &visible,
            &hosts,
            &panes,
            &viewport,
            region,
            logical,
            cap,
        );
        picture.built_for = signature;
        picture.hosts = hosts;
        return;
    }

    // Steady state: move what already exists.
    for (_, owner, mut camera, mut transform) in &mut cameras {
        let Some((_, screen, _, _)) = visible.iter().find(|(e, _, _, _)| *e == owner.0) else {
            continue;
        };
        let setup = pane_camera_setup_for(screen, logical, cap, region);
        camera.is_active = setup.visible;
        let changed = camera.viewport.as_ref().is_none_or(|current| {
            current.physical_position != setup.viewport.physical_position
                || current.physical_size != setup.viewport.physical_size
        });
        if changed {
            camera.viewport = Some(setup.viewport);
        }
        let want = Vec3::new(setup.cam_center.x, setup.cam_center.y, 0.0);
        if transform.translation != want {
            transform.translation = want;
        }
    }
    for (_, owner, mut sprite, mut transform) in &mut sprites {
        let Ok((_, rect, _, _, anchored, chrome)) = panes.get(owner.0) else {
            continue;
        };
        let Some((centre, shown)) =
            host_placement(rect, anchored.is_some(), chrome, &viewport, logical)
        else {
            continue;
        };
        if sprite.custom_size != Some(shown) {
            sprite.custom_size = Some(shown);
        }
        let want = Vec3::new(centre.x, centre.y, PICTURE_Z);
        if transform.translation != want {
            transform.translation = want;
        }
    }
}

/// How much smaller each level of the recursion is than the one around it.
///
/// The picture shows the whole window letterboxed into the host's content
/// area, so the host's own copy inside it is scaled by exactly this — and
/// so is every level below. It is the zoom of one dive.
fn level_ratio(area: Vec2, logical: Vec2) -> f32 {
    if area.x <= 0.0 || area.y <= 0.0 || logical.x <= 0.0 || logical.y <= 0.0 {
        return 0.0;
    }
    (area.x / logical.x).min(area.y / logical.y)
}

/// The sub-rectangle of the picture to show at dive progress `t`.
///
/// Diving samples a shrinking WINDOW of the texture rather than scaling the
/// sprite. Scaling the sprite would grow it 5x past the pane it lives in
/// and spill over the chrome; the sprite's on-screen bounds never move, so
/// there is nothing to clip and no geometry to get wrong.
///
/// `t = 0` is the whole image. `t = 1` is exactly the nested copy of this
/// host — centred on the host's content area, scaled by [`level_ratio`].
/// The size shrinks exponentially so the zoom reads at a constant rate;
/// the centre moves linearly so `t = 1` lands on the nested copy exactly,
/// which is what makes the snap back invisible.
fn dive_rect(t: f32, content_pos: Vec2, area: Vec2, logical: Vec2, cap: f32) -> Option<Rect> {
    let ratio = level_ratio(area, logical);
    if ratio <= 0.0 {
        return None;
    }
    let image = logical * cap;
    let centre = image * 0.5;
    let target = (content_pos + area * 0.5) * cap;
    let size = image * ratio.powf(t);
    Some(Rect::from_center_size(centre.lerp(target, t), size))
}

/// Begin a dive on the picture the user double-clicked.
///
/// Spawns the full-window overlay. It is created showing the whole picture
/// unshrunk, which is what the screen already looks like — so the frame it
/// appears on is indistinguishable from the one before it.
fn start_dive(
    mut commands: Commands,
    picture: Res<SlidePicture>,
    windows: Query<&Window>,
    mut clicks: MessageReader<jim_pane::PaneDoubleClicked>,
    hosts: Query<&PictureSpriteOf>,
    existing: Query<Entity, With<Diving>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let logical = Vec2::new(window.width(), window.height());
    for click in clicks.read() {
        if !hosts.iter().any(|owner| owner.0 == click.pane) {
            continue;
        }
        // One dive at a time. A second double-click mid-flight restarts
        // rather than stacking two overlays on top of each other.
        for entity in &existing {
            commands.entity(entity).despawn();
        }
        let layers = RenderLayers::from_layers(&[DIVE_LAYER]);
        commands.spawn((
            Camera2d,
            Camera {
                order: DIVE_CAMERA_ORDER,
                // Don't clear: until the zoom bites, the overlay is the
                // app's own image and there is nothing to wipe.
                clear_color: ClearColorConfig::None,
                ..default()
            },
            // NOT Msaa::Off. This camera draws to the WINDOW, where every
            // other camera is at Bevy's default Sample4, and one straggler
            // at a different sample count is a fatal validation error.
            layers.clone(),
            DiveCamera,
            Name::new("slide-dive:camera"),
        ));
        commands.spawn((
            Sprite {
                image: picture.shown.clone(),
                custom_size: Some(logical),
                ..default()
            },
            Transform::default(),
            layers,
            Diving {
                t: 0.0,
                host: click.pane,
            },
            Name::new("slide-dive:overlay"),
        ));
    }
}

/// Advance the dive: shrink the sampled window, then dissolve into reality.
#[allow(clippy::too_many_arguments)]
fn apply_dive(
    mut commands: Commands,
    time: Res<Time>,
    picture: Res<SlidePicture>,
    mut dive: ResMut<SlideDive>,
    windows: Query<&Window>,
    viewport: Option<Res<PaneViewport>>,
    panes: PaneQuery,
    mut diving: Query<(Entity, &mut Diving, &mut Sprite)>,
    cameras: Query<Entity, With<DiveCamera>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let logical = Vec2::new(window.width(), window.height());
    let viewport = viewport.as_deref().copied().unwrap_or_default();
    let mut running = false;

    for (entity, mut state, mut sprite) in &mut diving {
        state.t += time.delta_secs() / DIVE_SECONDS;

        // The host's content area is the zoom target, and it is where the
        // nested copy sits inside the picture.
        let target = panes
            .get(state.host)
            .ok()
            .and_then(|(_, rect, _, _, anchored, chrome)| {
                let screen = screen_rect(rect, anchored.is_some(), &viewport);
                let (pos, area) = content_rect(&screen, chrome.map_or(TITLE_H, |c| c.title_h));
                dive_rect(state.t.min(1.0), pos, area, logical, picture.cap).map(|r| (r, area))
            });

        if state.t >= 1.0 || target.is_none() {
            // Arrived. The overlay and the app behind it are showing the
            // same frame, so removing it is invisible — and leaves you one
            // level in.
            commands.entity(entity).despawn();
            for camera in &cameras {
                commands.entity(camera).despawn();
            }
            continue;
        }
        running = true;
        let (rect, _) = target.expect("checked above");
        sprite.rect = Some(rect);
        if sprite.custom_size != Some(logical) {
            sprite.custom_size = Some(logical);
        }
    }

    if dive.active != running {
        dive.active = running;
    }
    // Ticked here rather than in `drive_picture`: this runs every frame,
    // and while the counter is positive the loop is Continuous, so the
    // frames it is counting are the frames that actually happen.
    if dive.cooldown > 0 {
        dive.cooldown -= 1;
    }
}

/// Where a host pane's picture goes, in world space, and how big.
fn host_placement(
    rect: &PaneRect,
    anchored: bool,
    chrome: Option<&PaneChromeOverride>,
    viewport: &PaneViewport,
    logical: Vec2,
) -> Option<(Vec2, Vec2)> {
    let screen = screen_rect(rect, anchored, viewport);
    let title_h = chrome.map_or(TITLE_H, |c| c.title_h);
    let (pos, area) = content_rect(&screen, title_h);
    let shown = contain(area, logical);
    (shown.x > 0.0).then(|| (to_world(pos + area * 0.5, logical), shown))
}

/// Build the whole camera + sprite set from scratch.
#[allow(clippy::too_many_arguments)]
fn spawn_picture(
    commands: &mut Commands,
    picture: &SlidePicture,
    visible: &[(Entity, PaneRect, usize, f32)],
    hosts: &[Entity],
    panes: &PaneQuery,
    viewport: &PaneViewport,
    region: Option<PaneCanvasRegion>,
    logical: Vec2,
    cap: f32,
) {
    let scene = image_target(&picture.scene, cap);
    // Rebuilds are rare (a slide change, a pane opening), so this is quiet
    // — and it is the one line that says whether a slide that shows nothing
    // found a host at all.
    info!(
        "[slide-view] picture rebuilt: {} host(s), {} pane(s)",
        hosts.len(),
        visible.len()
    );

    // Layer 0: canvas background, sidebar, and every pane's chrome — the
    // camera the window has at order 0, aimed at the image instead.
    commands.spawn((
        Camera2d,
        Camera {
            order: BAND_START,
            ..default()
        },
        scene.clone(),
        RenderLayers::layer(0),
        bevy::render::view::Msaa::Off,
        PictureCamera,
        Name::new("slide-picture:chrome"),
    ));

    for (index, (pane, screen, layer, _)) in visible.iter().enumerate() {
        let setup = pane_camera_setup_for(screen, logical, cap, region);
        commands.spawn((
            Camera2d,
            Camera {
                order: BAND_START + 1 + index as isize,
                viewport: Some(setup.viewport),
                is_active: setup.visible,
                // Don't clear: each pane camera overlays what the chrome
                // camera drew, exactly as on the window.
                clear_color: ClearColorConfig::None,
                ..default()
            },
            scene.clone(),
            Transform::from_xyz(setup.cam_center.x, setup.cam_center.y, 0.0),
            // Growable ctor: pane layer ids are unbounded and the const
            // `layer()` asserts < 64.
            RenderLayers::from_layers(&[*layer]),
            bevy::render::view::Msaa::Off,
            PictureCamera,
            PictureCameraOf(*pane),
            Name::new("slide-picture:pane"),
        ));
    }

    // Copy the finished picture, so the sprites sample a COMPLETE frame —
    // and, being last frame's, one that already contains them. That lag is
    // what makes the nesting infinite instead of one level deep.
    commands.spawn((
        Camera2d,
        Camera {
            order: BAND_BLIT,
            ..default()
        },
        image_target(&picture.shown, cap),
        RenderLayers::from_layers(&[BLIT_LAYER]),
        bevy::render::view::Msaa::Off,
        PictureCamera,
        Name::new("slide-picture:blit"),
    ));
    commands.spawn((
        Sprite {
            image: picture.scene.clone(),
            custom_size: Some(logical),
            ..default()
        },
        Transform::default(),
        RenderLayers::from_layers(&[BLIT_LAYER]),
        PictureSprite,
        Name::new("slide-picture:blit-quad"),
    ));

    // And show it inside each host, on that host's own render layer — so
    // the host's camera draws it, INCLUDING the private one above. That is
    // where the recursion comes from.
    for host in hosts {
        let Ok((_, rect, layer, _, anchored, chrome)) = panes.get(*host) else {
            continue;
        };
        let Some((centre, shown)) =
            host_placement(rect, anchored.is_some(), chrome, viewport, logical)
        else {
            continue;
        };
        commands.spawn((
            Sprite {
                image: picture.shown.clone(),
                custom_size: Some(shown),
                ..default()
            },
            Transform::from_xyz(centre.x, centre.y, PICTURE_Z),
            RenderLayers::from_layers(&[layer.0]),
            PictureSprite,
            PictureSpriteOf(*host),
            Name::new("slide-picture:in-slide"),
        ));
    }
}

/// Render layer for the full-screen dive overlay. Reserved like
/// [`BLIT_LAYER`], and constructed the same way — `RenderLayers::layer()`
/// panics above 63.
pub const DIVE_LAYER: usize = 4098;

/// Render layer for the blit quad. Reserved in `PaneLayerAllocator` so no
/// pane is ever allocated it.
///
/// Always construct it with `RenderLayers::from_layers` — the const
/// `RenderLayers::layer()` asserts the id fits one inline u64 block and
/// PANICS above 63. `jim_pane::camera` carries the same warning for pane
/// layer ids; this one is 4097.
pub const BLIT_LAYER: usize = 4097;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picture_is_letterboxed_never_cropped() {
        let fitted = contain(Vec2::new(800.0, 600.0), Vec2::new(1600.0, 900.0));
        assert_eq!(fitted, Vec2::new(800.0, 450.0));
    }

    #[test]
    fn a_degenerate_area_shows_nothing() {
        assert_eq!(contain(Vec2::ZERO, Vec2::new(16.0, 9.0)), Vec2::ZERO);
    }

    /// `RenderLayers::layer()` is a const ctor that PANICS for ids that do
    /// not fit one inline u64 block, and this one is 4097. It took the app
    /// down once; `from_layers` is the only correct constructor here.
    #[test]
    fn the_blit_layer_needs_the_growable_constructor() {
        assert!(BLIT_LAYER >= 64, "below 64 the const ctor would be fine");
        let layers = RenderLayers::from_layers(&[BLIT_LAYER]);
        assert!(layers.intersects(&RenderLayers::from_layers(&[BLIT_LAYER])));
    }

    /// The private cameras must never collide with the window's, and the
    /// blit must run after every camera that contributes to the picture.
    #[test]
    fn the_camera_band_is_clear_of_the_window_cameras() {
        assert!(BAND_START > 75_150, "above every pane camera");
        assert!(
            BAND_BLIT < crate::WHITEBOARD_OVERLAY_CAMERA_ORDER,
            "below the overlays"
        );
        assert!(BAND_BLIT > BAND_START);
    }

    /// A dive travels exactly one level: at `t = 1` the sampled window is
    /// the nested copy of this host, so dropping back to `t = 0` shows the
    /// same pixels and the animation has no seam. Get this wrong and every
    /// dive ends in a visible jump.
    #[test]
    fn a_dive_lands_exactly_on_the_nested_copy() {
        let logical = Vec2::new(1600.0, 1000.0);
        let pos = Vec2::new(200.0, 150.0);
        let area = Vec2::new(400.0, 300.0);
        let cap = 0.5;

        let start = dive_rect(0.0, pos, area, logical, cap).expect("a rect");
        assert_eq!(start.min, Vec2::ZERO, "t=0 is the whole image");
        assert_eq!(start.max, logical * cap);

        // One level down is `ratio` of the window, centred on the host's
        // content area — which is where the host's own copy is drawn.
        let ratio = level_ratio(area, logical);
        assert!((ratio - 0.25).abs() < 1e-6, "height-limited: 300/1000");
        let end = dive_rect(1.0, pos, area, logical, cap).expect("a rect");
        let want = Rect::from_center_size((pos + area * 0.5) * cap, logical * cap * ratio);
        assert!((end.min - want.min).length() < 1e-3, "{end:?} vs {want:?}");
        assert!((end.max - want.max).length() < 1e-3, "{end:?} vs {want:?}");
    }

    /// The sampled window shrinks monotonically, so the zoom never stalls
    /// or reverses part-way.
    #[test]
    fn a_dive_zooms_in_the_whole_way() {
        let logical = Vec2::new(1600.0, 1000.0);
        let (pos, area) = (Vec2::new(200.0, 150.0), Vec2::new(400.0, 300.0));
        let mut previous = f32::INFINITY;
        for step in 0..=10 {
            let rect = dive_rect(step as f32 / 10.0, pos, area, logical, 1.0).expect("a rect");
            let width = rect.max.x - rect.min.x;
            assert!(width < previous, "step {step}: {width} !< {previous}");
            previous = width;
        }
    }

    /// A pane with no content area has no picture and therefore no dive —
    /// it must not produce a degenerate rect.
    #[test]
    fn a_collapsed_pane_cannot_be_dived_into() {
        assert!(dive_rect(0.5, Vec2::ZERO, Vec2::ZERO, Vec2::new(800.0, 600.0), 1.0).is_none());
    }

    /// The burst has to outlast the pipeline it is feeding: the picture
    /// gains one level per rendered frame, and a slide change starts from
    /// nothing.
    #[test]
    fn the_rebuild_burst_outlasts_the_first_few_levels() {
        let mut dive = SlideDive::default();
        assert!(!dive.animating(), "idle by default");
        dive.cooldown = REBUILD_FRAMES;
        assert!(dive.animating(), "a rebuild keeps the loop drawing");
        for _ in 0..REBUILD_FRAMES {
            assert!(dive.animating());
            dive.cooldown -= 1;
        }
        assert!(!dive.animating(), "and releases it again");
        assert!(
            REBUILD_FRAMES >= 10,
            "fewer than ~10 frames and the recursion is still visibly shallow"
        );
    }

    /// A dive keeps the loop awake on its own, independently of the burst.
    #[test]
    fn a_dive_keeps_drawing_without_a_burst() {
        let mut dive = SlideDive::default();
        dive.active = true;
        assert!(dive.animating());
    }

    /// A host's picture is placed against the pane's CONTENT area — what
    /// the widget is rendered at — not the pane rect, which is MARGIN
    /// larger on every side plus a title bar.
    #[test]
    fn the_picture_sits_in_the_content_area() {
        let rect = PaneRect {
            pos: Vec2::new(100.0, 100.0),
            size: Vec2::new(400.0, 300.0),
            z: 0.0,
        };
        let (pos, area) = content_rect(&rect, 0.0);
        assert_eq!(pos, rect.pos + Vec2::splat(MARGIN));
        assert_eq!(area, rect.size - Vec2::splat(2.0 * MARGIN));
    }
}
