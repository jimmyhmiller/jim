//! Whiteboard widget for Jim: an Excalidraw-style drawing surface, available
//! both as a floating pane and painted directly on the project canvas, driven by
//! a fixed on-screen toolbar.
//!
//! - [`pane`] — the floating "Whiteboard" pane kind.
//! - [`toolbar`] — a screen-anchored toolbar that drives [`WbToolState`].
//! - [`background`] — drawing directly on the project canvas behind the panes.
//! - [`render`] — shared `DrawCommand` → Bevy mesh tessellation.

use bevy::prelude::*;

use jim_pane::PaneRegistry;
use whiteboard_core::editor::Editor;
use whiteboard_core::interaction::Tool;
use whiteboard_core::render::{Color as WbColor, FillStyle, StrokeStyle};
use whiteboard_core::shape::RoughGenerator;

pub mod buttons;
pub mod island;
pub mod measurer;
pub mod pane;
pub mod render;
pub mod toolbar;

use measurer::WbMeasurer;

/// The concrete editor type every whiteboard surface owns: hand-drawn
/// ("sketchy") generator + our font-tuned measurer.
pub type WbEditor = Editor<WbMeasurer, RoughGenerator>;

/// Build a fresh editor seeded with the default tool.
pub fn new_editor() -> WbEditor {
    let mut e = Editor::new_rough(WbMeasurer);
    e.set_tool(Tool::Freedraw);
    e
}

/// The active tool plus the style new elements are stamped with. One of these
/// lives **per whiteboard pane** (Mode 1, driven by that pane's island toolbar)
/// and one lives in the [`WbToolState`] resource (Mode 2, driven by the floating
/// canvas toolbar) — so a pane's tool selection is independent of the canvas.
#[derive(Clone, Debug)]
pub struct ToolStyle {
    pub tool: Tool,
    pub stroke_color: WbColor,
    pub background_color: WbColor,
    pub stroke_width: f64,
    pub fill_style: FillStyle,
    pub stroke_style: StrokeStyle,
    pub roughness: f64,
    /// 0..=100, like Excalidraw.
    pub opacity: f64,
}

impl Default for ToolStyle {
    fn default() -> Self {
        ToolStyle {
            tool: Tool::Freedraw,
            stroke_color: WbColor::rgb(0x1e, 0x1e, 0x1e),
            background_color: WbColor::TRANSPARENT,
            stroke_width: 2.0,
            fill_style: FillStyle::Hachure,
            stroke_style: StrokeStyle::Solid,
            roughness: 1.0,
            opacity: 100.0,
        }
    }
}

/// Move the selection within the paint order.
#[derive(Clone, Copy, Debug)]
pub enum ZOrder {
    ToBack,
    Backward,
    Forward,
    ToFront,
}

/// A property change or action the canvas toolbar requests be applied to the
/// CURRENT selection (Excalidraw-style "select and change"). The host (jim-app)
/// owns the per-project board, so it applies these. Property variants also
/// update [`WbToolState`] (handled at the toolbar) so new elements inherit them.
#[derive(Message, Clone, Copy, Debug)]
pub enum CanvasEdit {
    Stroke(WbColor),
    Background(WbColor),
    Fill(FillStyle),
    Width(f64),
    StrokeStyle(StrokeStyle),
    Roughness(f64),
    Opacity(f64),
    ZOrder(ZOrder),
    Duplicate,
    Delete,
}

/// The **canvas** (Mode 2) tool/style state. The floating canvas toolbar mutates
/// it; the background surface reads it when creating elements. Derefs to the
/// inner [`ToolStyle`] so callers can still read/write `ts.tool` etc. directly.
#[derive(Resource, Clone, Debug, Default)]
pub struct WbToolState(pub ToolStyle);

impl std::ops::Deref for WbToolState {
    type Target = ToolStyle;
    fn deref(&self) -> &ToolStyle {
        &self.0
    }
}

impl std::ops::DerefMut for WbToolState {
    fn deref_mut(&mut self) -> &mut ToolStyle {
        &mut self.0
    }
}

/// True whenever a floating canvas toolbar (Mode 2) is open. The background
/// drawing surface keys off this instead of an explicit toggle — the toolbar's
/// presence *is* the "draw on the canvas" mode.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct CanvasDrawActive(pub bool);

/// Emitted by the floating canvas toolbar's "Clear" button. The host (jim-app)
/// owns the per-project background board, so it handles the wipe.
#[derive(Message, Clone, Copy, Debug)]
pub struct ClearCanvasRequested;

/// Installs the whiteboard pane kind, the toolbar, the background surface, and
/// all their systems.
pub struct WhiteboardPlugin;

impl Plugin for WhiteboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WbToolState>();
        app.init_resource::<CanvasDrawActive>();
        app.add_message::<ClearCanvasRequested>();
        app.add_message::<CanvasEdit>();
        app.add_systems(Startup, register_kinds);
        pane::build(app);
        island::build(app);
        toolbar::build(app);
    }
}

fn register_kinds(mut registry: ResMut<PaneRegistry>) {
    pane::register(&mut registry);
    toolbar::register(&mut registry);
}
