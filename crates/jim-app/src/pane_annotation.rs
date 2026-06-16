//! **Draw on a pane.** While the canvas Draw Tools toolbar is open with a drawing
//! tool, dragging over a normal pane paints onto *that pane* — the strokes are
//! stored in the pane's content frame and rendered as children of its
//! `content_root`, so they follow + clip with the pane (and sit on top of its
//! content) for free, with no per-frame repositioning.
//!
//! This complements `whiteboard_bg` (drawing on *empty* canvas): over a pane →
//! here; over empty canvas → there. `jim_pane::PaneInputSuppressed` (set below)
//! tells the pane mouse handler to ignore the press so we can claim it.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use bevy::sprite_render::ColorMaterial;

use jim_pane::{
    InputConsumed, PaneChrome, PaneInputSuppressed, PaneKindMarker, PaneRect, PaneScreenAnchored,
    PaneTag, PaneViewport, MARGIN, TITLE_H,
};

use whiteboard_core::element::{ElementId, ElementKind};
use whiteboard_core::interaction::{InputEvent, Modifiers, PointerButton, Tool};
use whiteboard_core::Point;

use jim_whiteboard::render::{render_scene_into, WbRendered};
use jim_whiteboard::{new_editor, CanvasDrawActive, ToolStyle, WbEditor, WbToolState};

/// A drawing tool (paints) vs. a manipulation tool (select/pan). Only drawing
/// tools claim presses over empty pane area; select/eraser only claim a press
/// that lands on an existing annotation.
fn is_drawing_tool(t: Tool) -> bool {
    matches!(
        t,
        Tool::Freedraw
            | Tool::Rectangle
            | Tool::Ellipse
            | Tool::Diamond
            | Tool::Line
            | Tool::Arrow
            | Tool::Text
    )
}

type PaneQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static PaneRect,
        Option<&'static Visibility>,
        Has<PaneScreenAnchored>,
        Option<&'static PaneKindMarker>,
    ),
    With<PaneTag>,
>;

/// The topmost normal pane whose *content area* contains the canvas-space cursor,
/// plus the cursor in that pane's content-local coords. `None` over empty canvas,
/// the toolbar, or a whiteboard pane (which has its own drawing).
fn pane_under_cursor(panes: &PaneQuery, cursor_canvas: Vec2) -> Option<(Entity, Point)> {
    let mut best: Option<(Entity, f32, Point)> = None;
    for (e, rect, vis, anchored, kind) in panes.iter() {
        if anchored
            || matches!(vis, Some(Visibility::Hidden))
            || matches!(kind, Some(k) if k.0 == "whiteboard")
        {
            continue;
        }
        let (origin, size) = content_rect_canvas(rect);
        let inside = cursor_canvas.x >= origin.x
            && cursor_canvas.x <= origin.x + size.x
            && cursor_canvas.y >= origin.y
            && cursor_canvas.y <= origin.y + size.y;
        if inside && best.map_or(true, |(_, z, _)| rect.z > z) {
            let local = Point::new(
                (cursor_canvas.x - origin.x) as f64,
                (cursor_canvas.y - origin.y) as f64,
            );
            best = Some((e, rect.z, local));
        }
    }
    best.map(|(e, _, local)| (e, local))
}

/// Per-pane annotation editors + the entity their geometry is parented under.
#[derive(Resource, Default)]
struct PaneAnnotations {
    editors: HashMap<Entity, WbEditor>,
    /// The `content_root` child each pane's geometry is rendered under (high z so
    /// it draws over the pane content).
    roots: HashMap<Entity, Entity>,
    dirty: HashSet<Entity>,
    /// The pane currently being drawn on (between press and release).
    drawing: Option<Entity>,
}

/// Marks an annotation geometry root (a child of a pane's `content_root`).
#[derive(Component)]
struct PaneAnnoRoot;

/// Tell the pane mouse handler to ignore a press on a normal pane when we want to
/// route it to an annotation: always for a drawing tool, or for select/eraser
/// only when the cursor is on an annotation (so clicking empty pane area still
/// moves the pane). Also stays suppressed mid-gesture so a drag that wanders off
/// the ink doesn't snap back to pane interaction.
fn update_suppression(
    canvas_active: Res<CanvasDrawActive>,
    ts: Res<WbToolState>,
    viewport: Res<PaneViewport>,
    windows: Query<&Window>,
    anno: Res<PaneAnnotations>,
    panes: PaneQuery,
    mut suppressed: ResMut<PaneInputSuppressed>,
) {
    let want = canvas_active.0
        && (is_drawing_tool(ts.tool) || anno.drawing.is_some() || {
            // select/eraser: suppress only when hovering an annotation element.
            windows
                .single()
                .ok()
                .and_then(|w| w.cursor_position())
                .and_then(|c| pane_under_cursor(&panes, viewport.window_to_canvas(c)))
                .and_then(|(pane, local)| {
                    anno.editors.get(&pane).map(|ed| ed.scene().topmost_at(local).is_some())
                })
                .unwrap_or(false)
        });
    if suppressed.0 != want {
        suppressed.0 = want;
    }
}

fn modifiers(keys: &ButtonInput<KeyCode>) -> Modifiers {
    Modifiers {
        shift: keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight),
        ctrl: keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight),
        alt: keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight),
        meta: keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight),
    }
}

/// Stamp the toolbar's active style onto newly-created elements (same as the
/// other surfaces — element creation uses fixed defaults otherwise).
fn stamp_new(editor: &mut WbEditor, before: &HashSet<ElementId>, style: &ToolStyle) {
    let new_ids: Vec<ElementId> = editor
        .scene()
        .iter_live()
        .filter(|e| !before.contains(&e.id))
        .map(|e| e.id.clone())
        .collect();
    for id in new_ids {
        if let Some(el) = editor.scene_mut().get_mut(&id) {
            el.stroke_color = style.stroke_color;
            el.stroke_width = style.stroke_width;
            el.roughness = style.roughness;
            if matches!(
                el.kind,
                ElementKind::Rectangle | ElementKind::Ellipse | ElementKind::Diamond
            ) {
                el.background_color = style.background_color;
                el.fill_style = style.fill_style;
            }
        }
    }
}

/// The content-area top-left of a pane in canvas space, and the content size.
fn content_rect_canvas(rect: &PaneRect) -> (Vec2, Vec2) {
    let origin = Vec2::new(rect.pos.x + MARGIN, rect.pos.y + TITLE_H + MARGIN);
    let size = Vec2::new(
        (rect.size.x - 2.0 * MARGIN).max(0.0),
        (rect.size.y - TITLE_H - 2.0 * MARGIN).max(0.0),
    );
    (origin, size)
}

#[allow(clippy::too_many_arguments)]
fn annotation_input(
    canvas_active: Res<CanvasDrawActive>,
    ts: Res<WbToolState>,
    viewport: Res<PaneViewport>,
    windows: Query<&Window>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut consumed: ResMut<InputConsumed>,
    mut anno: ResMut<PaneAnnotations>,
    panes: PaneQuery,
) {
    if !canvas_active.0 {
        anno.drawing = None;
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let cursor_canvas = viewport.window_to_canvas(cursor);
    let mods = modifiers(&keys);
    let drawing_tool = is_drawing_tool(ts.tool);

    // Mid-gesture we stay on the pane we started on (so a drag can run past its
    // edge); otherwise pick the topmost pane whose content contains the cursor.
    let (pane, pos) = if let (Some(pane), true) = (anno.drawing, buttons.pressed(MouseButton::Left))
    {
        let Ok((_, rect, _, _, _)) = panes.get(pane) else {
            anno.drawing = None;
            return;
        };
        let (origin, _) = content_rect_canvas(rect);
        (
            pane,
            Point::new(
                (cursor_canvas.x - origin.x) as f64,
                (cursor_canvas.y - origin.y) as f64,
            ),
        )
    } else {
        match pane_under_cursor(&panes, cursor_canvas) {
            Some(t) => t,
            None => {
                if buttons.just_pressed(MouseButton::Left) {
                    anno.drawing = None;
                }
                return;
            }
        }
    };
    let style = ts.0.clone();

    if buttons.just_pressed(MouseButton::Left) {
        if consumed.0 {
            return;
        }
        // A drawing tool paints anywhere; select/eraser only act when the press
        // lands on an existing annotation (else the pane keeps the click).
        let editor = anno.editors.entry(pane).or_insert_with(new_editor);
        if !drawing_tool && editor.scene().topmost_at(pos).is_none() {
            // Nothing to select here — clear any prior selection, let the pane have it.
            if !editor.selection().is_empty() {
                editor.clear_selection();
                anno.dirty.insert(pane);
            }
            return;
        }
        editor.set_tool(style.tool);
        let before: HashSet<ElementId> =
            editor.scene().iter_live().map(|e| e.id.clone()).collect();
        editor.handle(InputEvent::PointerDown {
            pos,
            button: PointerButton::Primary,
            mods,
        });
        stamp_new(editor, &before, &style);
        anno.drawing = Some(pane);
        anno.dirty.insert(pane);
        consumed.0 = true;
    } else if anno.drawing == Some(pane) && buttons.pressed(MouseButton::Left) {
        if let Some(editor) = anno.editors.get_mut(&pane) {
            editor.handle(InputEvent::PointerMove { pos, mods });
            anno.dirty.insert(pane);
        }
        consumed.0 = true;
    } else if anno.drawing == Some(pane) && buttons.just_released(MouseButton::Left) {
        if let Some(editor) = anno.editors.get_mut(&pane) {
            editor.handle(InputEvent::PointerUp {
                pos,
                button: PointerButton::Primary,
                mods,
            });
            anno.dirty.insert(pane);
        }
        anno.drawing = None;
    }
}

/// Render each dirtied pane's annotation editor into a high-z child of its
/// `content_root`. Children of `content_root` automatically move + clip with the
/// pane; the high-z root keeps the ink above the pane's own content.
#[allow(clippy::too_many_arguments)]
fn render_annotations(
    mut anno: ResMut<PaneAnnotations>,
    chromes: Query<&PaneChrome>,
    anno_roots: Query<(), With<PaneAnnoRoot>>,
    font: Option<Res<jim_pane::PaneFont>>,
    rendered: Query<(Entity, &ChildOf), With<WbRendered>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut commands: Commands,
) {
    let Some(font) = font else {
        return;
    };
    if anno.dirty.is_empty() {
        return;
    }
    let dirty: Vec<Entity> = anno.dirty.drain().collect();
    for pane in dirty {
        let Ok(chrome) = chromes.get(pane) else {
            // Pane gone — drop its annotations.
            anno.editors.remove(&pane);
            anno.roots.remove(&pane);
            continue;
        };
        // Ensure an annotation root exists under the content_root (z above the
        // pane content; the content area never overlaps the title cover, so a
        // high z here is safe).
        let root = match anno.roots.get(&pane) {
            Some(&r) if anno_roots.get(r).is_ok() => r,
            _ => {
                let r = commands
                    .spawn((
                        Transform::from_xyz(0.0, 0.0, 0.5),
                        Visibility::Visible,
                        PaneAnnoRoot,
                        ChildOf(chrome.content_root),
                    ))
                    .id();
                anno.roots.insert(pane, r);
                r
            }
        };
        // Clear previous geometry under this root.
        for (e, parent) in &rendered {
            if parent.0 == root {
                commands.entity(e).despawn();
            }
        }
        if let Some(editor) = anno.editors.get(&pane) {
            // Overlay shows the selection box / marquee for annotation editing.
            let scene = editor.render_with_overlay();
            render_scene_into(&scene, root, &font.0, &mut meshes, &mut materials, &mut commands);
        }
    }
}

/// The canvas toolbar's "Clear" button wipes pane annotations too.
fn clear_annotations(
    mut events: MessageReader<jim_whiteboard::ClearCanvasRequested>,
    mut anno: ResMut<PaneAnnotations>,
) {
    if events.read().count() == 0 {
        return;
    }
    let panes: Vec<Entity> = anno.editors.keys().copied().collect();
    for pane in panes {
        if let Some(editor) = anno.editors.get_mut(&pane) {
            let ids: Vec<ElementId> = editor.scene().iter_live().map(|e| e.id.clone()).collect();
            if !ids.is_empty() {
                editor.select(ids);
                editor.delete_selection();
            }
            editor.clear_selection();
        }
        anno.dirty.insert(pane);
    }
    anno.drawing = None;
}

/// Delete/undo for selected annotations (same gate as the canvas board keyboard:
/// toolbar-focused or nothing).
fn annotation_keyboard(
    canvas_active: Res<CanvasDrawActive>,
    focused: Res<jim_pane::FocusedPane>,
    pane_kinds: Query<&PaneKindMarker>,
    keys: Res<ButtonInput<KeyCode>>,
    mut key_events: MessageReader<bevy::input::keyboard::KeyboardInput>,
    mut anno: ResMut<PaneAnnotations>,
) {
    use bevy::input::keyboard::Key as BKey;
    let focused_kind = focused.0.and_then(|e| pane_kinds.get(e).ok()).map(|k| k.0);
    let owns = matches!(focused_kind, None | Some(jim_whiteboard::toolbar::PANE_KIND));
    if !canvas_active.0 || !owns {
        key_events.clear();
        return;
    }
    let cmd = keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight)
        || keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);

    let panes: Vec<Entity> = anno.editors.keys().copied().collect();
    for ev in key_events.read() {
        if ev.state != bevy::input::ButtonState::Pressed {
            continue;
        }
        for &pane in &panes {
            let Some(editor) = anno.editors.get_mut(&pane) else { continue };
            let changed = match &ev.logical_key {
                BKey::Backspace | BKey::Delete => editor.delete_selection(),
                BKey::Escape => {
                    editor.clear_selection();
                    true
                }
                BKey::Character(s) if cmd && s.eq_ignore_ascii_case("z") => {
                    if shift {
                        editor.redo()
                    } else {
                        editor.undo()
                    }
                }
                _ => false,
            };
            if changed {
                anno.dirty.insert(pane);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_rect_excludes_title_and_margins() {
        let rect = PaneRect { pos: Vec2::new(100.0, 50.0), size: Vec2::new(400.0, 300.0), z: 1.0 };
        let (origin, size) = content_rect_canvas(&rect);
        // Content starts MARGIN in and below the title band.
        assert_eq!(origin, Vec2::new(100.0 + MARGIN, 50.0 + TITLE_H + MARGIN));
        assert_eq!(size, Vec2::new(400.0 - 2.0 * MARGIN, 300.0 - TITLE_H - 2.0 * MARGIN));
        // A cursor at the content top-left maps to local (0,0).
        let cursor_canvas = origin;
        let local = cursor_canvas - origin;
        assert_eq!(local, Vec2::ZERO);
    }
}

pub struct PaneAnnotationPlugin;

impl Plugin for PaneAnnotationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PaneAnnotations>()
            .add_systems(
                Update,
                (
                    update_suppression,
                    annotation_input,
                    annotation_keyboard,
                    clear_annotations,
                ),
            )
            .add_systems(PostUpdate, render_annotations);
    }
}
