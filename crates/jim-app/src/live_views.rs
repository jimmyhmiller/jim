//! Live, interactive project/application views embedded in widget panes.
//!
//! This deliberately does not render to a texture. Each visible pane gets
//! another camera aimed at the window, clipped to the requested region. The
//! pane entities remain single-instance, so terminals, editors and widgets are
//! the real running objects and input can be routed through [`jim_pane::Views`].

use std::collections::{HashMap, HashSet};

use bevy::camera::visibility::RenderLayers;
use bevy::camera::{Camera, ClearColorConfig, Viewport};
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use jim_pane::{
    InputConsumed, MARGIN, PaneCanvas, PaneClosing, PaneLayer, PaneProject, PaneRect,
    PaneScreenAnchored, PaneTag, PaneViewport, ROOT_VIEW, TITLE_H, ViewId, Views,
};

use crate::projects::Projects;

pub const TOPIC: &str = "pane.project";
const VIEW_ORDER_BASE: isize = 90_000;

#[derive(Resource, Default, Debug, Clone)]
pub struct ProjectRegion {
    pub active: Option<ActiveRegion>,
}

#[derive(Debug, Clone, Copy)]
pub struct ActiveRegion {
    pub project: u64,
    pub host: Entity,
    pub inset: [f32; 4],
    /// Include layer zero (sidebar/background/global chrome) as a faithful
    /// view of the entire application, in addition to the project's panes.
    pub whole_app: bool,
}

/// Projects whose entities must remain visible because a child view draws
/// them. Root pane cameras still render only the active project.
#[derive(Resource, Default, Debug)]
pub struct ViewedProjects(pub HashSet<u64>);

#[derive(Component)]
struct RegionPaneCamera;

#[derive(Component)]
struct RegionChromeCamera;

#[derive(Resource, Default)]
struct RegionState {
    view: Option<ViewId>,
    pane_cameras: HashMap<Entity, Entity>,
    chrome_camera: Option<Entity>,
    framed_project: Option<u64>,
    pan: Vec2,
    zoom: f32,
}

pub struct ProjectRegionPlugin;

impl Plugin for ProjectRegionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ProjectRegion>()
            .init_resource::<ViewedProjects>()
            .init_resource::<RegionState>()
            .add_systems(
                Update,
                (apply_bus_project_messages, region_pan_zoom, drive_region)
                    .chain()
                    .before(crate::canvas::CanvasInputSet)
                    .after(crate::projects::sync_visibility),
            );
    }
}

fn region_pan_zoom(
    mut wheel: MessageReader<MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window>,
    views: Res<Views>,
    mut consumed: ResMut<InputConsumed>,
    mut state: ResMut<RegionState>,
) {
    let Some(view_id) = state.view else { return };
    let Ok(window) = windows.single() else { return };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let Some(view) = views.get(view_id) else {
        return;
    };
    if views.resolve(cursor).0 != view_id {
        return;
    }
    let command = keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight);
    if !command {
        return;
    }
    let option = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    let mut delta = Vec2::ZERO;
    for event in wheel.read() {
        let scale = match event.unit {
            MouseScrollUnit::Line => 16.0,
            MouseScrollUnit::Pixel => 1.0,
        };
        delta += Vec2::new(event.x, event.y) * scale;
    }
    if delta == Vec2::ZERO {
        return;
    }
    consumed.0 = true;
    if option {
        let old_zoom = state.zoom.max(0.2);
        let canvas_under_cursor = (cursor - view.transform.origin) / old_zoom + state.pan;
        state.zoom = (old_zoom * 1.08_f32.powf(delta.y / 16.0)).clamp(0.2, 4.0);
        state.pan = canvas_under_cursor - (cursor - view.transform.origin) / state.zoom;
    } else {
        let zoom = state.zoom.max(0.0001);
        state.pan += Vec2::new(-delta.x, -delta.y) / zoom;
    }
    state.pan = state.pan.max(Vec2::ZERO);
}

fn entity_from_widget_id(id: &str) -> Option<Entity> {
    let bits = id.strip_prefix("rw")?;
    u64::from_str_radix(bits, 16).ok().map(Entity::from_bits)
}

fn payload_bool(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(v)) => *v,
        Some(serde_json::Value::String(v)) => matches!(v.as_str(), "true" | "yes" | "1"),
        _ => false,
    }
}

fn apply_bus_project_messages(
    mut observed: MessageReader<jim_widget::BusMessageObserved>,
    mut region: ResMut<ProjectRegion>,
    mut viewed: ResMut<ViewedProjects>,
    projects: Res<Projects>,
    hosts: Query<&PaneProject, With<PaneTag>>,
) {
    for msg in observed.read() {
        if msg.topic != TOPIC {
            continue;
        }
        let name = msg
            .payload
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let whole_app = payload_bool(msg.payload.get("whole_app"));
        let Some(host) = entity_from_widget_id(&msg.sender) else {
            warn!("[view] sender `{}` is not a widget pane", msg.sender);
            continue;
        };
        // Several decks may be alive across projects. A deck outside the
        // active host project must not overwrite the view the presenter is
        // actually looking at. Checking project membership is safe during
        // startup; Visibility is not settled yet when restored decks publish.
        let Ok(host_project) = hosts.get(host) else {
            continue;
        };
        if Some(host_project.0) != projects.active {
            continue;
        }
        if name.is_empty() && !whole_app {
            region.active = None;
            viewed.0.clear();
            continue;
        }
        let project = if whole_app && name.is_empty() {
            projects.active
        } else {
            projects.list.iter().find(|p| p.name == name).map(|p| p.id)
        };
        let Some(project) = project else {
            warn!("[view] no project named `{name}`");
            continue;
        };
        let inset = msg
            .payload
            .get("inset")
            .and_then(|v| v.as_array())
            .filter(|a| a.len() == 4)
            .map(|a| {
                let f = |i: usize| a[i].as_f64().unwrap_or(0.0) as f32;
                [f(0), f(1), f(2), f(3)]
            })
            .unwrap_or([0.0, 0.0, 1.0, 1.0]);
        region.active = Some(ActiveRegion {
            project,
            host,
            inset,
            whole_app,
        });
        viewed.0.clear();
        viewed.0.insert(project);
    }
}

fn region_rect(host: &PaneRect, inset: [f32; 4], root: &PaneViewport) -> Rect {
    let screen = root.projected_rect(host);
    let [x, y, w, h] = inset;
    Rect::from_corners(
        screen.pos + screen.size * Vec2::new(x, y),
        screen.pos + screen.size * Vec2::new(x + w, y + h),
    )
}

fn logical_viewport(rect: Rect, window: &Window) -> Viewport {
    let sf = window.scale_factor();
    let max = Vec2::new(window.width(), window.height());
    let min = rect.min.max(Vec2::ZERO).min(max);
    let end = rect.max.max(Vec2::ZERO).min(max);
    let size = (end - min).max(Vec2::ONE);
    Viewport {
        physical_position: (min * sf).as_uvec2(),
        physical_size: (size * sf).as_uvec2().max(UVec2::ONE),
        depth: 0.0..1.0,
    }
}

fn to_world(screen: Vec2, window: Vec2) -> Vec2 {
    Vec2::new(screen.x - window.x * 0.5, window.y * 0.5 - screen.y)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn drive_region(
    region: Res<ProjectRegion>,
    mut state: ResMut<RegionState>,
    mut viewed: ResMut<ViewedProjects>,
    mut views: ResMut<Views>,
    windows: Query<&Window>,
    panes: Query<
        (
            Entity,
            &PaneProject,
            Option<&PaneCanvas>,
            &PaneRect,
            &PaneLayer,
            Has<PaneScreenAnchored>,
        ),
        (With<PaneTag>, Without<PaneClosing>),
    >,
    mut camera_q: Query<(&mut Camera, &mut Transform, &mut Projection)>,
    mut commands: Commands,
) {
    let Ok(window) = windows.single() else { return };
    let Some(active) = region.active else {
        teardown(&mut state, &mut views, &mut commands);
        viewed.0.clear();
        return;
    };
    let Ok((_, _, _, host_rect, _, fullscreen)) = panes.get(active.host) else {
        teardown(&mut state, &mut views, &mut commands);
        viewed.0.clear();
        return;
    };
    let root = views.root().clone();
    let mut inset = active.inset;
    if inset == [0.0, 0.0, 1.0, 1.0] && !fullscreen {
        inset = [
            MARGIN / host_rect.size.x,
            (TITLE_H + MARGIN) / host_rect.size.y,
            (host_rect.size.x - 2.0 * MARGIN).max(0.0) / host_rect.size.x,
            (host_rect.size.y - TITLE_H - 2.0 * MARGIN).max(0.0) / host_rect.size.y,
        ];
    }
    let rect = region_rect(host_rect, inset, &root.transform);
    if rect.size().min_element() < 2.0 {
        teardown(&mut state, &mut views, &mut commands);
        viewed.0.clear();
        return;
    }

    // Project views start at native scale and frame canvas origin. A whole-app
    // view instead faithfully scales the current window (including sidebar)
    // into the region, preserving aspect ratio.
    let window_size = Vec2::new(window.width(), window.height());
    if state.framed_project != Some(active.project) {
        state.framed_project = Some(active.project);
        state.pan = Vec2::ZERO;
        state.zoom = 1.0;
    }
    let child_transform = if active.whole_app {
        let fit = (rect.width() / window_size.x).min(rect.height() / window_size.y);
        let fitted = window_size * fit;
        let letterbox = (rect.size() - fitted) * 0.5;
        PaneViewport {
            origin: rect.min + letterbox + root.transform.origin * fit,
            pan: root.transform.pan,
            zoom: root.transform.zoom * fit,
        }
    } else {
        PaneViewport {
            origin: rect.min,
            pan: state.pan,
            zoom: state.zoom.max(0.2),
        }
    };
    let view_id = match state.view {
        Some(id) if views.update(id, rect, child_transform) => id,
        _ => {
            let Some(id) = views.insert(ROOT_VIEW, rect, child_transform, Some(active.project))
            else {
                teardown(&mut state, &mut views, &mut commands);
                viewed.0.clear();
                return;
            };
            state.view = Some(id);
            id
        }
    };
    let _ = view_id;

    let mut wanted = HashSet::new();
    for (pane, project, canvas, pane_rect, layer, _) in &panes {
        if project.0 != active.project || canvas.is_some_and(|c| c.0 != 0) {
            continue;
        }
        // Do not draw the publishing deck inside its own first-level view.
        // Whole-app recursion is represented by the bounded view tree, not by
        // an unbounded camera feedback loop.
        if pane == active.host && !active.whole_app {
            continue;
        }
        wanted.insert(pane);
        let projected = child_transform.projected_rect(pane_rect);
        let clipped = Rect::from_corners(
            projected.pos.max(rect.min),
            (projected.pos + projected.size).min(rect.max),
        );
        let visible = clipped.max.x > clipped.min.x && clipped.max.y > clipped.min.y;
        let viewport = logical_viewport(
            if visible {
                clipped
            } else {
                Rect::from_corners(rect.min, rect.min + Vec2::ONE)
            },
            window,
        );
        let child_center = if visible { clipped.center() } else { rect.min };
        let canvas_center = child_transform.window_to_canvas(child_center);
        let root_screen = root.transform.canvas_to_window(canvas_center);
        let camera_center = to_world(root_screen, window_size);
        let scale = root.transform.zoom / child_transform.zoom;
        let order = VIEW_ORDER_BASE
            + (pane_rect.z.clamp(0.0, 500.0) * 10.0) as isize
            + (pane.index().index() as isize % 10);

        let camera = if let Some(camera) = state.pane_cameras.get(&pane).copied() {
            camera
        } else {
            let camera = commands
                .spawn((
                    Camera2d,
                    Camera {
                        order,
                        viewport: Some(viewport.clone()),
                        clear_color: ClearColorConfig::None,
                        ..default()
                    },
                    Projection::from(OrthographicProjection {
                        scale,
                        ..OrthographicProjection::default_2d()
                    }),
                    Transform::from_translation(camera_center.extend(0.0)),
                    RenderLayers::from_layers(&[layer.0]),
                    RegionPaneCamera,
                    Name::new("live-view-pane-camera"),
                ))
                .id();
            state.pane_cameras.insert(pane, camera);
            camera
        };
        if let Ok((mut cam, mut transform, mut projection)) = camera_q.get_mut(camera) {
            cam.is_active = visible;
            cam.order = order;
            cam.viewport = Some(viewport);
            transform.translation = camera_center.extend(0.0);
            if let Projection::Orthographic(ortho) = &mut *projection {
                ortho.scale = scale;
            }
        }
        commands
            .entity(camera)
            .insert(RenderLayers::from_layers(&[layer.0]));
    }

    let stale: Vec<(Entity, Entity)> = state
        .pane_cameras
        .iter()
        .filter(|(pane, _)| !wanted.contains(pane))
        .map(|(pane, camera)| (*pane, *camera))
        .collect();
    for (pane, camera) in stale {
        commands.entity(camera).despawn();
        state.pane_cameras.remove(&pane);
    }

    if active.whole_app {
        // Layer zero contains the sidebar, canvas background and global app
        // chrome. Frame the actual window world at native scale into the
        // region. Pane layers are composited by the cameras above.
        let viewport = logical_viewport(rect, window);
        let scale = (window_size.x / rect.width()).max(window_size.y / rect.height());
        let camera = state.chrome_camera.unwrap_or_else(|| {
            let e = commands
                .spawn((
                    Camera2d,
                    Camera {
                        order: VIEW_ORDER_BASE - 1,
                        viewport: Some(viewport.clone()),
                        clear_color: ClearColorConfig::None,
                        ..default()
                    },
                    Projection::from(OrthographicProjection {
                        scale,
                        ..OrthographicProjection::default_2d()
                    }),
                    RenderLayers::layer(0),
                    RegionChromeCamera,
                    Name::new("live-view-app-camera"),
                ))
                .id();
            state.chrome_camera = Some(e);
            e
        });
        if let Ok((mut cam, mut transform, mut projection)) = camera_q.get_mut(camera) {
            cam.viewport = Some(viewport);
            transform.translation = Vec3::ZERO;
            if let Projection::Orthographic(ortho) = &mut *projection {
                ortho.scale = scale;
            }
        }
    } else if let Some(camera) = state.chrome_camera.take() {
        commands.entity(camera).despawn();
    }
}

fn teardown(state: &mut RegionState, views: &mut Views, commands: &mut Commands) {
    if let Some(id) = state.view.take() {
        views.remove(id);
    }
    for (_, camera) in state.pane_cameras.drain() {
        commands.entity(camera).despawn();
    }
    if let Some(camera) = state.chrome_camera.take() {
        commands.entity(camera).despawn();
    }
    state.framed_project = None;
    state.pan = Vec2::ZERO;
    state.zoom = 1.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_id_round_trips_to_an_entity() {
        let e = Entity::from_raw_u32(7).expect("valid entity");
        assert_eq!(
            entity_from_widget_id(&format!("rw{:x}", e.to_bits())),
            Some(e)
        );
    }

    #[test]
    fn inset_is_relative_to_host() {
        let host = PaneRect {
            pos: Vec2::new(100.0, 50.0),
            size: Vec2::new(800.0, 600.0),
            z: 1.0,
        };
        let got = region_rect(&host, [0.1, 0.2, 0.5, 0.5], &PaneViewport::default());
        assert_eq!(got.min, Vec2::new(180.0, 170.0));
        assert_eq!(got.max, Vec2::new(580.0, 470.0));
    }

    #[test]
    fn native_view_does_not_fit_content() {
        let view = PaneViewport {
            origin: Vec2::new(20.0, 30.0),
            pan: Vec2::ZERO,
            zoom: 1.0,
        };
        assert_eq!(
            view.canvas_to_window(Vec2::new(400.0, 200.0)),
            Vec2::new(420.0, 230.0)
        );
    }
}
