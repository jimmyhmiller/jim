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
//!   order -100_000  layer 0, whole image   → canvas + sidebar + chrome
//!   order  -99_999+ one per visible pane   → that pane's content, clipped
//!   order      -1   blit scene → shown     → what the slide samples
//!   order       0+  normal window cameras  → consume the completed picture
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
//! ## A slide that names a project, or drops the sidebar
//!
//! `project: Coil` and `<!-- sidebar: false -->` change what full screen
//! shows, so they have to change the picture too — otherwise a deck in a
//! floating pane previews the app as it stands while the talk would show
//! something else entirely. Each host's request resolves to a
//! [`PictureView`]; hosts that agree share one picture, and each distinct
//! view gets its own images and cameras (up to [`MAX_VIEWS`]).
//!
//! A view of the ACTIVE project is the window, photographed, exactly as
//! above. A view of any other project has to photograph panes that are
//! `Hidden`, because that is how a non-active project is kept off the
//! screen — and every hit-test in the app keys on it. So the pane roots
//! STAY `Hidden` (a pictured pane can never take a click) and only their
//! children are made `Visible`, which Bevy honours whatever the parent says.
//! All of those children are on the pane's own render layer, and nothing
//! draws that layer to the window: a pane's window camera is only active
//! for the active project (`cube::suppress_window_pane_cams`).
//! [`jim_pane::PanePictured`] tells the kinds that pause while hidden
//! (terminal grids, script widgets) to keep painting.
//!
//! Every pane in every project is placed in the world through the ACTIVE
//! project's viewport. A picture of another project therefore aims each
//! camera at where the pane really is and zooms it by `active zoom / that
//! project's zoom` — the same affine map for every canvas pane — so the
//! pane lands where that project's own pan and zoom would put it.
//!
//! ## MSAA
//!
//! Every camera here is `Msaa::Off`, without exception. An image render
//! target is single-sampled, Jim sets `Msaa` nowhere so cameras default to
//! `Sample4`, and Bevy treats the mismatch as a FATAL validation error —
//! it quits the app. One straggler is enough. This is the constraint
//! `cube.rs` satisfies the same way.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{Camera, ClearColorConfig, ImageRenderTarget, RenderTarget};
use bevy::ecs::system::SystemParam;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;

use jim_pane::camera::{PaneCameraSetup, PaneCanvasRegion, pane_camera_setup_for};
use jim_pane::{
    MARGIN, PaneCanvas, PaneChromeOverride, PaneClosing, PaneGroup, PaneLayer, PanePictured,
    PaneProject, PaneRect, PaneScreenAnchored, PaneTag, PaneViewport, TITLE_H,
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

/// Rendered frames to refresh the picture before a dive becomes visible.
///
/// The overlay drops a copy of the app over the live one, so the copy has
/// to BE the live one or the difference shows. Idle in reactive mode,
/// `shown` holds whatever the last frame drew — possibly seconds old — and
/// anything that moved since (a focus ring, a cursor, a sidebar highlight)
/// disagrees for a frame. That reads as a flash, or as the sidebar briefly
/// z-fighting with itself.
///
/// One rendered frame is enough to put a current `scene` into `shown`; two
/// covers the blit's own frame of lag. At the frame rates this runs at that
/// is under 20ms, so the dive still starts on the click.
const DIVE_WARMUP_FRAMES: u32 = 2;

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

/// Camera order band before every window camera.
///
/// Camera order is global even across render targets. The picture used to
/// run at 76_000–79_999, after the window's ordinary pane cameras. That made
/// the window sample `shown` before this frame had composed and copied it:
/// entering a live slide exposed the texture's empty initial contents, then
/// one or two intermediate recursion levels as visible flashes.
///
/// Compose `scene`, copy it to `shown`, and only then let order-zero-and-up
/// window cameras consume it. Each view owns [`SLOT_BAND`] orders of the
/// band, its blit last, which leaves room for far more pane cameras than
/// Jim can reasonably display.
const BAND_START: isize = -100_000;

/// Camera orders owned by one view, blit included.
const SLOT_BAND: isize = 20_000;

/// Distinct views composed at once.
///
/// Every host showing the same view shares one picture, so this bounds
/// the number of DIFFERENT `project:`/`sidebar:` requests among the decks
/// on screen at one moment — normally one. Past it a host gets no picture
/// and an error in the log, never somebody else's picture.
pub const MAX_VIEWS: usize = 4;

/// The first view's blit layer; view `n` uses `BLIT_LAYER_BASE + n`.
///
/// Always construct these with `RenderLayers::from_layers` — the const
/// `RenderLayers::layer()` asserts the id fits one inline u64 block and
/// PANICS above 63. `jim_pane::camera` carries the same warning for pane
/// layer ids.
const BLIT_LAYER_BASE: usize = 4100;

fn slot_order(slot: usize) -> isize {
    BAND_START + slot as isize * SLOT_BAND
}

fn slot_blit_order(slot: usize) -> isize {
    slot_order(slot) + SLOT_BAND - 1
}

fn blit_layer(slot: usize) -> usize {
    BLIT_LAYER_BASE + slot
}

/// Every global layer this module's cameras render. The shell reserves
/// them in `PanePlugin.reserved_layers` so no pane is ever allocated one.
pub fn reserved_layers() -> impl Iterator<Item = usize> {
    std::iter::once(DIVE_LAYER).chain((0..MAX_VIEWS).map(blit_layer))
}

/// The dive overlay draws to the WINDOW, over everything — sidebar,
/// whiteboard overlay, panes — because the whole app is what appears to
/// zoom. Below the menu overlay (100_000) so a menu can never end up
/// underneath it.
const DIVE_CAMERA_ORDER: isize = 95_000;

/// What a slide's picture shows: which project's canvas, and whether the
/// sidebar is in it — what full screen would show for the same slide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PictureView {
    /// The slide's `project:`, or the active project for `application:`.
    project: u64,
    /// `<!-- sidebar: false -->` turns it off.
    sidebar: bool,
}

/// One view's picture: its two images, and what to put in them.
struct ViewPicture {
    view: PictureView,
    /// Which slice of the camera band and which blit layer it owns.
    slot: usize,
    /// What the private cameras draw into, this frame.
    scene: Handle<Image>,
    /// A copy of `scene` from last frame — what the slide samples.
    shown: Handle<Image>,
    /// Recomputed every frame by [`resolve_views`] from here down.
    ///
    /// The view is of the active project: what the window already shows.
    live: bool,
    /// Host panes showing this view.
    hosts: Vec<Entity>,
    /// Panes to photograph in draw order: (pane, render layer, anchored).
    panes: Vec<(Entity, usize, bool)>,
    /// The canvas mapping this view's panes are laid out through.
    viewport: PaneViewport,
    /// Where pane cameras may draw — clear of the sidebar only when the
    /// view has one.
    region: PaneCanvasRegion,
    /// Canvas colour behind a view of another project: its own theme's.
    clear: Color,
    /// What the current camera set was built for.
    ///
    /// Rebuilt only when this changes. Respawning ~10 cameras every frame
    /// thrashes Bevy's view and texture caches, and this repo has already
    /// learned once that per-frame entity churn is what turns a working
    /// feature into an unusable one.
    built: Option<BuiltFor>,
}

#[derive(Debug, PartialEq)]
struct BuiltFor {
    live: bool,
    sidebar: bool,
    panes: Vec<(Entity, usize, bool)>,
    hosts: Vec<Entity>,
    size: UVec2,
}

/// Every picture currently being composed.
#[derive(Resource, Default)]
struct SlidePicture {
    /// Physical size of every image.
    size: UVec2,
    /// Resolution reduction folded into the render targets' scale factor,
    /// so viewports stay in the window's logical units.
    cap: f32,
    views: Vec<ViewPicture>,
    /// Last `SlideTargets::bump` acted on. Any change means a slide moved,
    /// which needs a burst of frames even when nothing about the camera set
    /// does.
    last_bump: u64,
}

impl SlidePicture {
    fn view_of(&self, host: Entity) -> Option<&ViewPicture> {
        self.views.iter().find(|v| v.hosts.contains(&host))
    }
}

/// Panes of a non-active project that a picture is photographing, and the
/// children this module made `Visible` to do it — so they can be put back.
#[derive(Resource, Default)]
struct PicturedPanes(HashMap<Entity, Vec<Entity>>);

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
    /// A dive waiting for the picture to be current: the host, and how many
    /// rendered frames still to wait. See [`DIVE_WARMUP_FRAMES`].
    pending: Option<(Entity, u32)>,
}

impl SlideDive {
    /// Does the loop need to keep drawing? True during a dive, and for a
    /// short burst after the picture is rebuilt so the recursion can fill.
    pub fn animating(&self) -> bool {
        self.active || self.cooldown > 0 || self.pending.is_some()
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

/// Everything spawned for one view's picture, by slot. A rebuild despawns
/// exactly its own slot's set.
#[derive(Component, Clone, Copy)]
struct PictureSlot(usize);

/// The camera a view of another project clears with that project's canvas
/// colour (and draws the sidebar with, when the view has one).
#[derive(Component)]
struct PictureBase;

/// The full-window camera that draws a dive overlay.
///
/// Deliberately NOT a `PictureSlot`: that marker means "part of the set
/// that composes a picture", and those are despawned wholesale whenever
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
            .init_resource::<SlidePicture>()
            .init_resource::<PicturedPanes>()
            .add_systems(
                Update,
                (
                    resolve_views,
                    picture_hidden_panes,
                    drive_picture,
                    start_dive,
                    spawn_pending_dive,
                    apply_dive,
                )
                    .chain()
                    .in_set(SlideViewSet)
                    .after(crate::present::PresentSet)
                    .after(crate::projects::sync_visibility)
                    // The picture's pane cameras are clipped by the SAME
                    // `PaneCanvasRegion` the window's are, and that region
                    // is what keeps panes off the sidebar. Aiming them from
                    // a region published earlier in the frame draws this
                    // frame's panes with the last frame's gutter.
                    .after(crate::canvas::publish_canvas_region),
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

/// The inverse of [`to_world`].
fn from_world(world: Vec2, window: Vec2) -> Vec2 {
    Vec2::new(world.x + window.x * 0.5, window.y * 0.5 - world.y)
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

/// The world a picture can be of: every project's canvas, not just the
/// active one's.
#[derive(SystemParam)]
struct Canvases<'w> {
    projects: Res<'w, crate::projects::Projects>,
    sidebar: Res<'w, crate::projects::Sidebar>,
    nav: Res<'w, crate::canvas_pane::CanvasNav>,
    groups: Res<'w, crate::pane_groups::VisibleGroups>,
    views: Res<'w, crate::canvas::CanvasView>,
    config: Res<'w, crate::canvas::CanvasConfig>,
    themes: Res<'w, jim_style::ProjectThemes>,
    clear: Res<'w, ClearColor>,
    viewport: Option<Res<'w, PaneViewport>>,
}

type MemberQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static PaneProject,
        Option<&'static PaneCanvas>,
        Option<&'static PaneGroup>,
    ),
    (With<PaneTag>, Without<PaneClosing>),
>;

/// Work out what every host's slide wants to show, and keep one picture
/// per distinct answer.
///
/// "What full screen would show" is the whole specification: the slide's
/// project (or the active one), laid out by that project's own pan and
/// zoom, with the sidebar only if the slide keeps it.
#[allow(clippy::too_many_arguments)]
fn resolve_views(
    mut picture: ResMut<SlidePicture>,
    mut dive: ResMut<SlideDive>,
    mut images: ResMut<Assets<Image>>,
    targets: Res<crate::slide_targets::SlideTargets>,
    canvases: Canvases,
    windows: Query<&Window>,
    panes: PaneQuery,
    members: MemberQuery,
    mut over_limit: Local<bool>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let logical = Vec2::new(window.width(), window.height());
    if logical.x <= 0.0 || logical.y <= 0.0 {
        return;
    }

    // A slide moved. Burst BEFORE anything can bail: leaving an
    // `application:` slide needs frames just as much as arriving on one —
    // the deck still has to draw whatever it moved to.
    if picture.last_bump != targets.bump {
        picture.last_bump = targets.bump;
        dive.cooldown = REBUILD_FRAMES;
    }

    // Every pane whose deck is on an `application:`/`project:` slide. NOT
    // gated on presenting: "any pane that has that presentation should show
    // it" — a deck sitting on the canvas previews the slide too, which is
    // also the only way to look at one while using the app normally.
    let active = canvases.projects.active;
    let mut hosts: Vec<(Entity, PictureView)> = targets
        .by_host
        .iter()
        .filter(|(host, _)| panes.get(**host).is_ok_and(|p| p.3.get()))
        .filter_map(|(host, target)| {
            // `application:` with no active project has nothing to show.
            let project = target.project.or(active)?;
            Some((
                *host,
                PictureView {
                    project,
                    sidebar: target.show_sidebar,
                },
            ))
        })
        .collect();
    hosts.sort_by_key(|(host, _)| *host);
    let mut wanted: Vec<(PictureView, Vec<Entity>)> = Vec::new();
    for (host, view) in hosts {
        match wanted.iter_mut().find(|(v, _)| *v == view) {
            Some((_, list)) => list.push(host),
            None => wanted.push((view, vec![host])),
        }
    }
    if wanted.len() > MAX_VIEWS {
        if !*over_limit {
            error!(
                "[slide-view] {} different slide views are on screen at once; only {MAX_VIEWS} \
                 can be composed, so decks showing the rest get no picture",
                wanted.len()
            );
            *over_limit = true;
        }
        wanted.truncate(MAX_VIEWS);
    } else {
        *over_limit = false;
    }

    let cap = picture_scale(window);
    let size = (logical * cap).ceil().as_uvec2().max(UVec2::ONE);
    let resized = picture.size != size;
    picture.size = size;
    picture.cap = cap;

    // Views nobody asks for any more. Their handles go with them, which
    // frees the images; `drive_picture` despawns their cameras.
    picture
        .views
        .retain(|v| wanted.iter().any(|(w, _)| *w == v.view));
    if resized {
        for view in &picture.views {
            for handle in [&view.scene, &view.shown] {
                if let Err(error) = images.insert(handle.id(), target_image(size)) {
                    error!("[slide-view] could not size the picture: {error}");
                }
            }
        }
    }

    let live_vp = canvases.viewport.as_deref().copied().unwrap_or_default();
    for (view, hosts) in wanted {
        let index = match picture.views.iter().position(|v| v.view == view) {
            Some(index) => index,
            None => {
                let slot = (0..MAX_VIEWS)
                    .find(|slot| picture.views.iter().all(|v| v.slot != *slot))
                    .expect("at most MAX_VIEWS views, so a slot is free");
                picture.views.push(ViewPicture {
                    view,
                    slot,
                    scene: images.add(target_image(size)),
                    shown: images.add(target_image(size)),
                    live: false,
                    hosts: Vec::new(),
                    panes: Vec::new(),
                    viewport: live_vp,
                    region: PaneCanvasRegion::default(),
                    clear: Color::NONE,
                    built: None,
                });
                picture.views.len() - 1
            }
        };

        let live = Some(view.project) == active;
        let viewport = if live {
            live_vp
        } else {
            // What `canvas::publish_canvas_region` would publish were this
            // project active. The origin is the same for every project: it
            // does not move when the sidebar hides, which is why a slide
            // without one simply shows more canvas on the left.
            let state = canvases
                .views
                .state_for((view.project, canvases.nav.level(view.project)));
            PaneViewport {
                origin: live_vp.origin,
                pan: state.pan,
                zoom: if canvases.config.zoom_enabled {
                    state.zoom
                } else {
                    1.0
                },
            }
        };
        let gutter = if view.sidebar {
            canvases.sidebar.width
        } else {
            0.0
        };
        let region = PaneCanvasRegion {
            min: Vec2::new(gutter, 0.0),
            max: logical,
            active: true,
        };
        // The window's own pane set when it IS the window; otherwise the
        // same rule `sync_visibility` applies, for the other project.
        let mut shown: Vec<(Entity, usize, bool, f32)> = if live {
            panes
                .iter()
                .filter(|p| p.3.get())
                .map(|(entity, rect, layer, _, anchored, _)| {
                    (entity, layer.0, anchored.is_some(), rect.z)
                })
                .collect()
        } else {
            members
                .iter()
                .filter(|(_, project, canvas, group)| {
                    crate::projects::pane_on_project_canvas(
                        project.0,
                        *canvas,
                        *group,
                        view.project,
                        &canvases.nav,
                        &canvases.groups,
                    )
                })
                .filter_map(|(entity, ..)| panes.get(entity).ok())
                .map(|(entity, rect, layer, _, anchored, _)| {
                    (entity, layer.0, anchored.is_some(), rect.z)
                })
                .collect()
        };
        shown.sort_by(|a, b| a.3.total_cmp(&b.3).then(a.0.cmp(&b.0)));
        let clear = canvases
            .themes
            .get(view.project)
            .map(|theme| Color::LinearRgba(theme.color(jim_style::tokens::CANVAS_BG)))
            .unwrap_or(canvases.clear.0);

        let entry = &mut picture.views[index];
        entry.live = live;
        entry.hosts = hosts;
        entry.panes = shown.into_iter().map(|(e, l, a, _)| (e, l, a)).collect();
        entry.viewport = viewport;
        entry.region = region;
        entry.clear = clear;
    }
}

/// Make the panes a picture of another project needs drawable, and put
/// back the ones it no longer needs.
///
/// Only CHILDREN are touched. The pane root stays `Hidden`, which is what
/// every hit-test checks, so a pane that is only being photographed can
/// never take a click. A child somebody else hid on purpose (a docked
/// pane's title bar, the presenting deck's chrome) is left alone.
fn picture_hidden_panes(
    mut commands: Commands,
    picture: Res<SlidePicture>,
    mut pictured: ResMut<PicturedPanes>,
    panes: Query<&Children, With<PaneTag>>,
    layers: Query<&RenderLayers>,
    mut visibility: Query<&mut Visibility, Without<PaneTag>>,
) {
    let wanted: Vec<Entity> = picture
        .views
        .iter()
        .filter(|v| !v.live)
        .flat_map(|v| v.panes.iter().map(|(pane, _, _)| *pane))
        .collect();

    let released: Vec<Entity> = pictured
        .0
        .keys()
        .filter(|pane| !wanted.contains(pane))
        .copied()
        .collect();
    for pane in released {
        let forced = pictured.0.remove(&pane).unwrap_or_default();
        for child in forced {
            if let Ok(mut vis) = visibility.get_mut(child)
                && *vis == Visibility::Visible
            {
                *vis = Visibility::Inherited;
            }
        }
        if let Ok(mut entity) = commands.get_entity(pane) {
            entity.remove::<PanePictured>();
        }
    }

    let layer0 = RenderLayers::layer(0);
    for pane in wanted {
        let Ok(children) = panes.get(pane) else {
            continue;
        };
        let forced = pictured.0.entry(pane).or_insert_with(|| {
            commands.entity(pane).insert(PanePictured);
            Vec::new()
        });
        for child in children.iter() {
            // Only what the pane's own camera draws. Anything that could be
            // on layer 0 would be drawn into the WINDOW by the main camera —
            // a ghost of another project on this one's canvas.
            if !layers.get(child).is_ok_and(|l| !l.intersects(&layer0)) {
                continue;
            }
            let Ok(mut vis) = visibility.get_mut(child) else {
                continue;
            };
            match *vis {
                Visibility::Inherited => {
                    *vis = Visibility::Visible;
                    if !forced.contains(&child) {
                        forced.push(child);
                    }
                }
                // Somebody hid it on purpose; that decision is theirs.
                Visibility::Hidden => forced.retain(|c| *c != child),
                Visibility::Visible => {}
            }
        }
    }
}

/// Where a picture's camera looks for one pane.
struct Aim {
    setup: PaneCameraSetup,
    /// World position the camera sits at.
    centre: Vec2,
    /// Orthographic scale: world units per picture pixel.
    scale: f32,
}

/// Aim a picture camera at `rect` as `view` lays it out.
///
/// The pane is laid out in the picture by the VIEW's canvas mapping, but it
/// exists in the world where the LIVE mapping put it — every pane in every
/// project is positioned through the active project's viewport. So the
/// viewport comes from the view, and the camera is aimed at the same canvas
/// point in the world, zoomed by the ratio of the two. For the active
/// project the two mappings are one and this is the window's own camera.
fn aim_pane(
    rect: &PaneRect,
    anchored: bool,
    view: &PaneViewport,
    live: &PaneViewport,
    logical: Vec2,
    cap: f32,
    region: PaneCanvasRegion,
) -> Aim {
    if anchored {
        // Window pixels in both, and never zoomed.
        let setup = pane_camera_setup_for(rect, logical, cap, Some(region));
        return Aim {
            centre: setup.cam_center,
            setup,
            scale: 1.0,
        };
    }
    let setup = pane_camera_setup_for(&view.projected_rect(rect), logical, cap, Some(region));
    let seen = from_world(setup.cam_center, logical);
    let actual = live.canvas_to_window(view.window_to_canvas(seen));
    Aim {
        centre: to_world(actual, logical),
        scale: live.zoom / view.zoom.max(1e-4),
        setup,
    }
}

fn projection(scale: f32) -> Projection {
    Projection::from(OrthographicProjection {
        scale,
        ..OrthographicProjection::default_2d()
    })
}

/// Keep every picture's cameras and sprites matching the world.
///
/// Two rates, deliberately. A view's camera SET is rebuilt only when what
/// it photographs changes — a slide change, a pane opening or closing.
/// Viewports and transforms are updated every frame, which is what
/// `jim_pane::camera::sync_pane_cameras` does for the real ones and costs a
/// few field writes. Respawning them per frame instead would churn Bevy's
/// view caches for no benefit.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn drive_picture(
    mut commands: Commands,
    mut picture: ResMut<SlidePicture>,
    windows: Query<&Window>,
    viewport: Option<Res<PaneViewport>>,
    panes: PaneQuery,
    owned: Query<(Entity, &PictureSlot)>,
    mut cameras: Query<(
        &PictureSlot,
        Option<&PictureCameraOf>,
        Has<PictureBase>,
        &mut Camera,
        &mut Transform,
        &mut Projection,
    )>,
    mut sprites: Query<
        (&PictureSlot, &PictureSpriteOf, &mut Sprite, &mut Transform),
        Without<Camera>,
    >,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let logical = Vec2::new(window.width(), window.height());
    if logical.x <= 0.0 || logical.y <= 0.0 {
        return;
    }
    let live_vp = viewport.as_deref().copied().unwrap_or_default();
    let (size, cap) = (picture.size, picture.cap);

    // Views that went away.
    for (entity, slot) in &owned {
        if picture.views.iter().all(|v| v.slot != slot.0) {
            commands.entity(entity).despawn();
        }
    }

    let mut rebuilt: Vec<usize> = Vec::new();
    for view in &mut picture.views {
        let want = BuiltFor {
            live: view.live,
            sidebar: view.view.sidebar,
            panes: view.panes.clone(),
            hosts: view.hosts.clone(),
            size,
        };
        if view.built.as_ref() == Some(&want) {
            continue;
        }
        for (entity, slot) in &owned {
            if slot.0 == view.slot {
                commands.entity(entity).despawn();
            }
        }
        spawn_view(&mut commands, view, &panes, &live_vp, logical, cap);
        view.built = Some(want);
        rebuilt.push(view.slot);
    }

    // Steady state: move what already exists.
    for (slot, owner, base, mut camera, mut transform, mut proj) in &mut cameras {
        if rebuilt.contains(&slot.0) {
            continue;
        }
        let Some(view) = picture.views.iter().find(|v| v.slot == slot.0) else {
            continue;
        };
        if base {
            let want = ClearColorConfig::Custom(view.clear);
            if !matches!(camera.clear_color, ClearColorConfig::Custom(c) if c == view.clear) {
                camera.clear_color = want;
            }
            continue;
        }
        let Some(owner) = owner else {
            continue;
        };
        let Ok((_, rect, _, _, anchored, _)) = panes.get(owner.0) else {
            continue;
        };
        let aim = aim_pane(
            rect,
            anchored.is_some(),
            &view.viewport,
            &live_vp,
            logical,
            cap,
            view.region,
        );
        camera.is_active = aim.setup.visible;
        let changed = camera.viewport.as_ref().is_none_or(|current| {
            current.physical_position != aim.setup.viewport.physical_position
                || current.physical_size != aim.setup.viewport.physical_size
        });
        if changed {
            camera.viewport = Some(aim.setup.viewport);
        }
        let want = Vec3::new(aim.centre.x, aim.centre.y, 0.0);
        if transform.translation != want {
            transform.translation = want;
        }
        let scaled = matches!(&*proj, Projection::Orthographic(o) if o.scale == aim.scale);
        if !scaled {
            *proj = projection(aim.scale);
        }
    }
    for (slot, owner, mut sprite, mut transform) in &mut sprites {
        if rebuilt.contains(&slot.0) {
            continue;
        }
        let Ok((_, rect, _, _, anchored, chrome)) = panes.get(owner.0) else {
            continue;
        };
        let Some((centre, shown)) =
            host_placement(rect, anchored.is_some(), chrome, &live_vp, logical)
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

/// Arm a dive on the picture the user double-clicked.
///
/// Nothing is shown yet. The overlay puts a copy of the app over the live
/// app, so it must not appear until that copy is CURRENT — see
/// [`DIVE_WARMUP_FRAMES`]. Arming also starts frames flowing, which is what
/// makes the copy current.
///
/// Only a picture of the window itself can be dived into. The dive ends by
/// dropping the overlay onto the live app, which is seamless only when the
/// picture IS the live app; a picture of another project, or one without
/// the sidebar the window has, would jump at the end — and contains no
/// nested copy of the host to zoom into in the first place.
fn start_dive(
    mut dive: ResMut<SlideDive>,
    mut clicks: MessageReader<jim_pane::PaneDoubleClicked>,
    picture: Res<SlidePicture>,
    presentation: Res<crate::present::Presentation>,
) {
    for click in clicks.read() {
        let Some(view) = picture.view_of(click.pane) else {
            continue;
        };
        if !view.live || view.view.sidebar != presentation.sidebar_visible() {
            continue;
        }
        dive.pending = Some((click.pane, DIVE_WARMUP_FRAMES));
        // The warmup needs rendered frames to happen at all.
        dive.cooldown = dive.cooldown.max(REBUILD_FRAMES);
    }
}

/// Show a warmed-up dive once the picture has caught up.
fn spawn_pending_dive(
    mut commands: Commands,
    mut dive: ResMut<SlideDive>,
    picture: Res<SlidePicture>,
    windows: Query<&Window>,
    existing: Query<Entity, Or<(With<Diving>, With<DiveCamera>)>>,
) {
    let Some((host, remaining)) = dive.pending else {
        return;
    };
    if remaining > 0 {
        dive.pending = Some((host, remaining - 1));
        return;
    }
    dive.pending = None;
    let Some(view) = picture.view_of(host) else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    let logical = Vec2::new(window.width(), window.height());
    // One dive at a time: a second double-click restarts rather than
    // stacking two overlays.
    for entity in &existing {
        commands.entity(entity).despawn();
    }
    let layers = RenderLayers::from_layers(&[DIVE_LAYER]);
    commands.spawn((
        Camera2d,
        Camera {
            order: DIVE_CAMERA_ORDER,
            // Don't clear: until the zoom bites, the overlay is the app's
            // own image and there is nothing to wipe.
            clear_color: ClearColorConfig::None,
            ..default()
        },
        // NOT Msaa::Off. This camera draws to the WINDOW, where every other
        // camera is at Bevy's default Sample4, and one straggler at a
        // different sample count is a fatal validation error.
        layers.clone(),
        DiveCamera,
        Name::new("slide-dive:camera"),
    ));
    commands.spawn((
        Sprite {
            image: view.shown.clone(),
            custom_size: Some(logical),
            ..default()
        },
        Transform::default(),
        layers,
        Diving { t: 0.0, host },
        Name::new("slide-dive:overlay"),
    ));
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

/// Build one view's whole camera + sprite set from scratch.
fn spawn_view(
    commands: &mut Commands,
    view: &ViewPicture,
    panes: &PaneQuery,
    live_vp: &PaneViewport,
    logical: Vec2,
    cap: f32,
) {
    let scene = image_target(&view.scene, cap);
    let slot = PictureSlot(view.slot);
    let base_order = slot_order(view.slot);
    // Rebuilds are rare (a slide change, a pane opening), so this is quiet
    // — and it is the one line that says whether a slide that shows nothing
    // found a host at all.
    info!(
        "[slide-view] picture rebuilt: project {}{}, sidebar {}, {} host(s), {} pane(s)",
        view.view.project,
        if view.live { " (active)" } else { "" },
        if view.view.sidebar { "on" } else { "off" },
        view.hosts.len(),
        view.panes.len()
    );

    // Global chrome plus the sidebar. The sidebar gained its own clipped
    // render layer when workspaces were added, so a picture that shows it
    // must photograph that layer explicitly (the window uses two cameras
    // for the same composition).
    if view.live {
        let mut layers = RenderLayers::layer(0);
        if view.view.sidebar {
            layers = layers.with(crate::projects::SIDEBAR_LAYER);
        }
        commands.spawn((
            Camera2d,
            Camera {
                order: base_order,
                ..default()
            },
            scene.clone(),
            layers,
            bevy::render::view::Msaa::Off,
            slot,
            Name::new("slide-picture:chrome"),
        ));
    } else {
        // Layer 0 is the ACTIVE project's canvas furniture, so a picture of
        // another project leaves it out and clears to that project's own
        // canvas colour instead.
        let layers = if view.view.sidebar {
            RenderLayers::from_layers(&[crate::projects::SIDEBAR_LAYER])
        } else {
            RenderLayers::none()
        };
        commands.spawn((
            Camera2d,
            Camera {
                order: base_order,
                clear_color: ClearColorConfig::Custom(view.clear),
                ..default()
            },
            scene.clone(),
            layers,
            bevy::render::view::Msaa::Off,
            slot,
            PictureBase,
            Name::new("slide-picture:canvas"),
        ));
    }

    for (index, (pane, layer, anchored)) in view.panes.iter().enumerate() {
        let Ok((_, rect, _, _, _, _)) = panes.get(*pane) else {
            continue;
        };
        let aim = aim_pane(
            rect,
            *anchored,
            &view.viewport,
            live_vp,
            logical,
            cap,
            view.region,
        );
        commands.spawn((
            Camera2d,
            Camera {
                order: base_order + 1 + index as isize,
                viewport: Some(aim.setup.viewport),
                is_active: aim.setup.visible,
                // Don't clear: each pane camera overlays what the chrome
                // camera drew, exactly as on the window.
                clear_color: ClearColorConfig::None,
                ..default()
            },
            scene.clone(),
            Transform::from_xyz(aim.centre.x, aim.centre.y, 0.0),
            projection(aim.scale),
            // Growable ctor: pane layer ids are unbounded and the const
            // `layer()` asserts < 64.
            RenderLayers::from_layers(&[*layer]),
            bevy::render::view::Msaa::Off,
            slot,
            PictureCameraOf(*pane),
            Name::new("slide-picture:pane"),
        ));
    }

    // Copy the finished picture, so the sprites sample a COMPLETE frame —
    // and, being last frame's, one that already contains them. That lag is
    // what makes the nesting infinite instead of one level deep.
    let blit = RenderLayers::from_layers(&[blit_layer(view.slot)]);
    commands.spawn((
        Camera2d,
        Camera {
            order: slot_blit_order(view.slot),
            ..default()
        },
        image_target(&view.shown, cap),
        blit.clone(),
        bevy::render::view::Msaa::Off,
        slot,
        Name::new("slide-picture:blit"),
    ));
    commands.spawn((
        Sprite {
            image: view.scene.clone(),
            custom_size: Some(logical),
            ..default()
        },
        Transform::default(),
        blit,
        slot,
        Name::new("slide-picture:blit-quad"),
    ));

    // And show it inside each host, on that host's own render layer — so
    // the host's camera draws it, INCLUDING the private one above when the
    // host is itself in the picture. That is where the recursion comes from.
    for host in &view.hosts {
        let Ok((_, rect, layer, _, anchored, chrome)) = panes.get(*host) else {
            continue;
        };
        let Some((centre, shown)) =
            host_placement(rect, anchored.is_some(), chrome, live_vp, logical)
        else {
            continue;
        };
        commands.spawn((
            Sprite {
                image: view.shown.clone(),
                custom_size: Some(shown),
                ..default()
            },
            Transform::from_xyz(centre.x, centre.y, PICTURE_Z),
            RenderLayers::from_layers(&[layer.0]),
            slot,
            PictureSpriteOf(*host),
            Name::new("slide-picture:in-slide"),
        ));
    }
}

/// Render layer for the full-screen dive overlay. Reserved like the blit
/// layers (see [`reserved_layers`]), and constructed the same way —
/// `RenderLayers::layer()` panics above 63.
pub const DIVE_LAYER: usize = 4098;

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
    /// not fit one inline u64 block, and these are above 4096. It took the
    /// app down once; `from_layers` is the only correct constructor here.
    #[test]
    fn the_blit_layers_need_the_growable_constructor() {
        for slot in 0..MAX_VIEWS {
            let layer = blit_layer(slot);
            assert!(layer >= 64, "below 64 the const ctor would be fine");
            let layers = RenderLayers::from_layers(&[layer]);
            assert!(layers.intersects(&RenderLayers::from_layers(&[layer])));
        }
    }

    /// Two views sharing a blit layer would each copy the other's picture
    /// over its own, and a reserved layer colliding with the dive overlay
    /// would draw the blit quads over the window.
    #[test]
    fn every_reserved_layer_is_distinct() {
        let layers: Vec<usize> = reserved_layers().collect();
        let mut unique = layers.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), layers.len(), "{layers:?}");
        assert_eq!(layers.len(), MAX_VIEWS + 1);
        assert!(!layers.contains(&crate::cube::CUBE_LAYER));
    }

    #[test]
    fn recursive_picture_includes_the_workspace_sidebar_layer() {
        let layers = RenderLayers::from_layers(&[0, crate::projects::SIDEBAR_LAYER]);
        assert!(layers.intersects(&RenderLayers::layer(0)));
        assert!(layers.intersects(&RenderLayers::from_layers(&[
            crate::projects::SIDEBAR_LAYER,
        ])));
    }

    /// The private cameras must finish before the window consumes the
    /// picture, and the blit must run after every contributing camera.
    #[test]
    fn the_camera_band_is_clear_of_the_window_cameras() {
        for slot in 0..MAX_VIEWS {
            assert!(slot_order(slot) < slot_blit_order(slot));
            assert!(
                slot_blit_order(slot) < 0,
                "before the first ordinary window camera"
            );
            assert!(
                slot_order(slot) + 10_000 < slot_blit_order(slot),
                "room for pane cameras before the private blit"
            );
            if slot > 0 {
                assert!(
                    slot_blit_order(slot - 1) < slot_order(slot),
                    "views never share camera orders"
                );
            }
        }
    }

    fn assert_near(a: Vec2, b: Vec2) {
        assert!((a - b).length() < 1e-3, "{a:?} vs {b:?}");
    }

    /// A pane of another project exists in the world where the ACTIVE
    /// project's viewport put it, but belongs in the picture where ITS
    /// project's viewport would. The camera has to frame exactly the pane's
    /// real world rect into exactly its slot in the picture, or the picture
    /// shows the wrong part of the canvas at the wrong size.
    #[test]
    fn another_projects_pane_is_framed_where_its_own_view_puts_it() {
        let logical = Vec2::new(1600.0, 1000.0);
        let region = PaneCanvasRegion {
            min: Vec2::ZERO,
            max: logical,
            active: true,
        };
        let live = PaneViewport {
            origin: Vec2::new(250.0, 0.0),
            pan: Vec2::new(-40.0, 10.0),
            zoom: 1.0,
        };
        let view = PaneViewport {
            origin: Vec2::new(250.0, 0.0),
            pan: Vec2::new(60.0, 30.0),
            zoom: 2.0,
        };
        let rect = PaneRect {
            pos: Vec2::new(100.0, 100.0),
            size: Vec2::new(200.0, 150.0),
            z: 0.0,
        };
        let aim = aim_pane(&rect, false, &view, &live, logical, 1.0, region);

        // Where it lands in the picture: the view's projection.
        let slot = view.projected_rect(&rect);
        assert_eq!(aim.setup.viewport.physical_position, slot.pos.as_uvec2());
        assert_eq!(aim.setup.viewport.physical_size, slot.size.as_uvec2());

        // What the camera sees: the pane's real world rect, exactly.
        let world = live.projected_rect(&rect);
        let seen = slot.size * aim.scale;
        assert_near(seen, world.size);
        let world_centre = to_world(world.pos + world.size * 0.5, logical);
        assert_near(aim.centre, world_centre);
    }

    /// For the active project the two mappings are the same, so the camera
    /// must be exactly the window's own — the recursive picture depends on
    /// it lining up with what is on screen.
    #[test]
    fn the_active_projects_pane_is_framed_like_the_window() {
        let logical = Vec2::new(1600.0, 1000.0);
        let region = PaneCanvasRegion {
            min: Vec2::new(250.0, 0.0),
            max: logical,
            active: true,
        };
        let live = PaneViewport {
            origin: Vec2::new(250.0, 0.0),
            pan: Vec2::new(-40.0, 10.0),
            zoom: 1.5,
        };
        let rect = PaneRect {
            pos: Vec2::new(-300.0, 100.0),
            size: Vec2::new(400.0, 300.0),
            z: 0.0,
        };
        let aim = aim_pane(&rect, false, &live, &live, logical, 2.0, region);
        let window = pane_camera_setup_for(&live.projected_rect(&rect), logical, 2.0, Some(region));
        assert_eq!(
            aim.setup.viewport.physical_position,
            window.viewport.physical_position
        );
        assert_eq!(
            aim.setup.viewport.physical_size,
            window.viewport.physical_size
        );
        assert_near(aim.centre, window.cam_center);
        assert!((aim.scale - 1.0).abs() < 1e-6);
    }

    /// A screen-anchored pane is window pixels in every project: no pan, no
    /// zoom, whatever the view.
    #[test]
    fn an_anchored_pane_ignores_both_viewports() {
        let logical = Vec2::new(1600.0, 1000.0);
        let region = PaneCanvasRegion {
            min: Vec2::ZERO,
            max: logical,
            active: true,
        };
        let live = PaneViewport::default();
        let view = PaneViewport {
            origin: Vec2::ZERO,
            pan: Vec2::new(500.0, 500.0),
            zoom: 3.0,
        };
        let rect = PaneRect {
            pos: Vec2::new(10.0, 20.0),
            size: Vec2::new(300.0, 40.0),
            z: 0.0,
        };
        let aim = aim_pane(&rect, true, &view, &live, logical, 1.0, region);
        assert_eq!(aim.scale, 1.0);
        assert_near(aim.centre, to_world(rect.pos + rect.size * 0.5, logical));
    }

    #[test]
    fn from_world_undoes_to_world() {
        let logical = Vec2::new(1600.0, 1000.0);
        let p = Vec2::new(123.0, 456.0);
        assert_near(from_world(to_world(p, logical), logical), p);
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

    /// A dive must not become visible until the picture it shows is
    /// current, or the copy disagrees with the live app underneath it for a
    /// frame — which looks like a flash, not a zoom.
    #[test]
    fn a_dive_waits_for_the_picture_to_catch_up() {
        let mut dive = SlideDive::default();
        dive.pending = Some((Entity::from_raw_u32(1).expect("valid"), DIVE_WARMUP_FRAMES));
        assert!(
            dive.animating(),
            "an armed dive keeps frames coming, which is what warms it"
        );
        assert!(
            DIVE_WARMUP_FRAMES >= 1,
            "one frame puts a fresh scene in shown"
        );
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
