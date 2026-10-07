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
    InputConsumed, PanePinned, PaneRect, PaneRegion, PaneTag, PaneViewportReaders,
    PendingPaneActions, region_at, topmost_pane_at,
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
    SplitRight,
    SplitBelow,
    /// Opens the project list as a submenu on hover (see [`Submenu`]).
    MoveToProject,
    Close,
}

impl ContextAction {
    fn label(self) -> &'static str {
        match self {
            ContextAction::Pin => "Pin to background",
            ContextAction::Unpin => "Unpin",
            ContextAction::EjectFromCanvas => "Move out of canvas",
            ContextAction::Undock => "Undock",
            ContextAction::SplitRight => "Split Right",
            ContextAction::SplitBelow => "Split Below",
            ContextAction::MoveToProject => "Move to project",
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
    WidgetClick {
        label: String,
        id: String,
    },
    /// A destination in the "Move to project" list.
    MoveTo {
        label: String,
        project_id: u64,
    },
}

impl ContextMenuItem {
    /// Rows that open a submenu on hover; drawn with a trailing `›`.
    fn has_submenu(&self) -> bool {
        matches!(self, ContextMenuItem::Builtin(ContextAction::MoveToProject))
    }

    fn label(&self) -> &str {
        match self {
            ContextMenuItem::Builtin(a) => a.label(),
            ContextMenuItem::WidgetClick { label, .. } => label.as_str(),
            ContextMenuItem::MoveTo { label, .. } => label.as_str(),
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
    /// Destinations for the "Move to project" submenu, computed when the
    /// menu opens. Empty when the menu has no such row.
    move_items: Vec<ContextMenuItem>,
    /// The cascading submenu, while it is open.
    sub: Option<Submenu>,
    /// Bumped whenever the rows on screen change (menu items replaced, the
    /// submenu opened or closed), so `context_render` knows to redraw.
    items_rev: u64,
    deferred: Vec<(ContextAction, Entity)>,
    /// `(pane, destination project)` picks, applied with World access in
    /// `context_deferred_actions`.
    deferred_moves: Vec<(Entity, u64)>,
}

impl ContextMenu {
    fn close(&mut self) {
        self.origin = None;
        self.target = None;
        self.items.clear();
        self.move_items.clear();
        self.sub = None;
        self.hovered = None;
        self.items_rev += 1;
    }

    fn set_items(&mut self, items: Vec<ContextMenuItem>) {
        self.items = items;
        self.hovered = None;
        self.items_rev += 1;
    }
}

/// A menu opened beside a parent row when that row is hovered.
struct Submenu {
    /// Window-space top-left.
    origin: Vec2,
    /// Index of the parent-menu row it hangs off; that row stays
    /// highlighted while the submenu is open.
    parent_row: usize,
    items: Vec<ContextMenuItem>,
    hovered: Option<usize>,
}

/// Height of a menu with `rows` rows.
fn menu_height(rows: usize) -> f32 {
    rows as f32 * ROW_H + 2.0 * MENU_PAD_Y
}

/// Row index under window-space `pt` for a menu at `origin`, if any.
fn row_at(pt: Vec2, origin: Vec2, items: &[ContextMenuItem]) -> Option<usize> {
    let w = menu_width(items);
    let h = menu_height(items.len());
    if pt.x < origin.x || pt.x > origin.x + w || pt.y < origin.y || pt.y > origin.y + h {
        return None;
    }
    let idx = ((pt.y - origin.y - MENU_PAD_Y) / ROW_H).floor();
    (idx >= 0.0 && (idx as usize) < items.len()).then_some(idx as usize)
}

/// Destinations for "Move to project": the projects open in the current
/// workspace (parked ones are left out, same as every other switcher),
/// minus the one the pane already lives in.
fn move_destinations(projects: &Projects, current: u64) -> Vec<ContextMenuItem> {
    projects
        .switchable()
        .filter(|p| p.id != current)
        .map(|p| ContextMenuItem::MoveTo {
            label: p.name.clone(),
            project_id: p.id,
        })
        .collect()
}

/// Menu width = widest label, clamped to a sensible minimum.
fn menu_width(items: &[ContextMenuItem]) -> f32 {
    // Submenu rows reserve two extra columns for the right-aligned `›`.
    let longest = items
        .iter()
        .map(|i| i.label().chars().count() + if i.has_submenu() { 2 } else { 0 })
        .max()
        .unwrap_or(0) as f32;
    (longest * MENU_CHAR_W + 2.0 * ROW_PAD_X).max(MENU_W_MIN)
}

#[derive(Component)]
struct ContextMenuEntity;

pub struct ContextMenuPlugin;

impl Plugin for ContextMenuPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContextMenu>()
            .add_systems(
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
            )
            .add_systems(Update, context_deferred_actions.after(context_open_close));
    }
}

fn context_open_close(
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut keys: MessageReader<KeyboardInput>,
    // Bundled: this system is at Bevy's 16-parameter ceiling.
    (sidebar, projects, docks): (
        Res<Sidebar>,
        Res<Projects>,
        Query<(), With<jim_pane::dock::Dock>>,
    ),
    views: Res<jim_pane::Views>,
    mut menu: ResMut<ContextMenu>,
    mut consumed: ResMut<InputConsumed>,
    panes: Query<
        (
            Entity,
            &PaneRect,
            &jim_pane::PaneProject,
            &Visibility,
            Has<PanePinned>,
            // A docked pane's header is SLIM. Carried along here rather than
            // as its own query because this system is already at Bevy's
            // 16-parameter ceiling. See "Docked panes have a SLIM header".
            Option<&jim_pane::PaneChromeOverride>,
            &jim_pane::PaneKindMarker,
        ),
        With<PaneTag>,
    >,
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
        let (_, pt_canvas) = views.resolve(pt);
        let target_project = views.project_at(pt);
        // Only consider visible panes; include pinned so the user can
        // right-click them to unpin.
        let visible: Vec<(Entity, PaneRect, bool, f32, bool)> = panes
            .iter()
            .filter(|(_, _, project, vis, _, _, _)| {
                target_project.is_none_or(|id| project.0 == id)
                    && !matches!(vis, Visibility::Hidden)
            })
            .map(|(e, r, _, _, pinned, ov, kind)| {
                (
                    e,
                    *r,
                    pinned,
                    jim_pane::override_title_h(ov),
                    kind.0 == jim_emacs::native::PANE_KIND,
                )
            })
            .collect();
        // First try to hit an unpinned pane (they sit on top); fall
        // back to pinned. Reuses topmost_pane_at's z-aware hit-test.
        let unpinned_rects: Vec<(Entity, PaneRect)> = visible
            .iter()
            .filter(|(_, _, pinned, _, _)| !pinned)
            .map(|(e, r, _, _, _)| (*e, *r))
            .collect();
        let target = topmost_pane_at(pt_canvas, &unpinned_rects).or_else(|| {
            let pinned_rects: Vec<(Entity, PaneRect)> = visible
                .iter()
                .filter(|(_, _, pinned, _, _)| *pinned)
                .map(|(e, r, _, _, _)| (*e, *r))
                .collect();
            topmost_pane_at(pt_canvas, &pinned_rects)
        });
        let Some(target) = target else {
            // Miss every pane — let the radial menu handle the click.
            return;
        };
        let rect = visible
            .iter()
            .find(|(e, _, _, _, _)| *e == target)
            .map(|(_, r, _, _, _)| *r);
        let is_pinned = visible
            .iter()
            .find(|(e, _, _, _, _)| *e == target)
            .map(|(_, _, p, _, _)| *p)
            .unwrap_or(false);
        let title_h = visible
            .iter()
            .find(|(e, _, _, _, _)| *e == target)
            .map(|(_, _, _, th, _)| *th)
            .unwrap_or(jim_pane::TITLE_H);
        let is_native_emacs = visible
            .iter()
            .find(|(e, _, _, _, _)| *e == target)
            .map(|(_, _, _, _, native)| *native)
            .unwrap_or(false);

        // If the target is a terminal whose child grabbed the mouse, a
        // plain right-click on its CONTENT belongs to that child
        // (tmux/mc/ranger menus, etc.), not to our per-pane menu. Yield
        // without consuming so jim_terminal's report system forwards it.
        // Only the content: the title bar is our chrome, and yielding there
        // too left a terminal running a mouse-tracking TUI (Claude Code,
        // vim) with no pane menu at all. Shift is the escape hatch —
        // Shift+right-click on the content still opens this menu.
        let shift = key_state.pressed(KeyCode::ShiftLeft) || key_state.pressed(KeyCode::ShiftRight);
        // A docked member has a slim header and no resize band of its own.
        let is_member = members.get(target).is_ok();
        let on_content = rect.is_some_and(|r| {
            matches!(
                jim_pane::region_at_ex(pt_canvas, &r, title_h, !is_member),
                Some(PaneRegion::Content)
            )
        });
        if !shift && on_content && jim_terminal::pane_mouse_tracking(&term_store, target) {
            return;
        }

        // The widget's own per-row menu for the row under the cursor, if any
        // (declared via `ListItem.context`). Computed once here because BOTH
        // the docked-member branch and the content-region branch below need
        // it: a docked file tree is the main consumer of row menus, and
        // taking Undock unconditionally would mean it could never show one.
        let widget_row_items = |rect: &PaneRect| -> Option<Vec<ContextMenuItem>> {
            let (wtargets, wscroll) = widgets.get(target).ok()?;
            let hit = jim_pane::pt_to_content_local_th(pt_canvas, rect, title_h)
                + jim_widget::scroll_offset(wscroll);
            let ct = wtargets
                .context_menus
                .iter()
                .find(|c| !c.items.is_empty() && c.rect.contains(hit))?;
            Some(
                ct.items
                    .iter()
                    .map(|it| ContextMenuItem::WidgetClick {
                        label: it.label.clone(),
                        id: it.id.clone(),
                    })
                    .collect(),
            )
        };

        // A docked member has no chrome of its own — its whole surface is
        // "content" — so a plain right-click anywhere on it offers Undock
        // (the only way to pop it back out), EXCEPT over a row that carries
        // its own menu. Undock stays reachable from the header and from any
        // empty space in the list.
        if members.get(target).is_ok() {
            if let Some(items) = rect.as_ref().and_then(&widget_row_items) {
                menu.origin = Some(pt);
                menu.target = Some(target);
                menu.set_items(items);
                consumed.0 = true;
                return;
            }
            // No "Move to project" here: a dock owns its members' rects,
            // so a member has to be undocked before it can move.
            let mut items = Vec::new();
            if is_native_emacs {
                items.extend([
                    ContextMenuItem::Builtin(ContextAction::SplitRight),
                    ContextMenuItem::Builtin(ContextAction::SplitBelow),
                ]);
            }
            items.extend([
                ContextMenuItem::Builtin(ContextAction::Undock),
                ContextMenuItem::Builtin(ContextAction::Close),
            ]);
            menu.origin = Some(pt);
            menu.target = Some(target);
            menu.set_items(items);
            consumed.0 = true;
            return;
        }

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
            if let Some(items) = rect.as_ref().and_then(&widget_row_items) {
                menu.origin = Some(pt);
                menu.target = Some(target);
                menu.set_items(items);
                consumed.0 = true;
                return;
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
        if is_native_emacs {
            items.extend([
                ContextMenuItem::Builtin(ContextAction::SplitRight),
                ContextMenuItem::Builtin(ContextAction::SplitBelow),
            ]);
        }
        // Offered only when there is somewhere to go. A dock carries its
        // members with it in principle, but moving it would strand them in
        // the old project, so docks are left out like docked members.
        let move_items = match panes.get(target) {
            Ok((_, _, project, ..)) if docks.get(target).is_err() => {
                move_destinations(&projects, project.0)
            }
            _ => Vec::new(),
        };
        if !move_items.is_empty() {
            items.push(ContextMenuItem::Builtin(ContextAction::MoveToProject));
        }
        items.push(ContextMenuItem::Builtin(ContextAction::Close));
        menu.origin = Some(pt);
        menu.target = Some(target);
        menu.set_items(items);
        menu.move_items = move_items;
        // Suppress the radial open + pane left-click for this frame.
        consumed.0 = true;
        return;
    }

    if menu.origin.is_some() && buttons.just_pressed(MouseButton::Left) {
        // At most one of the two menus has a hovered row (see
        // `context_hover`); neither means the click missed and dismisses.
        let pick = menu
            .sub
            .as_ref()
            .and_then(|sub| sub.hovered.and_then(|i| sub.items.get(i).cloned()))
            .or_else(|| menu.hovered.and_then(|i| menu.items.get(i).cloned()));
        let target = menu.target;
        // Click on the menu itself counts as "consumed" so the pane
        // beneath doesn't focus / drag on the same release.
        consumed.0 = true;
        // The submenu's parent row opens on hover; clicking it is a no-op
        // rather than a dismiss.
        if pick.as_ref().is_some_and(ContextMenuItem::has_submenu) {
            return;
        }
        menu.close();
        match (pick, target) {
            (Some(ContextMenuItem::Builtin(ContextAction::Pin)), Some(e)) => pending.pin.push(e),
            (Some(ContextMenuItem::Builtin(ContextAction::Unpin)), Some(e)) => {
                pending.unpin.push(e)
            }
            (Some(ContextMenuItem::Builtin(ContextAction::EjectFromCanvas)), Some(e)) => {
                eject.0.push(e)
            }
            (Some(ContextMenuItem::Builtin(ContextAction::Undock)), Some(e)) => {
                pending.undock.push(e)
            }
            (Some(ContextMenuItem::Builtin(ContextAction::Close)), Some(e)) => {
                pending.close.push(e)
            }
            (Some(ContextMenuItem::Builtin(action @ ContextAction::SplitRight)), Some(e))
            | (Some(ContextMenuItem::Builtin(action @ ContextAction::SplitBelow)), Some(e)) => {
                menu.deferred.push((action, e));
            }
            (Some(ContextMenuItem::MoveTo { project_id, .. }), Some(e)) => {
                menu.deferred_moves.push((e, project_id));
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

fn context_deferred_actions(world: &mut World) {
    let moves = std::mem::take(&mut world.resource_mut::<ContextMenu>().deferred_moves);
    for (pane, dest) in moves {
        // The menu never offers a move for a docked pane, but the pane
        // could have been docked between opening the menu and picking.
        let docked = world.get::<jim_pane::dock::DockMember>(pane).is_some()
            || world.get::<jim_pane::dock::Dock>(pane).is_some();
        if docked {
            warn!("not moving docked pane {pane:?}: undock it first");
            continue;
        }
        if world.get_entity(pane).is_err() {
            continue;
        }
        crate::projects::move_pane_to_project(world, pane, dest);
    }
    let actions = std::mem::take(&mut world.resource_mut::<ContextMenu>().deferred);
    for (action, pane) in actions {
        let direction = match action {
            ContextAction::SplitRight => jim_emacs::native::NativeSplitDirection::Right,
            ContextAction::SplitBelow => jim_emacs::native::NativeSplitDirection::Below,
            _ => continue,
        };
        if !jim_emacs::native::request_native_split(world, pane, direction) {
            warn!("could not split native Emacs pane {pane:?}");
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

    // Over the submenu: highlight its row, keep the parent row lit.
    if let Some(sub) = &menu.sub {
        if over_menu(pt, sub.origin, &sub.items) {
            let new_hover = row_at(pt, sub.origin, &sub.items);
            if menu.sub.as_ref().is_some_and(|s| s.hovered != new_hover) {
                menu.sub.as_mut().unwrap().hovered = new_hover;
            }
            if menu.hovered.is_some() {
                menu.hovered = None;
            }
            return;
        }
    }

    let new_hover = row_at(pt, origin, &menu.items);
    match new_hover {
        // Onto the submenu's parent row: open the submenu (if not already).
        Some(i) if menu.items[i].has_submenu() => {
            if menu.sub.as_ref().is_none_or(|s| s.parent_row != i) {
                let items = menu.move_items.clone();
                let menu_w = menu_width(&menu.items);
                let sub_w = menu_width(&items);
                let sub_h = menu_height(items.len());
                // Right of the menu, overlapping the border by a pixel;
                // flipped to the left when it would run off the window.
                let mut x = origin.x + menu_w;
                if x + sub_w > window.width() {
                    x = origin.x - sub_w;
                }
                // Top row lines up with the parent row, slid up to fit.
                let row_top = origin.y + i as f32 * ROW_H;
                let y = row_top.min(window.height() - sub_h).max(0.0);
                menu.sub = Some(Submenu {
                    origin: Vec2::new(x.max(0.0), y),
                    parent_row: i,
                    items,
                    hovered: None,
                });
                menu.items_rev += 1;
            }
        }
        // Onto any other row: that row takes over, the submenu closes.
        Some(_) => {
            if menu.sub.take().is_some() {
                menu.items_rev += 1;
            }
        }
        // Off both menus: leave an open submenu alone, so a diagonal move
        // from the parent row toward it doesn't snap it shut. (Its parent
        // row is drawn lit by `context_render`, not marked hovered here — a
        // hovered row is what a click picks, and a click out here must
        // dismiss the menu, not land on the parent row.)
        None => {
            if let Some(sub) = menu.sub.as_mut()
                && sub.hovered.is_some()
            {
                sub.hovered = None;
            }
        }
    }
    if menu.hovered != new_hover {
        menu.hovered = new_hover;
    }
}

/// Is window-space `pt` inside the box of a menu at `origin` (including
/// its top/bottom padding, which `row_at` doesn't count as a row)?
fn over_menu(pt: Vec2, origin: Vec2, items: &[ContextMenuItem]) -> bool {
    pt.x >= origin.x
        && pt.x <= origin.x + menu_width(items)
        && pt.y >= origin.y
        && pt.y <= origin.y + menu_height(items.len())
}

#[derive(Default)]
struct LastRender {
    open: bool,
    hovered: Option<usize>,
    sub_hovered: Option<usize>,
    origin: Option<Vec2>,
    items_rev: u64,
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

    let want_open = menu.origin.is_some();
    let already_open = existing.iter().next().is_some();
    let sub_hovered = menu.sub.as_ref().and_then(|s| s.hovered);
    let sig_changed = last.open != want_open
        || last.hovered != menu.hovered
        || last.sub_hovered != sub_hovered
        || last.origin != menu.origin
        || last.items_rev != menu.items_rev
        || theme.is_changed();
    if !sig_changed && !(want_open && !already_open) {
        return;
    }
    for e in &existing {
        commands.entity(e).despawn();
    }
    last.open = want_open;
    last.hovered = menu.hovered;
    last.sub_hovered = sub_hovered;
    last.origin = menu.origin;
    last.items_rev = menu.items_rev;

    let Some(origin) = menu.origin else {
        return;
    };

    let draw = MenuDraw {
        win: Vec2::new(window.width(), window.height()),
        font: &font,
        theme: &theme,
    };
    // The submenu's parent row stays lit while the submenu is open.
    let lit = menu.hovered.or(menu.sub.as_ref().map(|s| s.parent_row));
    draw.menu(&mut commands, origin, &menu.items, lit, MENU_Z);
    if let Some(sub) = &menu.sub {
        // A whole z-unit above the parent so the two never interleave.
        draw.menu(
            &mut commands,
            sub.origin,
            &sub.items,
            sub.hovered,
            MENU_Z + 1.0,
        );
    }
}

struct MenuDraw<'a> {
    win: Vec2,
    font: &'a MonoFont,
    theme: &'a jim_style::Theme,
}

impl MenuDraw<'_> {
    /// Spawn one menu box (border, background, hover wash, labels) with its
    /// top-left at window-space `origin`.
    fn menu(
        &self,
        commands: &mut Commands,
        origin: Vec2,
        items: &[ContextMenuItem],
        hovered: Option<usize>,
        z: f32,
    ) {
        use jim_style::tokens as t;
        let c = |id| Color::LinearRgba(self.theme.color(id));
        let bg = c(t::PANE_BG);
        let row_hover = c(t::SIDEBAR_ROW_ACTIVE_BG);
        let text = c(t::FG);
        let border = c(t::CHROME_DIVIDER);

        let menu_h = menu_height(items.len());
        let menu_w = menu_width(items);

        // Window-space (top-left, y-down) → world-space (center, y-up).
        let (win_w, win_h) = (self.win.x, self.win.y);
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
            Transform::from_xyz(menu_world_tl.x - 1.0, menu_world_tl.y + 1.0, z),
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
            Transform::from_xyz(menu_world_tl.x, menu_world_tl.y, z + 0.10),
            overlay.clone(),
        ));

        let text_font = TextFont {
            font: (self.font.0.clone()).into(),
            font_size: FontSize::Px(FONT_SIZE),
            ..default()
        };
        for (i, item) in items.iter().enumerate() {
            let row_top_window = origin + Vec2::new(0.0, MENU_PAD_Y + (i as f32) * ROW_H);
            let row_world_tl = to_world(row_top_window);
            if hovered == Some(i) {
                commands.spawn((
                    ContextMenuEntity,
                    Sprite {
                        color: row_hover,
                        custom_size: Some(Vec2::new(menu_w, ROW_H)),
                        ..default()
                    },
                    Anchor::TOP_LEFT,
                    Transform::from_xyz(row_world_tl.x, row_world_tl.y, z + 0.20),
                    overlay.clone(),
                ));
            }
            let row_mid_y = row_world_tl.y - ROW_H * 0.5;
            commands.spawn((
                ContextMenuEntity,
                Text2d::new(item.label()),
                text_font.clone(),
                LineHeight::Px(ROW_H),
                TextColor(text),
                Anchor::CENTER_LEFT,
                Transform::from_xyz(row_world_tl.x + ROW_PAD_X, row_mid_y, z + 0.30),
                overlay.clone(),
            ));
            if item.has_submenu() {
                commands.spawn((
                    ContextMenuEntity,
                    Text2d::new("›"),
                    text_font.clone(),
                    LineHeight::Px(ROW_H),
                    TextColor(text),
                    Anchor::CENTER_RIGHT,
                    Transform::from_xyz(row_world_tl.x + menu_w - ROW_PAD_X, row_mid_y, z + 0.30),
                    overlay.clone(),
                ));
            }
        }
    }
}
