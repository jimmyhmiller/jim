//! Consent gate for IPC screenshot requests.
//!
//! A screenshot asked for over the IPC socket ([`crate::ipc::IpcRequest::Screenshot`])
//! does NOT capture immediately. Instead it enqueues here and a small toast
//! appears (top-center, on [`MENU_OVERLAY_LAYER`]) describing what the
//! requester wants to see. The user can:
//!
//! - **click the toast** → capture now,
//! - **click the ✕** → skip (no capture),
//! - **do nothing** → it auto-captures after [`CONSENT_SECS`].
//!
//! This way an automated requester (e.g. me, verifying a change) never grabs
//! a frame out from under the user while they're working — they get a heads
//! up and a chance to defer, but it still happens on its own if ignored.
//!
//! The toast is despawned *before* the capture fires (we pop the queue in
//! the tick system, which runs before [`render_consent`] rebuilds the
//! overlay), so the toast never ends up in the screenshot itself.

use std::collections::VecDeque;
use std::path::PathBuf;

use bevy::camera::visibility::RenderLayers;
use bevy::input::ButtonInput;
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};

use jim_pane::{InputConsumed, PaneFont, PaneFontMetrics};
use jim_widget::protocol::{Align, Border, Edges, Element, Shadow, Style, Weight};
use jim_widget::render::{self, LayoutCtx, WidgetPalette};
use jim_widget::WidgetTargets;

use crate::MENU_OVERLAY_LAYER;

/// Auto-capture countdown: if the user doesn't act within this many seconds,
/// the screenshot happens on its own.
const CONSENT_SECS: f64 = 30.0;
const TOAST_W: f32 = 460.0;
/// Fixed hit-test height (the rendered toast is ~this tall). Click anywhere
/// in this box counts as "capture now".
const TOAST_H: f32 = 104.0;
/// Distance from the top of the window to the toast's top edge, in px.
const TOAST_TOP: f32 = 64.0;
/// Z within the overlay layer — above the palette (700).
const CONSENT_Z: f32 = 760.0;

/// One queued screenshot awaiting consent.
struct PendingShot {
    path: PathBuf,
    reason: String,
    /// Auto-capture deadline (seconds, `Time::elapsed`); set on first sight.
    deadline: Option<f64>,
}

/// Queue of screenshot requests awaiting consent, plus the overlay's spawned
/// root for diffed re-render.
#[derive(Resource, Default)]
pub struct ScreenshotConsent {
    queue: VecDeque<PendingShot>,
    root: Option<Entity>,
    last_sig: u64,
}

impl ScreenshotConsent {
    /// Enqueue a request (called from the IPC dispatch).
    pub fn request(&mut self, path: PathBuf, reason: Option<String>) {
        self.queue.push_back(PendingShot {
            path,
            reason: reason.unwrap_or_default(),
            deadline: None,
        });
    }
}

pub struct ScreenshotConsentPlugin;

impl Plugin for ScreenshotConsentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ScreenshotConsent>()
            .add_systems(Update, consent_tick)
            .add_systems(Update, render_consent.after(consent_tick));
    }
}

/// Each frame: arm the front request's deadline, handle a click on the
/// toast, and fire the capture on click-now or timeout.
fn consent_tick(
    time: Res<Time>,
    windows: Query<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut consent: ResMut<ScreenshotConsent>,
    mut consumed: ResMut<InputConsumed>,
    mut commands: Commands,
) {
    if consent.queue.is_empty() {
        return;
    }
    let now = time.elapsed_secs_f64();
    if let Some(front) = consent.queue.front_mut() {
        if front.deadline.is_none() {
            front.deadline = Some(now + CONSENT_SECS);
        }
    }

    let Ok(window) = windows.single() else { return };
    let timed_out = consent
        .queue
        .front()
        .and_then(|s| s.deadline)
        .map(|d| now >= d)
        .unwrap_or(false);

    // Click handling.
    let mut capture = timed_out;
    let mut skip = false;
    if buttons.just_pressed(MouseButton::Left) {
        if let Some(pt) = window.cursor_position() {
            let left = (window.width() - TOAST_W) * 0.5;
            let panel = Rect::from_corners(
                Vec2::new(left, TOAST_TOP),
                Vec2::new(left + TOAST_W, TOAST_TOP + TOAST_H),
            );
            // ✕ skip target: top-right corner of the toast.
            let close = Rect::from_corners(
                Vec2::new(left + TOAST_W - 34.0, TOAST_TOP + 6.0),
                Vec2::new(left + TOAST_W - 6.0, TOAST_TOP + 34.0),
            );
            if close.contains(pt) {
                skip = true;
                consumed.0 = true;
            } else if panel.contains(pt) {
                capture = true;
                consumed.0 = true;
            }
        }
    }

    if skip {
        consent.queue.pop_front();
        return;
    }
    if capture {
        // Pop BEFORE render_consent runs so the toast is gone from the frame
        // the screenshot grabs.
        if let Some(shot) = consent.queue.pop_front() {
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(shot.path));
        }
    }
}

/// Build/refresh the toast overlay for the front request (exclusive, mirrors
/// `command_palette::render_palette`).
fn render_consent(world: &mut World) {
    let (sig, has_front) = {
        let c = world.resource::<ScreenshotConsent>();
        (front_signature(world, c), c.queue.front().is_some())
    };
    let prev_root = world.resource::<ScreenshotConsent>().root;
    let last_sig = world.resource::<ScreenshotConsent>().last_sig;

    if !has_front {
        if let Some(root) = prev_root {
            let _ = world.despawn(root);
            world.resource_mut::<ScreenshotConsent>().root = None;
        }
        return;
    }
    if prev_root.is_some() && sig == last_sig {
        return;
    }
    if let Some(root) = prev_root {
        let _ = world.despawn(root);
    }

    let win_h = {
        let mut q = world.query::<&Window>();
        match q.iter(world).next() {
            Some(w) => w.height(),
            None => return,
        }
    };

    let el = build_toast(world);

    let theme = world.resource::<jim_style::Theme>().clone();
    let fonts = world.resource::<jim_style::FontRegistry>().clone();
    let font = world.resource::<PaneFont>().0.clone();
    let metrics = *world.resource::<PaneFontMetrics>();
    let colors = WidgetPalette::from_theme(&theme);

    // Top-center: world-space top-left of the toast.
    let top_left = Vec2::new(-TOAST_W * 0.5, win_h * 0.5 - TOAST_TOP);
    let root = world
        .spawn((
            Transform::from_xyz(top_left.x, top_left.y, CONSENT_Z),
            Visibility::Visible,
            RenderLayers::layer(MENU_OVERLAY_LAYER),
        ))
        .id();

    let ctx = LayoutCtx {
        font,
        metrics,
        owner_pane: root,
        content_root: root,
        content_size: Vec2::new(TOAST_W, win_h),
        palette: colors,
        theme,
        fonts,
        focused_input: None,
        caret_visible: true,
        hovered_click_id: None,
        anim: Default::default(),
    };
    let mut targets = WidgetTargets::default();
    {
        let mut commands = world.commands();
        render::render(&mut commands, &ctx, &mut targets, &el, Vec2::ZERO, TOAST_W, 0.0);
    }
    world.flush();
    stamp_layer(world, root, MENU_OVERLAY_LAYER);

    let mut c = world.resource_mut::<ScreenshotConsent>();
    c.root = Some(root);
    c.last_sig = sig;
}

/// Re-render only when the visible content changes: front identity + the
/// integer seconds remaining (so the countdown ticks).
fn front_signature(world: &World, c: &ScreenshotConsent) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(front) = c.queue.front() {
        front.path.hash(&mut h);
        front.reason.hash(&mut h);
        let now = world.resource::<Time>().elapsed_secs_f64();
        let left = front.deadline.map(|d| (d - now).ceil() as i64).unwrap_or(0);
        left.max(0).hash(&mut h);
    }
    c.queue.len().hash(&mut h);
    h.finish()
}

fn build_toast(world: &World) -> Element {
    let c = world.resource::<ScreenshotConsent>();
    let now = world.resource::<Time>().elapsed_secs_f64();
    let front = c.queue.front();
    let reason = front
        .map(|s| s.reason.clone())
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "(no description)".to_string());
    let left = front
        .and_then(|s| s.deadline)
        .map(|d| (d - now).ceil().max(0.0) as i64)
        .unwrap_or(CONSENT_SECS as i64);
    let queued = c.queue.len();

    let mut header_row = vec![
        frame_grow(vec![text("📸 Screenshot requested", "fg", 15.0, Weight::Bold)]),
        text("✕", "fg_muted", 15.0, Weight::Bold),
    ];
    if queued > 1 {
        header_row.insert(1, text(&format!("+{} more  ", queued - 1), "fg_muted", 12.0, Weight::Normal));
    }

    let footer = format!("click to capture now · ✕ to skip · auto in {left}s");

    Element::Frame {
        gap: 8.0,
        pad: 0.0,
        children: vec![
            Element::Hstack {
                gap: 8.0,
                pad: 0.0,
                align: Align::Center,
                children: header_row,
                style: Some(Style { width: Some("100%".into()), ..Default::default() }),
            },
            text(&reason, "fg_muted", 13.0, Weight::Normal),
            text(&footer, "accent", 12.0, Weight::Normal),
        ],
        style: Some(Style {
            background: Some("surface_2".into()),
            radius: Some("radius_lg".into()),
            border: Some(Border { color: "accent".into(), width: 1.0 }),
            padding: Some(Edges::all(14.0)),
            width: Some(format!("{}", TOAST_W as i32)),
            shadow: Some(Shadow { token: Some("shadow_lg".into()), ..Default::default() }),
            ..Default::default()
        }),
    }
}

fn text(s: &str, color: &str, size: f32, weight: Weight) -> Element {
    Element::Text {
        value: s.to_string(),
        color: Some(color.into()),
        size: Some(size),
        weight: Some(weight),
        family: None,
        selectable: false,
    }
}

fn frame_grow(children: Vec<Element>) -> Element {
    Element::Frame {
        gap: 0.0,
        pad: 0.0,
        children,
        style: Some(Style { flex_grow: Some(1.0), ..Default::default() }),
    }
}

fn stamp_layer(world: &mut World, root: Entity, layer: usize) {
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        let kids: Vec<Entity> = world
            .get::<Children>(e)
            .map(|c| c.iter().collect::<Vec<Entity>>())
            .unwrap_or_default();
        if let Ok(mut em) = world.get_entity_mut(e) {
            em.insert(RenderLayers::layer(layer));
        }
        stack.extend(kids);
    }
}
