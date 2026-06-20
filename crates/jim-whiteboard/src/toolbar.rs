//! The whiteboard **toolbar**: a small screen-anchored pane that stays fixed
//! while the canvas pans/zooms. Clicking its buttons mutates the shared
//! [`WbToolState`] — tool, stroke color, stroke width, and the
//! draw-on-background toggle — which every whiteboard surface reads.

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::sprite_render::{AlphaMode2d, ColorMaterial, MeshMaterial2d};

use crate::buttons::{self, ButtonTheme};

use jim_pane::{
    spawn_pane_from_registry, ChromeStyle, ChromeTextStyle, PaneChrome, PaneContentNoClip,
    PaneContentPressed, PaneFont, PaneHotZones, PaneKindSpec, PaneRect, PaneRegistry,
    PaneScreenAnchored, PaneTag, PaneViewportReaders,
};

use serde_json::Value;
use whiteboard_core::interaction::Tool;
use whiteboard_core::render::{Color as WbColor, FillStyle, StrokeStyle};

use crate::{CanvasDrawActive, CanvasEdit, ClearCanvasRequested, WbToolState, ZOrder};

pub const PANE_KIND: &str = "whiteboard-toolbar";

const PAD: f32 = 8.0;
const CELL: f32 = 40.0;
const GAP: f32 = 4.0;
const COLS: usize = 3;
/// Vertical space reserved for a section label ("Stroke", "Fill", …).
const LABEL_H: f32 = 16.0;

/// Background/fill palette (Excalidraw-style): "no fill" plus four light tints.
/// First entry is transparent (rendered as an outlined empty swatch).
fn bg_palette() -> [WbColor; 5] {
    [
        WbColor::TRANSPARENT,
        WbColor::rgb(0xff, 0xc9, 0xc9), // pink
        WbColor::rgb(0xb2, 0xf2, 0xbb), // green
        WbColor::rgb(0xa5, 0xd8, 0xff), // blue
        WbColor::rgb(0xff, 0xec, 0x99), // yellow
    ]
}

/// Pane chrome inset (mirrors `jim_pane::MARGIN` + `TITLE_H`); the content
/// area is the pane rect minus these, so the toolbar's intrinsic size has to
/// account for them or the rightmost column clips.
const CHROME_MARGIN: f32 = 8.0;
const CHROME_TITLE_H: f32 = 22.0;

/// Intrinsic content width/height the button layout needs (see [`layout`]).
const CONTENT_W: f32 = PAD + COLS as f32 * CELL + (COLS as f32 - 1.0) * GAP + PAD;
/// Tall enough for every section (tools, Stroke/Fill swatches, fill style,
/// width, stroke style, sloppiness, opacity, layers, actions, Clear) with
/// labels.
const CONTENT_H: f32 = 764.0;

/// The pane size that exactly fits the laid-out content plus chrome.
fn toolbar_size() -> Vec2 {
    Vec2::new(
        CONTENT_W + 2.0 * CHROME_MARGIN,
        CONTENT_H + CHROME_TITLE_H + 2.0 * CHROME_MARGIN,
    )
}

/// What a toolbar button does when clicked.
#[derive(Clone, Copy, Debug)]
enum Action {
    Tool(Tool),
    /// Stroke color.
    Color(WbColor),
    /// Background / fill color.
    Background(WbColor),
    /// Fill pattern style (hachure / cross-hatch / solid).
    Fill(FillStyle),
    Width(f64),
    StrokeStyle(StrokeStyle),
    /// Sloppiness / roughness (0 = architect, 1 = artist, 2 = cartoonist).
    Roughness(f64),
    /// Opacity 0..=100.
    Opacity(f64),
    /// Move selection in the paint order.
    ZOrder(ZOrder),
    /// Duplicate the selection.
    Duplicate,
    /// Delete the selection.
    Delete,
    /// Wipe the project's canvas background board.
    Clear,
}

/// A non-interactive section header drawn above a group of buttons.
struct Label {
    x: f32,
    y: f32,
    text: &'static str,
}

#[derive(Clone)]
struct Button {
    /// Content-local rect (top-left origin, y-down — same frame as
    /// `PaneContentPressed::local_pt`).
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    action: Action,
}

/// The toolbar pane: holds the laid-out buttons for hit-testing and a flag that
/// forces a UI rebuild.
#[derive(Component)]
pub struct ToolbarPane {
    buttons: Vec<Button>,
    needs_build: bool,
}

/// Marks a spawned toolbar UI entity so it can be cleared on rebuild.
#[derive(Component)]
struct ToolbarUi;

pub(crate) fn register(registry: &mut PaneRegistry) {
    registry.register(PaneKindSpec {
        kind: PANE_KIND,
        display_name: "Draw Tools",
        radial_icon: Some("⚒"),
        default_size: toolbar_size(),
        spawn: spawn_from_config,
        snapshot,
        on_close: None,
    });
}

fn spawn_from_config(world: &mut World, entity: Entity, _content_root: Entity, _config: &Value) {
    // The toolbar is screen-anchored, so its PaneRect is in window pixels. The
    // generic spawn path positions panes for the *canvas* frame (cascading from
    // the sidebar in canvas units), which for an anchored pane lands it on top
    // of the sidebar — where its left column is hidden and clicks fall in the
    // sidebar block-zone. Pin it to a fixed top-right spot instead.
    let size = toolbar_size();
    let win = world
        .query::<&Window>()
        .iter(world)
        .next()
        .map(|w| Vec2::new(w.width(), w.height()));
    let pos = match win {
        Some(w) => Vec2::new((w.x - size.x - 16.0).max(16.0), 64.0),
        None => Vec2::new(16.0, 64.0),
    };
    if let Some(mut rect) = world.get_mut::<PaneRect>(entity) {
        rect.pos = pos;
        rect.size = size;
    }
    world.entity_mut(entity).insert((
        ToolbarPane {
            buttons: Vec::new(),
            needs_build: true,
        },
        PaneScreenAnchored,
    ));
}

fn snapshot(_world: &World, _entity: Entity) -> Value {
    // Stateless beyond position; nothing kind-specific to persist.
    serde_json::json!({})
}

/// Spawn the toolbar at a fixed screen position (window-logical pixels).
pub fn spawn_toolbar(world: &mut World, screen_pos: Vec2, project: Option<u64>) -> Option<Entity> {
    let rect = PaneRect {
        pos: screen_pos,
        size: toolbar_size(),
        z: 50.0,
    };
    let e = spawn_pane_from_registry(world, PANE_KIND, "Draw Tools", rect, project, &Value::Null);
    if let Some(e) = e {
        world.entity_mut(e).insert(PaneScreenAnchored);
    }
    e
}

/// A grid of color swatches; returns the y past the grid.
fn swatch_grid(out: &mut Vec<Button>, y: f32, colors: &[WbColor], to_action: fn(WbColor) -> Action) -> f32 {
    for (i, c) in colors.iter().enumerate() {
        let col = i % COLS;
        let row = i / COLS;
        out.push(Button {
            x: PAD + col as f32 * (CELL + GAP),
            y: y + row as f32 * (CELL + GAP),
            w: CELL,
            h: CELL,
            action: to_action(*c),
        });
    }
    let rows = colors.len().div_ceil(COLS);
    y + rows as f32 * (CELL + GAP) + GAP
}

/// Build the button + label layout for the current tool state.
fn layout(stroke_palette: &[WbColor]) -> (Vec<Button>, Vec<Label>) {
    let mut out = Vec::new();
    let mut labels = Vec::new();
    let tools: &[Tool] = &[
        Tool::Select,
        Tool::Freedraw,
        Tool::Rectangle,
        Tool::Ellipse,
        Tool::Diamond,
        Tool::Line,
        Tool::Arrow,
        Tool::Text,
        Tool::Eraser,
    ];
    let mut y = PAD;
    for (i, tool) in tools.iter().enumerate() {
        let col = i % COLS;
        let row = i / COLS;
        out.push(Button {
            x: PAD + col as f32 * (CELL + GAP),
            y: y + row as f32 * (CELL + GAP),
            w: CELL,
            h: CELL,
            action: Action::Tool(*tool),
        });
    }
    let tool_rows = tools.len().div_ceil(COLS);
    y += tool_rows as f32 * (CELL + GAP) + GAP;

    // Stroke color swatches.
    labels.push(Label { x: PAD, y, text: "Stroke" });
    y += LABEL_H;
    y = swatch_grid(&mut out, y, stroke_palette, Action::Color);

    // Fill / background swatches.
    labels.push(Label { x: PAD, y, text: "Fill" });
    y += LABEL_H;
    y = swatch_grid(&mut out, y, &bg_palette(), Action::Background);

    let full_w = COLS as f32 * CELL + (COLS as f32 - 1.0) * GAP;

    // Fill pattern style (Excalidraw's three: hachure / cross-hatch / solid).
    labels.push(Label { x: PAD, y, text: "Fill style" });
    y += LABEL_H;
    y = button_row(
        &mut out,
        y,
        full_w,
        &[
            Action::Fill(FillStyle::Hachure),
            Action::Fill(FillStyle::CrossHatch),
            Action::Fill(FillStyle::Solid),
        ],
    );

    // Stroke width.
    labels.push(Label { x: PAD, y, text: "Stroke width" });
    y += LABEL_H;
    for (i, w) in [1.0_f64, 3.0, 6.0].iter().enumerate() {
        out.push(Button {
            x: PAD + i as f32 * (CELL + GAP),
            y,
            w: CELL,
            h: CELL * 0.7,
            action: Action::Width(*w),
        });
    }
    y += CELL * 0.7 + GAP * 2.0;

    // Stroke style.
    labels.push(Label { x: PAD, y, text: "Stroke style" });
    y += LABEL_H;
    y = button_row(
        &mut out,
        y,
        full_w,
        &[
            Action::StrokeStyle(StrokeStyle::Solid),
            Action::StrokeStyle(StrokeStyle::Dashed),
            Action::StrokeStyle(StrokeStyle::Dotted),
        ],
    );

    // Sloppiness (roughness).
    labels.push(Label { x: PAD, y, text: "Sloppiness" });
    y += LABEL_H;
    y = button_row(
        &mut out,
        y,
        full_w,
        &[
            Action::Roughness(0.0),
            Action::Roughness(1.0),
            Action::Roughness(2.0),
        ],
    );

    // Opacity (presets).
    labels.push(Label { x: PAD, y, text: "Opacity" });
    y += LABEL_H;
    y = button_row(
        &mut out,
        y,
        full_w,
        &[
            Action::Opacity(30.0),
            Action::Opacity(60.0),
            Action::Opacity(100.0),
        ],
    );

    // Layers (z-order), applied to the selection.
    labels.push(Label { x: PAD, y, text: "Layers" });
    y += LABEL_H;
    y = button_row(
        &mut out,
        y,
        full_w,
        &[
            Action::ZOrder(ZOrder::ToBack),
            Action::ZOrder(ZOrder::Backward),
            Action::ZOrder(ZOrder::Forward),
            Action::ZOrder(ZOrder::ToFront),
        ],
    );

    // Actions on the selection.
    labels.push(Label { x: PAD, y, text: "Actions" });
    y += LABEL_H;
    y = button_row(&mut out, y, full_w, &[Action::Duplicate, Action::Delete]);

    // Clear-the-canvas button (full width).
    out.push(Button {
        x: PAD,
        y,
        w: full_w,
        h: CELL * 0.7,
        action: Action::Clear,
    });

    (out, labels)
}

/// Lay out `actions` as an evenly spaced row of equal-width buttons spanning
/// `full_w`. Returns the y past the row.
fn button_row(out: &mut Vec<Button>, y: f32, full_w: f32, actions: &[Action]) -> f32 {
    let n = actions.len() as f32;
    let bw = (full_w - (n - 1.0) * GAP) / n;
    let h = CELL * 0.7;
    for (i, a) in actions.iter().enumerate() {
        out.push(Button {
            x: PAD + i as f32 * (bw + GAP),
            y,
            w: bw,
            h,
            action: *a,
        });
    }
    y + h + GAP * 2.0
}

fn is_active(action: &Action, ts: &WbToolState) -> bool {
    match action {
        Action::Tool(t) => *t == ts.tool,
        Action::Color(c) => *c == ts.stroke_color,
        Action::Background(c) => *c == ts.background_color,
        Action::Fill(f) => *f == ts.fill_style,
        Action::Width(w) => (*w - ts.stroke_width).abs() < 0.01,
        Action::StrokeStyle(s) => *s == ts.stroke_style,
        Action::Roughness(r) => (*r - ts.roughness).abs() < 0.01,
        Action::Opacity(o) => (*o - ts.opacity).abs() < 0.5,
        Action::ZOrder(_) | Action::Duplicate | Action::Delete | Action::Clear => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_mesh(
    mesh: Mesh,
    color: Color,
    z: f32,
    content_root: Entity,
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
    commands.spawn((
        Mesh2d(mesh_h),
        MeshMaterial2d(mat_h),
        Transform::from_xyz(0.0, 0.0, z),
        ToolbarUi,
        ChildOf(content_root),
    ));
}

/// Spawn a glyph/short label centered in a button cell.
fn glyph(
    b: &Button,
    text: &str,
    font: &Handle<Font>,
    color: Color,
    content_root: Entity,
    commands: &mut Commands,
) {
    commands.spawn((
        Text2d::new(text.to_string()),
        TextFont {
            font: (font.clone()).into(),
            font_size: FontSize::Px(13.0),
            ..default()
        },
        TextColor(color),
        Anchor::CENTER,
        PaneContentNoClip,
        Transform::from_xyz(b.x + b.w * 0.5, -(b.y + b.h * 0.5), 0.03),
        ToolbarUi,
        ChildOf(content_root),
    ));
}

#[allow(clippy::too_many_arguments)]
fn build_ui(
    panes: &mut Query<(&mut ToolbarPane, &PaneChrome, &mut PaneHotZones)>,
    ts: &WbToolState,
    theme: &ButtonTheme,
    font: &Handle<Font>,
    existing: &Query<(Entity, &ChildOf), With<ToolbarUi>>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    commands: &mut Commands,
) {
    for (mut tb, chrome, mut zones) in panes.iter_mut() {
        if !tb.needs_build {
            continue;
        }
        let content_root = chrome.content_root;
        for (e, parent) in existing.iter() {
            if parent.0 == content_root {
                commands.entity(e).despawn();
            }
        }
        let (buttons, labels) = layout(&theme.palette);
        // Section headers.
        for l in &labels {
            commands.spawn((
                Text2d::new(l.text),
                TextFont {
                    font: (font.clone()).into(),
                    font_size: FontSize::Px(11.0),
                    ..default()
                },
                TextColor(theme.label),
                Anchor::CENTER_LEFT,
                PaneContentNoClip,
                Transform::from_xyz(l.x, -(l.y + LABEL_H * 0.5), 0.03),
                ToolbarUi,
                ChildOf(content_root),
            ));
        }
        zones.clear();
        for b in &buttons {
            let active = is_active(&b.action, ts);
            // Button cell.
            let bg = if active { theme.cell_active } else { theme.cell };
            if let Some(m) = buttons::rounded_rect_mesh(b.x, b.y, b.w, b.h, 5.0) {
                spawn_mesh(m, bg, 0.01, content_root, meshes, materials, commands);
            }
            // Content.
            match b.action {
                Action::Tool(t) => {
                    if let Some(icon) = buttons::tool_icon(t) {
                        for m in buttons::icon_meshes(icon, b.x, b.y, b.w, b.h, 2.2) {
                            spawn_mesh(m, theme.label, 0.02, content_root, meshes, materials, commands);
                        }
                    }
                }
                Action::Color(c) => {
                    let inset = b.w * 0.22;
                    if let Some(m) = buttons::rounded_rect_mesh(
                        b.x + inset,
                        b.y + inset,
                        b.w - 2.0 * inset,
                        b.h - 2.0 * inset,
                        4.0,
                    ) {
                        spawn_mesh(m, buttons::wb_to_color(c), 0.02, content_root, meshes, materials, commands);
                    }
                }
                Action::Background(c) => {
                    let inset = b.w * 0.22;
                    let (sx, sy, sw, sh) = (
                        b.x + inset,
                        b.y + inset,
                        b.w - 2.0 * inset,
                        b.h - 2.0 * inset,
                    );
                    if c.is_transparent() {
                        // "No fill" — an outlined empty swatch (border ring over
                        // the cell bg) so it reads as transparent, not a color.
                        if let Some(m) = buttons::rounded_rect_mesh(sx, sy, sw, sh, 4.0) {
                            spawn_mesh(m, theme.label.with_alpha(0.5), 0.02, content_root, meshes, materials, commands);
                        }
                        let r = 1.5;
                        if let Some(m) = buttons::rounded_rect_mesh(sx + r, sy + r, sw - 2.0 * r, sh - 2.0 * r, 3.0) {
                            spawn_mesh(m, bg, 0.03, content_root, meshes, materials, commands);
                        }
                    } else if let Some(m) = buttons::rounded_rect_mesh(sx, sy, sw, sh, 4.0) {
                        spawn_mesh(m, buttons::wb_to_color(c), 0.02, content_root, meshes, materials, commands);
                    }
                }
                Action::Fill(f) => {
                    let t = match f {
                        FillStyle::Hachure => "╱╱",
                        FillStyle::CrossHatch => "╳╳",
                        FillStyle::Solid => "██",
                        FillStyle::Zigzag => "ＮＮ",
                        FillStyle::Dots => "∴∴",
                    };
                    glyph(b, t, font, theme.label, content_root, commands);
                }
                Action::Width(w) => {
                    if let Some(m) = buttons::width_sample_mesh(b.x, b.y, b.w, b.h, w as f32) {
                        spawn_mesh(m, theme.label, 0.02, content_root, meshes, materials, commands);
                    }
                }
                Action::StrokeStyle(s) => {
                    let t = match s {
                        StrokeStyle::Solid => "──",
                        StrokeStyle::Dashed => "- -",
                        StrokeStyle::Dotted => "···",
                    };
                    glyph(b, t, font, theme.label, content_root, commands);
                }
                Action::Roughness(r) => {
                    let t = if r < 0.5 {
                        "1"
                    } else if r < 1.5 {
                        "2"
                    } else {
                        "3"
                    };
                    glyph(b, t, font, theme.label, content_root, commands);
                }
                Action::Opacity(o) => {
                    glyph(b, &format!("{}", o as i32), font, theme.label, content_root, commands);
                }
                Action::ZOrder(z) => {
                    let t = match z {
                        ZOrder::ToBack => "«",
                        ZOrder::Backward => "‹",
                        ZOrder::Forward => "›",
                        ZOrder::ToFront => "»",
                    };
                    glyph(b, t, font, theme.label, content_root, commands);
                }
                Action::Duplicate => glyph(b, "Dup", font, theme.label, content_root, commands),
                Action::Delete => glyph(b, "Del", font, theme.label, content_root, commands),
                Action::Clear => {
                    for m in buttons::icon_meshes(buttons::Icon::Trash, b.x, b.y, b.h, b.h, 2.0) {
                        spawn_mesh(m, theme.label, 0.02, content_root, meshes, materials, commands);
                    }
                    commands.spawn((
                        Text2d::new("Clear"),
                        TextFont {
                            font: (font.clone()).into(),
                            font_size: FontSize::Px(13.0),
                            ..default()
                        },
                        TextColor(theme.label),
                        Anchor::CENTER_LEFT,
                        PaneContentNoClip,
                        Transform::from_xyz(b.x + b.h + 2.0, -(b.y + b.h * 0.5), 0.03),
                        ToolbarUi,
                        ChildOf(content_root),
                    ));
                }
            }
            zones.push(Rect::new(b.x, b.y, b.x + b.w, b.y + b.h));
        }
        tb.buttons = buttons;
        tb.needs_build = false;
    }
}

#[allow(clippy::too_many_arguments)]
fn toolbar_build_system(
    mut panes: Query<(&mut ToolbarPane, &PaneChrome, &mut PaneHotZones)>,
    mut ts: ResMut<WbToolState>,
    chrome: Option<Res<ChromeStyle>>,
    text_style: Option<Res<ChromeTextStyle>>,
    font: Option<Res<PaneFont>>,
    existing: Query<(Entity, &ChildOf), With<ToolbarUi>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut commands: Commands,
) {
    let Some(font) = font else {
        return;
    };
    let theme = ButtonTheme::from_theme(chrome.as_deref(), text_style.as_deref());

    // Heal the stroke color if it isn't one of the current palette swatches —
    // covers the initial default (an invisible near-black ink) and a theme
    // switch. Setting it here also moves the active-swatch highlight onto the
    // foreground ink. Done before reading `is_changed` semantics so the write
    // triggers exactly one rebuild.
    if !theme.palette.iter().any(|c| *c == ts.stroke_color) {
        ts.stroke_color = theme.palette[0];
    }

    // Rebuild when tool state or theme changes (to refresh highlights/colors),
    // or on first spawn.
    let theme_changed = chrome.as_ref().map(|r| r.is_changed()).unwrap_or(false)
        || text_style.as_ref().map(|r| r.is_changed()).unwrap_or(false);
    if ts.is_changed() || theme_changed {
        for (mut tb, _, _) in panes.iter_mut() {
            tb.needs_build = true;
        }
    }
    build_ui(
        &mut panes,
        &ts,
        &theme,
        &font.0,
        &existing,
        &mut meshes,
        &mut materials,
        &mut commands,
    );
}

fn toolbar_click(
    mut pressed: MessageReader<PaneContentPressed>,
    mut ts: ResMut<WbToolState>,
    mut clear: MessageWriter<ClearCanvasRequested>,
    mut edits: MessageWriter<CanvasEdit>,
    panes: Query<&ToolbarPane>,
) {
    for ev in pressed.read() {
        let Ok(tb) = panes.get(ev.pane) else {
            continue;
        };
        for b in &tb.buttons {
            if ev.local_pt.x >= b.x
                && ev.local_pt.x <= b.x + b.w
                && ev.local_pt.y >= b.y
                && ev.local_pt.y <= b.y + b.h
            {
                // Style buttons update WbToolState (so the NEXT element inherits
                // it) AND emit a CanvasEdit so the host applies it to the current
                // selection ("select and change"). Layers/actions are
                // selection-only operations.
                match b.action {
                    Action::Tool(t) => ts.tool = t,
                    Action::Color(c) => {
                        ts.stroke_color = c;
                        edits.write(CanvasEdit::Stroke(c));
                    }
                    Action::Background(c) => {
                        ts.background_color = c;
                        edits.write(CanvasEdit::Background(c));
                    }
                    Action::Fill(f) => {
                        ts.fill_style = f;
                        edits.write(CanvasEdit::Fill(f));
                    }
                    Action::Width(w) => {
                        ts.stroke_width = w;
                        edits.write(CanvasEdit::Width(w));
                    }
                    Action::StrokeStyle(s) => {
                        ts.stroke_style = s;
                        edits.write(CanvasEdit::StrokeStyle(s));
                    }
                    Action::Roughness(r) => {
                        ts.roughness = r;
                        edits.write(CanvasEdit::Roughness(r));
                    }
                    Action::Opacity(o) => {
                        ts.opacity = o;
                        edits.write(CanvasEdit::Opacity(o));
                    }
                    Action::ZOrder(z) => {
                        edits.write(CanvasEdit::ZOrder(z));
                    }
                    Action::Duplicate => {
                        edits.write(CanvasEdit::Duplicate);
                    }
                    Action::Delete => {
                        edits.write(CanvasEdit::Delete);
                    }
                    Action::Clear => {
                        clear.write(ClearCanvasRequested);
                    }
                }
                break;
            }
        }
    }
}

/// Canvas-draw mode is "on" exactly while at least one floating canvas toolbar
/// exists. The background drawing surface (in jim-app) reads this.
fn track_canvas_active(
    toolbars: Query<(), With<ToolbarPane>>,
    mut active: ResMut<CanvasDrawActive>,
) {
    let now = toolbars.iter().next().is_some();
    if active.0 != now {
        active.0 = now;
    }
}

/// Keep the floating canvas toolbar above every other pane so it can't be
/// covered (e.g. by the Garden pane). Pins its `PaneRect.z` to just above the
/// current max of all other panes.
fn pin_toolbar_z(mut panes: Query<(Entity, &mut PaneRect, Has<ToolbarPane>), With<PaneTag>>) {
    let max_other = panes
        .iter()
        .filter(|(_, _, is_toolbar)| !is_toolbar)
        .map(|(_, r, _)| r.z)
        .fold(0.0_f32, f32::max);
    // Stay above all panes (max_other + 1) AND above the canvas whiteboard
    // overlay camera so the Draw Tools float over the drawing — you can't draw
    // on them. Pane camera order = z*100+1; the whiteboard overlay camera runs
    // at 80_000 (see jim_app::WHITEBOARD_OVERLAY_CAMERA_ORDER), so z ≥ 850
    // (order 85_001) clears it while staying below the menu overlay (100_000).
    const Z_FLOOR_ABOVE_WHITEBOARD: f32 = 850.0;
    let target = (max_other + 1.0).max(Z_FLOOR_ABOVE_WHITEBOARD);
    for (_, mut rect, is_toolbar) in panes.iter_mut() {
        if is_toolbar && (rect.z - target).abs() > f32::EPSILON {
            rect.z = target;
        }
    }
}

pub(crate) fn build(app: &mut App) {
    // `toolbar_click` reads `PaneContentPressed`, which `handle_pane_mouse`
    // (inside `PaneViewportReaders`) writes. Order it AFTER so it reads the
    // same frame's press instead of racing the writer — without this the
    // toolbar click is flaky ("can't click the Select tool").
    app.add_systems(
        Update,
        (toolbar_click.after(PaneViewportReaders), track_canvas_active),
    );
    app.add_systems(PostUpdate, (toolbar_build_system, pin_toolbar_z));
}
