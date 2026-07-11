//! Per-pane right-click context menu.
//!
//! Right-click that lands on a pane opens a small vertical list of
//! actions (Pin/Unpin, Close). Right-click that misses every pane
//! falls through to the radial spawn menu in [`crate::radial`]. The
//! menu consumes [`InputConsumed`] on open and on item-pick so the
//! pane-mouse handler and the radial open-handler don't also act on
//! the same press/release.
//!
//! The menu is rendered as a couple of sprites + Text2d entities on a
//! dedicated z above the radial backdrop so it always sits on top.

use bevy::camera::visibility::RenderLayers;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::text::LineHeight;

use jim_pane::{
    pt_to_content_local, region_at, topmost_pane_at, InputConsumed, PaneRect, PaneRegion, PaneTag,
    PanePinned, PaneViewportReaders, PendingPaneActions,
};
use jim_widget::protocol::HostEvent;
use jim_widget::script_widget::ScriptWidget;
use jim_widget::{WidgetScroll, WidgetTargets};

use crate::projects::{Projects, Sidebar};
use jim_terminal::MonoFont;

/// Above the radial menu's RADIAL_Z (=600) so a context menu opened on
/// a pane never sits behind a wedge.
const MENU_Z: f32 = 700.0;

const ROW_H: f32 = 24.0;
const ROW_PAD_X: f32 = 12.0;
const MENU_W_MIN: f32 = 140.0;
/// Approx advance width of the mono menu font at `FONT_SIZE`, used to size
/// the menu to its widest label (widget items can be long, e.g. "Stage
/// selected (3)").
const MENU_CHAR_W: f32 = 7.0;
const MENU_PAD_Y: f32 = 4.0;
const FONT_SIZE: f32 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextAction {
    Pin,
    Unpin,
    /// Move a gathered pane back out of its nested canvas, one level up.
    EjectFromCanvas,
    /// Pop a docked member back out into a free-floating pane.
    Undock,
    Close,
}

impl ContextAction {
    fn label(self) -> &'static str {
        match self {
            ContextAction::Pin => "Pin to background",
            ContextAction::Unpin => "Unpin",
            ContextAction::EjectFromCanvas => "Move out of canvas",
            ContextAction::Undock => "Undock",
            ContextAction::Close => "Close",
        }
    }
}

/// One row in the pane context menu: either a host built-in (Pin/Close) or
/// an item contributed by the widget under the cursor (routed back to it as
/// a `Click {id}` when picked).
#[derive(Clone, Debug)]
pub enum ContextMenuItem {
    Builtin(ContextAction),
    WidgetClick { label: String, id: String },
}

impl ContextMenuItem {
    fn label(&self) -> &str {
        match self {
            ContextMenuItem::Builtin(a) => a.label(),
            ContextMenuItem::WidgetClick { label, .. } => label.as_str(),
        }
    }
}

#[derive(Resource, Default)]
pub struct ContextMenu {
    /// Window-space top-left of the menu (None = closed).
    pub origin: Option<Vec2>,
    pub target: Option<Entity>,
    pub items: Vec<ContextMenuItem>,
    pub hovered: Option<usize>,
}

impl ContextMenu {
    fn close(&mut self) {
        self.origin = None;
        self.target = None;
        self.items.clear();
        self.hovered = None;
    }
}

/// Menu width = widest label, clamped to a sensible minimum.
fn menu_width(items: &[ContextMenuItem]) -> f32 {
    let longest = items
        .iter()
        .map(|i| i.label().chars().count())
        .max()
        .unwrap_or(0) as f32;
    (longest * MENU_CHAR_W + 2.0 * ROW_PAD_X).max(MENU_W_MIN)
}

#[derive(Component)]
struct ContextMenuEntity;

pub struct ContextMenuPlugin;

impl Plugin for ContextMenuPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContextMenu>().add_systems(
            Update,
            (
                // MUST run before radial::radial_open_close so it can
                // set `InputConsumed` on right-click-on-pane and the
                // radial sees that flag and stays closed. Also before
                // `PaneViewportReaders` (which holds `handle_pane_mouse`)
                // so that when a left-click PICKS a menu item, we set
                // `InputConsumed` first and the pane-mouse handler skips
                // it — otherwise the same click would also leak through to
                // the widget under the menu (e.g. toggling a diff line).
                context_open_close
                    .before(crate::radial::radial_open_close)
                    .before(PaneViewportReaders),
                context_hover,
                context_render,
            )
                .chain(),
        );
    }
}

fn context_open_close(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut keys: MessageReader<KeyboardInput>,
    sidebar: Res<Sidebar>,
    viewport: Res<jim_pane::PaneViewport>,
    mut menu: ResMut<ContextMenu>,
    mut consumed: ResMut<InputConsumed>,
    panes: Query<(Entity, &PaneRect, &Visibility, Has<PanePinned>), With<PaneTag>>,
    members: Query<(), With<jim_pane::dock::DockMember>>,
    gathered: Query<&jim_pane::PaneCanvas>,
    mut pending: ResMut<PendingPaneActions>,
    mut eject: ResMut<crate::canvas_pane::CanvasEjectQueue>,
    // Widget panes can contribute their own context-menu items for the row
    // under the cursor (declared via `ListItem.context`). When one is hit, we
    // show those instead of the default pane menu and route a pick back as a
    // `Click {id}` (see the Left-pick branch).
    widgets: Query<(&WidgetTargets, Option<&WidgetScroll>)>,
    script_widgets: Query<&ScriptWidget>,
    _projects: Res<Projects>,
    key_state: Res<ButtonInput<KeyCode>>,
    term_store: Res<jim_terminal::TerminalStore>,
) {
    let Ok(window) = windows.single() else {
        return;
    };

    let mut esc = false;
    for ev in keys.read() {
        if ev.state.is_pressed() && matches!(ev.key_code, KeyCode::Escape) {
            esc = true;
        }
    }
    if esc && menu.origin.is_some() {
        menu.close();
        return;
    }

    if buttons.just_pressed(MouseButton::Right) {
        // Close any previously open menu before considering a re-open.
        let was_open = menu.origin.is_some();
        if was_open {
            menu.close();
        }
        let Some(pt) = window.cursor_position() else {
            return;
        };
        if pt.x < sidebar.width {
            return;
        }
        // PaneRect lives in canvas-space; convert the cursor into the
        // same frame before hit-testing, otherwise panning/zooming the
        // canvas makes the radial menu open on top of visible panes.
        let pt_canvas = viewport.window_to_canvas(pt);
        // Only consider visible panes; include pinned so the user can
        // right-click them to unpin.
        let visible: Vec<(Entity, PaneRect, bool)> = panes
            .iter()
            .filter(|(_, _, vis, _)| !matches!(vis, Visibility::Hidden))
            .map(|(e, r, _, pinned)| (e, *r, pinned))
            .collect();
        // First try to hit an unpinned pane (they sit on top); fall
        // back to pinned. Reuses topmost_pane_at's z-aware hit-test.
        let unpinned_rects: Vec<(Entity, PaneRect)> = visible
            .iter()
            .filter(|(_, _, pinned)| !pinned)
            .map(|(e, r, _)| (*e, *r))
            .collect();
        let target = topmost_pane_at(pt_canvas, &unpinned_rects).or_else(|| {
            let pinned_rects: Vec<(Entity, PaneRect)> = visible
                .iter()
                .filter(|(_, _, pinned)| *pinned)
                .map(|(e, r, _)| (*e, *r))
                .collect();
            topmost_pane_at(pt_canvas, &pinned_rects)
        });
        let Some(target) = target else {
            // Miss every pane — let the radial menu handle the click.
            return;
        };
        // If the target is a terminal whose child grabbed the mouse, a
        // plain right-click belongs to that child (tmux/mc/ranger menus,
        // etc.), not to our per-pane menu. Yield without consuming so
        // jim_terminal's report system forwards it. Shift is the escape
        // hatch — Shift+right-click still opens this menu.
        let shift = key_state.pressed(KeyCode::ShiftLeft)
            || key_state.pressed(KeyCode::ShiftRight);
        if !shift && jim_terminal::pane_mouse_tracking(&term_store, target) {
            return;
        }
        let rect = visible
            .iter()
            .find(|(e, _, _)| *e == target)
            .map(|(_, r, _)| *r);
        let is_pinned = visible
            .iter()
            .find(|(e, _, _)| *e == target)
            .map(|(_, _, p)| *p)
            .unwrap_or(false);

        // Offer "Move out of canvas" when this pane is gathered into a
        // nested canvas (PaneCanvas != 0).
        let in_canvas = gathered.get(target).map(|c| c.0 != 0).unwrap_or(false);
        let region = rect.map(|r| region_at(pt_canvas, &r));

        // Right-click on the CONTENT area of an UNPINNED pane: the host pane
        // menu (Pin/Close) lives on the title bar only, so content never shows
        // it. A widget can offer its own per-row menu (declared via
        // `ListItem.context`); otherwise the content right-click is a no-op
        // (but still consumed so the radial spawn menu doesn't open over the
        // pane). Pinned panes hide their chrome, so they keep the old
        // anywhere-right-click → menu behavior (it's the only way to unpin).
        if !is_pinned && matches!(region, Some(Some(PaneRegion::Content))) {
            if let (Some(rect), Ok((wtargets, wscroll))) = (rect, widgets.get(target)) {
                let scroll_y = wscroll.map(|s| s.y).unwrap_or(0.0);
                let hit = pt_to_content_local(pt_canvas, &rect) + Vec2::new(0.0, scroll_y);
                if let Some(ct) = wtargets
                    .context_menus
                    .iter()
                    .find(|c| !c.items.is_empty() && c.rect.contains(hit))
                {
                    menu.origin = Some(pt);
                    menu.target = Some(target);
                    menu.items = ct
                        .items
                        .iter()
                        .map(|it| ContextMenuItem::WidgetClick {
                            label: it.label.clone(),
                            id: it.id.clone(),
                        })
                        .collect();
                    menu.hovered = None;
                    consumed.0 = true;
                    return;
                }
            }
            consumed.0 = true;
            return;
        }

        // Otherwise the click is on the pane's chrome (title bar / close /
        // resize edge), or on a pinned pane (chrome hidden): show the host
        // pane menu.
        let mut items = if is_pinned {
            vec![ContextMenuItem::Builtin(ContextAction::Unpin)]
        } else {
            vec![ContextMenuItem::Builtin(ContextAction::Pin)]
        };
        if in_canvas {
            items.push(ContextMenuItem::Builtin(ContextAction::EjectFromCanvas));
        }
        items.push(ContextMenuItem::Builtin(ContextAction::Close));
        menu.origin = Some(pt);
        menu.target = Some(target);
        menu.items = items;
        menu.hovered = None;
        // Suppress the radial open + pane left-click for this frame.
        consumed.0 = true;
        return;
    }

    if menu.origin.is_some() && buttons.just_pressed(MouseButton::Left) {
        let pick = menu.hovered.and_then(|i| menu.items.get(i).cloned());
        let target = menu.target;
        menu.close();
        // Click on the menu itself counts as "consumed" so the pane
        // beneath doesn't focus / drag on the same release.
        consumed.0 = true;
        match (pick, target) {
            (Some(ContextMenuItem::Builtin(ContextAction::Pin)), Some(e)) => pending.pin.push(e),
            (Some(ContextMenuItem::Builtin(ContextAction::Unpin)), Some(e)) => {
                pending.unpin.push(e)
            }
            (Some(ContextMenuItem::Builtin(ContextAction::Close)), Some(e)) => {
                pending.close.push(e)
            }
            (Some(ContextMenuItem::WidgetClick { id, .. }), Some(e)) => {
                // Route the pick back to the widget as a normal button click;
                // its `on_click(id)` runs the staging action (script widgets).
                if let Ok(sw) = script_widgets.get(e) {
                    sw.send_host_event(&HostEvent::Click { id });
                }
            }
            _ => {}
        }
    }
}

fn context_hover(windows: Query<&Window>, mut menu: ResMut<ContextMenu>) {
    let Some(origin) = menu.origin else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(pt) = window.cursor_position() else {
        return;
    };
    let menu_h = menu.items.len() as f32 * ROW_H + 2.0 * MENU_PAD_Y;
    let menu_w = menu_width(&menu.items);
    let in_menu = pt.x >= origin.x
        && pt.x <= origin.x + menu_w
        && pt.y >= origin.y
        && pt.y <= origin.y + menu_h;
    let new_hover = if !in_menu {
        None
    } else {
        let local_y = pt.y - origin.y - MENU_PAD_Y;
        let idx = (local_y / ROW_H).floor() as i32;
        if idx < 0 || idx as usize >= menu.items.len() {
            None
        } else {
            Some(idx as usize)
        }
    };
    if menu.hovered != new_hover {
        menu.hovered = new_hover;
    }
}

#[derive(Default)]
struct LastRender {
    open: bool,
    hovered: Option<usize>,
    origin: Option<Vec2>,
    item_count: usize,
}

fn context_render(
    mut commands: Commands,
    menu: Res<ContextMenu>,
    windows: Query<&Window>,
    font: Res<MonoFont>,
    theme: Res<jim_style::Theme>,
    existing: Query<Entity, With<ContextMenuEntity>>,
    mut last: Local<LastRender>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let win_w = window.width();
    let win_h = window.height();

    let want_open = menu.origin.is_some();
    let already_open = existing.iter().next().is_some();
    let sig_changed = last.open != want_open
        || last.hovered != menu.hovered
        || last.origin != menu.origin
        || last.item_count != menu.items.len()
        || theme.is_changed();
    if !sig_changed && !(want_open && !already_open) {
        return;
    }
    for e in &existing {
        commands.entity(e).despawn();
    }
    last.open = want_open;
    last.hovered = menu.hovered;
    last.origin = menu.origin;
    last.item_count = menu.items.len();

    let Some(origin) = menu.origin else {
        return;
    };

    use jim_style::tokens as t;
    let c = |id| Color::LinearRgba(theme.color(id));
    let bg = c(t::PANE_BG);
    let row_hover = c(t::SIDEBAR_ROW_ACTIVE_BG);
    let text = c(t::FG);
    let text_hover = c(t::FG);
    let border = c(t::CHROME_DIVIDER);

    let menu_h = menu.items.len() as f32 * ROW_H + 2.0 * MENU_PAD_Y;
    let menu_w = menu_width(&menu.items);

    // Window-space (top-left, y-down) → world-space (center, y-up).
    let to_world = |p: Vec2| Vec2::new(p.x - win_w * 0.5, win_h * 0.5 - p.y);

    let menu_world_tl = to_world(origin);
    let overlay = RenderLayers::layer(crate::MENU_OVERLAY_LAYER);

    // Border / drop sprite (1px ring via slightly-larger sprite behind).
    commands.spawn((
        ContextMenuEntity,
        Sprite {
            color: border,
            custom_size: Some(Vec2::new(menu_w + 2.0, menu_h + 2.0)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(menu_world_tl.x - 1.0, menu_world_tl.y + 1.0, MENU_Z),
        overlay.clone(),
    ));

    // Background.
    commands.spawn((
        ContextMenuEntity,
        Sprite {
            color: bg,
            custom_size: Some(Vec2::new(menu_w, menu_h)),
            ..default()
        },
        Anchor::TOP_LEFT,
        Transform::from_xyz(menu_world_tl.x, menu_world_tl.y, MENU_Z + 0.10),
        overlay.clone(),
    ));

    for (i, action) in menu.items.iter().enumerate() {
        let row_top_window = origin + Vec2::new(0.0, MENU_PAD_Y + (i as f32) * ROW_H);
        let row_world_tl = to_world(row_top_window);
        let hovered = menu.hovered == Some(i);
        if hovered {
            commands.spawn((
                ContextMenuEntity,
                Sprite {
                    color: row_hover,
                    custom_size: Some(Vec2::new(menu_w, ROW_H)),
                    ..default()
                },
                Anchor::TOP_LEFT,
                Transform::from_xyz(row_world_tl.x, row_world_tl.y, MENU_Z + 0.20),
                overlay.clone(),
            ));
        }
        let label_color = if hovered { text_hover } else { text };
        commands.spawn((
            ContextMenuEntity,
            Text2d::new(action.label()),
            TextFont {
                font: (font.0.clone()).into(),
                font_size: FontSize::Px(FONT_SIZE),
                ..default()
            },
            LineHeight::Px(ROW_H),
            TextColor(label_color),
            Anchor::CENTER_LEFT,
            Transform::from_xyz(
                row_world_tl.x + ROW_PAD_X,
                row_world_tl.y - ROW_H * 0.5,
                MENU_Z + 0.30,
            ),
            overlay.clone(),
        ));
    }
}
