//! The in-pane **island toolbar** (Mode 1): a compact, rounded tool palette that
//! floats over the right edge of a whiteboard pane's drawing area. Unlike the
//! floating *canvas* toolbar (`toolbar.rs`, Mode 2), the island belongs to one
//! pane and drives that pane's own [`crate::ToolStyle`] — so each whiteboard
//! pane remembers its own tool independently.
//!
//! The island is rendered as `IslandUi`-tagged `Mesh2d`/`Text2d` children of the
//! pane's `content_root`. Presses are intercepted in `pane.rs` (before drawing)
//! via [`island_hit`]; hovers drive a tooltip.

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::sprite_render::{AlphaMode2d, ColorMaterial, MeshMaterial2d};

use jim_pane::{
    content_area, ChromeStyle, ChromeTextStyle, PaneChrome, PaneContentHovered, PaneFont, PaneRect,
};

use whiteboard_core::interaction::Tool;
use whiteboard_core::render::Color as WbColor;

use crate::buttons::{self, ButtonTheme, Icon};
use crate::pane::WhiteboardPane;

const INSET: f32 = 8.0;
const PAD: f32 = 6.0;
const CELL: f32 = 28.0;
const GAP: f32 = 4.0;
const COLS: usize = 2;
const SECTION_GAP: f32 = 8.0;
const WIDTH_H: f32 = 18.0;
const CLEAR_H: f32 = 24.0;

/// Panel inner content width (two columns of cells).
const INNER_W: f32 = COLS as f32 * CELL + (COLS as f32 - 1.0) * GAP;
const PANEL_W: f32 = PAD + INNER_W + PAD;

// Local z band, kept just above the drawn scene (≤0.05) and below the pane's
// title cover (content-local 0.06).
const Z_PANEL: f32 = 0.051;
const Z_CELL: f32 = 0.053;
const Z_ICON: f32 = 0.055;
const Z_TEXT: f32 = 0.057;
const Z_TOOLTIP_BG: f32 = 0.058;
const Z_TOOLTIP_TX: f32 = 0.059;

/// What an island button does — also the value [`island_hit`] returns.
#[derive(Clone, Copy, Debug)]
pub enum IslandAction {
    Tool(Tool),
    Color(WbColor),
    Width(f64),
    /// Wipe this pane's scene.
    Clear,
}

#[derive(Clone)]
struct Button {
    /// Content-local rect (top-left origin, y-down).
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    action: IslandAction,
    tooltip: String,
}

/// Per-pane island state: laid-out buttons + a rebuild flag + the last content
/// size (so it repositions on resize).
#[derive(Component)]
pub struct Island {
    buttons: Vec<Button>,
    needs_build: bool,
    last_size: Vec2,
    /// Index of the button the tooltip currently describes, if any.
    tooltip_for: Option<usize>,
    /// True while a press that landed on the island is still held (so the
    /// in-progress drag/release don't leak into drawing).
    pub capturing: bool,
}

impl Default for Island {
    fn default() -> Self {
        Island {
            buttons: Vec::new(),
            needs_build: true,
            last_size: Vec2::ZERO,
            tooltip_for: None,
            capturing: false,
        }
    }
}

impl Island {
    /// Force a UI rebuild (e.g. after the pane's tool/style changed).
    pub fn mark_dirty(&mut self) {
        self.needs_build = true;
    }
}

/// Markers for spawned island geometry so they can be cleared on rebuild.
#[derive(Component)]
struct IslandUi;

#[derive(Component)]
struct IslandTooltip;

/// Hit-test a content-local press point against the island's buttons.
pub fn island_hit(island: &Island, local: Vec2) -> Option<IslandAction> {
    for b in &island.buttons {
        if local.x >= b.x && local.x <= b.x + b.w && local.y >= b.y && local.y <= b.y + b.h {
            return Some(b.action);
        }
    }
    None
}

fn tools() -> &'static [(Tool, &'static str, &'static str)] {
    &[
        (Tool::Select, "Select", "V"),
        (Tool::Freedraw, "Draw", "P"),
        (Tool::Rectangle, "Rectangle", "R"),
        (Tool::Ellipse, "Ellipse", "O"),
        (Tool::Diamond, "Diamond", "D"),
        (Tool::Line, "Line", "L"),
        (Tool::Arrow, "Arrow", "A"),
        (Tool::Text, "Text", "T"),
        (Tool::Eraser, "Eraser", "E"),
    ]
}

/// Lay out the island for the given content size, anchored to the right edge.
/// Returns the backdrop panel rect and the buttons.
fn layout(content: Vec2, palette: &[WbColor]) -> ((f32, f32, f32, f32), Vec<Button>) {
    let panel_x = (content.x - INSET - PANEL_W).max(0.0);
    let panel_y = 8.0;
    let inner_x = panel_x + PAD;
    let mut y = panel_y + PAD;
    let mut out = Vec::new();

    // Tools (2-col grid).
    for (i, (tool, name, key)) in tools().iter().enumerate() {
        let col = i % COLS;
        let row = i / COLS;
        out.push(Button {
            x: inner_x + col as f32 * (CELL + GAP),
            y: y + row as f32 * (CELL + GAP),
            w: CELL,
            h: CELL,
            action: IslandAction::Tool(*tool),
            tooltip: format!("{name}  {key}"),
        });
    }
    let tool_rows = tools().len().div_ceil(COLS);
    y += tool_rows as f32 * (CELL + GAP) - GAP + SECTION_GAP;

    // Colors (2-col grid).
    for (i, c) in palette.iter().enumerate() {
        let col = i % COLS;
        let row = i / COLS;
        out.push(Button {
            x: inner_x + col as f32 * (CELL + GAP),
            y: y + row as f32 * (CELL + GAP),
            w: CELL,
            h: CELL,
            action: IslandAction::Color(*c),
            tooltip: "Color".to_string(),
        });
    }
    let color_rows = palette.len().div_ceil(COLS);
    y += color_rows as f32 * (CELL + GAP) - GAP + SECTION_GAP;

    // Stroke widths (full-width rows, each shows its thickness).
    for w in [1.0_f64, 3.0, 6.0] {
        out.push(Button {
            x: inner_x,
            y,
            w: INNER_W,
            h: WIDTH_H,
            action: IslandAction::Width(w),
            tooltip: format!("Width {}", w as i32),
        });
        y += WIDTH_H + GAP;
    }
    y += SECTION_GAP - GAP;

    // Clear (full width).
    out.push(Button {
        x: inner_x,
        y,
        w: INNER_W,
        h: CLEAR_H,
        action: IslandAction::Clear,
        tooltip: "Clear board".to_string(),
    });
    y += CLEAR_H;

    let panel_h = (y - panel_y) + PAD;
    ((panel_x, panel_y, PANEL_W, panel_h), out)
}

fn is_active(action: &IslandAction, style: &crate::ToolStyle) -> bool {
    match action {
        IslandAction::Tool(t) => *t == style.tool,
        IslandAction::Color(c) => *c == style.stroke_color,
        IslandAction::Width(w) => (*w - style.stroke_width).abs() < 0.01,
        IslandAction::Clear => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_mesh(
    mesh: Mesh,
    color: Color,
    z: f32,
    parent: Entity,
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
        IslandUi,
        ChildOf(parent),
    ));
}

#[allow(clippy::too_many_arguments)]
fn build_island(
    mut panes: Query<(&mut WhiteboardPane, &mut Island, &PaneChrome, &PaneRect)>,
    chrome: Option<Res<ChromeStyle>>,
    text_style: Option<Res<ChromeTextStyle>>,
    font: Option<Res<PaneFont>>,
    existing: Query<(Entity, &ChildOf), With<IslandUi>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut commands: Commands,
) {
    let Some(font) = font else {
        return;
    };
    let theme = ButtonTheme::from_theme(chrome.as_deref(), text_style.as_deref());
    let theme_changed = chrome.as_ref().map(|r| r.is_changed()).unwrap_or(false)
        || text_style.as_ref().map(|r| r.is_changed()).unwrap_or(false);

    for (mut wp, mut island, chrome_c, rect) in panes.iter_mut() {
        let (_, content) = content_area(rect);

        // Heal the pen ink so it's visible against the current surface (mirrors
        // the canvas toolbar). Triggers a rebuild via needs_build.
        if !theme.palette.iter().any(|c| *c == wp.style.stroke_color) {
            wp.style.stroke_color = theme.palette[0];
            island.needs_build = true;
        }

        let size_changed = island.last_size != content;
        if !(island.needs_build || size_changed || theme_changed) {
            continue;
        }
        island.last_size = content;
        island.needs_build = false;

        let content_root = chrome_c.content_root;
        for (e, parent) in existing.iter() {
            if parent.0 == content_root {
                commands.entity(e).despawn();
            }
        }

        let (panel, btns) = layout(content, &theme.palette);

        // Backdrop panel: a subtle border behind a translucent fill.
        if let Some(m) = buttons::rounded_rect_mesh(panel.0 - 1.0, panel.1 - 1.0, panel.2 + 2.0, panel.3 + 2.0, 11.0) {
            spawn_mesh(m, theme.cell_active.with_alpha(0.5), Z_PANEL, content_root, &mut meshes, &mut materials, &mut commands);
        }
        if let Some(m) = buttons::rounded_rect_mesh(panel.0, panel.1, panel.2, panel.3, 10.0) {
            spawn_mesh(m, Color::srgba(0.10, 0.10, 0.12, 0.93), Z_PANEL + 0.0005, content_root, &mut meshes, &mut materials, &mut commands);
        }

        for b in &btns {
            let active = is_active(&b.action, &wp.style);
            // Cell fill.
            let cell_color = if active { theme.cell_active } else { theme.cell };
            if let Some(m) = buttons::rounded_rect_mesh(b.x, b.y, b.w, b.h, 5.0) {
                spawn_mesh(m, cell_color, Z_CELL, content_root, &mut meshes, &mut materials, &mut commands);
            }
            // Content.
            match b.action {
                IslandAction::Tool(t) => {
                    if let Some(icon) = buttons::tool_icon(t) {
                        for m in buttons::icon_meshes(icon, b.x, b.y, b.w, b.h, 2.2) {
                            spawn_mesh(m, theme.label, Z_ICON, content_root, &mut meshes, &mut materials, &mut commands);
                        }
                    }
                }
                IslandAction::Color(c) => {
                    let inset = b.w * 0.22;
                    if let Some(m) = buttons::rounded_rect_mesh(
                        b.x + inset,
                        b.y + inset,
                        b.w - 2.0 * inset,
                        b.h - 2.0 * inset,
                        4.0,
                    ) {
                        spawn_mesh(m, buttons::wb_to_color(c), Z_ICON, content_root, &mut meshes, &mut materials, &mut commands);
                    }
                }
                IslandAction::Width(w) => {
                    if let Some(m) = buttons::width_sample_mesh(b.x, b.y, b.w, b.h, w as f32) {
                        spawn_mesh(m, theme.label, Z_ICON, content_root, &mut meshes, &mut materials, &mut commands);
                    }
                }
                IslandAction::Clear => {
                    // Trash icon (square at left) + "Clear" label.
                    for m in buttons::icon_meshes(Icon::Trash, b.x, b.y, b.h, b.h, 2.0) {
                        spawn_mesh(m, theme.label, Z_ICON, content_root, &mut meshes, &mut materials, &mut commands);
                    }
                    commands.spawn((
                        Text2d::new("Clear"),
                        TextFont {
                            font: (font.0.clone()).into(),
                            font_size: FontSize::Px(12.0),
                            ..default()
                        },
                        TextColor(theme.label),
                        Anchor::CENTER_LEFT,
                        Transform::from_xyz(b.x + b.h + 2.0, -(b.y + b.h * 0.5), Z_TEXT),
                        IslandUi,
                        ChildOf(content_root),
                    ));
                }
            }
        }

        island.buttons = btns;
    }
}

/// Show a tooltip when hovering an island button; clear it otherwise.
#[allow(clippy::too_many_arguments)]
fn island_tooltip(
    mut hovered: MessageReader<PaneContentHovered>,
    mut panes: Query<(&mut Island, &PaneChrome)>,
    font: Option<Res<PaneFont>>,
    chrome: Option<Res<ChromeStyle>>,
    text_style: Option<Res<ChromeTextStyle>>,
    existing: Query<(Entity, &ChildOf), With<IslandTooltip>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut commands: Commands,
) {
    let Some(font) = font else {
        return;
    };
    let theme = ButtonTheme::from_theme(chrome.as_deref(), text_style.as_deref());
    for ev in hovered.read() {
        let Ok((mut island, chrome_c)) = panes.get_mut(ev.pane) else {
            continue;
        };
        // Guard the window/pane-leave sentinel (x≈inf) BEFORE any hit-test.
        let hit = if ev.local_pt.x > 100_000.0 {
            None
        } else {
            island
                .buttons
                .iter()
                .position(|b| {
                    ev.local_pt.x >= b.x
                        && ev.local_pt.x <= b.x + b.w
                        && ev.local_pt.y >= b.y
                        && ev.local_pt.y <= b.y + b.h
                })
        };
        if hit == island.tooltip_for {
            continue;
        }
        island.tooltip_for = hit;

        let content_root = chrome_c.content_root;
        for (e, parent) in existing.iter() {
            if parent.0 == content_root {
                commands.entity(e).despawn();
            }
        }

        let Some(idx) = hit else { continue };
        let b = &island.buttons[idx];
        let label = b.tooltip.clone();
        // Width of a rough text box (≈7px/char at size 12) so we can place the
        // bubble fully to the LEFT of the button.
        let tw = label.chars().count() as f32 * 6.6 + 10.0;
        let th = 18.0;
        let tx = b.x - tw - 6.0;
        let ty = b.y + (b.h - th) * 0.5;
        if let Some(m) = buttons::rounded_rect_mesh(tx, ty, tw, th, 5.0) {
            let mesh_h = meshes.add(m);
            let mat_h = materials.add(ColorMaterial {
                color: Color::srgba(0.06, 0.06, 0.08, 0.95),
                alpha_mode: AlphaMode2d::Blend,
                ..default()
            });
            commands.spawn((
                Mesh2d(mesh_h),
                MeshMaterial2d(mat_h),
                Transform::from_xyz(0.0, 0.0, Z_TOOLTIP_BG),
                IslandTooltip,
                ChildOf(content_root),
            ));
        }
        commands.spawn((
            Text2d::new(label),
            TextFont {
                font: (font.0.clone()).into(),
                font_size: FontSize::Px(12.0),
                ..default()
            },
            TextColor(theme.label),
            Anchor::CENTER_LEFT,
            Transform::from_xyz(tx + 6.0, -(ty + th * 0.5), Z_TOOLTIP_TX),
            IslandTooltip,
            ChildOf(content_root),
        ));
    }
}

pub(crate) fn build(app: &mut App) {
    app.add_systems(Update, island_tooltip);
    app.add_systems(PostUpdate, build_island);
}
