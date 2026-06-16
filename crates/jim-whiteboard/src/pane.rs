//! The whiteboard **drawing pane** kind: a floating pane whose content area is
//! an Excalidraw-style canvas. The pane owns a `whiteboard-core` `Editor`; pane
//! content mouse events and keyboard shortcuts drive it, and its draw-command
//! output is tessellated into the pane's `content_root` each time it changes.

use bevy::input::keyboard::{Key as BKey, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::sprite_render::ColorMaterial;

use std::collections::HashSet;

use jim_pane::{
    spawn_pane_from_registry, FocusedPane, KeyboardOwner, PaneChrome, PaneContentDragged,
    PaneContentPressed, PaneContentReleased, PaneFont, PaneKindSpec, PaneRect, PaneRegistry,
};

use serde_json::Value;
use whiteboard_core::element::{Element, ElementKind};
use whiteboard_core::interaction::{InputEvent, Key as WbKey, Modifiers, PointerButton, Tool};
use whiteboard_core::{ElementId, Point};

use crate::island::{island_hit, Island, IslandAction};
use crate::render::{render_scene_into, WbRendered};
use crate::{ToolStyle, WbEditor};

pub const PANE_KIND: &str = "whiteboard";

/// A floating whiteboard pane: owns the editor, its own tool/style state (driven
/// by the in-pane island toolbar — Mode 1), and a dirty flag that gates mesh
/// rebuilds.
#[derive(Component)]
pub struct WhiteboardPane {
    pub editor: WbEditor,
    /// This pane's active tool + stamp style, independent of the canvas toolbar.
    pub style: ToolStyle,
    /// Set whenever the scene/view changed; the render system rebuilds and
    /// clears it.
    pub dirty: bool,
}

impl WhiteboardPane {
    fn new() -> Self {
        WhiteboardPane {
            editor: crate::new_editor(),
            style: ToolStyle::default(),
            dirty: true,
        }
    }
}

pub(crate) fn register(registry: &mut PaneRegistry) {
    registry.register(PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Whiteboard",
        radial_icon: Some("✎"),
        default_size: Vec2::new(640.0, 460.0),
        spawn: spawn_from_config,
        snapshot,
        on_close: None,
    });
}

fn spawn_from_config(world: &mut World, entity: Entity, _content_root: Entity, config: &Value) {
    let mut wp = WhiteboardPane::new();
    // Restore persisted elements (if any) straight into the scene without
    // touching undo history.
    if let Some(els) = config.get("elements").and_then(|v| v.as_array()) {
        for ev in els {
            if let Ok(el) = serde_json::from_value::<Element>(ev.clone()) {
                wp.editor.scene_mut().insert(el);
            }
        }
    }
    world.entity_mut(entity).insert((wp, Island::default()));
}

fn snapshot(world: &World, entity: Entity) -> Value {
    let Some(wp) = world.get::<WhiteboardPane>(entity) else {
        return Value::Null;
    };
    let elements: Vec<&Element> = wp.editor.scene().iter_live().collect();
    serde_json::json!({ "elements": elements })
}

/// Convenience: spawn a whiteboard pane through the registry.
pub fn spawn_whiteboard(world: &mut World, rect: PaneRect, project: Option<u64>) -> Option<Entity> {
    spawn_pane_from_registry(world, PANE_KIND, "Whiteboard", rect, project, &Value::Null)
}

// ---------- Input ----------

fn modifiers(keys: &ButtonInput<KeyCode>, shift_override: Option<bool>) -> Modifiers {
    Modifiers {
        shift: shift_override.unwrap_or_else(|| {
            keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight)
        }),
        ctrl: keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight),
        alt: keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight),
        meta: keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight),
    }
}

fn live_ids(editor: &WbEditor) -> HashSet<ElementId> {
    editor.scene().iter_live().map(|e| e.id.clone()).collect()
}

/// Stamp the active style onto every element created since `before`. Element
/// creation in whiteboard-core uses fixed defaults; this is where the toolbar's
/// color/width/fill actually take effect.
fn stamp_new(editor: &mut WbEditor, before: &HashSet<ElementId>, ts: &ToolStyle) {
    let new_ids: Vec<ElementId> = editor
        .scene()
        .iter_live()
        .filter(|e| !before.contains(&e.id))
        .map(|e| e.id.clone())
        .collect();
    for id in new_ids {
        if let Some(el) = editor.scene_mut().get_mut(&id) {
            el.stroke_color = ts.stroke_color;
            el.stroke_width = ts.stroke_width;
            el.roughness = ts.roughness;
            // Closed shapes get the fill; lines/arrows/freedraw/text don't.
            if matches!(
                el.kind,
                ElementKind::Rectangle | ElementKind::Ellipse | ElementKind::Diamond
            ) {
                el.background_color = ts.background_color;
                el.fill_style = ts.fill_style;
            }
        }
    }
}

/// Apply an island button action to a pane, returning true if it changed
/// anything visible (so the caller can mark the pane dirty).
fn apply_island(wp: &mut WhiteboardPane, action: IslandAction) {
    match action {
        IslandAction::Tool(t) => {
            wp.style.tool = t;
            wp.editor.set_tool(t);
        }
        IslandAction::Color(c) => wp.style.stroke_color = c,
        IslandAction::Width(w) => wp.style.stroke_width = w,
        IslandAction::Clear => {
            let ids: Vec<ElementId> =
                wp.editor.scene().iter_live().map(|e| e.id.clone()).collect();
            if !ids.is_empty() {
                wp.editor.select(ids);
                wp.editor.delete_selection();
                wp.dirty = true;
            }
        }
    }
}

fn whiteboard_mouse(
    mut pressed: MessageReader<PaneContentPressed>,
    mut dragged: MessageReader<PaneContentDragged>,
    mut released: MessageReader<PaneContentReleased>,
    keys: Res<ButtonInput<KeyCode>>,
    mut panes: Query<(&mut WhiteboardPane, &mut Island)>,
) {
    for ev in pressed.read() {
        let Ok((mut wp, mut island)) = panes.get_mut(ev.pane) else {
            continue;
        };
        // The island toolbar floats over the drawing area; clicks on it drive
        // tool/style/clear and must NOT start a stroke.
        if let Some(action) = island_hit(&island, ev.local_pt) {
            apply_island(&mut wp, action);
            island.mark_dirty();
            island.capturing = true;
            continue;
        }
        let tool = wp.style.tool;
        wp.editor.set_tool(tool);
        let before = live_ids(&wp.editor);
        let pos = Point::new(ev.local_pt.x as f64, ev.local_pt.y as f64);
        let mods = modifiers(&keys, Some(ev.shift));
        let r = wp.editor.handle(InputEvent::PointerDown {
            pos,
            button: PointerButton::Primary,
            mods,
        });
        let style = wp.style.clone();
        stamp_new(&mut wp.editor, &before, &style);
        if r.needs_redraw() {
            wp.dirty = true;
        }
        // Creation/selection always needs a repaint of the in-progress shape.
        wp.dirty = true;
    }
    for ev in dragged.read() {
        let Ok((mut wp, island)) = panes.get_mut(ev.pane) else {
            continue;
        };
        // A drag that began on the island isn't a stroke.
        if island.capturing {
            continue;
        }
        let pos = Point::new(ev.local_pt.x as f64, ev.local_pt.y as f64);
        let mods = modifiers(&keys, None);
        let r = wp.editor.handle(InputEvent::PointerMove { pos, mods });
        if r.needs_redraw() {
            wp.dirty = true;
        }
    }
    for ev in released.read() {
        let Ok((mut wp, mut island)) = panes.get_mut(ev.pane) else {
            continue;
        };
        if island.capturing {
            island.capturing = false;
            continue;
        }
        let pos = Point::new(ev.local_pt.x as f64, ev.local_pt.y as f64);
        let mods = modifiers(&keys, None);
        let r = wp.editor.handle(InputEvent::PointerUp {
            pos,
            button: PointerButton::Primary,
            mods,
        });
        if r.needs_redraw() {
            wp.dirty = true;
        }
        wp.dirty = true;
    }
}

/// Map a Bevy `KeyCode` to a tool for the single-letter shortcuts (no modifier).
fn tool_for_key(code: KeyCode) -> Option<Tool> {
    Some(match code {
        KeyCode::KeyV => Tool::Select,
        KeyCode::KeyH => Tool::Pan,
        KeyCode::KeyR => Tool::Rectangle,
        KeyCode::KeyO => Tool::Ellipse,
        KeyCode::KeyD => Tool::Diamond,
        KeyCode::KeyL => Tool::Line,
        KeyCode::KeyA => Tool::Arrow,
        KeyCode::KeyP | KeyCode::KeyF => Tool::Freedraw,
        KeyCode::KeyT => Tool::Text,
        KeyCode::KeyE => Tool::Eraser,
        _ => return None,
    })
}

fn whiteboard_keyboard(
    mut key_events: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    focused: Res<FocusedPane>,
    owner: Res<KeyboardOwner>,
    mut panes: Query<(&mut WhiteboardPane, &mut Island)>,
) {
    let Some(pane) = focused.0 else {
        // Still drain the reader so events don't pile up.
        key_events.clear();
        return;
    };
    if !owner.allows_pane(pane) {
        key_events.clear();
        return;
    }
    let Ok((mut wp, mut island)) = panes.get_mut(pane) else {
        key_events.clear();
        return;
    };

    let cmd = keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight)
        || keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    let mods = modifiers(&keys, None);

    for ev in key_events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }

        // While typing into a text element, route everything into the editor.
        if wp.editor.is_editing_text() {
            let wk = match &ev.logical_key {
                BKey::Character(s) => s.chars().next().map(WbKey::Char),
                BKey::Space => Some(WbKey::Char(' ')),
                BKey::Enter => Some(WbKey::Enter),
                BKey::Backspace => Some(WbKey::Backspace),
                BKey::Escape => Some(WbKey::Escape),
                _ => None,
            };
            if let Some(k) = wk {
                let r = wp.editor.handle(InputEvent::KeyDown { key: k, mods });
                if r.needs_redraw() {
                    wp.dirty = true;
                }
            }
            continue;
        }

        // Command shortcuts.
        if cmd {
            match &ev.logical_key {
                BKey::Character(s) => {
                    let c = s.to_lowercase();
                    let changed = match c.as_str() {
                        "z" if shift => wp.editor.redo(),
                        "z" => wp.editor.undo(),
                        "y" => wp.editor.redo(),
                        "c" => {
                            wp.editor.copy();
                            false
                        }
                        "x" => wp.editor.cut(),
                        "v" => {
                            !wp.editor
                                .paste(whiteboard_core::Vec2::new(12.0, 12.0))
                                .is_empty()
                        }
                        "d" => !wp.editor.duplicate_selection().is_empty(),
                        "a" => {
                            let ids: Vec<ElementId> =
                                wp.editor.scene().iter_live().map(|e| e.id.clone()).collect();
                            wp.editor.select(ids);
                            true
                        }
                        _ => false,
                    };
                    if changed {
                        wp.dirty = true;
                    }
                }
                _ => {}
            }
            continue;
        }

        // Plain keys: tool switches + delete/escape.
        match &ev.logical_key {
            BKey::Backspace | BKey::Delete => {
                if wp.editor.delete_selection() {
                    wp.dirty = true;
                }
            }
            BKey::Escape => {
                wp.editor.clear_selection();
                wp.dirty = true;
            }
            _ => {
                if let Some(tool) = tool_for_key(ev.key_code) {
                    wp.style.tool = tool;
                    wp.editor.set_tool(tool);
                    wp.dirty = true;
                    island.mark_dirty();
                }
            }
        }
    }
}

// ---------- Rendering ----------

fn render_whiteboard_panes(
    mut panes: Query<(&mut WhiteboardPane, &PaneChrome)>,
    rendered: Query<(Entity, &ChildOf), With<WbRendered>>,
    font: Option<Res<PaneFont>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut commands: Commands,
) {
    let Some(font) = font else {
        return;
    };
    for (mut wp, chrome) in &mut panes {
        if !wp.dirty {
            continue;
        }
        let content_root = chrome.content_root;
        // Clear previous geometry for this pane.
        for (e, parent) in &rendered {
            if parent.0 == content_root {
                commands.entity(e).despawn();
            }
        }
        let scene = wp.editor.render_with_overlay();
        render_scene_into(
            &scene,
            content_root,
            &font.0,
            &mut meshes,
            &mut materials,
            &mut commands,
        );
        wp.dirty = false;
    }
}

pub(crate) fn build(app: &mut App) {
    app.add_systems(Update, (whiteboard_mouse, whiteboard_keyboard));
    app.add_systems(PostUpdate, render_whiteboard_panes);
}
