//! Drawing directly on the **project canvas background**.
//!
//! When the whiteboard toolbar's "BG" toggle is on, left-dragging on empty
//! canvas paints onto a per-project background board (instead of doing nothing —
//! canvas panning is bound to middle-mouse / space-drag, so the left button is
//! free). The board pans and zooms with the canvas and renders on the main
//! camera's layer 0, behind every pane.
//!
//! This lives in `jim-app` rather than `jim-whiteboard` because it needs the
//! canvas viewport, the active project, and correct ordering against canvas pan
//! — all of which are app-shell concerns.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bevy::camera::visibility::RenderLayers;
use bevy::input::keyboard::{Key as BKey, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::sprite_render::ColorMaterial;

use jim_pane::{
    FocusedPane, InputConsumed, PaneRect, PaneScreenAnchored, PaneTag, PaneViewport,
    PaneViewportReaders,
};

use whiteboard_core::element::{apply_bound_endpoints, update_bound_arrow, Element, ElementKind};
use whiteboard_core::interaction::{
    InputEvent, Key as WbKey, Modifiers, PointerButton, Tool, Viewport as WbViewport,
};
use whiteboard_core::render::Color as WbColor;
use whiteboard_core::{ElementId, Point, Vec2 as WbVec2};

use jim_whiteboard::render::{render_scene_into_layer, WbRendered};
use jim_whiteboard::{
    new_editor, CanvasDrawActive, CanvasEdit, ClearCanvasRequested, WbEditor, WbToolState, ZOrder,
};

use crate::projects::Projects;

/// Z of the background board on layer 0 — behind panes (their cameras draw over
/// the main camera) and behind the sidebar (z ≥ 0), but in front of the canvas
/// backdrop.
const Z_BG: f32 = -1.0;

/// Per-project background boards, lazily created/loaded.
#[derive(Resource, Default)]
pub struct BackgroundBoards {
    boards: HashMap<u64, WbEditor>,
    loaded: HashSet<u64>,
}

impl BackgroundBoards {
    fn board_mut(&mut self, project: u64) -> &mut WbEditor {
        if self.loaded.insert(project) {
            let mut editor = new_editor();
            if let Some(els) = load_board(project) {
                for el in els {
                    editor.scene_mut().insert(el);
                }
            }
            self.boards.insert(project, editor);
        }
        self.boards.entry(project).or_insert_with(new_editor)
    }
}

/// Remembers each project's last-used canvas tool. `WbToolState` is a single
/// global resource (the floating toolbar mutates it), so without this the tool
/// bleeds across projects — pick Draw in one project and every other project's
/// canvas is in Draw mode too. We swap the tool in/out when the active project
/// changes; color/width stay shared (only the tool was reported as leaking).
#[derive(Resource, Default)]
struct ProjectTools {
    by_project: HashMap<u64, Tool>,
    current: Option<u64>,
}

/// Swap the active canvas tool in/out of [`ProjectTools`] whenever the active
/// project changes, so each project keeps its own tool. Runs before the input
/// systems so the swap is in effect before any draw/select this frame.
fn sync_project_tool(
    projects: Res<Projects>,
    mut ts: ResMut<WbToolState>,
    mut stash: ResMut<ProjectTools>,
) {
    if stash.current == projects.active {
        return;
    }
    if let Some(prev) = stash.current {
        stash.by_project.insert(prev, ts.0.tool);
    }
    if let Some(now) = projects.active {
        // Unvisited project → Tool::default() (Select), so entering a project
        // never silently leaves you in Draw mode.
        ts.0.tool = stash.by_project.get(&now).copied().unwrap_or_default();
    }
    stash.current = projects.active;
}

/// The single root entity that all background geometry is parented under. Its
/// transform encodes the canvas→world mapping so the (identity-parented) meshes
/// land in the right place; the board editor's own viewport bakes pan/zoom.
#[derive(Resource)]
struct BackgroundRoot(Entity);

/// Tracks an in-progress background stroke and what view it was last rendered
/// at, so we only re-tessellate when something actually changed.
#[derive(Resource, Default)]
struct BgState {
    drawing: bool,
    /// Force a rebuild next frame.
    dirty: bool,
    last_project: Option<u64>,
    last_pan: Vec2,
    last_zoom: f32,
    last_origin: Vec2,
    last_win: Vec2,
}

fn bg_dir() -> Option<PathBuf> {
    let d = crate::data_dir()?.join("whiteboard");
    let _ = std::fs::create_dir_all(&d);
    Some(d)
}

fn board_path(project: u64) -> Option<PathBuf> {
    Some(bg_dir()?.join(format!("bg-{project}.json")))
}

fn load_board(project: u64) -> Option<Vec<Element>> {
    let path = board_path(project)?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Vec<Element>>(&bytes).ok()
}

fn save_board(project: u64, editor: &WbEditor) {
    let Some(path) = board_path(project) else {
        return;
    };
    // Pane proxies are ephemeral (re-synced from live panes each frame); never
    // persist them.
    let els: Vec<&Element> = editor
        .scene()
        .iter_live()
        .filter(|e| !is_proxy(&e.id))
        .collect();
    if let Ok(json) = serde_json::to_vec(&els) {
        let _ = std::fs::write(path, json);
    }
}

/// Id prefix marking the invisible, locked proxy elements that mirror live panes
/// so canvas arrows can bind to them.
const PROXY_PREFIX: &str = "pane:";

fn proxy_id(entity: Entity) -> ElementId {
    ElementId::from(format!("{PROXY_PREFIX}{}", entity.to_bits()))
}

fn is_proxy(id: &ElementId) -> bool {
    id.as_str().starts_with(PROXY_PREFIX)
}

fn setup_background_root(mut commands: Commands) {
    let e = commands
        .spawn((
            Transform::from_xyz(0.0, 0.0, Z_BG),
            Visibility::Visible,
            Name::new("whiteboard-background-root"),
            // Render on the whiteboard overlay layer so the drawing draws ON
            // TOP of panes (via the dedicated overlay camera), not behind them.
            RenderLayers::layer(crate::WHITEBOARD_OVERLAY_LAYER),
        ))
        .id();
    commands.insert_resource(BackgroundRoot(e));
}

fn modifiers(keys: &ButtonInput<KeyCode>) -> Modifiers {
    Modifiers {
        shift: keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight),
        ctrl: keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight),
        alt: keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight),
        meta: keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight),
    }
}

/// Where a left-press on the canvas should go. The whiteboard is a drawing
/// LAYER over the panes, but it does NOT seize all input: the active tool
/// decides. Draw/shape/eraser tools paint on the board (over panes). The
/// Select/Pan tools leave panes fully interactive — a press on a pane drags
/// the pane, a press on empty canvas selects drawn strokes. The toolbar always
/// switches tools, and the sidebar gutter is off-limits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CanvasPress {
    /// Not in canvas-draw mode (toolbar closed) or on the sidebar gutter —
    /// leave the press to the normal pane systems (drag/resize/focus).
    Ignore,
    /// Press is on the toolbar — let the pane/toolbar systems handle it so it
    /// switches tools instead of drawing.
    DeferToToolbar,
    /// Select/Pan tool over a pane — let the pane systems drag/resize it.
    DeferToPanes,
    /// Claim the press for the board (draw, or select/marquee of drawn
    /// elements). We set `InputConsumed` so panes don't also react.
    Board,
}

/// True for tools that paint on the board (and so claim the surface, drawing
/// over panes). Select/Pan instead interact with panes.
fn tool_claims_surface(tool: Tool) -> bool {
    !matches!(tool, Tool::Select | Tool::Pan)
}

/// Pure routing decision — no Bevy types, so it's unit-tested directly. Keeping
/// this explicit and total is what removes the old order-dependent jank (the
/// background surface used to race the pane system over `InputConsumed`).
fn decide_canvas_press(
    canvas_active: bool,
    tool_claims_surface: bool,
    over_toolbar: bool,
    over_pane: bool,
    on_sidebar: bool,
) -> CanvasPress {
    if !canvas_active || on_sidebar {
        CanvasPress::Ignore
    } else if over_toolbar {
        // Always switch tools — never draw on the toolbar.
        CanvasPress::DeferToToolbar
    } else if tool_claims_surface {
        // Draw/shape/eraser → paint on the board, over panes.
        CanvasPress::Board
    } else if over_pane {
        // Select/Pan on a pane → drag/resize the pane.
        CanvasPress::DeferToPanes
    } else {
        // Select/Pan on empty canvas → board select / marquee.
        CanvasPress::Board
    }
}

/// True if `cursor` (window px) lands on a visible, non-anchored pane. Used to
/// decide whether a Select/Pan press should drive a pane instead of the board.
fn over_pane(
    cursor: Vec2,
    viewport: &PaneViewport,
    panes: &Query<(&PaneRect, Option<&Visibility>, Has<PaneScreenAnchored>), With<PaneTag>>,
) -> bool {
    let cursor_canvas = viewport.window_to_canvas(cursor);
    for (rect, vis, anchored) in panes.iter() {
        if anchored || matches!(vis, Some(Visibility::Hidden)) {
            continue;
        }
        if cursor_canvas.x >= rect.pos.x
            && cursor_canvas.x <= rect.pos.x + rect.size.x
            && cursor_canvas.y >= rect.pos.y
            && cursor_canvas.y <= rect.pos.y + rect.size.y
        {
            return true;
        }
    }
    false
}

/// True if `cursor` (window px) lands on a visible Draw Tools toolbar. The
/// toolbar is screen-anchored, so its `PaneRect` is already in window space.
fn over_toolbar(
    cursor: Vec2,
    toolbars: &Query<(&PaneRect, Option<&Visibility>), With<jim_whiteboard::toolbar::ToolbarPane>>,
) -> bool {
    for (rect, vis) in toolbars.iter() {
        if matches!(vis, Some(Visibility::Hidden)) {
            continue;
        }
        if cursor.x >= rect.pos.x
            && cursor.x <= rect.pos.x + rect.size.x
            && cursor.y >= rect.pos.y
            && cursor.y <= rect.pos.y + rect.size.y
        {
            return true;
        }
    }
    false
}

fn stamp_new(editor: &mut WbEditor, before: &HashSet<ElementId>, ts: &WbToolState) {
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
            el.stroke_style = ts.stroke_style;
            el.roughness = ts.roughness;
            el.opacity = ts.opacity;
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

#[allow(clippy::too_many_arguments)]
/// Canvas drawing/selection input. Runs BEFORE the pane systems
/// (`.before(PaneViewportReaders)`) so that when we claim a press we set
/// `InputConsumed` first and the pane system skips it — no more racing over
/// who owns the click. Routing is decided by the pure [`decide_canvas_press`].
#[allow(clippy::too_many_arguments)]
fn background_input(
    ts: Res<WbToolState>,
    canvas_active: Res<CanvasDrawActive>,
    projects: Res<Projects>,
    viewport: Res<PaneViewport>,
    windows: Query<&Window>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut consumed: ResMut<InputConsumed>,
    mut boards: ResMut<BackgroundBoards>,
    mut state: ResMut<BgState>,
    toolbars: Query<(&PaneRect, Option<&Visibility>), With<jim_whiteboard::toolbar::ToolbarPane>>,
    panes: Query<(&PaneRect, Option<&Visibility>, Has<PaneScreenAnchored>), With<PaneTag>>,
) {
    // A press we already claimed keeps driving until release — even if the
    // cursor wanders over the toolbar mid-stroke — so a stroke can't be
    // hijacked partway through.
    let press_active = state.drawing;

    if !canvas_active.0 {
        // Toolbar closed → not in canvas-draw mode. End any in-progress stroke
        // and leave the panes to the normal systems.
        state.drawing = false;
        return;
    }
    let Some(project) = projects.active else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let origin = viewport.origin;
    let mods = modifiers(&keys);

    // Sync the board's viewport to the canvas pan/zoom so editor screen-space ==
    // window-space-minus-origin.
    let board = boards.board_mut(project);
    board.set_viewport(WbViewport {
        scroll: WbVec2::new(viewport.pan.x as f64, viewport.pan.y as f64),
        zoom: viewport.zoom.max(0.0001) as f64,
    });
    let pos = Point::new((cursor.x - origin.x) as f64, (cursor.y - origin.y) as f64);

    if buttons.just_pressed(MouseButton::Left) {
        let route = decide_canvas_press(
            canvas_active.0,
            tool_claims_surface(ts.tool),
            over_toolbar(cursor, &toolbars),
            over_pane(cursor, &viewport, &panes),
            cursor.x < origin.x,
        );
        if route != CanvasPress::Board {
            // Toolbar click, pane drag (Select/Pan), or sidebar — let the pane
            // systems own it. Don't consume, so panes drag and the toolbar can
            // switch tools.
            return;
        }
        board.set_tool(ts.tool);
        let before: HashSet<ElementId> =
            board.scene().iter_live().map(|e| e.id.clone()).collect();
        board.handle(InputEvent::PointerDown {
            pos,
            button: PointerButton::Primary,
            mods,
        });
        stamp_new(board, &before, &ts);
        state.drawing = true;
        state.dirty = true;
        consumed.0 = true;
    } else if press_active && buttons.pressed(MouseButton::Left) {
        board.handle(InputEvent::PointerMove { pos, mods });
        state.dirty = true;
        consumed.0 = true;
    } else if press_active && buttons.just_released(MouseButton::Left) {
        board.handle(InputEvent::PointerUp {
            pos,
            button: PointerButton::Primary,
            mods,
        });
        state.drawing = false;
        state.dirty = true;
        save_board(project, board);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_background(
    projects: Res<Projects>,
    viewport: Res<PaneViewport>,
    windows: Query<&Window>,
    boards: Res<BackgroundBoards>,
    root: Option<Res<BackgroundRoot>>,
    mut state: ResMut<BgState>,
    font: Option<Res<jim_pane::PaneFont>>,
    rendered: Query<(Entity, &ChildOf), With<WbRendered>>,
    mut transforms: Query<&mut Transform>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut commands: Commands,
) {
    let (Some(root), Some(font)) = (root, font) else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    let win = Vec2::new(window.width(), window.height());
    let origin = viewport.origin;
    let project = projects.active;

    // Decide whether anything changed that requires a re-tessellation.
    let view_changed = state.last_project != project
        || state.last_pan != viewport.pan
        || (state.last_zoom - viewport.zoom).abs() > 1e-4
        || state.last_origin != origin
        || state.last_win != win;
    if !state.dirty && !view_changed {
        return;
    }
    state.dirty = false;
    state.last_project = project;
    state.last_pan = viewport.pan;
    state.last_zoom = viewport.zoom;
    state.last_origin = origin;
    state.last_win = win;

    // Position the root so identity-parented, y-flipped vertices land at the
    // correct world position (see module/header math).
    if let Ok(mut t) = transforms.get_mut(root.0) {
        t.translation = Vec3::new(origin.x - win.x * 0.5, win.y * 0.5 - origin.y, Z_BG);
    }

    // Clear old geometry.
    for (e, parent) in &rendered {
        if parent.0 == root.0 {
            commands.entity(e).despawn();
        }
    }

    let Some(project) = project else {
        return;
    };
    let Some(board) = boards.boards.get(&project) else {
        return;
    };
    // `render_with_overlay` adds the selection box / endpoint handles and — the
    // bit that was missing — the live **marquee rectangle** while drag-selecting
    // on the canvas. Same screen-space mapping as the scene, so it lands aligned.
    let scene = board.render_with_overlay();
    let layer = RenderLayers::layer(crate::WHITEBOARD_OVERLAY_LAYER);
    render_scene_into_layer(
        &scene,
        root.0,
        &font.0,
        Some(&layer),
        &mut meshes,
        &mut materials,
        &mut commands,
    );
}

/// Wipe the active project's background board when the canvas toolbar's "Clear"
/// button is pressed.
fn handle_clear_canvas(
    mut events: MessageReader<ClearCanvasRequested>,
    projects: Res<Projects>,
    mut boards: ResMut<BackgroundBoards>,
    mut state: ResMut<BgState>,
) {
    let mut any = false;
    for _ in events.read() {
        any = true;
    }
    if !any {
        return;
    }
    let Some(project) = projects.active else {
        return;
    };
    let board = boards.board_mut(project);
    // Clear the user's drawing but leave the pane proxies (re-synced anyway).
    let ids: Vec<ElementId> = board
        .scene()
        .iter_live()
        .filter(|e| !is_proxy(&e.id))
        .map(|e| e.id.clone())
        .collect();
    if !ids.is_empty() {
        board.select(ids);
        board.delete_selection();
    }
    state.dirty = true;
    save_board(project, board);
}

/// Mirror every live, visible canvas pane into the active project's background
/// board as an invisible, locked proxy `Rectangle`, so canvas-drawn arrows can
/// bind to panes. When a pane moves, its proxy moves and any bound arrow's
/// endpoint is recomputed to follow it. Only arrows the user explicitly bound
/// (by drawing onto a pane) move; freehand strokes and shapes stay fixed.
fn sync_pane_proxies(
    projects: Res<Projects>,
    canvas_active: Res<CanvasDrawActive>,
    mut boards: ResMut<BackgroundBoards>,
    mut state: ResMut<BgState>,
    panes: Query<
        (Entity, &PaneRect, Option<&Visibility>, Has<PaneScreenAnchored>),
        With<PaneTag>,
    >,
) {
    let Some(project) = projects.active else {
        return;
    };
    // Only maintain proxies once a board exists (user has drawn) or while the
    // canvas toolbar is open — don't conjure empty boards just by viewing.
    if !boards.boards.contains_key(&project) && !canvas_active.0 {
        return;
    }
    let board = boards.board_mut(project);

    // Desired proxy rects (canvas-space == board scene-space) from live panes.
    let mut desired: HashMap<ElementId, (f64, f64, f64, f64)> = HashMap::new();
    for (e, rect, vis, anchored) in panes.iter() {
        if anchored || matches!(vis, Some(Visibility::Hidden)) {
            continue;
        }
        desired.insert(
            proxy_id(e),
            (
                rect.pos.x as f64,
                rect.pos.y as f64,
                rect.size.x as f64,
                rect.size.y as f64,
            ),
        );
    }

    let mut moved = false;

    // Remove proxies whose pane is gone / hidden.
    let stale: Vec<ElementId> = board
        .scene()
        .iter_live()
        .filter(|e| is_proxy(&e.id) && !desired.contains_key(&e.id))
        .map(|e| e.id.clone())
        .collect();
    for id in stale {
        board.scene_mut().remove(&id);
        moved = true;
    }

    // Upsert proxies.
    for (id, (x, y, w, h)) in &desired {
        if let Some(el) = board.scene_mut().get_mut(id) {
            if el.x != *x || el.y != *y || el.width != *w || el.height != *h {
                el.x = *x;
                el.y = *y;
                el.width = *w;
                el.height = *h;
                moved = true;
            }
        } else {
            let mut el =
                Element::new(id.clone(), 1, *x, *y, *w, *h, ElementKind::Rectangle);
            el.locked = true;
            el.opacity = 0.0;
            el.stroke_color = WbColor::TRANSPARENT;
            el.background_color = WbColor::TRANSPARENT;
            board.scene_mut().insert(el);
            moved = true;
        }
    }

    // A proxy moved → recompute every bound arrow so it tracks its target.
    if moved {
        let arrows: Vec<ElementId> = board
            .scene()
            .iter_live()
            .filter(|e| matches!(e.kind, ElementKind::Arrow(_) | ElementKind::Line(_)))
            .map(|e| e.id.clone())
            .collect();
        for aid in arrows {
            let Some(endpoints) = update_bound_arrow(board.scene(), &aid) else {
                continue;
            };
            if let Some(el) = board.scene_mut().get_mut(&aid) {
                let origin = Point::new(el.x, el.y);
                let changed = if let ElementKind::Arrow(ref mut d) | ElementKind::Line(ref mut d) =
                    el.kind
                {
                    apply_bound_endpoints(d, origin, endpoints)
                } else {
                    false
                };
                if changed {
                    // Keep the arrow's bbox in sync with its moved endpoint.
                    whiteboard_core::element::resync_linear_box(el);
                    state.dirty = true;
                }
            }
        }
    }
}

/// Keyboard for canvas drawing (Mode 2): Delete/Backspace removes the selection,
/// Escape clears it, Cmd/Ctrl+Z / Shift+Z undo/redo. Only acts while the canvas
/// toolbar is open AND no pane is focused (so it never eats a terminal/editor's
/// keystrokes).
/// Map a plain character (no modifier) to a canvas tool — mirrors the
/// per-pane whiteboard's single-letter shortcuts so the canvas behaves the
/// same. Without this the canvas had no keyboard tool switching at all.
fn tool_for_char(c: &str) -> Option<Tool> {
    Some(match c {
        "v" => Tool::Select,
        "h" => Tool::Pan,
        "r" => Tool::Rectangle,
        "o" => Tool::Ellipse,
        "d" => Tool::Diamond,
        "l" => Tool::Line,
        "a" => Tool::Arrow,
        "p" | "f" => Tool::Freedraw,
        "t" => Tool::Text,
        "e" => Tool::Eraser,
        _ => return None,
    })
}

fn background_keyboard(
    canvas_active: Res<CanvasDrawActive>,
    focused: Res<FocusedPane>,
    projects: Res<Projects>,
    keys: Res<ButtonInput<KeyCode>>,
    pane_kinds: Query<&jim_pane::PaneKindMarker>,
    mut key_events: MessageReader<KeyboardInput>,
    mut boards: ResMut<BackgroundBoards>,
    mut state: ResMut<BgState>,
    mut ts: ResMut<WbToolState>,
) {
    // The canvas has no pane of its own, so focus stays on whatever pane was last
    // touched (usually the floating Draw Tools toolbar). Act when nothing is
    // focused OR the focused pane is the canvas toolbar; defer to any *other*
    // focused pane (a terminal/editor/whiteboard pane owns its own keys).
    let focused_kind: Option<&'static str> =
        focused.0.and_then(|e| pane_kinds.get(e).ok()).map(|k| k.0);
    let owns_keys = matches!(focused_kind, None | Some(jim_whiteboard::toolbar::PANE_KIND));
    if !canvas_active.0 || !owns_keys {
        key_events.clear();
        return;
    }
    let Some(project) = projects.active else {
        key_events.clear();
        return;
    };
    let cmd = keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight)
        || keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);

    let mods = modifiers(&keys);
    let board = boards.board_mut(project);
    let mut changed = false;
    for ev in key_events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        // While editing a text node, ALL keystrokes go into the text — never
        // the tool shortcuts (otherwise typing "v"/"r"/… switches tools instead
        // of inserting the character). Mirrors the per-pane whiteboard.
        if board.is_editing_text() {
            let wk = match &ev.logical_key {
                BKey::Character(s) => s.chars().next().map(WbKey::Char),
                BKey::Space => Some(WbKey::Char(' ')),
                BKey::Enter => Some(WbKey::Enter),
                BKey::Backspace => Some(WbKey::Backspace),
                BKey::Escape => Some(WbKey::Escape),
                _ => None,
            };
            if let Some(k) = wk {
                if board.handle(InputEvent::KeyDown { key: k, mods }).needs_redraw() {
                    changed = true;
                }
            }
            continue;
        }
        match &ev.logical_key {
            BKey::Backspace | BKey::Delete => {
                if board.delete_selection() {
                    changed = true;
                }
            }
            BKey::Escape => {
                board.clear_selection();
                changed = true;
            }
            BKey::Character(s) if cmd && s.eq_ignore_ascii_case("z") => {
                let ok = if shift { board.redo() } else { board.undo() };
                if ok {
                    changed = true;
                }
            }
            // Plain single-letter tool shortcuts (no modifier). Switches the
            // active canvas tool; background_input applies it on the next press.
            BKey::Character(s) if !cmd => {
                if let Some(tool) = tool_for_char(&s.to_lowercase()) {
                    ts.0.tool = tool;
                    board.set_tool(tool);
                }
            }
            _ => {}
        }
    }
    if changed {
        state.dirty = true;
        save_board(project, board);
    }
}

/// Apply a toolbar [`CanvasEdit`] to the active project's board: style edits hit
/// every selected element (Excalidraw "select and change"); layer/duplicate/
/// delete are selection operations. Property edits also updated `WbToolState` at
/// the toolbar, so the next new element inherits them too.
fn apply_canvas_edit(
    mut edits: MessageReader<CanvasEdit>,
    projects: Res<Projects>,
    mut boards: ResMut<BackgroundBoards>,
    mut state: ResMut<BgState>,
) {
    let pending: Vec<CanvasEdit> = edits.read().copied().collect();
    if pending.is_empty() {
        return;
    }
    let Some(project) = projects.active else {
        return;
    };
    let board = boards.board_mut(project);
    let mut changed = false;
    for edit in pending {
        // Snapshot selection ids first so the per-element mutable borrow is free.
        let sel: Vec<ElementId> = board.selection().iter().cloned().collect();
        match edit {
            CanvasEdit::Stroke(c) => {
                for id in &sel {
                    if let Some(el) = board.scene_mut().get_mut(id) {
                        el.stroke_color = c;
                        changed = true;
                    }
                }
            }
            CanvasEdit::Background(c) => {
                for id in &sel {
                    if let Some(el) = board.scene_mut().get_mut(id) {
                        el.background_color = c;
                        changed = true;
                    }
                }
            }
            CanvasEdit::Fill(f) => {
                for id in &sel {
                    if let Some(el) = board.scene_mut().get_mut(id) {
                        el.fill_style = f;
                        changed = true;
                    }
                }
            }
            CanvasEdit::Width(w) => {
                for id in &sel {
                    if let Some(el) = board.scene_mut().get_mut(id) {
                        el.stroke_width = w;
                        changed = true;
                    }
                }
            }
            CanvasEdit::StrokeStyle(s) => {
                for id in &sel {
                    if let Some(el) = board.scene_mut().get_mut(id) {
                        el.stroke_style = s;
                        changed = true;
                    }
                }
            }
            CanvasEdit::Roughness(r) => {
                for id in &sel {
                    if let Some(el) = board.scene_mut().get_mut(id) {
                        el.roughness = r;
                        changed = true;
                    }
                }
            }
            CanvasEdit::Opacity(o) => {
                for id in &sel {
                    if let Some(el) = board.scene_mut().get_mut(id) {
                        el.opacity = o;
                        changed = true;
                    }
                }
            }
            CanvasEdit::ZOrder(z) => {
                changed |= match z {
                    ZOrder::ToBack => board.send_to_back(),
                    ZOrder::Backward => board.lower(),
                    ZOrder::Forward => board.raise(),
                    ZOrder::ToFront => board.bring_to_front(),
                };
            }
            CanvasEdit::Duplicate => changed |= !board.duplicate_selection().is_empty(),
            CanvasEdit::Delete => changed |= board.delete_selection(),
        }
    }
    if changed {
        state.dirty = true;
        save_board(project, board);
    }
}

pub struct WhiteboardBackgroundPlugin;

impl Plugin for WhiteboardBackgroundPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BackgroundBoards>()
            .init_resource::<BgState>()
            .init_resource::<ProjectTools>()
            .add_systems(Startup, setup_background_root)
            .add_systems(Update, (sync_project_tool, background_keyboard).chain())
            // Canvas input MUST run before the pane systems so a claimed press
            // sets `InputConsumed` before `handle_pane_mouse` reads it — that
            // ordering is what removes the old who-owns-the-click race.
            .add_systems(
                Update,
                background_input
                    .after(background_keyboard)
                    .before(PaneViewportReaders),
            )
            .add_systems(Update, (handle_clear_canvas, apply_canvas_edit))
            // `sync_pane_proxies` mirrors each pane into the board as a
            // bindable (invisible, locked) Rectangle so ARROWS drawn to a pane
            // bind to it and follow when it moves — desired behavior. Only
            // bound arrows move; freehand/shapes stay fixed (they're not
            // arrows, so `update_bound_arrow` never touches them).
            .add_systems(PostUpdate, (sync_pane_proxies, render_background).chain());
    }
}

#[cfg(test)]
mod tests {
    use super::{decide_canvas_press, tool_claims_surface, tool_for_char, CanvasPress};
    use whiteboard_core::interaction::Tool;

    // Args, in order: (canvas_active, tool_claims_surface, over_toolbar,
    // over_pane, on_sidebar). Each test maps to a bug the user reported.

    #[test]
    fn toolbar_closed_lets_panes_work() {
        // Not in canvas-draw mode → never claim; panes drag/resize normally,
        // no matter the tool or where the cursor is.
        assert_eq!(
            decide_canvas_press(false, true, false, true, false),
            CanvasPress::Ignore
        );
        assert_eq!(
            decide_canvas_press(false, false, true, false, true),
            CanvasPress::Ignore
        );
    }

    #[test]
    fn clicking_toolbar_always_switches_tools() {
        // "Can't click the Select tool while in draw tool": clicking the
        // toolbar must defer regardless of tool or what's underneath.
        assert_eq!(
            decide_canvas_press(true, true, true, true, false),
            CanvasPress::DeferToToolbar
        );
        assert_eq!(
            decide_canvas_press(true, false, true, false, false),
            CanvasPress::DeferToToolbar
        );
    }

    #[test]
    fn select_tool_drags_panes() {
        // "Can't move windows while using Select": a Select/Pan press on a pane
        // must defer to the pane systems so the pane drags.
        assert_eq!(
            decide_canvas_press(true, false, false, true, false),
            CanvasPress::DeferToPanes
        );
    }

    #[test]
    fn select_tool_on_empty_canvas_selects_strokes() {
        assert_eq!(
            decide_canvas_press(true, false, false, false, false),
            CanvasPress::Board
        );
    }

    #[test]
    fn draw_tools_paint_over_panes() {
        // A draw/shape tool claims the surface even over a pane (drawing layer
        // on top) — but never on the toolbar (covered above).
        assert_eq!(
            decide_canvas_press(true, true, false, true, false),
            CanvasPress::Board
        );
        assert_eq!(
            decide_canvas_press(true, true, false, false, false),
            CanvasPress::Board
        );
    }

    #[test]
    fn sidebar_gutter_is_off_limits() {
        assert_eq!(
            decide_canvas_press(true, true, false, false, true),
            CanvasPress::Ignore
        );
    }

    #[test]
    fn select_and_pan_are_the_only_non_claiming_tools() {
        assert!(!tool_claims_surface(Tool::Select));
        assert!(!tool_claims_surface(Tool::Pan));
        assert!(tool_claims_surface(Tool::Freedraw));
        assert!(tool_claims_surface(Tool::Rectangle));
        assert!(tool_claims_surface(Tool::Arrow));
        assert!(tool_claims_surface(Tool::Eraser));
    }

    #[test]
    fn tool_shortcuts_map_as_expected() {
        assert_eq!(tool_for_char("v"), Some(Tool::Select));
        assert_eq!(tool_for_char("p"), Some(Tool::Freedraw));
        assert_eq!(tool_for_char("f"), Some(Tool::Freedraw));
        assert_eq!(tool_for_char("e"), Some(Tool::Eraser));
        assert_eq!(tool_for_char("z"), None); // undo, not a tool
        assert_eq!(tool_for_char(""), None);
    }
}
